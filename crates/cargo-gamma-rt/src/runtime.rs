// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(any(unix, windows))]
use core::cell::UnsafeCell;
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
/// One bit each, in a zeroed static, so the whole table costs 128 KiB of address space that is
/// never paged in unless a census actually runs. A population past this is not silently
/// half-recorded: a site over the edge records [`OVERFLOW`], which tells the tool to throw
/// the whole census away rather than mistake an unrecorded site for an unreached one.
#[cfg(any(unix, windows))]
const SITES: usize = 1 << 20;

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

/// The marker record the runtime writes last, at a clean exit, to vouch that the census is whole.
///
/// A census file is written at process exit, with this record last, so a reader that finds it intact
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
/// path into static storage without an allocator. A path beyond this bound was already unusable on
/// Windows and now leaves the census unsealed on Unix too, which is the conservative outcome.
#[cfg(any(unix, windows))]
const PATH_LIMIT: usize = 4096;

/// The longest value worth reading: ten digits is every `u32`, and the rest is room for spaces.
#[cfg(any(unix, windows))]
const READ_LIMIT: usize = 32;

/// The selection captured by [`install`] before user code can start threads.
///
/// `NONE` is the safe fallback for an exotic loader that reaches a guard before this runtime's
/// constructor. Crucially, that fallback does not try to consult the environment lazily.
static ACTIVE: AtomicU32 = AtomicU32::new(NONE);

/// The captured census path's length, excluding its terminator.
///
/// A zero length means no usable path was captured. The path is written before this is published
/// with `Release`, and [`open`] acquires it before reading the corresponding static buffer.
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
    // Census mode wins over an active ordinal, as it did when guards read these variables lazily.
    // Retain its path now as well: `open` must never have to touch the environment later.
    if capture_census_path() {
        CENSUS
    } else {
        #[cfg(unix)]
        match capture_active() {
            Ok(active) => active,
            Err(()) => environment_error(),
        }

        #[cfg(windows)]
        capture_active()
    }
}

