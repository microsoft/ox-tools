// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(any(unix, windows))]
use core::cell::UnsafeCell;
#[cfg(unix)]
use core::ffi::c_char;
#[cfg(any(unix, windows))]
use core::ffi::{c_int, c_void};
#[cfg(any(unix, windows, test))]
use core::num::NonZeroUsize;
#[cfg(all(any(unix, windows), not(all(loom, feature = "loom"))))]
use core::sync::atomic::AtomicUsize as RecorderAtomicUsize;
#[cfg(any(unix, windows))]
use core::sync::atomic::{AtomicBool, AtomicUsize};
use core::sync::atomic::{AtomicU32, Ordering};

#[cfg(all(any(unix, windows), loom, feature = "loom"))]
use loom::sync::atomic::AtomicUsize as RecorderAtomicUsize;

/// The ordinal reserved for "no mutant is active".
///
/// Mutant ordinals are 1-based so that an unset or unparsable `GAMMA_ACTIVE` is indistinguishable
/// from an explicit request for unmutated behavior.
///
/// ```rust
/// assert_eq!(gamma_rt::NONE, 0);
/// ```
pub const NONE: u32 = 0;

/// The environment variable naming the active mutant.
///
/// Its value is a decimal mutant ordinal. Setting it to [`NONE`], to nothing, or to something that
/// is not a number all select unmutated behavior.
///
/// ```rust
/// assert_eq!(gamma_rt::ACTIVE_VAR, "GAMMA_ACTIVE");
/// ```
pub const ACTIVE_VAR: &str = "GAMMA_ACTIVE";

/// The environment variable that puts a process into census mode.
///
/// Its value is the path of the file where the process writes the ordinals it reached. Setting it
/// selects unmutated behavior — a census runs the code the author wrote — and turns every guard
/// into a probe that records the site it stands at.
///
/// The path is fetched and opened through the platform's native encoding — `getenv` and `fopen` on
/// Unix, the wide Win32 and CRT entry points on Windows — so a scratch tree whose path lies outside
/// the active ANSI code page is opened like any other. A path that cannot be opened at all costs
/// only speed: the reader treats an absent census as a spoiled sample rather than as a run that
/// reached nothing, so no mutant is convicted or acquitted on the strength of it.
///
/// ```rust
/// assert_eq!(gamma_rt::CENSUS_VAR, "GAMMA_CENSUS");
/// ```
pub const CENSUS_VAR: &str = "GAMMA_CENSUS";

/// The variable's name as the C interfaces below want it: NUL-terminated, and a compile-time
/// constant so that asking for it costs nothing.
///
/// Absent on a platform with neither interface, which has nothing to pass it to.
#[cfg(any(unix, windows))]
const ACTIVE_VAR_C: &[u8] = b"GAMMA_ACTIVE\0";

/// [`CENSUS_VAR`] in the same form, for the same reason.
#[cfg(any(unix, windows))]
const CENSUS_VAR_C: &[u8] = b"GAMMA_CENSUS\0";

/// [`CENSUS_VAR`] as UTF-16, which is the form the wide Win32 and CRT entry points want.
///
/// The census *path* has to be fetched and opened through the wide interfaces: a scratch tree
/// under a user profile whose name lies outside the active ANSI code page either loses characters
/// on the way through the narrow ones or cannot be opened at all, and the census then silently
/// never works on that host. Widening the variable's own name is the price of asking for it that
/// way, and it costs nothing at run time — it is a constant.
#[cfg(windows)]
const CENSUS_VAR_W: [u16; CENSUS_VAR_C.len()] = widen(CENSUS_VAR_C);

/// Widens an ASCII byte string, terminator included, to UTF-16 at compile time.
///
/// Only ASCII is passed to it — the two constants it builds are a variable name and an `fopen`
/// mode — for which widening is a byte-wise zero extension. There is no allocator here and no
/// dependency to reach for, so the alternative would be spelling the code units out by hand.
///
/// Fails to compile if `ascii` is shorter than `N`, which is the check that keeps the two in step.
#[cfg(windows)]
const fn widen<const N: usize>(ascii: &[u8]) -> [u16; N] {
    let mut wide = [0_u16; N];
    let mut index = 0;

    while index < N {
        // `u16::from` is not a const function, so the widening is spelled as a cast; from `u8` it
        // is lossless whatever the value.
        wide[index] = ascii[index] as u16;
        index += 1;
    }

    wide
}

/// The pseudo-ordinal that means "record every site reached, and activate nothing".
///
/// Census mode rides on the same startup-captured selection as mutant selection so that the guard
/// keeps its single load. `parse` refuses this value because a mode is not something
/// `GAMMA_ACTIVE` may ask for.
const CENSUS: u32 = u32::MAX - 1;

/// How many sites a census can record.
///
/// This is the shared coordinator/runtime protocol maximum. A population past it is not silently
/// half-recorded: a site over the edge records [`OVERFLOW`], which tells the coordinator to discard
/// the whole census rather than mistake an unrecorded site for an unreached one.
pub const MAX_CENSUS_SITES: usize = 1 << 20;

/// The number of site bits retained by the runtime's private bitmap.
///
/// One bit per site makes the zeroed table occupy 128 KiB of address space, with pages committed
/// only when a census reaches the corresponding sites.
#[cfg(any(unix, windows))]
const SITES: usize = MAX_CENSUS_SITES;

#[cfg(any(unix, windows))]
const WORD_BITS: usize = 32;

/// The sites reached by this process, serialized to the census file only when it exits.
#[cfg(any(unix, windows))]
static REACHED: [AtomicU32; SITES / WORD_BITS] = [const { AtomicU32::new(0) }; SITES / WORD_BITS];

/// Whether a reached site was too large to fit in [`REACHED`].
#[cfg(any(unix, windows))]
static OVERFLOWED: AtomicBool = AtomicBool::new(false);

/// The marker record meaning "this census is incomplete, do not trust it".
///
/// `u32::MAX` is not an ordinal — `parse` refuses it — so it cannot collide with a real record.
pub const OVERFLOW: u32 = u32::MAX;

/// The marker record written last, at a clean exit, meaning the census is whole.
///
/// The runtime writes this record last, at process exit, so a reader that finds it intact
/// knows the whole buffered bitmap was serialized first. Its absence — a truncated write, a crash
/// before exit, or a failed open — stops the reader mistaking a missing record for an unreached
/// site: an unsealed file is discarded whole rather than believed in part.
///
/// `u32::MAX - 2` is not an ordinal — `parse` refuses it alongside the overflow marker and
/// `CENSUS`, so the environment cannot name it — and it is neither [`OVERFLOW`] nor a site id:
/// `note` diverts every id at or above `SITES` to the [`OVERFLOW`] path, so no larger id ever
/// reaches the file. That bound is what actually keeps the encoding unambiguous, and widening
/// `SITES` past `u32::MAX - 2` would break it. It is also not `CENSUS`, which never reaches the
/// file. The reader in `cargo-gamma-lib` imports this marker from the runtime so the protocol has
/// one source of truth.
pub const SEAL: u32 = u32::MAX - 2;

/// The longest census path this will retain, in native code units.
///
/// It is long enough for ordinary Unix and Win32 paths, and bounded because startup copies the
/// path into static storage without an allocator. A path beyond this bound leaves the census
/// unsealed, which prevents the coordinator from trusting a partial result.
#[cfg(any(unix, windows))]
const PATH_LIMIT: usize = 4096;

/// The longest value worth reading: ten digits is every `u32`, and the rest is room for spaces.
#[cfg(any(unix, windows))]
const READ_LIMIT: usize = 32;

/// The pseudo-ordinal meaning "`install` has not run on this platform yet".
///
/// Distinct from [`NONE`], which is the correct, permanent value on a target with no `unix` or
/// `windows` constructor mechanism at all — a genuinely non-hosted target that never links
/// `install`, documented at the crate root under "`no_std`, and what it does not buy" — and, for
/// the same reason, under Miri, which cannot execute a real ELF, Mach-O, or PE constructor and so
/// never runs `install` either. This sentinel instead marks the narrow, transient window on a
/// hosted, non-Miri target between process start and this crate's own constructor running, so a
/// guard reached in that window is not mistaken for a legitimate unmutated run: see [`selected`].
///
/// Refused by [`parse`] alongside [`OVERFLOW`], [`CENSUS`], and [`SEAL`], so the environment cannot
/// name it either.
#[cfg(any(unix, windows))]
const UNINSTALLED: u32 = u32::MAX - 3;

/// The selection captured by `install` before user code can start threads.
///
/// On a hosted, non-Miri target this starts at [`UNINSTALLED`]: the only other
/// constructor-ordering signal available is [`NONE`], which is indistinguishable from a legitimate
/// request for unmutated behavior and would let a guard reached before `install` silently
/// misreport a mutant or census run as a baseline one. [`selected`] turns an observation of
/// [`UNINSTALLED`] into immediate, non-unwinding process termination instead. A target with neither
/// `unix` nor `windows`, or a Miri execution of either, never links `install` at all, so it starts
/// at, and stays at, the actually-correct [`NONE`] fallback described at the crate root.
#[cfg(all(any(unix, windows), not(miri)))]
static ACTIVE: AtomicU32 = AtomicU32::new(UNINSTALLED);

/// The selection on a target — or a Miri execution — with no working `install` constructor,
/// which is documented at the crate root as the permanently safe fallback.
#[cfg(any(not(any(unix, windows)), miri))]
static ACTIVE: AtomicU32 = AtomicU32::new(NONE);

/// The captured census path's length, excluding its terminator.
///
/// A zero length means no usable path was captured. The path and the NUL terminator that follows
/// it are both written before this is published with `Release`, and [`open`] acquires it before
/// reading the corresponding static buffer — so a non-zero length is also the promise that a
/// terminator sits at that offset.
#[cfg(any(unix, windows))]
static CENSUS_PATH_LENGTH: AtomicUsize = AtomicUsize::new(0);

/// Native census-path storage captured during process startup.
///
/// The constructor is the only writer, and it publishes completed bytes through
/// [`CENSUS_PATH_LENGTH`] before any later reader can observe them. `UnsafeCell` is needed only to
/// initialize no-allocator static storage; its contained bytes are never mutated after publication.
#[cfg(unix)]
struct CensusPath {
    bytes: UnsafeCell<[u8; PATH_LIMIT]>,
}

#[cfg(unix)]
// SAFETY: the constructor is the sole writer and publishes the completed buffer before readers
// access it; no code mutates it thereafter.
unsafe impl Sync for CensusPath {}

#[cfg(unix)]
static CENSUS_PATH: CensusPath = CensusPath {
    bytes: UnsafeCell::new([0; PATH_LIMIT]),
};

/// Windows' UTF-16 variant of [`CensusPath`].
#[cfg(windows)]
struct CensusPath {
    bytes: UnsafeCell<[u16; PATH_LIMIT]>,
}

#[cfg(windows)]
// SAFETY: as for the Unix variant above.
unsafe impl Sync for CensusPath {}

#[cfg(windows)]
static CENSUS_PATH: CensusPath = CensusPath {
    bytes: UnsafeCell::new([0; PATH_LIMIT]),
};

/// Counts platform environment reads in unit-test builds.
///
/// This is a deliberately narrow seam: every runtime environment access increments it, so the
/// regression below proves guard calls cannot accidentally reintroduce a lazy environment read.
#[cfg(all(test, any(unix, windows)))]
static STARTUP_ENVIRONMENT_READS: AtomicUsize = AtomicUsize::new(0);

/// Captures selection and the optional census path while the process is still starting.
///
/// This is called only by the loader/CRT constructor below. Linux reads the immutable environment
/// image captured by `exec`, so an earlier native constructor may start threads without racing this
/// capture; other platforms use their startup environment API. Guard calls only load [`ACTIVE`]
/// afterwards.
#[cfg(any(unix, windows))]
fn capture_selection() -> u32 {
    match selection_from(capture_census_path(), capture_active) {
        Ok(selection) => selection,
        Err(()) => environment_error(),
    }
}

/// Turns what startup learned about `GAMMA_CENSUS` into a selection, or into a startup failure.
///
/// Census mode takes precedence over an active ordinal, so `active` is consulted only when the
/// census variable was genuinely absent.
///
/// A census request this process could not read is **not** absence, and must not fall through to
/// active selection: the process would then execute a mutant, write no census file, and report a
/// baseline failure the coordinator would read as a fact about that mutant. It is reported as a
/// startup failure instead, which [`capture_selection`] turns into the environment-error protocol.
///
/// Taking the census answer and the active read as parameters — rather than performing them — is
/// what lets a test drive every combination, including native API failures that cannot be produced
/// on demand.
#[cfg(any(unix, windows))]
fn selection_from(census: CensusRequest, active: impl FnOnce() -> Result<u32, ()>) -> Result<u32, ()> {
    match census {
        CensusRequest::Absent => active(),
        // A path too long to retain still selects census mode: the file simply cannot be opened,
        // so the census stays unsealed and the reader rejects it, which is the conservative answer.
        CensusRequest::Path(_) | CensusRequest::Unusable => Ok(CENSUS),
        CensusRequest::Error => Err(()),
    }
}

/// Marker emitted when the runtime cannot acquire the startup environment.
///
/// `cargo-gamma-lib` recognizes this exact byte sequence before interpreting a test runner's exit
/// status. It is public because the vendored runtime and its parent must share one protocol value.
pub const ENVIRONMENT_ERROR_MARKER: &[u8] = b"cargo-gamma: startup environment acquisition failed\n";

/// The diagnostic emitted when instrumented code runs before this runtime's constructor.
pub const PRE_INSTALL_ERROR_MARKER: &[u8] =
    b"gamma_rt: a guard executed before this crate's own constructor installed the runtime selection\n";

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvironmentValue {
    Found(usize),
    Absent,
    Error,
}

/// What startup learned about `GAMMA_CENSUS`, which is not the same question as what it holds.
///
/// Absence and failure require opposite decisions: absence permits active-mutant selection, while
/// failure must stop startup before a run can produce evidence under the wrong mode.
#[cfg(any(unix, windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CensusRequest {
    /// `GAMMA_CENSUS` was unset, or set to nothing: this process is not taking a census.
    Absent,

    /// Census mode, with a NUL-terminated path of this non-zero length retained in [`CENSUS_PATH`].
    Path(NonZeroUsize),

    /// Census mode, with a path too long for [`PATH_LIMIT`] to retain and therefore never opened.
    Unusable,

    /// The startup environment could not be read, so nothing is known about this variable.
    Error,
}

/// Copies a bounded C-string prefix into `destination`, returning its length without the terminator.
///
/// # Safety
///
/// `source` must be valid to read the initialized bytes this function examines, and those bytes
/// must remain valid and unmodified for this call. At most `destination.len()` consecutive bytes
/// are examined: the first NUL ends the scan, or all of `destination` is read when no NUL occurs.
/// Thus the source need not be NUL-terminated; a non-terminated source need only be readable for
/// `destination.len()` bytes. An empty destination does not read `source`.
#[cfg(all(unix, any(test, not(target_os = "linux"))))]
unsafe fn copy_c_string(source: *const u8, destination: &mut [u8]) -> Option<usize> {
    for (length, slot) in destination.iter_mut().enumerate() {
        // SAFETY: the caller made every byte examined by this bounded scan readable, and `length`
        // stays within the destination bound.
        let at = unsafe { source.add(length) };

        // SAFETY: `at` points at the corresponding readable source byte. The terminator is read
        // before the corresponding destination slot is used, so no byte beyond it is accessed.
        let byte = unsafe { *at };

        if byte == 0 {
            *slot = 0;
            return Some(length);
        }

        *slot = byte;
    }

    None
}

/// Reads and copies `GAMMA_ACTIVE` during startup without allocation.
#[cfg(unix)]
fn capture_active() -> Result<u32, ()> {
    let mut buffer = [0_u8; READ_LIMIT];

    let length = match copy_environment(ACTIVE_VAR_C, &mut buffer) {
        EnvironmentValue::Found(length) => length,
        EnvironmentValue::Absent => return Ok(NONE),
        EnvironmentValue::Error => return Err(()),
    };

    if length == buffer.len() {
        return Ok(NONE);
    }

    Ok(parse(&buffer[..length]))
}

/// What a raw `GetEnvironmentVariable{A,W}` return means against a buffer of `buffer_len` units.
///
/// Isolated from the syscall — see [`environment_read_outcome`] — so the exact-fit and
/// one-past-it boundaries can be driven directly by a test instead of only by however long a
/// variable actually set in the test process happens to be. The unit is whatever the caller's
/// buffer holds — `u8` for the ANSI entry point, `u16` for the wide one — since the boundary
/// arithmetic does not depend on which.
#[cfg(any(windows, test))]
#[derive(Debug, Eq, PartialEq)]
enum EnvironmentReadOutcome {
    /// The variable was unset.
    Absent,
    /// The variable was set to an empty value.
    Empty,
    /// The API failed for a reason other than the variable being absent.
    Error,
    /// The variable was set, but its reported length does not fit the buffer.
    TooLongToStore,
    /// The variable was set, fits, and has this non-zero length.
    Found(NonZeroUsize),
}

/// Successful Win32 last-error value, used to distinguish an empty value from API failure.
#[cfg(any(windows, test))]
const ERROR_SUCCESS: u32 = 0;

/// The Win32 `ERROR_ENVVAR_NOT_FOUND` value.
#[cfg(any(windows, test))]
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

/// `GetEnvironmentVariable{A,W}` returns zero both for an empty or absent value and for an API
/// failure. `last_error` is therefore meaningful only when `written` is zero: the caller clears it
/// immediately before the call and reads it immediately afterwards.
#[cfg(any(windows, test))]
fn environment_read_outcome(written: u32, last_error: u32, buffer_len: usize) -> EnvironmentReadOutcome {
    if written == 0 {
        return match last_error {
            ERROR_SUCCESS => EnvironmentReadOutcome::Empty,
            ERROR_ENVVAR_NOT_FOUND => EnvironmentReadOutcome::Absent,
            _ => EnvironmentReadOutcome::Error,
        };
    }

    let Ok(length) = usize::try_from(written) else {
        return EnvironmentReadOutcome::Error;
    };

    if length >= buffer_len {
        return EnvironmentReadOutcome::TooLongToStore;
    }

    EnvironmentReadOutcome::Found(NonZeroUsize::new(length).expect("the zero return was handled before converting the reported length"))
}