/// Marker emitted when the runtime cannot acquire the startup environment.
///
/// `cargo-gamma-lib` recognizes this exact byte sequence before interpreting a test runner's exit
/// status. It is public because the vendored runtime and its parent must share one protocol value.
pub const ENVIRONMENT_ERROR_MARKER: &[u8] = b"cargo-gamma: startup environment acquisition failed\n";

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvironmentValue {
    Found(usize),
    Absent,
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

/// Reads `GAMMA_ACTIVE` during startup without allocation.
#[cfg(windows)]
fn capture_active() -> u32 {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let mut buffer = [0_u8; READ_LIMIT];

    // SAFETY: `ACTIVE_VAR_C` is NUL-terminated and `buffer` is writable for the supplied length.
    let written = unsafe {
        GetEnvironmentVariableA(
            ACTIVE_VAR_C.as_ptr(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };

    let Ok(length) = usize::try_from(written) else {
        return NONE;
    };

    if length == 0 || length >= buffer.len() {
        return NONE;
    }

    parse(&buffer[..length])
}

/// Captures a non-empty `GAMMA_CENSUS` path and reports whether census mode was requested.
///
/// A requested path that does not fit static storage still selects census mode; it simply cannot
/// be opened, and therefore remains unsealed and is rejected by the reader.
#[cfg(unix)]
fn capture_census_path() -> bool {
    // SAFETY: only this constructor writes the `UnsafeCell`, before the release publication below.
    let path = unsafe { &mut *CENSUS_PATH.bytes.get() };

    let length = match copy_environment(CENSUS_VAR_C, path) {
        EnvironmentValue::Found(length) => length,
        EnvironmentValue::Absent | EnvironmentValue::Error => return false,
    };

    if length == 0 {
        return false;
    }

    if length == path.len() {
        return true;
    }

    CENSUS_PATH_LENGTH.store(length, Ordering::Release);
    true
}

/// Windows' equivalent of [`capture_census_path`].
#[cfg(windows)]
fn capture_census_path() -> bool {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let mut buffer = [0_u16; PATH_LIMIT];

    // SAFETY: `CENSUS_VAR_W` is NUL-terminated and `buffer` is writable for the supplied length.
    let written = unsafe {
        GetEnvironmentVariableW(
            CENSUS_VAR_W.as_ptr(),
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };

    let Ok(length) = usize::try_from(written) else {
        return false;
    };

    if length == 0 {
        return false;
    }

    if length >= buffer.len() {
        return true;
    }

    // SAFETY: only this constructor writes the `UnsafeCell`, and `length` bytes plus the API's
    // terminator all fit in both arrays.
    unsafe {
        core::ptr::copy_nonoverlapping(buffer.as_ptr(), CENSUS_PATH.bytes.get().cast(), length + 1);
    }
    CENSUS_PATH_LENGTH.store(length, Ordering::Release);

    true
}

/// Parses an ordinal out of copied bytes, treating anything unexpected as [`NONE`].
///
/// Surrounding ASCII whitespace is tolerated because a value threaded through a shell can pick it
/// up. Everything else — an empty value, a sign, a non-digit, a number too large to be an ordinal
/// — selects unmutated behavior, which is the answer that cannot turn a mutated program into a
/// passing one.
///
/// The three reserved words of the encoding — the overflow marker, [`CENSUS`], and the census
/// file's [`SEAL`] — are refused as one range: a population would have to hold four billion
/// mutants to reach them, and a mode or a file marker is not something `GAMMA_ACTIVE` may ask for.
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

    if value >= SEAL { NONE } else { value }
}

/// Copies one Linux environment value from the immutable image captured by `exec`.
///
/// `/proc/self/environ` does not follow later `setenv` changes, so a native constructor that
/// started a thread before this constructor cannot make this read race with environment mutation.
#[cfg(target_os = "linux")]
fn copy_environment(name: &[u8], destination: &mut [u8]) -> EnvironmentValue {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    let Some(target) = name.strip_suffix(&[0]) else {
        return EnvironmentValue::Absent;
    };
    let path = b"/proc/self/environ\0";

    // SAFETY: `path` is NUL-terminated and `O_RDONLY` takes no variadic mode argument.
    let descriptor = unsafe { open_fd(path.as_ptr().cast(), 0) };

    if descriptor < 0 {
        return EnvironmentValue::Error;
    }

    let mut chunk = [0_u8; 512];
    let mut key_at = 0;
    let mut matching = true;
    let mut value_at = None;
    let mut overflowed = false;
    let mut answer = EnvironmentValue::Absent;
    let mut found = false;
    let mut failed = false;

    loop {
        // SAFETY: `chunk` is writable for the supplied length, and `descriptor` remains open.
        let read = unsafe { read_fd(descriptor, chunk.as_mut_ptr().cast(), chunk.len()) };
        let Ok(read) = usize::try_from(read) else {
            failed = true;
            break;
        };

        if read == 0 {
            break;
        }

        for &byte in &chunk[..read] {
            if let Some(length) = value_at.as_mut() {
                if byte == 0 {
                    found = true;
                    answer = EnvironmentValue::Found(if overflowed || *length == destination.len() {
                        destination.len()
                    } else {
                        *length
                    });
                    break;
                }

                if let Some(slot) = destination.get_mut(*length) {
                    *slot = byte;
                    *length += 1;
                } else {
                    overflowed = true;
                }

                continue;
            }

            if byte == 0 {
                key_at = 0;
                matching = true;
            } else if matching && key_at < target.len() && byte == target[key_at] {
                key_at += 1;
            } else if matching && key_at == target.len() && byte == b'=' {
                value_at = Some(0);
            } else {
                matching = false;
            }
        }

        if found {
            break;
        }
    }

    // SAFETY: `descriptor` was returned open above and is closed exactly once here.
    let _closed = unsafe { close_fd(descriptor) };
    if failed { EnvironmentValue::Error } else { answer }
}

/// Copies one environment value on Unix targets without an immutable process-environment image.
#[cfg(all(unix, not(target_os = "linux")))]
fn copy_environment(name: &[u8], destination: &mut [u8]) -> EnvironmentValue {
    #[cfg(test)]
    let _previous = STARTUP_ENVIRONMENT_READS.fetch_add(1, Ordering::Relaxed);

    // SAFETY: every caller supplies one of this module's NUL-terminated constant names. The
    // returned pointer is copied immediately during platform startup and is never retained.
    let value = unsafe { getenv(name.as_ptr().cast()) };

    if value.is_null() {
        return EnvironmentValue::Absent;
    }

    // SAFETY: `getenv` returned a C string that remains valid for this startup-only copy.
    EnvironmentValue::Found(unsafe { copy_c_string(value.cast(), destination) }.unwrap_or(destination.len()))
}

/// Stops startup in a shape the parent recognizes as an infrastructure failure.
#[cfg(unix)]
fn environment_error() -> ! {
    let mut remaining = ENVIRONMENT_ERROR_MARKER;

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

        remaining = remaining
            .get(written..)
            .expect("POSIX write cannot report more bytes than the supplied buffer contains");
    }

    // SAFETY: startup cannot continue without knowing whether the requested mutant was selected.
    // `_exit` avoids running handlers registered by constructors that may only be partly complete.
    unsafe { exit_immediately(86) }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "open"]
    fn open_fd(path: *const core::ffi::c_char, flags: core::ffi::c_int, ...) -> core::ffi::c_int;

    #[link_name = "read"]
    fn read_fd(descriptor: core::ffi::c_int, buffer: *mut core::ffi::c_void, count: usize) -> isize;

    #[link_name = "close"]
    fn close_fd(descriptor: core::ffi::c_int) -> core::ffi::c_int;
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "write"]
    fn write_fd(descriptor: core::ffi::c_int, buffer: *const core::ffi::c_void, count: usize) -> isize;

    #[link_name = "_exit"]
    fn exit_immediately(status: core::ffi::c_int) -> !;
}