/// Reads `GAMMA_ACTIVE` during startup without allocation.
#[cfg(windows)]
fn capture_active() -> Result<u32, ()> {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let mut buffer = [0_u8; READ_LIMIT];

    // SAFETY: clearing the calling thread's last-error value has no precondition.
    unsafe { SetLastError(ERROR_SUCCESS) };

    // SAFETY: `ACTIVE_VAR_C` is NUL-terminated and `buffer` is writable for the supplied length.
    let written = unsafe {
        GetEnvironmentVariableA(
            ACTIVE_VAR_C.as_ptr(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    let last_error = if written == 0 {
        // SAFETY: read immediately after the API call whose zero return needs classifying.
        unsafe { GetLastError() }
    } else {
        ERROR_SUCCESS
    };

    match environment_read_outcome(written, last_error, buffer.len()) {
        EnvironmentReadOutcome::Absent | EnvironmentReadOutcome::Empty | EnvironmentReadOutcome::TooLongToStore => Ok(NONE),
        EnvironmentReadOutcome::Error => Err(()),
        EnvironmentReadOutcome::Found(length) => Ok(parse(&buffer[..length.get()])),
    }
}

/// Captures a non-empty `GAMMA_CENSUS` path and reports what the environment said about it.
///
/// A requested path that does not fit static storage still selects census mode; it simply cannot
/// be opened, and therefore remains unsealed and is rejected by the reader.
#[cfg(unix)]
fn capture_census_path() -> CensusRequest {
    // SAFETY: only this constructor writes the `UnsafeCell`, before the release publication below.
    let path = unsafe { &mut *CENSUS_PATH.bytes.get() };
    let value = copy_environment(CENSUS_VAR_C, path);
    let request = terminate_census_path(path, value);

    if let CensusRequest::Path(length) = request {
        CENSUS_PATH_LENGTH.store(length.get(), Ordering::Release);
    }

    request
}

/// Terminates a captured census path in place, and says what was captured.
///
/// The terminator is written here rather than assumed, and that is the whole point of this
/// function. Linux copies value bytes out of `/proc/self/environ` and stops at the value's own
/// NUL without storing it, so a captured path is followed only by whatever the destination already
/// held — today the initial zero of a static, which is not a property the code that reads the path
/// back can check. `fopen` reads until it finds a NUL, so the invariant it depends on is written
/// explicitly, on the one path that publishes a length for it to trust.
///
/// A value that fills the buffer exactly has nowhere to put a terminator, so it is reported as
/// [`CensusRequest::Unusable`] and no length is published; nothing is written past the buffer, and
/// [`open`] refuses to open a census with no published length.
///
/// Taking the environment answer as a parameter — rather than performing the read — is what lets a
/// test drive the `PATH_LIMIT - 1` boundary against a buffer it pre-filled with non-zero bytes,
/// which is the case a zeroed static would otherwise hide.
#[cfg(unix)]
fn terminate_census_path(path: &mut [u8; PATH_LIMIT], value: EnvironmentValue) -> CensusRequest {
    match value {
        EnvironmentValue::Absent | EnvironmentValue::Found(0) => CensusRequest::Absent,
        EnvironmentValue::Error => CensusRequest::Error,
        EnvironmentValue::Found(length) => match path.get_mut(length) {
            Some(terminator) => {
                *terminator = 0;

                CensusRequest::Path(
                    NonZeroUsize::new(length).expect("the zero-length environment value was handled before path termination"),
                )
            }

            // The value filled the buffer, so it was truncated and there is no room to terminate
            // what remains. Census mode still holds; the path does not.
            None => CensusRequest::Unusable,
        },
    }
}

/// Windows' equivalent of [`capture_census_path`].
#[cfg(windows)]
fn capture_census_path() -> CensusRequest {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let mut buffer = [0_u16; PATH_LIMIT];

    // SAFETY: clearing the calling thread's last-error value has no precondition.
    unsafe { SetLastError(ERROR_SUCCESS) };

    // SAFETY: `CENSUS_VAR_W` is NUL-terminated and `buffer` is writable for the supplied length.
    let written = unsafe {
        GetEnvironmentVariableW(
            CENSUS_VAR_W.as_ptr(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    let last_error = if written == 0 {
        // SAFETY: read immediately after the API call whose zero return needs classifying.
        unsafe { GetLastError() }
    } else {
        ERROR_SUCCESS
    };

    match environment_read_outcome(written, last_error, buffer.len()) {
        EnvironmentReadOutcome::Absent | EnvironmentReadOutcome::Empty => CensusRequest::Absent,
        EnvironmentReadOutcome::Error => CensusRequest::Error,
        EnvironmentReadOutcome::TooLongToStore => CensusRequest::Unusable,
        EnvironmentReadOutcome::Found(length) => {
            // SAFETY: only this constructor writes the `UnsafeCell`, and `length` bytes plus the
            // API's terminator all fit in both arrays.
            unsafe {
                core::ptr::copy_nonoverlapping(buffer.as_ptr(), CENSUS_PATH.bytes.get().cast(), length.get() + 1);
            }
            CENSUS_PATH_LENGTH.store(length.get(), Ordering::Release);

            CensusRequest::Path(length)
        }
    }
}

/// Parses an ordinal out of copied bytes, treating anything unexpected as [`NONE`].
///
/// Surrounding ASCII whitespace is tolerated because a value threaded through a shell can pick it
/// up. Everything else — an empty value, a sign, a non-digit, a number too large to be an ordinal
/// — selects unmutated behavior, which is the answer that cannot turn a mutated program into a
/// passing one.
///
/// The reserved words of the encoding — [`UNINSTALLED`], the census file's [`SEAL`], [`CENSUS`],
/// and the overflow marker — are refused as one contiguous range: a population would
/// have to hold four billion mutants to reach them, and a startup sentinel, a mode, or a file
/// marker is not something `GAMMA_ACTIVE` may ask for.
#[cfg(any(unix, windows))]
fn parse(bytes: &[u8]) -> u32 {
    let trimmed = bytes.trim_ascii();

    if trimmed.is_empty() {
        return NONE;
    }

    let mut value = 0_u32;

    for byte in trimmed {
        let Some(digit) = (*byte as char).to_digit(10) else {
            return NONE;
        };

        let Some(shifted) = value.checked_mul(10).and_then(|shifted| shifted.checked_add(digit)) else {
            return NONE;
        };

        value = shifted;
    }

    if value >= UNINSTALLED { NONE } else { value }
}

/// One in-progress scan of an environ-format (`NAME=value\0NAME=value\0...`) byte stream for the
/// entry named by a `target` key, fed one chunk at a time.
///
/// Kept separate from the syscalls that supply its bytes — see [`scan_environ`] — so a chunk
/// boundary landing inside a key, the `=` delimiter, a value, or its terminating NUL can be driven
/// directly by a test instead of only by whatever one `read` of `/proc/self/environ` happens to
/// return.
#[cfg(target_os = "linux")]
struct EnvironScan {
    key_at: usize,
    matching: bool,
    value_at: Option<usize>,
    overflowed: bool,
}

#[cfg(target_os = "linux")]
impl EnvironScan {
    const fn new() -> Self {
        Self {
            key_at: 0,
            matching: true,
            value_at: None,
            overflowed: false,
        }
    }

    /// Feeds one chunk of environ bytes, matching against `target` and copying a matched value's
    /// bytes into `destination`. Returns the value's length, bounded by `destination.len()`, once
    /// this or an earlier chunk has reached the value's terminating NUL.
    fn feed(&mut self, chunk: &[u8], target: &[u8], destination: &mut [u8]) -> Option<usize> {
        for &byte in chunk {
            if let Some(length) = self.value_at.as_mut() {
                if byte == 0 {
                    return Some(if self.overflowed || *length == destination.len() {
                        destination.len()
                    } else {
                        *length
                    });
                }

                if let Some(slot) = destination.get_mut(*length) {
                    *slot = byte;
                    *length += 1;
                } else {
                    self.overflowed = true;
                }

                continue;
            }

            if byte == 0 {
                self.key_at = 0;
                self.matching = true;
            } else if self.matching && self.key_at < target.len() && byte == target[self.key_at] {
                self.key_at += 1;
            } else if self.matching && self.key_at == target.len() && byte == b'=' {
                self.value_at = Some(0);
            } else {
                self.matching = false;
            }
        }

        None
    }
}

/// What one attempt at reading the startup environment produced.
///
/// A signal that arrives while a `read` is blocked makes it return `-1` with `EINTR` and no bytes
/// moved. Folding that into the same failure as a real I/O error would turn an ordinary, retryable
/// interruption into a refused startup, so the two are distinguished here and only one of them is
/// worth another attempt.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadOutcome {
    /// This many bytes were placed in the chunk buffer; zero means the stream ended.
    Read(usize),

    /// A signal interrupted the read before it moved anything, so repeating it is meaningful.
    Interrupted,

    /// The read failed for a reason repeating it cannot fix.
    Failed,
}

/// Classifies one raw `read` return, consulting `errno` only when the call actually failed.
///
/// `errno` is supplied rather than read here so a test can drive the interrupted and final-failure
/// arms directly, and so a successful read cannot pay for a thread-local lookup it does not need.
#[cfg(target_os = "linux")]
fn read_outcome(read: isize, errno: impl FnOnce() -> c_int) -> ReadOutcome {
    match usize::try_from(read) {
        Ok(read_len) => ReadOutcome::Read(read_len),
        Err(_negative) if errno() == EINTR => ReadOutcome::Interrupted,
        Err(_negative) => ReadOutcome::Failed,
    }
}

/// How many interrupted attempts one startup capture tolerates before reporting a failure.
///
/// Counted across the whole capture rather than reset per call, so a process being signalled
/// without pause ends in a loud, bounded failure rather than spinning inside a loader constructor
/// forever. This is a deliberately conservative policy limit rather than a benchmark-derived
/// threshold: it tolerates a short burst of dozens of signals, while values in the same
/// power-of-two range from 32 through 256 preserve the intended trade-off between ordinary burst
/// tolerance and bounded pre-main delay.
#[cfg(target_os = "linux")]
const INTERRUPTED_ATTEMPTS: usize = 64;

/// The Linux UAPI-defined `EINTR` value from `<asm-generic/errno-base.h>`.
///
/// Spelled out because this crate carries no dependencies, `libc` among them.
#[cfg(target_os = "linux")]
const EINTR: c_int = 4;

/// Bytes read from `/proc/self/environ` per syscall.
///
/// The size is deliberately a modest power-of-two policy choice rather than a platform limit. It
/// keeps the native-constructor stack buffer small while reading ordinary environment images in a
/// handful of syscalls; any non-zero size preserves the framing algorithm.
#[cfg(target_os = "linux")]
const ENVIRONMENT_READ_CHUNK: usize = 512;

/// The `errno` the last failed C library call left behind on this thread.
#[cfg(target_os = "linux")]
fn last_errno() -> c_int {
    // SAFETY: `__errno_location` returns a pointer to this thread's `errno`, which the C library
    // keeps valid for as long as the thread exists.
    let location = unsafe { __errno_location() };

    // SAFETY: exactly the one `int` that pointer names is read, and nothing else on this thread can
    // be writing it between the call above and here.
    unsafe { *location }
}

/// Drives one [`EnvironScan`] across a sequence of read attempts, matching the framing and failure
/// handling of the real `/proc/self/environ` loop. `interruptions` carries attempts already spent
/// opening the environment image, so the read phase cannot replenish the whole-capture budget.
///
/// Isolated from `open_fd`/`read_fd` so a test can script exactly the split, empty, interrupted and
/// failure transitions the real loop cannot be steered through without controlling the file it
/// reads and the signals delivered to the process reading it.
#[cfg(target_os = "linux")]
fn scan_environ_with_interruptions(
    target: &[u8],
    destination: &mut [u8],
    mut interruptions: usize,
    mut read: impl FnMut(&mut [u8; ENVIRONMENT_READ_CHUNK]) -> ReadOutcome,
) -> EnvironmentValue {
    let mut chunk = [0_u8; ENVIRONMENT_READ_CHUNK];
    let mut scan = EnvironScan::new();

    loop {
        let read_len = match read(&mut chunk) {
            ReadOutcome::Read(read_len) => read_len,
            ReadOutcome::Interrupted => {
                interruptions += 1;

                if interruptions > INTERRUPTED_ATTEMPTS {
                    return EnvironmentValue::Error;
                }

                continue;
            }
            ReadOutcome::Failed => return EnvironmentValue::Error,
        };

        if read_len == 0 {
            return EnvironmentValue::Absent;
        }

        if let Some(length) = scan.feed(&chunk[..read_len], target, destination) {
            return EnvironmentValue::Found(length);
        }
    }
}

/// Test-facing scan that starts with an unused interruption budget.
#[cfg(all(test, target_os = "linux"))]
fn scan_environ(
    target: &[u8],
    destination: &mut [u8],
    read: impl FnMut(&mut [u8; ENVIRONMENT_READ_CHUNK]) -> ReadOutcome,
) -> EnvironmentValue {
    scan_environ_with_interruptions(target, destination, 0, read)
}

/// Copies one Linux environment value from the immutable image captured by `exec`.
///
/// `/proc/self/environ` does not follow later `setenv` changes, so a native constructor that
/// started a thread before this constructor cannot make this read race with environment mutation.
///
/// A signal delivered to this process can interrupt the open or any of the reads without moving a
/// byte. Those attempts are repeated, bounded by [`INTERRUPTED_ATTEMPTS`], because such an
/// interruption is transient and does not make the captured startup environment untrustworthy.
#[cfg(target_os = "linux")]
fn copy_environment(name: &[u8], destination: &mut [u8]) -> EnvironmentValue {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let Some(target) = name.strip_suffix(&[0]) else {
        return EnvironmentValue::Absent;
    };
    let path = b"/proc/self/environ\0";
    let mut interruptions = 0_usize;

    let descriptor = loop {
        // SAFETY: `path` is NUL-terminated and `O_RDONLY` takes no variadic mode argument.
        let descriptor = unsafe { open_fd(path.as_ptr().cast(), 0) };

        if descriptor >= 0 {
            break descriptor;
        }

        interruptions += 1;

        if last_errno() != EINTR || interruptions > INTERRUPTED_ATTEMPTS {
            return EnvironmentValue::Error;
        }
    };

    let answer = scan_environ_with_interruptions(target, destination, interruptions, |chunk| {
        // SAFETY: `chunk` is writable for the supplied length, and `descriptor` remains open.
        let read = unsafe { read_fd(descriptor, chunk.as_mut_ptr().cast(), chunk.len()) };

        read_outcome(read, last_errno)
    });

    // SAFETY: `descriptor` was returned open above and is closed exactly once here.
    let _closed = unsafe { close_fd(descriptor) };

    answer
}

/// Copies one environment value on Unix targets without an immutable process-environment image.
///
/// Delegates to [`copy_environment_via_getenv`], named separately so the concurrency regression
/// below can call it directly on every Unix this crate tests, including Linux, even though
/// production Linux never takes this path.
#[cfg(all(unix, not(target_os = "linux")))]
fn copy_environment(name: &[u8], destination: &mut [u8]) -> EnvironmentValue {
    copy_environment_via_getenv(name, destination)
}

/// Copies one environment value through `getenv`, the only startup interface available on a Unix
/// with no immutable process-environment image.
///
/// # POSIX precondition
///
/// POSIX permits `getenv` to be called concurrently with other environment reads, but not with
/// native environment mutation through `setenv`, `putenv`, `unsetenv`, or equivalent direct
/// mutation. This safe function relies on that process-wide precondition. It runs before Rust
/// `main`, so safe Rust code has not had an opportunity to start a thread that performs such a
/// mutation; Rust's process-environment mutation APIs are unsafe for the same reason. A foreign
/// native constructor that starts concurrent environment mutation before this constructor violates
/// the abstraction's precondition and is outside what this runtime can make sound.
///
/// # Integrity detection
///
/// The second `getenv` and copy are an integrity check, not a memory-safety proof. Under the POSIX
/// precondition, each dereference is already valid. Comparing the pointer, length, and bytes detects
/// an inconsistent observation if foreign code violates that precondition in a way visible to both
/// reads, allowing startup to emit the environment-error marker and terminate through
/// [`environment_error`] rather than accepting a torn value. Matching reads do not prove that a
/// forbidden concurrent mutation did not occur.
#[cfg(all(unix, any(test, not(target_os = "linux"))))]
fn copy_environment_via_getenv(name: &[u8], destination: &mut [u8]) -> EnvironmentValue {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    // SAFETY: every caller supplies one of this module's NUL-terminated constant names.
    let first_pointer = unsafe { getenv(name.as_ptr().cast()) };

    if first_pointer.is_null() {
        // A lone null answer could itself be a torn observation of a variable another thread is
        // concurrently setting for the first time, so absence is trusted only once a second,
        // independent call agrees.
        // SAFETY: as above.
        return if unsafe { getenv(name.as_ptr().cast()) }.is_null() {
            EnvironmentValue::Absent
        } else {
            EnvironmentValue::Error
        };
    }

    // SAFETY: under the POSIX precondition documented above, no concurrent native mutation can
    // invalidate the storage returned by `getenv` during this bounded copy.
    let first_length = unsafe { copy_c_string(first_pointer.cast(), destination) };

    // SAFETY: as above; `getenv` may be called at any time.
    let second_pointer = unsafe { getenv(name.as_ptr().cast()) };

    if second_pointer != first_pointer {
        return EnvironmentValue::Error;
    }

    let mut confirmation = [0_u8; PATH_LIMIT];
    let region = &mut confirmation[..destination.len()];

    // SAFETY: under the POSIX precondition documented above, `second_pointer` remains valid during
    // this bounded copy; `region` has exactly `destination`'s length.
    let second_length = unsafe { copy_c_string(second_pointer.cast(), region) };

    if second_length != first_length || *destination != *region {
        return EnvironmentValue::Error;
    }

    EnvironmentValue::Found(first_length.unwrap_or(destination.len()))
}

/// Stops startup in a shape the parent recognizes as an infrastructure failure.
///
/// Excluded from coverage because immediate process termination cannot flush coverage counters.
#[cfg(any(unix, windows))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn environment_error() -> ! {
    terminate_with_marker(ENVIRONMENT_ERROR_MARKER)
}

/// Writes a fixed startup diagnostic and terminates without unwinding or running exit handlers.
///
/// Excluded from coverage because immediate process termination cannot flush coverage counters.
#[cfg(any(unix, windows))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn terminate_with_marker(marker: &[u8]) -> ! {
    #[cfg(unix)]
    {
        let mut remaining = marker;

        while !remaining.is_empty() {
            // SAFETY: file descriptor 2 is the process's conventional stderr, and `remaining` is
            // readable for the supplied length.
            let written = unsafe { write_fd(2, remaining.as_ptr().cast(), remaining.len()) };
            let Ok(written) = usize::try_from(written) else {
                break;
            };

            if written == 0 {
                break;
            }

            let Some(rest) = remaining.get(written..) else {
                break;
            };

            remaining = rest;
        }

        // SAFETY: startup cannot continue without knowing whether the requested mutant was
        // selected. `_exit` avoids running handlers registered by constructors that may only be
        // partly complete.
        unsafe { exit_immediately(86) }
    }

    #[cfg(windows)]
    {
        // SAFETY: retrieving the process's standard-error handle has no precondition.
        let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

        if !stderr.is_null() && stderr.addr() != INVALID_HANDLE_VALUE {
            let mut remaining = marker;

            while !remaining.is_empty() {
                let mut written = 0_u32;
                let mut overlapped = WindowsOverlapped::for_append();
                let bytes_to_write = u32::try_from(remaining.len()).unwrap_or(u32::MAX);

                // A valid OVERLAPPED is required when stderr is an inherited pipe or file opened
                // for overlapped I/O. Seekable synchronous handles honour its all-ones offset as
                // append-to-end, while pipes ignore the offset.
                // SAFETY: null security and name pointers request an unnamed event with default
                // security. The returned handle is checked before use.
                let event = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
                let mut retry_synchronously = event.is_null();
                let mut pending = false;

                let completed = if retry_synchronously {
                    false
                } else {
                    overlapped.event = event;

                    // SAFETY: `stderr` is the process standard-error handle, `remaining` is
                    // readable for the supplied length, `overlapped` remains writable until a
                    // pending operation completes, and it owns a live event.
                    let succeeded = unsafe {
                        WriteFile(
                            stderr,
                            remaining.as_ptr().cast(),
                            bytes_to_write,
                            core::ptr::null_mut(),
                            core::ptr::from_mut(&mut overlapped).cast(),
                        )
                    };

                    if succeeded != 0 {
                        written = bytes_to_write;
                        true
                    } else {
                        // SAFETY: `GetLastError` reads thread-local Win32 state immediately after
                        // the failed `WriteFile`.
                        pending = unsafe { GetLastError() } == ERROR_IO_PENDING;
                        retry_synchronously = !pending;

                        // SAFETY: the operation owns `overlapped` and its event until this blocking
                        // call reports completion or failure.
                        pending
                            && unsafe {
                                GetOverlappedResult(
                                    stderr,
                                    core::ptr::from_mut(&mut overlapped).cast(),
                                    core::ptr::from_mut(&mut written),
                                    1,
                                ) != 0
                            }
                    }
                };

                if !event.is_null() && (!pending || completed) {
                    // SAFETY: `event` came from `CreateEventW` above. No operation used it, or its
                    // operation completed before this close.
                    let _closed = unsafe { CloseHandle(event) };
                }

                let completed = if retry_synchronously {
                    written = 0;

                    // Some synchronous handles, notably consoles, reject a non-null OVERLAPPED.
                    // Retrying without one preserves their marker output. An overlapped handle is
                    // expected to have returned `ERROR_IO_PENDING` and therefore not reach here.
                    // SAFETY: `stderr` is the process standard-error handle, `remaining` is
                    // readable for the supplied length, and `written` is writable.
                    unsafe {
                        WriteFile(
                            stderr,
                            remaining.as_ptr().cast(),
                            bytes_to_write,
                            core::ptr::from_mut(&mut written),
                            core::ptr::null_mut(),
                        ) != 0
                    }
                } else {
                    completed
                };

                if !completed || written == 0 {
                    break;
                }

                let Ok(written) = usize::try_from(written) else {
                    break;
                };

                let Some(rest) = remaining.get(written..) else {
                    break;
                };

                remaining = rest;
            }
        }

        // SAFETY: startup cannot continue without a trustworthy environment result. `ExitProcess`
        // terminates immediately instead of running partially initialized exit handlers.
        unsafe { ExitProcess(86) }
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "open"]
    fn open_fd(path: *const c_char, flags: c_int, ...) -> c_int;

    #[link_name = "read"]
    fn read_fd(descriptor: c_int, buffer: *mut c_void, count: usize) -> isize;

    #[link_name = "close"]
    fn close_fd(descriptor: c_int) -> c_int;

    /// The Linux C library's accessor for this thread's `errno`, which is how a failed `open` or
    /// `read` says whether a signal interrupted it. Declared here rather than taken from `libc`
    /// because this crate is injected into the user's dependency graph and must stay free of
    /// dependencies; the symbol is the one glibc, musl and uClibc all publish for it.
    fn __errno_location() -> *mut c_int;
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "write"]
    fn write_fd(descriptor: c_int, buffer: *const c_void, count: usize) -> isize;

    #[link_name = "_exit"]
    fn exit_immediately(status: c_int) -> !;
}

#[cfg(all(unix, any(test, not(target_os = "linux"))))]
unsafe extern "C" {
    /// The C library's `getenv`, whose signature is fixed by POSIX. Declared here rather than
    /// taken from `libc` because this crate is injected into the user's dependency graph and must
    /// stay free of dependencies.
    fn getenv(name: *const c_char) -> *const c_char;
}

// Test-only access used to mutate the environment while copy_environment_via_getenv captures it.
#[cfg(all(unix, test))]
unsafe extern "C" {
    fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;

    fn unsetenv(name: *const c_char) -> c_int;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    /// `kernel32`'s `GetEnvironmentVariableA`, whose signature is fixed by the Win32 API. Windows
    /// has no POSIX `getenv`, and the copy held by the C library is not guaranteed to see variables
    /// set through the Win32 side of the process.
    ///
    /// Narrow is enough for [`ACTIVE_VAR`], whose value is a decimal ordinal and therefore ASCII by
    /// construction, so no code page can change what it says. [`CENSUS_VAR`] names a path and goes
    /// through [`GetEnvironmentVariableW`] instead.
    fn GetEnvironmentVariableA(name: *const u8, buffer: *mut u8, size: u32) -> u32;

    /// The wide form of the same call, used for [`CENSUS_VAR`] because its value is a filesystem
    /// path. Windows stores the environment as UTF-16; the narrow call converts to the active ANSI
    /// code page on the way out, which drops any character the code page cannot represent — an
    /// ordinary occurrence for a user profile directory with a non-Latin name.
    fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32;

    /// Sets the calling thread's last-error value so a subsequent zero return can be classified.
    fn SetLastError(error: u32);

    /// Returns the calling thread's last-error value.
    fn GetLastError() -> u32;

    /// Returns one of the process standard handles.
    fn GetStdHandle(which: u32) -> *mut c_void;

    /// Creates an event used to wait for an overlapped write.
    fn CreateEventW(event_attributes: *const c_void, manual_reset: c_int, initial_state: c_int, name: *const u16) -> *mut c_void;

    /// Writes bytes directly to a Win32 handle.
    fn WriteFile(
        handle: *mut c_void,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut c_void,
    ) -> c_int;

    /// Waits for an overlapped operation and returns its transferred byte count.
    fn GetOverlappedResult(handle: *mut c_void, overlapped: *mut c_void, bytes_transferred: *mut u32, wait: c_int) -> c_int;

    /// Closes a Win32 object handle.
    fn CloseHandle(handle: *mut c_void) -> c_int;

    /// Terminates the process without running exit handlers.
    fn ExitProcess(exit_code: u32) -> !;

    /// Changes one process environment variable. Used only by the pre-main test helper.
    #[cfg(test)]
    fn SetEnvironmentVariableA(name: *const u8, value: *const u8) -> c_int;
}

/// The Win32 `STD_ERROR_HANDLE` value.
#[cfg(windows)]
const STD_ERROR_HANDLE: u32 = (-12_i32).cast_unsigned();

/// The address value used by Win32 for an invalid handle.
#[cfg(windows)]
const INVALID_HANDLE_VALUE: usize = usize::MAX;

/// The Win32 `ERROR_IO_PENDING` status.
#[cfg(windows)]
const ERROR_IO_PENDING: u32 = 997;

/// The Win32 `OVERLAPPED` layout used by `WriteFile`.
#[cfg(windows)]
#[repr(C)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

#[cfg(windows)]
impl WindowsOverlapped {
    const fn for_append() -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: u32::MAX,
            offset_high: u32::MAX,
            event: core::ptr::null_mut(),
        }
    }
}