#[cfg(all(unix, not(target_os = "linux")))]
unsafe extern "C" {
    /// The C library's `getenv`, whose signature is fixed by POSIX. Declared here rather than
    /// taken from `libc` because this crate is injected into the user's dependency graph and must
    /// stay free of dependencies.
    fn getenv(name: *const core::ffi::c_char) -> *const core::ffi::c_char;
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
    fn fopen(path: *const core::ffi::c_char, mode: *const core::ffi::c_char) -> *mut core::ffi::c_void;

    /// The Microsoft CRT's wide `fopen`, identical to it in everything said above and taking its
    /// path and mode as UTF-16. Windows uses this rather than the narrow `fopen`, which interprets
    /// its path in the active ANSI code page: a scratch tree whose path holds a character that code
    /// page cannot represent would fail to open, `sink` would latch the failure, and the census
    /// would silently never work on that host — the run merely slower, forever, with the reason
    /// invisible.
    #[cfg(windows)]
    fn _wfopen(path: *const u16, mode: *const u16) -> *mut core::ffi::c_void;

    /// The C library's `fwrite`, likewise fixed by the standard, and likewise thread-safe: it
    /// locks the stream, so the guards of a multi-threaded test cannot tear each other's records.
    fn fwrite(buffer: *const core::ffi::c_void, size: usize, count: usize, stream: *mut core::ffi::c_void) -> usize;

    /// The C library's `fflush`, fixed by the standard. Called once at a clean exit to force the
    /// buffered records and the seal out to the file at a point where a failure merely leaves the
    /// file unsealed, rather than deferring to a close whose failure would go unnoticed.
    fn fflush(stream: *mut core::ffi::c_void) -> core::ffi::c_int;

    /// The C library's `atexit`, fixed by the standard. Registers [`seal`] to run at a normal exit;
    /// an abnormal one — `abort`, a fatal signal, `_exit` — skips it by design, which is exactly
    /// what leaves an unsealed file for the reader to reject.
    fn atexit(handler: extern "C" fn()) -> core::ffi::c_int;
}

/// Records that the site with ordinal `id` was reached, if it has not been recorded already.
///
/// The bitmap is what keeps this affordable. A site inside a loop is reached millions of times but
/// occupies one bit, and no file is opened or written until the process exits.
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
fn write_bytes(bytes: &[u8], stream: *mut core::ffi::c_void) -> usize {
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
fn write_record(record: u32, stream: *mut core::ffi::c_void) -> bool {
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
fn buffer_record(record: u32, buffer: &mut [u8; OUTPUT_BUFFER], used: &mut usize, stream: *mut core::ffi::c_void) -> bool {
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
fn write_reached(stream: *mut core::ffi::c_void) -> bool {
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
fn sink() -> *mut core::ffi::c_void {
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
fn open() -> *mut core::ffi::c_void {
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
        // before publishing the non-zero length. The buffer is immutable after publication;
        // `MODE` is NUL-terminated too.
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
fn selection_after_registration(selection: u32, mut register: impl FnMut(extern "C" fn()) -> core::ffi::c_int) -> u32 {
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

/// Returns the ordinal of the mutant active in this process.
///
/// The selection is captured without allocation during process startup. It cannot change later:
/// a test manipulating its own environment must not change which mutant is live halfway through a
/// run, and guards must not read an environment another thread may safely be changing.
///
/// ```rust
/// // Whatever this process was launched with, the answer never changes.
/// assert_eq!(gamma_rt::active(), gamma_rt::active());
/// ```
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
/// needs not to.
#[inline]
fn selected() -> u32 {
    // The constructor's `Release` store publishes the copied census path before a guard observing
    // census mode can use it. The `NONE` initializer is a conservative fallback if an unusual
    // loader invokes a guard before this constructor; it deliberately does not perform a lazy read.
    ACTIVE.load(Ordering::Acquire)
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
/// assert_eq!(gamma_rt::any(), gamma_rt::active() != gamma_rt::NONE);
/// ```
#[inline]
#[must_use]
pub fn any() -> bool {
    active() != NONE
}
#[cfg(test)]
mod tests {
    extern crate std;

    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::Cell;
    use std::alloc::System;
    use std::prelude::v1::*;
    use std::{format, println, thread_local, vec};

    use super::*;

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
        assert_eq!(parse(b"4294967292"), u32::MAX - 3);
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

    #[test]
    fn the_environment_decides_what_is_read() {
        // Setting a variable in this process would be a data race against every other test, so the
        // check runs in a child launched with the variable already set — which is exactly how the
        // tool passes an ordinal to a real test binary.
        let executable = std::env::current_exe().expect("the test binary knows its own path");

        // Thirty-eight bytes, longer than `READ_LIMIT`, whose first thirty-two trim to `1234567`.
        // A truncated read must not be mistaken for that ordinal.
        let truncated = format!("{}1234567890123", " ".repeat(25));

        for (value, expected) in [
            ("31", "read=31"),
            ("not a number", "read=0"),
            ("", "read=0"),
            (truncated.as_str(), "read=0"),
        ] {
            let output = std::process::Command::new(&executable)
                .args(["--exact", "runtime::tests::the_child_reports_what_it_read", "--nocapture"])
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
    #[cfg(any(unix, windows))]
    fn the_reserved_ordinal_is_inactive_in_baseline_census_and_mutant_processes() {
        let executable = std::env::current_exe().expect("the test binary knows its own path");

        let baseline = std::process::Command::new(&executable)
            .args(["--exact", "runtime::tests::the_child_reports_guard_selection", "--nocapture"])
            .env_remove(ACTIVE_VAR)
            .env_remove(CENSUS_VAR)
            .output()
            .expect("the baseline child runs");
        assert!(baseline.status.success(), "{}", String::from_utf8_lossy(&baseline.stderr));
        let baseline = String::from_utf8_lossy(&baseline.stdout);

        assert!(baseline.contains("active=0 none=false seven=false"), "{baseline}");

        let mutant = std::process::Command::new(&executable)
            .args(["--exact", "runtime::tests::the_child_reports_guard_selection", "--nocapture"])
            .env(ACTIVE_VAR, "7")
            .env_remove(CENSUS_VAR)
            .output()
            .expect("the mutant child runs");
        assert!(mutant.status.success(), "{}", String::from_utf8_lossy(&mutant.stderr));
        let mutant = String::from_utf8_lossy(&mutant.stdout);

        assert!(mutant.contains("active=7 none=false seven=true"), "{mutant}");

        let sequence = CENSUS_NONE_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()
            .expect("the test process has a working directory")
            .join(format!(".gamma-census-none-{}-{sequence}.bin", std::process::id()));
        let _removed = std::fs::remove_file(&path);

        let census = std::process::Command::new(&executable)
            .args(["--exact", "runtime::tests::the_child_reports_guard_selection", "--nocapture"])
            .env(ACTIVE_VAR, "7")
            .env(CENSUS_VAR, &path)
            .output()
            .expect("the census child runs");
        assert!(census.status.success(), "{}", String::from_utf8_lossy(&census.stderr));
        let census = String::from_utf8_lossy(&census.stdout);
        let _removed = std::fs::remove_file(&path);

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

        let executable = std::env::current_exe().expect("the test binary knows its own path");
        let sequence = CENSUS_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()
            .expect("the test process has a working directory")
            .join(format!(".gamma-census-{}-{sequence}.bin", std::process::id()));

        // Opened for appending, so a leftover from an earlier run would be read as part of this
        // one. Removing it first is what makes the assertion below about the whole file sound.
        let _ = std::fs::remove_file(&path);

        let output = std::process::Command::new(&executable)
            .args(["--exact", test, "--nocapture"])
            .env(CENSUS_VAR, &path)
            .output()
            .expect("the child runs");

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));

        // Nothing flushes the census before the child exits, so this read has to happen after the
        // wait that `output` performed.
        let bytes = std::fs::read(&path).expect("the child wrote a census");
        let _ = std::fs::remove_file(&path);

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
        assert_eq!(census_of("runtime::tests::the_child_walks_a_few_sites"), vec![3, 9]);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_census_does_not_touch_its_file_until_process_exit() {
        assert_eq!(census_of("runtime::tests::the_child_buffers_its_sites"), vec![17]);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_census_serializes_more_than_one_output_batch_in_order() {
        let expected: Vec<u32> = (0..1100).collect();

        assert_eq!(census_of("runtime::tests::the_child_fills_multiple_output_batches"), expected);
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_census_that_reached_no_site_is_still_sealed() {
        // A test can legitimately reach no instrumented site — that convicts no mutant, which is an
        // answer, not a failure. The seal is written even then, so an *empty* census is a lone seal
        // and an *absent* one is unambiguously a run that failed. `census_of` pops the seal, so
        // what a zero-reach child leaves behind is nothing at all.
        assert_eq!(census_of("runtime::tests::the_child_touches_no_sites"), Vec::<u32>::new());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_site_claimed_while_sealing_waits_is_written_before_the_seal() {
        assert_eq!(
            census_of("runtime::tests::the_child_holds_a_lease_before_claiming_a_site"),
            vec![79]
        );
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_short_exit_write_leaves_the_census_unsealed() {
        let bytes = census_bytes_of("runtime::tests::the_child_forces_a_short_exit_write");

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

        std::thread::scope(|scope| {
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
        std::thread::scope(|scope| {
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

        let path = std::env::var_os(CENSUS_VAR).expect("the parent supplied a census path");

        assert!(
            !std::path::Path::new(&path).exists(),
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
        assert_eq!(census_of("runtime::tests::the_child_walks_past_the_table"), vec![1, 2, OVERFLOW]);
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