#[cfg(any(unix, windows))]
unsafe extern "C" {
    /// The C library's `fopen`, whose signature is fixed by the C standard. Used rather than
    /// `std::fs::File` because this crate is `no_std`, and rather than the platforms' own
    /// `open`/`CreateFileA` because the C library buffers the batches serialized at process exit.
    ///
    /// The exit handler writes the reached-site bitmap in batches, appends a seal, and flushes the
    /// stream. A reader that finds the seal intact at the end knows every record before it arrived
    /// too. A process that aborts skips the handler and leaves no sealed census to trust.
    #[cfg(unix)]
    fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void;

    /// The Microsoft CRT's wide `fopen`, identical to it in everything said above and taking its
    /// path and mode as UTF-16. Windows uses this rather than the narrow `fopen`, which interprets
    /// its path in the active ANSI code page: a scratch tree whose path holds a character that code
    /// page cannot represent would fail to open, `sink` would latch the failure, and the census
    /// would silently never work on that host — the run merely slower, forever, with the reason
    /// invisible.
    #[cfg(windows)]
    fn _wfopen(path: *const u16, mode: *const u16) -> *mut c_void;

    /// The C library's `fwrite`, likewise fixed by the standard, and likewise thread-safe: it
    /// locks the stream, so the guards of a multi-threaded test cannot tear each other's records.
    fn fwrite(buffer: *const c_void, size: usize, count: usize, stream: *mut c_void) -> usize;

    /// The C library's `fflush`, fixed by the standard. Called once at a clean exit to force the
    /// buffered records and the seal out to the file at a point where a failure merely leaves the
    /// file unsealed, rather than deferring to a close whose failure would go unnoticed.
    fn fflush(stream: *mut c_void) -> c_int;

    /// The C library's `atexit`, fixed by the standard. Registers [`seal`] to run at a normal exit;
    /// an abnormal one — `abort`, a fatal signal, `_exit` — skips it by design, which is exactly
    /// what leaves an unsealed file for the reader to reject.
    fn atexit(handler: extern "C" fn()) -> c_int;
}

/// Records that the site with ordinal `id` was reached, if it has not been recorded already.
///
/// The bitmap is what keeps this affordable. A site inside a loop is reached millions of times but
/// occupies one bit, and no file is opened or written until the process exits. Every hit after the
/// first takes a cheap relaxed precheck of the bit (or [`OVERFLOWED`]) it would set and returns
/// immediately if it is already set, skipping the lease/compare-exchange/decrement protocol below
/// entirely.
///
/// # Why the precheck cannot lose a site under concurrent sealing
///
/// [`REACHED`] bits and [`OVERFLOWED`] are set-only for a census's whole lifetime — nothing ever
/// clears them — so a relaxed load that observes one already set is observing a fact that stays
/// true forever after, regardless of what ordering did or did not make visible to this thread yet.
/// The precheck can therefore only ever take a shortcut that is *correct to take*: it never
/// fabricates a "set" it did not really witness. The reverse case is symmetric and does not need
/// the precheck to be right at all: if the relevant bit is not yet visible to this thread — whether
/// truly unset or set by a write this thread has not observed — falling through to the full
/// lease/fetch-or/release protocol below is exactly what already happens on every call today, so
/// no case exists where the precheck causes a site to go unrecorded that would otherwise have been.
// Cost: this deliberately makes no empirical throughput promise. The structural trade-off is
// sufficient here: census-only first and unique hits pay one relaxed load, while repeated hits
// avoid the recorder lease and compare-exchange protocol entirely.
// Cold: reached only under a census, never in a scoring run, and even then only off the inlined
// per-site guard `a`. Keeping it out of line stops its bitmap machinery being inlined into every
// guard site and bloating the instrumented build, which is the dominant fixed cost of a run.
// `#[inline(never)]` turns the `#[cold]` hint into a guarantee at no cost: a census can afford one
// call per site.
#[cold]
#[inline(never)]
#[cfg(any(unix, windows))]
fn note(id: u32) {
    let Ok(index) = usize::try_from(id) else {
        return;
    };

    if index >= SITES {
        if OVERFLOWED.load(Ordering::Relaxed) {
            return;
        }
    } else {
        let bit = 1_u32 << (index % WORD_BITS);

        if REACHED[index / WORD_BITS].load(Ordering::Relaxed) & bit != 0 {
            return;
        }
    }

    // The lease starts before the bitmap claim. A seal must therefore wait from the instant this
    // guard makes a site reachable rather than scan past an update that has not landed yet.
    if !begin_recording() {
        return;
    }

    #[cfg(test)]
    pause_after_lease();

    if index >= SITES {
        OVERFLOWED.store(true, Ordering::Relaxed);
        end_recording();

        return;
    }

    let bit = 1_u32 << (index % WORD_BITS);

    let _previous = REACHED[index / WORD_BITS].fetch_or(bit, Ordering::Relaxed);
    end_recording();
}

/// The high bit closes census recording; the remaining bits count bitmap updates in progress.
///
/// `seal` claims the closed state only from zero recorders. A guard increments the count before
/// touching the bitmap and decrements it with `Release` afterwards. An acquiring successful seal
/// claim therefore observes every bitmap update it waited for. Once claimed, no new recorder can
/// enter while the bitmap is being serialized.
#[cfg(any(unix, windows))]
struct RecorderState {
    value: RecorderAtomicUsize,
}

#[cfg(any(unix, windows))]
impl RecorderState {
    #[cfg(not(all(loom, feature = "loom")))]
    const fn new() -> Self {
        Self {
            value: RecorderAtomicUsize::new(0),
        }
    }

    #[cfg(all(loom, feature = "loom"))]
    fn new() -> Self {
        Self {
            value: RecorderAtomicUsize::new(0),
        }
    }

    fn begin_recording(&self) -> bool {
        loop {
            let state = self.value.load(Ordering::Acquire);

            if state & SEALING != 0 {
                return false;
            }

            if state == RECORDER_MASK {
                core::hint::spin_loop();
                continue;
            }

            if self
                .value
                .compare_exchange_weak(state, state + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn end_recording(&self) {
        let previous = self.value.fetch_sub(1, Ordering::Release);
        debug_assert!(previous & RECORDER_MASK != 0, "a recording lease must exist before it is released");
    }

    fn begin_seal(&self) -> bool {
        loop {
            if let Some(claimed) = self.try_begin_seal() {
                return claimed;
            }

            #[cfg(all(test, not(loom)))]
            TEST_SEAL_WAITING.store(true, Ordering::Release);

            core::hint::spin_loop();
        }
    }

    /// Attempts one non-blocking step of sealing.
    ///
    /// `None` means a recorder or competing transition is in progress; `Some(false)` means another
    /// sealer already won, and `Some(true)` means this caller closed recording.
    fn try_begin_seal(&self) -> Option<bool> {
        let state = self.value.load(Ordering::Acquire);

        if state & SEALING != 0 {
            return Some(false);
        }

        if state != 0 {
            return None;
        }

        #[expect(
            clippy::if_then_some_else_none,
            reason = "the test observation must occur only after the compare-exchange succeeds"
        )]
        if self.value.compare_exchange(0, SEALING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            #[cfg(all(test, not(loom)))]
            TEST_SEAL_CLAIMED.store(true, Ordering::Release);

            Some(true)
        } else {
            None
        }
    }
}

#[cfg(all(any(unix, windows), not(all(loom, feature = "loom"))))]
static CENSUS_RECORDERS: RecorderState = RecorderState::new();

#[cfg(all(any(unix, windows), loom, feature = "loom"))]
loom::lazy_static! {
    static ref CENSUS_RECORDERS: RecorderState = RecorderState::new();
}

#[cfg(any(unix, windows))]
const SEALING: usize = 1_usize << (usize::BITS - 1);

#[cfg(any(unix, windows))]
const RECORDER_MASK: usize = !SEALING;

/// Enters the recording side of the recording/seal protocol.
#[cfg(any(unix, windows))]
fn begin_recording() -> bool {
    CENSUS_RECORDERS.begin_recording()
}

/// Leaves the recording side after its bitmap update is complete.
#[cfg(any(unix, windows))]
fn end_recording() {
    CENSUS_RECORDERS.end_recording();
}

/// Closes the protocol once no bitmap update is in progress.
#[cfg(any(unix, windows))]
fn begin_seal() -> bool {
    CENSUS_RECORDERS.begin_seal()
}

/// Test-only controlled write seam for the exit-write regression.
#[cfg(all(test, any(unix, windows)))]
const BLOCK_AND_SHORT: usize = 1;
#[cfg(all(test, any(unix, windows)))]
const WRITE_ENTERED: usize = 2;
#[cfg(all(test, any(unix, windows)))]
const RELEASE_SHORT_WRITE: usize = 3;

#[cfg(all(test, any(unix, windows)))]
static TEST_WRITE_STATE: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, any(unix, windows)))]
const BLOCK_AFTER_LEASE: usize = 1;
#[cfg(all(test, any(unix, windows)))]
const LEASE_HELD: usize = 2;
#[cfg(all(test, any(unix, windows)))]
const RELEASE_LEASE: usize = 3;

#[cfg(all(test, any(unix, windows)))]
static TEST_NOTE_STATE: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, any(unix, windows)))]
static TEST_SEAL_WAITING: AtomicBool = AtomicBool::new(false);
#[cfg(all(test, any(unix, windows)))]
static TEST_SEAL_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Pauses a test writer after it has a lease but before it claims its bitmap bit.
#[cfg(all(test, any(unix, windows)))]
fn pause_after_lease() {
    if TEST_NOTE_STATE
        .compare_exchange(BLOCK_AFTER_LEASE, LEASE_HELD, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        while TEST_NOTE_STATE.load(Ordering::Acquire) != RELEASE_LEASE {
            core::hint::spin_loop();
        }
    }
}

/// Writes one byte slice, with a test-only zero-write seam.
#[cfg(any(unix, windows))]
fn write_bytes(bytes: &[u8], stream: *mut c_void) -> usize {
    #[cfg(test)]
    if TEST_WRITE_STATE
        .compare_exchange(BLOCK_AND_SHORT, WRITE_ENTERED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        while TEST_WRITE_STATE.load(Ordering::Acquire) != RELEASE_SHORT_WRITE {
            core::hint::spin_loop();
        }

        return 0;
    }

    // SAFETY: `stream` is a stream returned by `open`, remains open for this process, and `bytes`
    // describes exactly the readable elements passed to `fwrite`.
    unsafe { fwrite(bytes.as_ptr().cast(), 1, bytes.len(), stream) }
}

/// Writes one four-byte little-endian record.
#[cfg(any(unix, windows))]
fn write_record(record: u32, stream: *mut c_void) -> bool {
    let bytes = record.to_le_bytes();

    write_bytes(&bytes, stream) == bytes.len()
}

/// Number of census bytes accumulated before one `fwrite`.
#[cfg(any(unix, windows))]
const OUTPUT_BUFFER: usize = 4096;

#[cfg(any(unix, windows))]
const _: () = assert!(OUTPUT_BUFFER.is_multiple_of(core::mem::size_of::<u32>()));

/// Adds one record to an exit-time output batch, flushing a full batch first.
#[cfg(any(unix, windows))]
fn buffer_record(record: u32, buffer: &mut [u8; OUTPUT_BUFFER], used: &mut usize, stream: *mut c_void) -> bool {
    if *used == buffer.len() {
        if write_bytes(buffer, stream) != buffer.len() {
            return false;
        }

        *used = 0;
    }

    let bytes = record.to_le_bytes();
    buffer[*used..*used + bytes.len()].copy_from_slice(&bytes);
    *used += bytes.len();

    true
}

/// Serializes every reached site from the bitmap in ascending ordinal order.
#[cfg(any(unix, windows))]
fn write_reached(stream: *mut c_void) -> bool {
    let mut buffer = [0_u8; OUTPUT_BUFFER];
    let mut used = 0_usize;

    for (word_at, word) in REACHED.iter().enumerate() {
        let mut bits = word.load(Ordering::Relaxed);

        while bits != 0 {
            let bit_at = usize::try_from(bits.trailing_zeros()).unwrap_or(0);
            let site = word_at * WORD_BITS + bit_at;
            let Ok(record) = u32::try_from(site) else {
                return false;
            };

            if !buffer_record(record, &mut buffer, &mut used, stream) {
                return false;
            }

            bits &= bits - 1;
        }
    }

    if OVERFLOWED.load(Ordering::Relaxed) && !buffer_record(OVERFLOW, &mut buffer, &mut used, stream) {
        return false;
    }

    used == 0 || write_bytes(&buffer[..used], stream) == used
}

/// Returns the census stream, opening it once, or null if it could not be opened.
///
/// Opening is serialized rather than left to race the way [`active`]'s read does, because two
/// winners would leave two independently buffered streams on one file and their buffers could
/// interleave mid-record. The window being contended is a single `fopen`, so spinning through it
/// costs less than the machinery to avoid spinning would.
#[cfg(any(unix, windows))]
fn sink() -> *mut c_void {
    /// No stream has been opened yet.
    const UNOPENED: usize = 0;

    /// Another thread is opening it.
    const OPENING: usize = 1;

    /// Opening was tried and failed; there is nothing to retry.
    const FAILED: usize = 2;

    // The three states are addresses no allocation can occupy, so a real stream is anything else.
    //
    // The stream is stored as an integer, so its provenance has to survive the round trip: the
    // pointer rebuilt from this word is handed to `fwrite` and `fflush`, which dereference it, and
    // the abstract machine considers a non-zero-sized access through a pointer with no provenance
    // undefined however right the address is. That is why the store below is
    // `expose_provenance` — which marks the allocation as reachable through an integer — and the
    // reconstruction is `with_exposed_provenance_mut`, the matching accessor. `addr` and
    // `without_provenance_mut` are the deliberately *non*-exposing pair and must not be
    // substituted here; they are correct only for a word that is never dereferenced.
    static SINK: AtomicUsize = AtomicUsize::new(UNOPENED);

    loop {
        match SINK.load(Ordering::Acquire) {
            UNOPENED => {
                if SINK
                    .compare_exchange(UNOPENED, OPENING, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    let stream = open();

                    SINK.store(
                        if stream.is_null() { FAILED } else { stream.expose_provenance() },
                        Ordering::Release,
                    );

                    return stream;
                }
            }
            OPENING => core::hint::spin_loop(),
            FAILED => return core::ptr::null_mut(),
            ready => return core::ptr::with_exposed_provenance_mut(ready),
        }
    }
}

/// Opens the census file named by [`CENSUS_VAR`] for appending, returning null on any failure.
///
/// Appending rather than truncating, so that a path reused by mistake is caught by cargo-gamma as
/// a census with impossible contents rather than quietly losing the earlier one.
#[cfg(any(unix, windows))]
#[cfg_attr(
    windows,
    expect(
        clippy::needless_return,
        reason = "the Windows branch is followed by the Unix branch, which is compiled away here \
                  and leaves the return looking like a tail expression"
    )
)]
fn open() -> *mut c_void {
    /// Append, binary: the records are bytes, not text, and a platform that would translate line
    /// endings must not touch them.
    #[cfg(unix)]
    const MODE: &[u8] = b"ab\0";

    /// The same mode as UTF-16, which is what `_wfopen` takes.
    #[cfg(windows)]
    const MODE: [u16; 3] = widen(b"ab\0");

    #[cfg(unix)]
    {
        if CENSUS_PATH_LENGTH.load(Ordering::Acquire) == 0 {
            return core::ptr::null_mut();
        }

        // SAFETY: startup copied the path and explicitly wrote its NUL terminator into this static
        // — see `terminate_census_path` — before publishing the non-zero length acquired above.
        // The buffer is immutable after publication; `MODE` is NUL-terminated too.
        unsafe { fopen(CENSUS_PATH.bytes.get().cast(), MODE.as_ptr().cast()) }
    }

    #[cfg(windows)]
    {
        if CENSUS_PATH_LENGTH.load(Ordering::Acquire) == 0 {
            return core::ptr::null_mut();
        }

        // SAFETY: startup copied a NUL-terminated UTF-16 path into this static before publishing
        // its non-zero length. It is immutable after publication; `MODE` is NUL-terminated too.
        return unsafe { _wfopen(CENSUS_PATH.bytes.get().cast(), MODE.as_ptr()) };
    }
}

/// Writes the reached-site bitmap and the seal that vouches it is whole, at a normal process exit.
///
/// Registered with `atexit` by [`install`] whenever a census was requested, so it runs even for a
/// test that reached no site and therefore opened no file of its own: the lone seal it writes is
/// what lets the reader treat an *absent* file as a failed run rather than an honest empty census.
///
/// It withholds the seal if the stream cannot be opened or any buffered record cannot be written.
/// Any failure leaves an unsealed file, which the reader already rejects.
#[cfg(any(unix, windows))]
extern "C" fn seal() {
    // Claiming the protocol waits for every bitmap update that already started and prevents any
    // later update from starting.
    if !begin_seal() {
        return;
    }

    let stream = sink();

    // No file to seal. The reader reads an absent census as a failed run, which is the right
    // verdict for a process whose open never succeeded.
    if stream.is_null() {
        return;
    }

    if !write_reached(stream) {
        return;
    }

    if !write_record(SEAL, stream) {
        return;
    }

    // Force records and seal out now, where a failure just leaves the file unsealed, rather than
    // trusting a later close whose failure would pass unnoticed.
    // SAFETY: `stream` is a stream `open` returned — `fopen` on Unix, `_wfopen` on Windows —
    // nothing here closes it, and it carries the stream's provenance for the reason argued at the
    // `fwrite` above.
    let _ = unsafe { fflush(stream) };
}

/// Returns the selection that is safe to publish after exit-handler registration.
#[cfg(any(unix, windows))]
fn selection_after_registration(selection: u32, mut register: impl FnMut(extern "C" fn()) -> c_int) -> u32 {
    if selection != CENSUS || register(seal) == 0 {
        selection
    } else {
        // There is no Result channel from a loader constructor. Refusing census mode makes the
        // requested census file remain absent, which the parent surfaces as a failed run, rather
        // than silently running a census that can never receive its integrity seal.
        NONE
    }
}

/// Registers [`seal`] to run at exit, but only for a process actually taking a census.
///
/// Placed in the platform's constructor table so selection and the census path are captured before
/// `main`. It also lets the seal cover a test that touches no instrumented site: there is no first
/// guard to hang registration off.
#[cfg(any(unix, windows))]
extern "C" fn install() {
    let selection = capture_selection();
    let protected = selection_after_registration(selection, |handler| {
        // SAFETY: `seal` has the `extern "C" fn()` signature `atexit` requires, and registration
        // has no other precondition.
        unsafe { atexit(handler) }
    });

    ACTIVE.store(protected, Ordering::Release);
}

/// Puts [`install`] in the ELF constructor array, which the loader walks before `main`.
///
/// `#[used]` keeps the entry even though nothing references it, so it survives even when the
/// runtime is linked as a dependency and only its guard is called.
#[cfg(all(unix, not(target_vendor = "apple"), not(miri)))]
#[used]
#[unsafe(link_section = ".init_array")]
static INSTALL: extern "C" fn() = install;

/// Puts [`install`] in the Mach-O module-initialiser section, which the dynamic loader runs before
/// `main`, for the same reason as the ELF entry above.
#[cfg(all(target_vendor = "apple", not(miri)))]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static INSTALL: extern "C" fn() = install;

/// Puts [`install`] in the CRT initialiser section, which the C runtime walks before `main`, for
/// the same reason as the ELF entry above.
#[cfg(all(windows, not(miri)))]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static INSTALL: extern "C" fn() = install;

/// Marks a test-binary child that must verify the captured selection before `main`.
#[cfg(all(test, any(unix, windows), not(miri)))]
const ENVIRONMENT_HELPER_VAR: &str = "GAMMA_RT_ENVIRONMENT_HELPER";

/// [`ENVIRONMENT_HELPER_VAR`] in the NUL-terminated form required by native APIs.
#[cfg(all(test, any(unix, windows), not(miri)))]
const ENVIRONMENT_HELPER_VAR_C: &[u8] = b"GAMMA_RT_ENVIRONMENT_HELPER\0";

/// The replacement value used after the helper has captured ordinal 7.
#[cfg(all(test, any(unix, windows), not(miri)))]
const ENVIRONMENT_HELPER_REPLACEMENT_C: &[u8] = b"99\0";

/// Marks a Windows test-binary child that must exercise the fatal environment-error writer.
#[cfg(all(test, windows, not(miri)))]
const ENVIRONMENT_ERROR_HELPER_VAR_C: &[u8] = b"GAMMA_RT_ENVIRONMENT_ERROR_HELPER\0";

/// Returns whether this process was launched as the pre-main environment helper.
#[cfg(all(test, any(unix, windows), not(miri)))]
fn environment_helper_requested() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: the name is NUL-terminated. This runs during single-threaded process startup,
        // before the helper performs any environment mutation.
        let value = unsafe { getenv(ENVIRONMENT_HELPER_VAR_C.as_ptr().cast()) };

        if value.is_null() {
            return false;
        }

        // SAFETY: under the same startup precondition, the returned value remains valid.
        let first = unsafe { *value.cast::<u8>() };

        if first != b'1' {
            return false;
        }

        // SAFETY: because the first byte is non-NUL, the C string has a subsequent byte.
        let terminator = unsafe { value.cast::<u8>().add(1) };
        // SAFETY: `terminator` points at that second byte.
        let second = unsafe { *terminator };

        first == b'1' && second == 0
    }

    #[cfg(windows)]
    {
        let mut value = [0_u8; 2];

        // SAFETY: clearing last error has no precondition.
        unsafe { SetLastError(ERROR_SUCCESS) };
        // SAFETY: the name is NUL-terminated and `value` is writable for the supplied length.
        let written = unsafe {
            GetEnvironmentVariableA(
                ENVIRONMENT_HELPER_VAR_C.as_ptr(),
                value.as_mut_ptr(),
                u32::try_from(value.len()).unwrap_or(u32::MAX),
            )
        };

        written == 1 && value[0] == b'1'
    }
}

/// Runs the environment-stability regression before the libtest harness can start any threads.
///
/// The parent re-executes this test binary with [`ENVIRONMENT_HELPER_VAR_C`] set. This constructor
/// then performs the entire check and terminates the child without entering `main`, so the native
/// environment mutation occurs in a genuinely single-threaded process rather than in a filtered
/// libtest test.
#[cfg(all(test, any(unix, windows), not(miri)))]
extern "C" fn run_environment_helper() {
    if !environment_helper_requested() {
        return;
    }

    // Constructor order is platform- and linker-dependent. Calling `install` explicitly makes
    // the helper capture its launch environment before it performs the mutation, whether or not
    // the ordinary runtime constructor already ran.
    install();
    let before = active();

    #[cfg(unix)]
    // SAFETY: this constructor exits before `main`; no other Rust thread exists, and the two
    // arguments are NUL-terminated strings.
    let changed = unsafe { setenv(ACTIVE_VAR_C.as_ptr().cast(), ENVIRONMENT_HELPER_REPLACEMENT_C.as_ptr().cast(), 1) == 0 };

    #[cfg(windows)]
    // SAFETY: this constructor exits before `main`; no other Rust thread exists, and both
    // arguments are NUL-terminated strings.
    let changed = unsafe { SetEnvironmentVariableA(ACTIVE_VAR_C.as_ptr(), ENVIRONMENT_HELPER_REPLACEMENT_C.as_ptr()) != 0 };

    let after = active();
    let succeeded = changed && before == 7 && after == 7;

    #[cfg(unix)]
    // SAFETY: the helper must not enter the libtest harness after mutating the process environment.
    unsafe {
        exit_immediately(i32::from(!succeeded));
    }

    #[cfg(windows)]
    // SAFETY: the helper must not enter the libtest harness after mutating the process environment.
    unsafe {
        ExitProcess(u32::from(!succeeded));
    }
}

/// Installs the pre-main environment helper in Unix test binaries.
#[cfg(all(test, unix, not(target_vendor = "apple"), not(miri)))]
#[used]
#[unsafe(link_section = ".init_array")]
static RUN_ENVIRONMENT_HELPER: extern "C" fn() = run_environment_helper;

/// Exercises [`environment_error`] before the Windows test harness replaces inherited handles.
///
/// The selected child terminates inside [`environment_error`] before coverage counters can flush.
#[cfg(all(test, windows, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
extern "C" fn run_environment_error_helper() {
    let mut value = [0_u8; 2];

    // SAFETY: the name is NUL-terminated and `value` is writable for the supplied length.
    let written = unsafe {
        GetEnvironmentVariableA(
            ENVIRONMENT_ERROR_HELPER_VAR_C.as_ptr(),
            value.as_mut_ptr(),
            u32::try_from(value.len()).unwrap_or(u32::MAX),
        )
    };

    if written == 1 && value[0] == b'1' {
        environment_error();
    }
}

/// Installs the fatal-writer helper in Windows test binaries.
#[cfg(all(test, windows, not(miri)))]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static RUN_ENVIRONMENT_ERROR_HELPER: extern "C" fn() = run_environment_error_helper;

/// Installs the pre-main environment helper in Apple test binaries.
#[cfg(all(test, target_vendor = "apple", not(miri)))]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func")]
static RUN_ENVIRONMENT_HELPER: extern "C" fn() = run_environment_helper;

/// Installs the pre-main environment helper in Windows test binaries.
#[cfg(all(test, windows, not(miri)))]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static RUN_ENVIRONMENT_HELPER: extern "C" fn() = run_environment_helper;

/// Returns the ordinal of the mutant active in this process.
///
/// The selection is captured without allocation during process startup. It cannot change later:
/// a test manipulating its own environment must not change which mutant is live halfway through a
/// run, and guards must not read an environment another thread may safely be changing. The
/// `active_is_immune_to_the_process_changing_its_own_environment` process regression in this
/// crate's test suite exercises that startup-to-runtime transition.
#[inline]
#[must_use]
pub fn active() -> u32 {
    let selected = selected();

    // Census mode is not a mutant, and callers outside the guard ask this question to find out
    // whether they are running mutated code. They are not, so they are told so.
    if selected == CENSUS { NONE } else { selected }
}

/// Returns the raw startup-captured selection, which is an ordinal, [`NONE`], or [`CENSUS`].
///
/// Split from [`active`] because the guard needs to distinguish census mode and every other caller
/// needs not to. Never returns `UNINSTALLED`: observing it means some other native constructor
/// ran instrumented code before this crate's own, so `uninstalled_guard` terminates the process
/// instead of letting the caller mistake it for a legitimate [`NONE`].
#[inline]
fn selected() -> u32 {
    // The constructor's `Release` store publishes the copied census path before a guard observing
    // census mode can use it.
    let value = ACTIVE.load(Ordering::Acquire);

    #[cfg(all(any(unix, windows), not(miri)))]
    if value == UNINSTALLED {
        uninstalled_guard();
    }

    value
}

/// Terminates when a guard observes [`UNINSTALLED`], rather than silently reporting the safe-looking
/// [`NONE`] a truly non-hosted target would report forever.
///
/// Those two situations must not be conflated: a target with no `unix` or `windows` constructor
/// mechanism, or a Miri execution of either, never links [`install`] and [`NONE`] is its correct,
/// permanent answer, documented at the crate root. [`UNINSTALLED`] instead means a *hosted,
/// non-Miri* target reached a guard before its own [`install`] ran — necessarily because some
/// other native constructor executed instrumented Rust first, since nothing else can observe this
/// crate's statics before then. No check of `ACTIVE` alone can tell that apart from an ordinary
/// unmutated run, so it is called out the moment the otherwise-unreachable sentinel is observed:
/// pre-install guard execution terminates the process instead of silently selecting baseline
/// behavior. Termination does not unwind, so instrumented constructor code cannot absorb it with
/// `catch_unwind` and continue into this crate's later [`install`].
///
/// Excluded from coverage because immediate process termination cannot flush coverage counters.
#[cfg(all(any(unix, windows), not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
#[cold]
fn uninstalled_guard() -> ! {
    terminate_with_marker(PRE_INSTALL_ERROR_MARKER)
}

/// Returns `true` when the mutant with ordinal `id` is the active one.
///
/// This is the function every injected guard calls. It is a cached atomic load and a comparison,
/// which is what makes the whole schema approach affordable: an inactive guard costs a predictable
/// branch that the CPU learns immediately.
///
/// Census mode rides on the same load rather than on a second one, which is why it is a reserved
/// value of the cached ordinal rather than a flag of its own. An ordinary run therefore pays one
/// extra comparison against a constant, on a branch that is never taken and so is predicted
/// perfectly after the first guard.
///
/// The name is one character because it appears at every mutation site of every rewritten file,
/// where it is read far less often than it is written.
///
/// ```rust
/// // What an instrumented `a < b` becomes, for the mutant with ordinal 7.
/// let (a_val, b_val) = (1_u32, 2_u32);
/// let result = if gamma_rt::a(7) {
///     a_val <= b_val
/// } else {
///     a_val < b_val
/// };
///
/// assert!(result);
/// ```
#[inline]
#[must_use]
pub fn a(id: u32) -> bool {
    let active = selected();

    // A census wants the code the author wrote, so the answer is always `false` and the site is
    // recorded on the way past. The record is what tells cargo-gamma which tests can possibly
    // reach this mutant: a test that never runs the guard in an unmutated process cannot run it in
    // a mutated one either, because the mutant changes nothing before its own site executes.
    #[cfg(any(unix, windows))]
    if active == CENSUS {
        note(id);

        return false;
    }

    id != NONE && active == id
}

/// Returns `true` when any mutant is active in this process.
///
/// Nothing in the instrumented source calls this; it is for diagnostics, and for code that wants
/// to know whether it is running under mutation at all.
///
/// ```rust
/// // Outside a mutation run nothing is active, which is what lets an ordinary build of an
/// // instrumented crate behave exactly like an uninstrumented one. Restating this as a
/// // comparison against `active()` would only repeat the implementation back at itself.
/// assert!(!gamma_rt::any());
/// ```
#[inline]
#[must_use]
pub fn any() -> bool {
    active() != NONE
}
#[cfg(all(test, not(all(miri, windows))))]
mod tests {
    extern crate std;

    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::Cell;
    use std::alloc::System;
    use std::prelude::v1::*;
    #[cfg(not(miri))]
    use std::{format, println};
    use std::{thread_local, vec};

    use super::*;

    #[cfg(not(miri))]
    static CENSUS_NONE_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    /// Counts every allocation the calling thread makes, so a test can prove a region made none.
    struct Counting;

    thread_local! {
        /// How many times [`Counting`] has been asked for memory *by this thread*.
        ///
        /// Per-thread rather than per-process because the harness runs tests on a thread pool, and
        /// a process-wide counter measures whatever the other tests in this binary happened to be
        /// doing at the same moment. That is not theoretical: it is a test that passes alone and
        /// fails in a full workspace run, which is the worst shape a failure can take. Serializing
        /// the measurements against each other does not fix it either, because the threads that
        /// pollute the count are not the ones taking the lock.
        ///
        /// `const` initialization is required, not merely tidy: the lazy form allocates on first
        /// access, and allocating inside the allocator is unbounded recursion.
        static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    }

    // SAFETY: every method forwards to the system allocator with the arguments it was given, so
    // the contract is exactly the system allocator's, which upholds it.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();

            // SAFETY: `layout` is whatever the caller passed, which is what `System` expects.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the pointer and layout come from a matching `alloc` on this same allocator,
            // which forwarded to `System`.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();

            // SAFETY: as for `dealloc`, plus the size is the caller's, which is what `System` wants.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    /// Records one allocation against the calling thread, if that thread can still be asked.
    ///
    /// `try_with` rather than `with`: a thread allocating while its thread-locals are being torn
    /// down would panic on access, and a panic inside the global allocator is not recoverable.
    /// Such an allocation goes uncounted, which is correct — it belongs to no measurement.
    fn count() {
        let _counted = ALLOCATIONS.try_with(|allocations| allocations.set(allocations.get().saturating_add(1)));
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Counts the allocations `body` makes.
    ///
    /// The count covers this thread and nothing else, so what the rest of the suite is doing while
    /// it runs cannot change the answer. There is deliberately no lock: with the counter per
    /// thread there is nothing to serialize, and a lock would only have made the test slower while
    /// leaving the interference it was meant to exclude exactly where it was.
    ///
    /// `body` must therefore do its work on the calling thread. One that spawns is measuring
    /// nothing.
    fn allocations(body: impl FnOnce()) -> usize {
        let before = ALLOCATIONS.with(Cell::get);

        body();

        ALLOCATIONS.with(Cell::get) - before
    }

    #[test]
    fn none_is_zero_so_unset_means_baseline() {
        assert_eq!(NONE, 0);
    }

    #[test]
    fn active_is_stable_across_calls() {
        // Whatever the ambient environment is, the answer must not change between calls.
        let first = active();
        let second = active();

        assert_eq!(first, second);
    }

    #[test]
    fn a_matches_only_a_positive_active_ordinal() {
        let live = active();

        assert!(!a(NONE));

        if live != NONE {
            assert!(a(live));
        }

        assert!(!a(live.wrapping_add(1)));
    }

    #[test]
    fn any_agrees_with_active() {
        assert_eq!(any(), active() != NONE);
    }

    /// The measurement ignores what other threads allocate, which is the property the two tests
    /// below depend on and the one a process-wide counter did not have.
    ///
    /// Without it those tests assert zero against a number every other test in this binary can add
    /// to, so they pass alone and fail under load — a flake with no reproduction. This drives that
    /// interference deliberately: a sibling thread allocating hard throughout the measured region
    /// must not move the answer.
    #[test]
    fn a_measurement_does_not_count_what_another_thread_allocates() {
        use core::sync::atomic::AtomicBool;

        // A handshake rather than a busy sibling, because the measured region is a few nanoseconds
        // long and a sibling merely running alongside it would usually miss the window entirely —
        // the test would pass against a process-wide counter for want of a collision rather than
        // because collisions cannot happen.
        let (go, done) = (AtomicBool::new(false), AtomicBool::new(false));

        std::thread::scope(|scope| {
            let sibling = scope.spawn(|| {
                while !go.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }

                for _ in 0..64 {
                    // Black-boxed so the optimizer cannot delete the allocation this exists to make.
                    let block = core::hint::black_box(vec![0_u8; 1024]);

                    assert_eq!(block.len(), 1024);
                }

                done.store(true, Ordering::Release);
            });

            // Warmed first, so a cache fill inside the measured region cannot be mistaken for the
            // sibling's allocations leaking in.
            let _ = active();

            let counted = allocations(|| {
                go.store(true, Ordering::Release);

                // Spinning rather than joining: the region has to stay open until the sibling has
                // finished allocating, and it must not allocate to do the waiting.
                while !done.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }

                let _live = a(7);
            });

            sibling.join().expect("the allocating thread must not panic");

            assert_eq!(counted, 0, "another thread's allocations must not reach this count");
        });
    }

    #[cfg(all(any(unix, windows), not(miri)))]
    #[test]
    fn guards_never_read_the_environment_after_startup() {
        // The counting seam covers every platform environment query in this runtime. The
        // constructor must have made at least the census query before this test starts, and all
        // guard entry points must leave its count unchanged even while user threads could mutate
        // the environment.
        let startup_reads = STARTUP_ENVIRONMENT_READS.load(Ordering::Acquire);
        assert!(startup_reads > 0, "selection was not captured during startup");

        let _ = (a(7), active(), any());

        assert_eq!(
            STARTUP_ENVIRONMENT_READS.load(Ordering::Acquire),
            startup_reads,
            "a guard read the environment after startup"
        );
    }

    #[test]
    fn a_guard_allocates_nothing() {
        // Warm the cache first: what a guard costs on the millionth site is the number that gets
        // multiplied by the whole population.
        let _ = active();

        assert_eq!(
            allocations(|| {
                let _live = a(7);
            }),
            0
        );
    }

    #[test]
    fn an_unset_or_unparsable_value_selects_the_baseline() {
        // Anything that is not a plain decimal number has to mean "run the code the author wrote".
        // Guessing an ordinal instead could report a mutated program as passing.
        assert_eq!(parse(b""), NONE);
        assert_eq!(parse(b"   "), NONE);
        assert_eq!(parse(b"-1"), NONE);
        assert_eq!(parse(b"+1"), NONE);
        assert_eq!(parse(b"7x"), NONE);
        assert_eq!(parse(b"0x7"), NONE);
        assert_eq!(parse("٧".as_bytes()), NONE, "only ASCII digits count");
    }

    #[test]
    fn a_well_formed_value_parses() {
        assert_eq!(parse(b"0"), NONE);
        assert_eq!(parse(b"7"), 7);
        assert_eq!(parse(b" 42 \n"), 42);
        assert_eq!(parse(b"4294967291"), u32::MAX - 4);
    }

    #[test]
    fn a_value_too_large_to_be_an_ordinal_selects_the_baseline() {
        // Overflow must not wrap into a valid ordinal, and the cache's sentinel must not be
        // reachable from the environment or a first read would look like no read at all.
        assert_eq!(parse(b"4294967296"), NONE);
        assert_eq!(parse(b"99999999999999999999"), NONE);
        assert_eq!(parse(b"4294967295"), NONE, "the sentinel is not an ordinal");
        assert_eq!(parse(b"4294967294"), NONE, "nor is the census mode");
        assert_eq!(parse(b"4294967293"), NONE, "nor is the census file's seal");
        assert_eq!(parse(b"4294967292"), NONE, "nor is the pre-install sentinel");
    }

    #[test]
    fn an_absurdly_long_value_is_bounded() {
        // A corrupt environment must not be able to walk this process off the end of its memory,
        // and a value this long cannot be an ordinal anyway.
        let long = [b'9'; READ_LIMIT * 4];

        assert_eq!(parse(&long), NONE);
    }

    #[cfg(unix)]
    #[test]
    fn copying_stops_at_the_terminator() {
        let text = b"123\x004\x35\x36";
        let mut copied = [0; READ_LIMIT];

        // SAFETY: `text` is a NUL-terminated byte string and remains unmodified during the copy.
        let length = unsafe { copy_c_string(text.as_ptr(), &mut copied) };

        assert_eq!(length, Some(3));
        assert_eq!(&copied[..3], b"123");
        assert_eq!(copied[3], 0, "the successful copy must write its own terminator");
    }

    #[cfg(unix)]
    #[test]
    fn copying_a_shorter_value_replaces_the_old_terminator() {
        let mut copied = *b"a-long-old-value";
        let shorter = b"new\0";

        // SAFETY: `shorter` is readable through its terminator and remains unmodified.
        let length = unsafe { copy_c_string(shorter.as_ptr(), &mut copied) };

        assert_eq!(length, Some(3));
        assert_eq!(&copied[..4], b"new\0");
    }

    #[cfg(unix)]
    #[test]
    fn copying_reports_a_value_that_outruns_the_limit() {
        let text = [b'9'; READ_LIMIT * 2];
        let mut copied = [0; READ_LIMIT];

        // SAFETY: `text` is readable for the whole bounded copy and remains unmodified.
        let length = unsafe { copy_c_string(text.as_ptr(), &mut copied) };

        // A prefix is not the value, and its caller could not tell the two apart.
        assert_eq!(length, None);
    }

    #[cfg(unix)]
    #[test]
    fn copying_rejects_a_value_that_exactly_fills_the_limit() {
        let mut text = [b'9'; READ_LIMIT + 1];
        text[READ_LIMIT] = 0;
        let mut copied = [0; READ_LIMIT];

        // SAFETY: `text` is NUL-terminated and remains unmodified during the bounded copy.
        let length = unsafe { copy_c_string(text.as_ptr(), &mut copied) };

        // The terminator sits exactly at the limit, so the scan never reads it: indistinguishable
        // from truncation, and reported as such rather than guessed at.
        assert_eq!(length, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_truncated_over_long_value_selects_the_baseline() {
        // Twenty-five spaces then thirteen digits: the first `READ_LIMIT` bytes trim to `1234567`,
        // a perfectly plausible ordinal that the environment never named. Reading it would
        // activate a mutant nobody asked for and record its verdict against a run that never
        // happened, so a truncated read has to mean `NONE`.
        let mut text = [b' '; 25 + 13];
        text[25..].copy_from_slice(b"1234567890123");
        let mut copied = [0; READ_LIMIT];

        // SAFETY: `text` is readable for the bounded copy and remains unmodified.
        let length = unsafe { copy_c_string(text.as_ptr(), &mut copied) };

        assert_eq!(length, None, "a truncated value is no value");
        assert_eq!(length.map_or(NONE, |length| parse(&copied[..length])), NONE);
    }

    /// A captured path is terminated where it ends, rather than where the storage happens to be
    /// zero.
    #[cfg(unix)]
    #[test]
    fn a_captured_path_is_terminated_by_the_capture_itself() {
        let mut path = [0xFF_u8; PATH_LIMIT];

        path[..4].copy_from_slice(b"/tmp");

        assert_eq!(
            terminate_census_path(&mut path, EnvironmentValue::Found(4)),
            CensusRequest::Path(NonZeroUsize::new(4).expect("four is non-zero"))
        );
        assert_eq!(path[4], 0, "the capture must write the terminator `fopen` stops at");
        assert_eq!(path[5], 0xFF, "and must write nothing beyond it");
    }

    /// The boundary the zeroed static hides: a value one byte short of the limit.
    ///
    /// Its terminator is the buffer's very last byte, so a capture that wrote no terminator would
    /// leave `fopen` reading past the end of the static — undefined behavior — the moment the
    /// storage stopped being zero-initialized. The buffer is pre-filled with non-zero bytes here
    /// precisely so that nothing but an explicit write can make this pass.
    #[cfg(unix)]
    #[test]
    fn a_path_one_byte_short_of_the_limit_is_terminated_at_the_last_byte() {
        let mut path = [0xFF_u8; PATH_LIMIT];
        let length = PATH_LIMIT - 1;

        path[..length].fill(b'p');

        assert_eq!(
            terminate_census_path(&mut path, EnvironmentValue::Found(length)),
            CensusRequest::Path(NonZeroUsize::new(length).expect("PATH_LIMIT minus one is non-zero"))
        );
        assert_eq!(path[length], 0, "the longest retainable path is left unterminated");
        assert!(path[..length].iter().all(|byte| *byte == b'p'), "the path itself was disturbed");
    }

    /// A value that fills the buffer has nowhere to put a terminator, so no path is published.
    #[cfg(unix)]
    #[test]
    fn a_path_that_exactly_fills_the_limit_is_census_mode_without_a_usable_path() {
        let mut path = [b'p'; PATH_LIMIT];

        assert_eq!(
            terminate_census_path(&mut path, EnvironmentValue::Found(PATH_LIMIT)),
            CensusRequest::Unusable
        );
        assert!(
            path.iter().all(|byte| *byte == b'p'),
            "a truncated value must not be terminated over, nor written past"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_or_unset_census_variable_is_absence_rather_than_census_mode() {
        let mut path = [0_u8; PATH_LIMIT];

        assert_eq!(terminate_census_path(&mut path, EnvironmentValue::Absent), CensusRequest::Absent);
        assert_eq!(terminate_census_path(&mut path, EnvironmentValue::Found(0)), CensusRequest::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_environment_is_not_reported_as_an_absent_census_variable() {
        let mut path = [0_u8; PATH_LIMIT];

        assert_eq!(terminate_census_path(&mut path, EnvironmentValue::Error), CensusRequest::Error);
    }

    /// A census request this process could not read never selects an active mutant.
    ///
    /// The failure this pins: falling through to `GAMMA_ACTIVE` would run the mutant, write no
    /// census file, and hand the coordinator a baseline failure it would read as a verdict about
    /// that mutant. The caller turns this `Err` into the environment-error protocol instead.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_census_environment_failure_is_a_startup_failure_rather_than_a_mutant_selection() {
        let consulted = Cell::new(0_usize);
        let answer = selection_from(CensusRequest::Error, || {
            consulted.set(consulted.get() + 1);

            Ok(7)
        });

        assert_eq!(answer, Err(()));
        assert_eq!(consulted.get(), 0, "a failed census read went on to select an active mutant");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_absent_census_variable_falls_through_to_the_active_ordinal() {
        assert_eq!(selection_from(CensusRequest::Absent, || Ok(7)), Ok(7));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn an_active_environment_failure_is_still_a_startup_failure() {
        assert_eq!(selection_from(CensusRequest::Absent, || Err(())), Err(()));
    }

    /// Either census answer selects census mode, and neither reads `GAMMA_ACTIVE` at all.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_census_request_selects_census_mode_without_consulting_the_active_ordinal() {
        for request in [
            CensusRequest::Path(NonZeroUsize::new(4).expect("four is non-zero")),
            CensusRequest::Unusable,
        ] {
            let consulted = Cell::new(0_usize);
            let answer = selection_from(request, || {
                consulted.set(consulted.get() + 1);

                Ok(7)
            });

            assert_eq!(answer, Ok(CENSUS), "{request:?}");
            assert_eq!(consulted.get(), 0, "census mode consulted `GAMMA_ACTIVE`: {request:?}");
        }
    }

    /// One scripted `read` of the environ stream.
    ///
    /// The three shapes a real read has: bytes placed in the chunk buffer — an empty chunk being
    /// end of stream — a signal that interrupted it before it moved anything, and a failure no
    /// repetition can fix.
    #[cfg(target_os = "linux")]
    enum ScriptedRead<'a> {
        Chunk(&'a [u8]),
        Interrupted,
        Failed,
    }

    /// Reports the reads a script supplies in order, mirroring what a real `read` syscall means to
    /// [`scan_environ`]. Panics if [`scan_environ`] asks for more reads than the script supplies,
    /// which would mean a change grew the number of reads a case needs.
    #[cfg(target_os = "linux")]
    fn scripted_reads<'a>(reads: &'a [ScriptedRead<'a>]) -> impl FnMut(&mut [u8; ENVIRONMENT_READ_CHUNK]) -> ReadOutcome + 'a {
        let mut reads = reads.iter();

        move |buffer: &mut [u8; ENVIRONMENT_READ_CHUNK]| {
            let read = reads
                .next()
                .expect("the script must supply a read for every loop iteration scan_environ takes");

            match read {
                ScriptedRead::Chunk(chunk) => {
                    buffer[..chunk.len()].copy_from_slice(chunk);

                    ReadOutcome::Read(chunk.len())
                }
                ScriptedRead::Interrupted => ReadOutcome::Interrupted,
                ScriptedRead::Failed => ReadOutcome::Failed,
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_key_split_across_a_read_boundary_still_matches() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NA"), ScriptedRead::Chunk(b"ME=value\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_split_right_at_the_delimiter_still_matches() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAME"), ScriptedRead::Chunk(b"=value\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_split_inside_the_value_still_matches() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAME=val"), ScriptedRead::Chunk(b"ue\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_split_immediately_before_the_terminator_still_matches() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAME=value"), ScriptedRead::Chunk(b"\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_multibyte_unicode_value_split_across_a_read_boundary_still_matches() {
        // "héllo" as UTF-8: 'é' is the two-byte sequence 0xC3 0xA9, split here between two reads.
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAME=h\xC3"), ScriptedRead::Chunk(b"\xA9llo\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(6));
        assert_eq!(&destination[..6], "héllo".as_bytes());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_over_limit_value_split_across_reads_still_reports_the_destination_length() {
        let mut destination = [0_u8; 4];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[
                ScriptedRead::Chunk(b"NAME=ab"),
                ScriptedRead::Chunk(b"cd"),
                ScriptedRead::Chunk(b"efghij\0"),
            ]),
        );

        assert_eq!(answer, EnvironmentValue::Found(4));
        assert_eq!(&destination, b"abcd");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_key_that_diverges_after_a_partial_match_is_not_mistaken_for_the_target() {
        // "NAMES" shares its first four bytes with the target "NAME" but is a different, longer
        // key; its fifth byte must break the match rather than being treated as the `=` that would
        // start a value.
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAMES=wrong\0NAME=value\0")]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_absent_key_is_reported_once_the_stream_ends() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"OTHER=x\0"), ScriptedRead::Chunk(b"")]),
        );

        assert_eq!(answer, EnvironmentValue::Absent);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_read_is_reported_as_an_error_rather_than_absence() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[ScriptedRead::Chunk(b"NAME="), ScriptedRead::Failed]),
        );

        assert_eq!(answer, EnvironmentValue::Error);
    }

    /// A signal that interrupts a read costs the capture nothing but the repeat.
    ///
    /// Reporting the transient interruption as a failure would make the caller emit the
    /// environment-error marker and terminate, so a run under an ordinary job-control or timer
    /// signal would end instead of taking its census.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_interrupted_read_is_retried_rather_than_reported_as_a_failure() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ(
            b"NAME",
            &mut destination,
            scripted_reads(&[
                ScriptedRead::Interrupted,
                ScriptedRead::Chunk(b"NAME=val"),
                ScriptedRead::Interrupted,
                ScriptedRead::Chunk(b"ue\0"),
            ]),
        );

        assert_eq!(answer, EnvironmentValue::Found(5));
        assert_eq!(&destination[..5], b"value");
    }

    /// Retrying is bounded, so a process being signalled without pause fails rather than spins.
    ///
    /// This runs inside a loader constructor, where an unbounded retry loop is a hang with no
    /// diagnostic at all: the process never reaches `main` and never says why.
    #[cfg(target_os = "linux")]
    #[test]
    fn endless_interruptions_are_reported_as_a_failure_rather_than_spun_on() {
        let script: Vec<ScriptedRead<'_>> = (0..=INTERRUPTED_ATTEMPTS).map(|_| ScriptedRead::Interrupted).collect();
        let mut destination = [0_u8; 16];
        let answer = scan_environ(b"NAME", &mut destination, scripted_reads(&script));

        assert_eq!(answer, EnvironmentValue::Error);
    }

    /// The interruption budget covers a whole capture rather than being refilled by every read.
    ///
    /// A per-read budget would let an interruption arriving between chunks reset the count and
    /// leave the loop unbounded after all.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_interruption_budget_is_not_refilled_by_a_read_that_succeeded() {
        let mut script: Vec<ScriptedRead<'_>> = (0..INTERRUPTED_ATTEMPTS).map(|_| ScriptedRead::Interrupted).collect();

        script.push(ScriptedRead::Chunk(b"NAME=v"));
        script.push(ScriptedRead::Interrupted);

        let mut destination = [0_u8; 16];
        let answer = scan_environ(b"NAME", &mut destination, scripted_reads(&script));

        assert_eq!(answer, EnvironmentValue::Error);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_read_phase_honors_interruptions_spent_while_opening() {
        let mut destination = [0_u8; 16];
        let answer = scan_environ_with_interruptions(
            b"NAME",
            &mut destination,
            INTERRUPTED_ATTEMPTS,
            scripted_reads(&[ScriptedRead::Interrupted]),
        );

        assert_eq!(answer, EnvironmentValue::Error);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_successful_read_reports_its_length_without_consulting_errno() {
        let lookups = Cell::new(0_usize);
        let outcome = read_outcome(12, || {
            lookups.set(lookups.get() + 1);

            EINTR
        });

        assert_eq!(outcome, ReadOutcome::Read(12));
        assert_eq!(lookups.get(), 0, "a successful read paid for an errno lookup it cannot need");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_end_of_stream_read_is_a_length_rather_than_a_failure() {
        let lookups = Cell::new(0_usize);
        let outcome = read_outcome(0, || {
            lookups.set(lookups.get() + 1);

            EINTR
        });

        assert_eq!(outcome, ReadOutcome::Read(0));
        assert_eq!(lookups.get(), 0, "end of stream is not a failure and must not consult errno");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_read_interrupted_by_a_signal_is_worth_repeating() {
        assert_eq!(read_outcome(-1, || EINTR), ReadOutcome::Interrupted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_read_that_failed_for_any_other_reason_is_final() {
        // `EIO`. Repeating it would only produce the same answer more slowly.
        assert_eq!(read_outcome(-1, || 5), ReadOutcome::Failed);
    }

    #[cfg(any(windows, test))]
    #[test]
    fn an_unset_variable_is_absent() {
        assert_eq!(
            environment_read_outcome(0, ERROR_ENVVAR_NOT_FOUND, 16),
            EnvironmentReadOutcome::Absent
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn an_empty_variable_is_distinguished_from_absence() {
        assert_eq!(environment_read_outcome(0, ERROR_SUCCESS, 16), EnvironmentReadOutcome::Empty);
    }

    #[cfg(any(windows, test))]
    #[test]
    fn a_zero_return_with_an_api_error_is_not_absence() {
        assert_eq!(environment_read_outcome(0, 5, 16), EnvironmentReadOutcome::Error);
    }

    #[cfg(any(windows, test))]
    #[test]
    fn a_value_that_exactly_fills_the_buffer_is_too_long_to_store() {
        // `GetEnvironmentVariable{A,W}` reports a length equal to the buffer when the value did
        // not fit — there is no room left for the terminator this crate always keeps.
        assert_eq!(
            environment_read_outcome(16, ERROR_SUCCESS, 16),
            EnvironmentReadOutcome::TooLongToStore
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn a_value_one_past_the_buffer_is_too_long_to_store() {
        assert_eq!(
            environment_read_outcome(17, ERROR_SUCCESS, 16),
            EnvironmentReadOutcome::TooLongToStore
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn a_value_that_leaves_room_for_the_terminator_is_found() {
        assert_eq!(
            environment_read_outcome(15, ERROR_SUCCESS, 16),
            EnvironmentReadOutcome::Found(NonZeroUsize::new(15).expect("fifteen is non-zero"))
        );
    }

    #[test]
    fn the_variable_names_agree() {
        // Two spellings of one name is two chances to change only one of them.
        assert_eq!(ACTIVE_VAR.as_bytes(), &ACTIVE_VAR_C[..ACTIVE_VAR_C.len() - 1]);
        assert_eq!(ACTIVE_VAR_C.last(), Some(&0));
        assert_eq!(CENSUS_VAR.as_bytes(), &CENSUS_VAR_C[..CENSUS_VAR_C.len() - 1]);
        assert_eq!(CENSUS_VAR_C.last(), Some(&0));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_failed_exit_registration_refuses_census_mode() {
        let registrations = Cell::new(0);
        let selected = selection_after_registration(CENSUS, |_handler| {
            registrations.set(registrations.get() + 1);
            -1
        });

        assert_eq!(selected, NONE, "an unsealable census must not be published as active");
        assert_eq!(registrations.get(), 1);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn parallel_shared_state_recording_and_sealing_drains_every_admitted_writer() {
        use core::sync::atomic::AtomicUsize;

        for _round in 0..250 {
            let state = RecorderState::new();
            let admitted = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);

            std::thread::scope(|scope| {
                for _writer in 0..4 {
                    let _worker = scope.spawn(|| {
                        if state.begin_recording() {
                            let _admitted = admitted.fetch_add(1, Ordering::Relaxed);
                            let _completed = completed.fetch_add(1, Ordering::Relaxed);
                            state.end_recording();
                        }
                    });
                }

                let _sealer = scope.spawn(|| assert!(state.begin_seal()));
            });

            assert_eq!(completed.load(Ordering::Relaxed), admitted.load(Ordering::Relaxed));
            assert!(!state.begin_recording(), "recording reopened after the state was sealed");
        }
    }

    #[cfg(not(miri))]
    mod process_tests {
        use std::env;
        #[cfg(any(unix, windows))]
        use std::fs;
        #[cfg(windows)]
        use std::fs::OpenOptions;
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt as _;
        #[cfg(any(unix, windows))]
        use std::path::Path;
        #[cfg(any(unix, windows))]
        use std::process;
        use std::process::Command;
        #[cfg(windows)]
        use std::process::Stdio;
        #[cfg(any(unix, windows))]
        use std::thread;

        use super::*;

        #[test]
        fn the_environment_decides_what_is_read() {
            // Setting a variable in this process would be a data race against every other test, so the
            // check runs in a child launched with the variable already set — which is exactly how the
            // tool passes an ordinal to a real test binary.
            let executable = env::current_exe().expect("the test binary knows its own path");

            // Thirty-eight bytes, longer than `READ_LIMIT`, whose first thirty-two trim to `1234567`.
            // A truncated read must not be mistaken for that ordinal.
            let truncated = format!("{}1234567890123", " ".repeat(25));

            for (value, expected) in [
                ("31", "read=31"),
                ("not a number", "read=0"),
                ("", "read=0"),
                (truncated.as_str(), "read=0"),
            ] {
                let output = Command::new(&executable)
                    .args([
                        "--exact",
                        "runtime::tests::process_tests::the_child_reports_what_it_read",
                        "--nocapture",
                    ])
                    .env(ACTIVE_VAR, value)
                    .output()
                    .expect("the child runs");

                let text = String::from_utf8_lossy(&output.stdout).into_owned();

                assert!(text.contains(expected), "`{value}` produced {text}");
            }
        }

        #[test]
        fn the_child_reports_what_it_read() {
            // Only meaningful when launched by the test above; harmless on its own.
            println!("read={}", active());
        }

        #[test]
        fn active_is_immune_to_the_process_changing_its_own_environment() {
            // The child exits from a test-only constructor before libtest reaches `main`, so its
            // process-global environment mutation is genuinely single-threaded. The exit status
            // reports whether it captured 7, changed the native environment to 99, and still
            // observed 7 afterwards.
            let executable = env::current_exe().expect("the test binary knows its own path");

            let output = Command::new(&executable)
                .env(ENVIRONMENT_HELPER_VAR, "1")
                .env(ACTIVE_VAR, "7")
                .env_remove(CENSUS_VAR)
                .output()
                .expect("the pre-main environment helper runs");

            assert!(
                output.status.success(),
                "pre-main environment helper failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[test]
        #[cfg(all(any(unix, windows), not(miri)))]
        fn a_guard_reached_before_installation_terminates_instead_of_reporting_baseline() {
            // Linking a fixture whose own pre-main constructor calls a guard, at varying object
            // and library order across ELF, Mach-O, and Windows, is beyond what a source-level
            // regression can drive; this proves the underlying mechanism directly instead. The
            // child's own `install` constructor already ran before its `main`, exactly like every
            // other test in this binary, so the sentinel is forced back afterward purely to
            // simulate a guard that runs in the window before that constructor executes.
            //
            // A subprocess of its own, selected by name and admitted by an environment marker: the
            // forcing is one-way, so the inner half would invalidate sibling test results in any
            // process it shared with them.
            let executable = env::current_exe().expect("the test binary knows its own path");

            let output = Command::new(&executable)
                .args([
                    "--exact",
                    "runtime::tests::process_tests::the_child_simulates_a_pre_install_guard",
                    "--nocapture",
                ])
                .env(PRE_INSTALL_CHILD, "1")
                .output()
                .expect("the child runs");

            let said = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Checked before the exit status, and the reason the inner half announces itself at
            // all: a filter that matched nothing — a renamed inner test — and an inner half that
            // returned early because the marker never reached it are both successful runs of zero
            // tests, and a failure for some third reason is a non-zero exit that proves nothing
            // about the guard. Only this marker says the child really reached the pre-install
            // state, which is what the two assertions below are then reading the consequences of.
            assert!(
                said.contains(PRE_INSTALL_REACHED),
                "the child never reached the simulated pre-install state\n{said}\n{stderr}"
            );
            assert!(!output.status.success(), "a pre-install guard must not exit successfully");
            assert_eq!(
                output.status.code(),
                Some(86),
                "a pre-install guard must use the reserved infrastructure-failure status"
            );
            assert!(
                stderr.contains("gamma_rt: a guard executed before this crate's own constructor"),
                "{stderr}"
            );
            assert!(
                !said.contains(PRE_INSTALL_CONTINUED),
                "catch_unwind absorbed the pre-install failure and execution continued\n{said}\n{stderr}"
            );
        }

        #[test]
        #[cfg(windows)]
        fn a_startup_error_is_written_to_overlapped_stderr() {
            const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

            let executable = env::current_exe().expect("the test binary knows its own path");
            let directory = tempfile::tempdir().expect("an owned temporary directory");
            let stderr_path = directory.path().join("stderr");
            let stderr = OpenOptions::new()
                .create_new(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(&stderr_path)
                .expect("an overlapped stderr file");

            let status = Command::new(&executable)
                .env("GAMMA_RT_ENVIRONMENT_ERROR_HELPER", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr))
                .status()
                .expect("the child runs");
            let bytes = fs::read(&stderr_path).expect("the child's stderr remains readable");

            assert_eq!(
                status.code(),
                Some(86),
                "the startup failure keeps its reserved exit code: {}",
                String::from_utf8_lossy(&bytes)
            );
            assert!(
                bytes
                    .windows(ENVIRONMENT_ERROR_MARKER.len())
                    .any(|window| window == ENVIRONMENT_ERROR_MARKER),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
        }

        /// Marks a re-run of this test binary as the inner half of the pre-install guard test.
        ///
        /// Carried on the environment rather than the command line because the command line belongs
        /// to the test harness, which rejects arguments it does not know.
        #[cfg(all(any(unix, windows), not(miri)))]
        const PRE_INSTALL_CHILD: &str = "GAMMA_RT_PRE_INSTALL_CHILD";

        /// What the inner half prints once it has forced the pre-install state, before it terminates.
        #[cfg(all(any(unix, windows), not(miri)))]
        const PRE_INSTALL_REACHED: &str = "the pre-install state was forced";

        /// What a catchable pre-install panic would let the inner half print incorrectly.
        #[cfg(all(any(unix, windows), not(miri)))]
        const PRE_INSTALL_CONTINUED: &str = "execution continued after the pre-install guard";

        #[test]
        #[cfg(all(any(unix, windows), not(miri)))]
        fn the_child_simulates_a_pre_install_guard() {
            // Inert unless the outer test asked for it, because what it does cannot be undone.
            // `install` runs once, before `main`, so a process whose sentinel has been forced back
            // to `UNINSTALLED` stays that way: every sibling test the harness schedules afterward
            // in this binary would then reach a guard that correctly terminates the process. Which
            // tests those are is a matter of harness scheduling, so the resulting invalid test
            // results would differ across hosts. Under a direct `cargo test`, where this runs
            // alongside everything else rather than alone, the marker is absent and this returns
            // without touching anything.
            if env::var_os(PRE_INSTALL_CHILD).is_none() {
                return;
            }

            // Said before the sentinel is touched, so that it is on the wire whichever way the
            // guard below ends this process. `println!` writes through a line buffer, so the
            // newline flushes it without relying on the orderly shutdown that immediate
            // termination skips.
            println!("{PRE_INSTALL_REACHED}");

            ACTIVE.store(UNINSTALLED, Ordering::Release);

            let _caught = std::panic::catch_unwind(active);

            println!("{PRE_INSTALL_CONTINUED}");
        }

        #[test]
        #[cfg(all(unix, not(miri)))]
        fn copy_environment_via_getenv_reads_stable_values_in_an_isolated_process() {
            // Environment mutation is process-global, so even a sequential set-and-read belongs in
            // a child where no sibling test can read the environment at the same time.
            let executable = env::current_exe().expect("the test binary knows its own path");

            let output = Command::new(&executable)
                .env(GETENV_CHILD, "1")
                .args([
                    "--exact",
                    "runtime::tests::process_tests::the_child_reads_stable_values_via_getenv",
                    "--nocapture",
                ])
                .output()
                .expect("the child runs");

            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }

        /// Marks a re-run of this test binary as the isolated half of the `getenv` test.
        #[cfg(all(unix, not(miri)))]
        const GETENV_CHILD: &str = "GAMMA_RT_GETENV_CHILD";

        #[test]
        #[cfg(all(unix, not(miri)))]
        fn the_child_reads_stable_values_via_getenv() {
            if env::var_os(GETENV_CHILD).is_none() {
                return;
            }

            let name = b"GAMMA_RT_COR1_TEST_VARIABLE\0";
            let values: [&[u8]; 4] = [b"1", b"22", b"333", b"4444"];

            // SAFETY: `name` is a NUL-terminated C string and this isolated process has no
            // concurrent environment access.
            unsafe { unsetenv(name.as_ptr().cast()) };

            let mut destination = [0_u8; 32];
            assert_eq!(copy_environment_via_getenv(name, &mut destination), EnvironmentValue::Absent);

            for value in values {
                let mut buffer = [0_u8; 8];
                buffer[..value.len()].copy_from_slice(value);

                // SAFETY: `name` and `buffer` are NUL-terminated C strings, and this isolated
                // process has no concurrent environment access.
                let status = unsafe { setenv(name.as_ptr().cast(), buffer.as_ptr().cast(), 1) };
                assert_eq!(status, 0, "setenv rejected a valid name and value");

                let mut destination = [0_u8; 32];
                let EnvironmentValue::Found(length) = copy_environment_via_getenv(name, &mut destination) else {
                    core::panic!("getenv did not return the stable value");
                };

                assert_eq!(&destination[..length], value);
            }
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_reserved_ordinal_is_inactive_in_baseline_census_and_mutant_processes() {
            let executable = env::current_exe().expect("the test binary knows its own path");

            let baseline = Command::new(&executable)
                .args([
                    "--exact",
                    "runtime::tests::process_tests::the_child_reports_guard_selection",
                    "--nocapture",
                ])
                .env_remove(ACTIVE_VAR)
                .env_remove(CENSUS_VAR)
                .output()
                .expect("the baseline child runs");
            assert!(baseline.status.success(), "{}", String::from_utf8_lossy(&baseline.stderr));
            let baseline = String::from_utf8_lossy(&baseline.stdout);

            assert!(baseline.contains("active=0 none=false seven=false"), "{baseline}");

            let mutant = Command::new(&executable)
                .args([
                    "--exact",
                    "runtime::tests::process_tests::the_child_reports_guard_selection",
                    "--nocapture",
                ])
                .env(ACTIVE_VAR, "7")
                .env_remove(CENSUS_VAR)
                .output()
                .expect("the mutant child runs");
            assert!(mutant.status.success(), "{}", String::from_utf8_lossy(&mutant.stderr));
            let mutant = String::from_utf8_lossy(&mutant.stdout);

            assert!(mutant.contains("active=7 none=false seven=true"), "{mutant}");

            let sequence = CENSUS_NONE_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = env::current_dir()
                .expect("the test process has a working directory")
                .join(format!(".gamma-census-none-{}-{sequence}.bin", process::id()));
            let _removed = fs::remove_file(&path);

            let census = Command::new(&executable)
                .args([
                    "--exact",
                    "runtime::tests::process_tests::the_child_reports_guard_selection",
                    "--nocapture",
                ])
                .env(ACTIVE_VAR, "7")
                .env(CENSUS_VAR, &path)
                .output()
                .expect("the census child runs");
            assert!(census.status.success(), "{}", String::from_utf8_lossy(&census.stderr));
            let census = String::from_utf8_lossy(&census.stdout);
            let _removed = fs::remove_file(&path);

            assert!(census.contains("active=0 none=false seven=false"), "{census}");
        }

        #[test]
        fn the_child_reports_guard_selection() {
            // Only meaningful when launched by the parent regression; harmless on its own.
            println!("active={} none={} seven={}", active(), a(NONE), a(7));
        }

        /// Runs a census child and returns its raw output after removing the project-local test file.
        #[cfg(any(unix, windows))]
        fn census_bytes_of(test: &str) -> Vec<u8> {
            static CENSUS_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

            let executable = env::current_exe().expect("the test binary knows its own path");
            let sequence = CENSUS_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = env::current_dir()
                .expect("the test process has a working directory")
                .join(format!(".gamma-census-{}-{sequence}.bin", process::id()));

            // Opened for appending, so a leftover from an earlier run would be read as part of this
            // one. Removing it first is what makes the assertion below about the whole file sound.
            let _ = fs::remove_file(&path);

            let output = Command::new(&executable)
                .args(["--exact", test, "--nocapture"])
                .env(CENSUS_VAR, &path)
                .output()
                .expect("the child runs");

            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));

            // Nothing flushes the census before the child exits, so this read has to happen after the
            // wait that `output` performed.
            let bytes = fs::read(&path).expect("the child wrote a census");
            let _ = fs::remove_file(&path);

            bytes
        }

        /// Runs one of the census children below and returns the sealed records it wrote, in order.
        #[cfg(any(unix, windows))]
        fn census_of(test: &str) -> Vec<u32> {
            let bytes = census_bytes_of(test);

            assert_eq!(bytes.len() % 4, 0, "a census is whole four-byte records");

            let mut records: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|record| u32::from_le_bytes(record.try_into().expect("a chunk of four is four bytes")))
                .collect();

            // Every clean census ends with the seal that vouches it is whole. The reader requires it,
            // and so does every assertion below, which is about the records written *before* it.
            assert_eq!(records.pop(), Some(SEAL), "a clean census is sealed");

            records
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_census_records_each_site_it_reached_exactly_once() {
            assert_eq!(census_of("runtime::tests::process_tests::the_child_walks_a_few_sites"), vec![3, 9]);
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_census_does_not_touch_its_file_until_process_exit() {
            assert_eq!(census_of("runtime::tests::process_tests::the_child_buffers_its_sites"), vec![17]);
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_census_serializes_more_than_one_output_batch_in_order() {
            let expected: Vec<u32> = (0..1100).collect();

            assert_eq!(
                census_of("runtime::tests::process_tests::the_child_fills_multiple_output_batches"),
                expected
            );
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_census_that_reached_no_site_is_still_sealed() {
            // A test can legitimately reach no instrumented site — that convicts no mutant, which is an
            // answer, not a failure. The seal is written even then, so an *empty* census is a lone seal
            // and an *absent* one is unambiguously a run that failed. `census_of` pops the seal, so
            // what a zero-reach child leaves behind is nothing at all.
            assert_eq!(
                census_of("runtime::tests::process_tests::the_child_touches_no_sites"),
                Vec::<u32>::new()
            );
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_site_claimed_while_sealing_waits_is_written_before_the_seal() {
            assert_eq!(
                census_of("runtime::tests::process_tests::the_child_holds_a_lease_before_claiming_a_site"),
                vec![79]
            );
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_short_exit_write_leaves_the_census_unsealed() {
            let bytes = census_bytes_of("runtime::tests::process_tests::the_child_forces_a_short_exit_write");

            assert_eq!(bytes.len() % 4, 0, "the test seam never writes a partial record");
            assert!(
                !bytes
                    .chunks_exact(4)
                    .map(|record| u32::from_le_bytes(record.try_into().expect("a chunk of four is four bytes")))
                    .any(|record| record == SEAL),
                "a short exit write must prevent the seal"
            );
        }

        #[test]
        fn the_child_touches_no_sites() {
            // Reaches no guard, so records nothing. Only meaningful when launched by the test above,
            // which checks the census still exists and is still sealed; harmless on its own.
            assert_eq!(active(), NONE);
            assert!(!any());
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_child_holds_a_lease_before_claiming_a_site() {
            // This child is launched by the parent test with census mode selected. Running it directly
            // under the ordinary test harness exercises no census path and must remain harmless.
            if selected() != CENSUS {
                return;
            }

            TEST_NOTE_STATE.store(BLOCK_AFTER_LEASE, Ordering::Release);
            TEST_SEAL_WAITING.store(false, Ordering::Release);
            TEST_SEAL_CLAIMED.store(false, Ordering::Release);

            thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    assert!(!a(79));
                });

                while TEST_NOTE_STATE.load(Ordering::Acquire) != LEASE_HELD {
                    core::hint::spin_loop();
                }

                let sealer = scope.spawn(|| seal());

                while !TEST_SEAL_WAITING.load(Ordering::Acquire) && !TEST_SEAL_CLAIMED.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }

                let waited = TEST_SEAL_WAITING.load(Ordering::Acquire);
                let claimed = TEST_SEAL_CLAIMED.load(Ordering::Acquire);

                TEST_NOTE_STATE.store(RELEASE_LEASE, Ordering::Release);

                writer.join().expect("the site writer must not panic");
                sealer.join().expect("the sealing thread must not panic");

                assert!(waited, "seal claimed an empty writer set before the site claim acquired its lease");
                assert!(!claimed, "seal completed while a pre-claim writer lease was held");
            });
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_child_forces_a_short_exit_write() {
            // This test is also discovered in the parent process. Only the child launched above has
            // census mode selected during startup, so only it can drive the runtime's exit-time write.
            if selected() != CENSUS {
                return;
            }

            assert!(!a(73));

            TEST_WRITE_STATE.store(BLOCK_AND_SHORT, Ordering::Release);
            thread::scope(|scope| {
                let sealer = scope.spawn(|| seal());

                while TEST_WRITE_STATE.load(Ordering::Acquire) != WRITE_ENTERED {
                    core::hint::spin_loop();
                }

                // The exit writer is inside `fwrite`'s test seam. Releasing it as a forced zero write
                // must make the handler return before appending its seal.
                TEST_WRITE_STATE.store(RELEASE_SHORT_WRITE, Ordering::Release);

                sealer.join().expect("the sealing thread must not panic");
            });
        }

        #[test]
        fn the_child_walks_a_few_sites() {
            // Every guard answers `false` under a census, exactly as it does with no mutant selected,
            // so the child runs the code its author wrote and the sites it reaches are the real ones.
            assert!(!a(3));
            assert!(!a(3));
            assert!(!a(9));
            assert!(!a(3));

            // A census is not a mutant, and nothing outside the guard should be able to tell it from
            // an ordinary unmutated run.
            assert_eq!(active(), NONE);
            assert!(!any());
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_child_buffers_its_sites() {
            if selected() != CENSUS {
                return;
            }

            assert!(!a(17));

            let path = env::var_os(CENSUS_VAR).expect("the parent supplied a census path");

            assert!(
                !Path::new(&path).exists(),
                "recording a site touched the census file before process exit"
            );
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_child_fills_multiple_output_batches() {
            if selected() != CENSUS {
                return;
            }

            for site in 0..1100 {
                assert!(!a(site));
            }
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn a_site_past_the_table_marks_the_whole_census_untrustworthy() {
            // The marker is what stops an unrecordable site being mistaken for an unreached one. The
            // bitmap is serialized in ordinal order at exit, followed by the overflow state.
            assert_eq!(
                census_of("runtime::tests::process_tests::the_child_walks_past_the_table"),
                vec![1, 2, OVERFLOW]
            );
        }

        #[test]
        #[cfg(any(unix, windows))]
        fn the_child_walks_past_the_table() {
            let past = u32::try_from(SITES).expect("the table is smaller than the ordinal space");

            assert!(!a(1));
            assert!(!a(past));

            // Recording continues past the overflow; all reached in-range sites remain available when
            // the bitmap is serialized at exit.
            assert!(!a(2));

            // Marked once however many sites overflow, because the tool needs the fact and not a
            // record of every site that produced it.
            assert!(!a(past + 1));
        }
    }
}

#[cfg(all(loom, feature = "loom", any(unix, windows)))]
mod loom_models {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, Ordering};

    use super::RecorderState;

    pub(super) fn record_versus_seal_never_seals_over_an_admitted_record() {
        loom::model(|| {
            let state = Arc::new(RecorderState::new());
            let admitted = Arc::new(AtomicBool::new(false));
            let recorded = Arc::new(AtomicBool::new(false));

            let writer = {
                let state = Arc::clone(&state);
                let admitted = Arc::clone(&admitted);
                let recorded = Arc::clone(&recorded);

                loom::thread::spawn(move || {
                    if state.begin_recording() {
                        admitted.store(true, Ordering::Relaxed);
                        recorded.store(true, Ordering::Relaxed);
                        state.end_recording();
                    }
                })
            };
            let sealer = {
                let state = Arc::clone(&state);

                loom::thread::spawn(move || state.try_begin_seal())
            };

            writer.join().expect("writer thread");
            let sealed = sealer.join().expect("sealer thread");

            if sealed != Some(true) {
                assert!(state.begin_seal(), "the retry must seal after the writer drains");
            }

            if admitted.load(Ordering::Relaxed) {
                assert!(recorded.load(Ordering::Relaxed), "the seal passed an admitted record");
            }
            assert!(!state.begin_recording(), "recording reopened after sealing");
        });
    }
}

/// Runs the deterministic concurrency model selected by the dedicated Loom test target.
#[cfg(all(loom, feature = "loom", any(unix, windows)))]
#[doc(hidden)]
pub fn run_loom_models() {
    loom_models::record_versus_seal_never_seals_over_an_admitted_record();
}

/// Runs no models on targets without a supported synchronization backend.
#[cfg(all(loom, feature = "loom", not(any(unix, windows))))]
#[doc(hidden)]
pub fn run_loom_models() {}
