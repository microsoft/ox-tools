// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared fakes for the tests, unit and integration alike.
//!
//! Nearly every command is a function of a [`Host`], so nearly every test needs one. Defining a
//! capturing host once means the stream-plumbing is written and exercised in exactly one place
//! instead of being copied, subtly differently, into a dozen `mod tests` blocks.
//!
//! Reached from `tests/` through the `internals` feature. A `cfg(test)` gate would not do: `tests/`
//! compiles as its own crate and sees nothing such a gate creates, which would leave the
//! integration tests maintaining a second capturing host of their own.

// This module is test scaffolding that happens to be compiled as a library module, so the lints
// written for production code do not apply to it: a fixture builder whose result is dropped is a
// test bug the test itself reveals, and `panic` is how a fixture reports one.
#![allow(
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::panic,
    reason = "test scaffolding, not production code"
)]

use core::cell::Cell;
use core::fmt;
use core::panic::AssertUnwindSafe;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock, mpsc};
use std::{fs, panic, thread};

use camino::{Utf8Path, Utf8PathBuf};

use crate::commands::Host;
use crate::exec::Workspace;

type PauseChannels = (mpsc::SyncSender<()>, mpsc::Receiver<()>);

static WORKSPACE_PREPARATION_PAUSES: OnceLock<Mutex<crate::HashMap<Utf8PathBuf, PauseChannels>>> = OnceLock::new();

/// A deterministic pause after workspace preparation receives a command's cache locks.
#[derive(Debug)]
pub struct WorkspacePreparationPause {
    reached: mpsc::Receiver<()>,
    release: Option<mpsc::SyncSender<()>>,
    waiting: bool,
}

impl WorkspacePreparationPause {
    /// Waits until workspace preparation receives the cache locks.
    pub fn wait(&mut self) {
        self.reached
            .recv_timeout(Duration::from_mins(2))
            .expect("the registered command should reach workspace preparation before the test budget expires");
        self.waiting = true;
    }

    /// Allows the paused command to continue.
    pub fn release(mut self) {
        self.release_waiter();
    }

    fn release_waiter(&mut self) {
        if self.waiting {
            let sender = self
                .release
                .take()
                .expect("a pause that reached the boundary retains exactly one release sender");
            let _released = sender.send(());
            self.waiting = false;
        }
    }
}

impl Drop for WorkspacePreparationPause {
    fn drop(&mut self) {
        self.release_waiter();
    }
}

/// Registers a workspace-preparation pause for one workspace root.
pub fn hold_during_workspace_preparation(root: Utf8PathBuf) -> WorkspacePreparationPause {
    let (reached_sender, reached) = mpsc::sync_channel(0);
    let (release, release_receiver) = mpsc::sync_channel(0);
    let pauses = WORKSPACE_PREPARATION_PAUSES.get_or_init(|| Mutex::new(crate::HashMap::default()));
    let prior = pauses
        .lock()
        .expect("a prior workspace-preparation test panicked while registering its pause")
        .insert(root, (reached_sender, release_receiver));

    assert!(prior.is_none(), "only one workspace-preparation pause may target a workspace");

    WorkspacePreparationPause {
        reached,
        release: Some(release),
        waiting: false,
    }
}

pub(crate) fn pause_during_workspace_preparation(root: &Utf8Path) {
    let Some(pauses) = WORKSPACE_PREPARATION_PAUSES.get() else {
        return;
    };
    let pause = pauses
        .lock()
        .expect("a prior workspace-preparation test panicked while taking its pause")
        .remove(root);

    if let Some((reached, release)) = pause
        && reached.send(()).is_ok()
    {
        let _released = release.recv();
    }
}

/// A [`Host`] that captures both streams in memory.
///
/// The defaults describe a plain redirected pipe: not a terminal, no width, and an empty
/// environment. [`Sink::terminal`] and [`Sink::with_env`] override those for the tests that care.
#[derive(Debug, Default)]
pub struct Sink {
    /// Everything written to the result stream.
    pub out: Vec<u8>,

    /// Everything written to the diagnostic stream.
    pub err: Vec<u8>,

    terminal: bool,
    width: Option<u16>,
    env: Vec<(String, String)>,
}

impl Sink {
    /// Presents the host as a terminal of the given width.
    #[must_use]
    pub fn terminal(mut self, width: u16) -> Self {
        self.terminal = true;
        self.width = Some(width);
        self
    }

    /// Adds a variable to the fake environment.
    #[must_use]
    pub fn with_env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The captured result stream, as text.
    #[must_use]
    pub fn out(&self) -> String {
        String::from_utf8(self.out.clone()).expect("output should be utf-8")
    }

    /// The captured diagnostic stream, as text.
    #[must_use]
    pub fn err(&self) -> String {
        String::from_utf8(self.err.clone()).expect("diagnostics should be utf-8")
    }
}

impl Host for Sink {
    fn output(&mut self) -> impl Write {
        &mut self.out
    }

    fn error(&mut self) -> impl Write {
        &mut self.err
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn terminal_width(&self) -> Option<u16> {
        self.width
    }

    fn env(&self, name: &str) -> Option<String> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone())
    }
}

/// A writer that refuses every write.
///
/// This stands in for the closed pipe you get when the user pipes the tool into `head`, which is
/// the only way the `?` on a `writeln!` to the console ever fires in practice.
#[derive(Debug)]
pub struct Broken;

impl Write for Broken {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

/// A [`Host`] whose streams are both closed pipes.
#[derive(Debug)]
pub struct BrokenHost;
impl Host for BrokenHost {
    fn output(&mut self) -> impl Write {
        Broken
    }

    fn error(&mut self) -> impl Write {
        Broken
    }

    fn is_terminal(&self) -> bool {
        false
    }

    fn terminal_width(&self) -> Option<u16> {
        None
    }

    fn env(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Wraps a scratch directory as a workspace whose test binaries are `/bin/sh` running `body`.
///
/// The real spawn, drain, wait and kill machinery is exercised; only the compiled test harness is
/// stood in for. The script is passed as an argument rather than written to disk, because a file
/// made executable while other threads are forking can be refused with `ETXTBSY`, which would make
/// such tests fail intermittently for a reason unrelated to what they assert.
///
/// That argument is a *positional* one, and a real test binary reads its positional arguments as
/// test-name filters — so the tool drops them whenever it narrows a run to chosen tests, and the
/// shell would then be started with no script at all. A test that narrows the selection therefore
/// belongs on [`helper_workspace`], whose directives are `--`-prefixed and survive.
#[cfg(unix)]
pub fn shell_workspace(prefix: &str, body: &str) -> (tempfile::TempDir, Workspace) {
    let directory = workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
    let target = root.join("target");
    let mut work = Workspace::adopt(root, target);

    work.set_test_args(vec!["-c".to_owned(), body.to_owned()]);

    (directory, work)
}

/// The source of the stand-in test binary [`helper_workspace`] runs.
///
/// A compiled helper rather than a shell script because the verdict machinery has to be exercised
/// on every platform the tool builds for, and a fixture that only exists on Unix leaves the whole
/// of it unproven on Windows — which is how a build that does not compile there gets to land.
///
/// The helper reads its behaviour from its command line, one directive per argument, in order:
///
/// | Directive | Effect |
/// |---|---|
/// | `print:TEXT` | Writes `TEXT` and a newline to stdout, flushed |
/// | `sleep:MS` | Sleeps for `MS` milliseconds |
/// | `exit:CODE` | Stops there and exits with `CODE` |
/// | `require-file:PATH` | Exits 1 unless `PATH` names a file, relative to the working directory |
/// | `wait-file:PATH\|MS` | Waits up to `MS` milliseconds for `PATH` to name a file, then exits 1 if it does not |
/// | `write-env:FILE\|VAR\|VAR` | Writes the named variables' values, space separated, to `FILE` |
/// | `write-le:VAR\|N\|N` | Writes each `N` as a little-endian `u32` to the file the variable `VAR` names |
/// | `flood:BYTES` | Writes at least `BYTES` of filler to stdout |
/// | `when-env:VAR\|DIRECTIVE` | Runs `DIRECTIVE` only when `VAR` is set |
/// | `when-arg:ARG\|DIRECTIVE` | Runs `DIRECTIVE` only when `ARG` appears in command line arguments |
/// | `touch:PATH` | Creates an empty file at `PATH` |
/// | `spawn:DIRECTIVE\|DIRECTIVE` | Starts another copy of the helper on those directives and does not wait for it |
/// | `eat:MIB` | Holds `MIB` mebibytes of touched memory until the process ends |
///
/// Directives are written as `--gamma-step=DIRECTIVE`, and every other argument is ignored — which
/// is what lets a test pass harness arguments such as `--nocapture` through the same list, and what
/// lets the helper survive being handed the test-name filters a narrowed run selects. A real
/// libtest harness reads its bare arguments as those filters, so a fixture carrying its script in
/// one would be claiming that the semantics the tool has to compose with do not exist. A
/// `--gamma-step=` argument naming no known directive exits 97, so a typo in a fixture is loud
/// rather than silently passing.
const HELPER_SOURCE: &str = r#"
use std::io::Write as _;

thread_local! {
    /// Whatever `eat` faulted in, kept alive so the pages stay resident until the process ends.
    static HELD: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn main() {
    let mut code = 0;

    for argument in std::env::args().skip(1) {
        let Some(directive) = argument.strip_prefix("--gamma-step=") else {
            continue;
        };

        if let Some(status) = step(directive) {
            code = status;
            break;
        }
    }

    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn step(directive: &str) -> Option<i32> {
    let (name, payload) = directive.split_once(':').unwrap_or((directive, ""));

    match name {
        "print" => {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{payload}");
            let _ = out.flush();
            None
        }
        "sleep" => {
            let ms = payload.parse().unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            None
        }
        "exit" => Some(payload.parse().unwrap_or(0)),
        "require-file" => {
            if std::path::Path::new(payload).is_file() {
                None
            } else {
                Some(1)
            }
        }
        "wait-file" => {
            let (path, timeout) = payload.split_once('|').unwrap_or((payload, "0"));
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(timeout.parse().unwrap_or(0));

            while !std::path::Path::new(path).is_file() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            if std::path::Path::new(path).is_file() {
                None
            } else {
                Some(1)
            }
        }
        "write-env" => {
            let mut parts = payload.split('|');
            let file = parts.next().unwrap_or_default();
            let values: Vec<String> = parts.map(|name| std::env::var(name).unwrap_or_default()).collect();
            let _ = std::fs::write(file, values.join(" "));
            None
        }
        "write-le" => {
            let mut parts = payload.split('|');
            let file = std::env::var(parts.next().unwrap_or_default()).unwrap_or_default();
            let bytes: Vec<u8> = parts
                .flat_map(|word| word.parse::<u32>().unwrap_or(0).to_le_bytes())
                .collect();
            let _ = std::fs::write(file, bytes);
            None
        }
        "flood" => {
            let wanted: usize = payload.parse().unwrap_or(0);
            let line = [b'y'; 64];
            let mut out = std::io::stdout();
            let mut written = 0;

            while written < wanted {
                let _ = out.write_all(&line[..63]);
                let _ = out.write_all(b"\n");
                written += line.len();
            }

            let _ = out.flush();
            None
        }
        "touch" => {
            let _ = std::fs::write(payload, b"");
            None
        }
        // The grandchild the containment tests need: it outlives its parent deliberately, which is
        // the shape that leaves an orphan holding the scratch tree when a kill reaches only the
        // process it was aimed at.
        "spawn" => {
            let mut child = std::process::Command::new(std::env::current_exe().expect("the helper knows its own path"));

            for inner in payload.split('|') {
                let _ = child.arg(format!("--gamma-step={inner}"));
            }

            let spawned = child.spawn().expect("the helper can start another copy of itself");
            let mut out = std::io::stdout();
            let _ = writeln!(out, "spawned {}", spawned.id());
            let _ = out.flush();
            None
        }
        // Touched a page at a time, because a ceiling is enforced against pages a process has
        // really faulted in rather than against an allocator's bookkeeping.
        "eat" => {
            let mib: usize = payload.parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();

            for _ in 0..mib {
                let mut block = vec![0_u8; 1024 * 1024];

                for at in (0..block.len()).step_by(4096) {
                    block[at] = 1;
                }

                held.push(block);
            }

            HELD.with(|slot| slot.borrow_mut().extend(held));
            None
        }
        "when-env" => match payload.split_once('|') {
            Some((name, inner)) if std::env::var_os(name).is_some() => step(inner),
            Some(_) => None,
            None => Some(97),
        },
        "when-arg" => match payload.split_once('|') {
            Some((target, inner)) if std::env::args().any(|arg| arg == target) => step(inner),
            Some(_) => None,
            None => Some(97),
        },
        _ => Some(97),
    }
}
"#;

/// Bumped whenever [`HELPER_SOURCE`] changes, so a binary left behind by an older tree is replaced
/// rather than reused.
const HELPER_VERSION: u32 = 7;

/// The compiled stand-in test binary, built on first use and shared by every test in the process.
///
/// Built rather than declared as a second `[[bin]]` target so that the fixture is entirely a
/// property of the test scaffolding: a binary target would be produced for every `cargo build` any
/// consumer of this crate ever ran, and would have to be excluded from the tool's own sweeps by
/// hand.
pub fn helper_binary_path() -> &'static camino::Utf8Path {
    static BUILT: OnceLock<Utf8PathBuf> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let work = Utf8PathBuf::from_path_buf(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work"))
                .expect("the target path is UTF-8");

            fs::create_dir_all(work.as_std_path()).expect("the test work directory should be creatable");

            let suffix = if cfg!(windows) { ".exe" } else { "" };
            let helper = work.join(format!("gamma-helper-{HELPER_VERSION}{suffix}"));

            if helper.exists() {
                return helper;
            }

            // Compiled to a name nothing else can be holding and then moved into place, because
            // another test process may be doing the same thing at the same moment and a half-written
            // executable is refused with `ETXTBSY` on Unix and a sharing violation on Windows.
            let staging = tempfile::Builder::new()
                .prefix("gamma-helper-build")
                .tempdir_in(work.as_std_path())
                .expect("the staging directory should be creatable");
            let source = staging.path().join("helper.rs");
            let staged = staging.path().join(format!("helper{suffix}"));

            fs::write(&source, HELPER_SOURCE).expect("the helper source should be writable");

            let built = Command::new("rustc")
                .arg("--edition")
                .arg("2024")
                .arg("-C")
                .arg("debuginfo=0")
                .arg("-o")
                .arg(&staged)
                .arg(&source)
                .output()
                .expect("rustc should be on the path of anything running these tests");

            assert!(
                built.status.success(),
                "the test helper should compile: {}",
                String::from_utf8_lossy(&built.stderr)
            );

            // A rename that loses the race is not an error: whoever won wrote the same bytes from the
            // same source, and the loser's copy goes with the staging directory.
            let _moved = fs::rename(&staged, helper.as_std_path());

            assert!(helper.exists(), "the test helper should be in place at {helper}");

            helper
        })
        .as_path()
}

/// Wraps a scratch directory as a workspace whose test binary is the portable helper.
///
/// The real spawn, drain, wait and kill machinery is exercised on every platform; only the
/// compiled test harness is stood in for. See `HELPER_SOURCE` for the directives `script` is
/// written in.
pub fn helper_workspace(prefix: &str, script: &[&str]) -> (tempfile::TempDir, Workspace) {
    let directory = workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
    let target = root.join("target");
    let mut work = Workspace::adopt(root, target);

    // A step that is already a harness argument is passed through as written — a fixture uses those
    // to exercise the tool's reading of the user's own arguments, and wrapping one would hide it.
    work.set_test_args(
        script
            .iter()
            .map(|step| {
                if step.starts_with("--") {
                    (*step).to_owned()
                } else {
                    format!("--gamma-step={step}")
                }
            })
            .collect(),
    );

    (directory, work)
}

/// One `HELPER_SOURCE` directive, spelled the way the helper reads it from its command line.
pub fn directive(step: impl fmt::Display) -> String {
    format!("--gamma-step={step}")
}

/// A [`TestBinary`](crate::exec::TestBinary) naming the portable helper.
pub fn helper() -> crate::exec::TestBinary {
    test_binary(helper_binary_path().as_str())
}

/// A [`TestBinary`](crate::exec::TestBinary) at `path`, with everything else left at its default.
///
/// Most tests care about one field — where the executable is, or which package it belongs to — and
/// spelling out the rest at each site makes adding a field a sweep through the whole suite.
pub fn test_binary(path: &str) -> crate::exec::TestBinary {
    crate::exec::TestBinary {
        path: Utf8PathBuf::from(path),
        package: String::new(),
        package_id: String::new(),
        target: String::new(),
        manifest_dir: Utf8PathBuf::new(),
        baseline: Duration::ZERO,
        budget: None,
        tests: None,
        peak: None,
        memory: None,
    }
}

/// Records every event a run publishes, so a test can assert on what it announced.
///
/// The console implementations format and discard; this keeps the structure, which is what an
/// assertion about "did this phase run" actually needs.
#[derive(Debug, Default)]
pub struct Recorder {
    /// Every `(verb, detail)` pair, in the order it was published.
    pub phases: Vec<(String, String)>,

    /// How many mutants were announced.
    pub mutants: usize,

    /// Every warning the run raised, in order.
    pub warnings: Vec<String>,
}

impl crate::exec::Events for Recorder {
    fn phase(&mut self, verb: &str, detail: &str) {
        self.phases.push((verb.to_owned(), detail.to_owned()));
    }

    fn warn(&mut self, message: &str) {
        self.warnings.push(message.to_owned());
    }

    fn mutant(&mut self, _mutant: &crate::model::Mutant) {
        self.mutants = self.mutants.saturating_add(1);
    }
}

/// Creates a temporary directory under the workspace target directory.
///
/// Tests that shell out to cargo need their scratch space on the same file system as the target
/// directory, and keeping it there also means `cargo clean` sweeps up anything a panicking test
/// leaked.
#[must_use]
pub fn workdir(prefix: &str) -> tempfile::TempDir {
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-work");

    fs::create_dir_all(&work).expect("the test work directory should be creatable");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(work)
        .expect("the temporary directory should be creatable")
}

/// Announces that a test is standing down, and says why.
///
/// A test that returns early because the host cannot support it is indistinguishable, in the run
/// output, from one that ran and passed — and the tests that do this are the ones asserting that a
/// runaway mutant is stopped, which is not a contract anybody should be able to believe is checked
/// when it is not. Whoever reads a green run is owed the list of what was not asked.
///
/// Written straight to the standard error stream rather than through `eprintln!`, because libtest
/// captures the macros and replays them only for tests that *fail* — so a passing test has no other
/// way to put a line in the output. Under `--nocapture` both would work; the point is the ordinary
/// run.
///
/// Returns `true`, so a caller can write `if unsupported(…) { return; }` and read it as a sentence.
pub fn standing_down(what: &str, why: &str) -> bool {
    let count = STOOD_DOWN.fetch_add(1, Ordering::Relaxed) + 1;
    let line = format!("standing down ({count}): {what} — {why}\n");
    let _written = io::stderr().write_all(line.as_bytes());

    true
}

/// How many tests have stood down in this process, so the lines can be counted and told apart.
static STOOD_DOWN: AtomicUsize = AtomicUsize::new(0);

/// Reports whether the host can bound a subtree's memory, standing down loudly when it cannot.
///
/// The one place the check is written, so that the reason a test skipped is the reason the tool
/// itself would give a user.
#[must_use]
pub fn without_memory_support(what: &str) -> bool {
    match crate::exec::memory_support() {
        Ok(()) => false,
        Err(reason) => standing_down(what, &reason),
    }
}

/// How long [`within`] waits before calling a closure hung.
///
/// Deliberately far longer than any correct closure needs. The failure this guards against is a
/// deadlock, which does not finish in *any* budget, so the only thing a tight budget could buy is
/// a false failure on a machine that was merely busy — and a watchdog that cries wolf is one
/// somebody eventually deletes. A minute is nothing against a suite that would otherwise hang
/// until a human noticed.
pub const WATCHDOG: Duration = Duration::from_mins(1);

/// Runs `body` on its own thread and fails the test if it has not finished within `budget`.
///
/// Nothing else in the workspace can turn a hang into a failure: a deadlocked test does not go red,
/// it stops the run and holds the machine. `#[test]` has no timeout, `join` has no deadline, and
/// the two places that already needed one each hand-rolled a thread, a channel and a diagnostic —
/// which is three chances to get the budget wrong and one to get the join wrong, the latter wedging
/// the suite the test was written to protect.
///
/// The hung thread is deliberately **not** joined and not killed. There is no way to kill a thread
/// in Rust, and joining one that is stuck is exactly the wedge being escaped; the process is about
/// to fail and exit, and the operating system reclaims the thread. Leaving it running is the whole
/// point.
///
/// `body` must own everything it touches, because it outlives this call whenever it hangs — which
/// is the case that matters and the reason the bound is `'static` rather than scoped.
///
/// ```
/// use cargo_gamma_lib::testing::{WATCHDOG, within};
///
/// let doubled = within(WATCHDOG, "doubling", || 21 * 2);
///
/// assert_eq!(doubled, 42);
/// ```
///
/// # Panics
///
/// If `body` does not finish within `budget`, or panics. A panic inside `body` is re-raised here so
/// that it fails the test rather than being swallowed by the worker thread.
pub fn within<T: Send + 'static>(budget: Duration, what: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();

    // Detached on purpose: see above. The handle is dropped rather than joined.
    let _worker = thread::spawn(move || {
        // Caught rather than allowed to propagate, so that a panicking `body` arrives here as a
        // panic on the test's own thread instead of as a disconnected channel, which would be
        // reported as a hang and send the reader looking for a deadlock that is not there.
        let outcome = panic::catch_unwind(AssertUnwindSafe(body));

        let _delivered = sender.send(outcome);
    });

    match receiver.recv_timeout(budget) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => panic::resume_unwind(panic),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} did not finish within {budget:?}; it is hung")
        }

        // The worker sends before it can be dropped, so a disconnect means it vanished without
        // sending — which `catch_unwind` makes impossible for anything but an abort.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what} ended without an answer")
        }
    }
}

/// A writer that accepts a fixed number of lines and then behaves like a closed pipe.
///
/// Lines rather than writes, because `writeln!` turns one call into several `write` calls and a
/// test that counted those would be asserting on the internals of `format_args!`.
#[derive(Debug)]
pub struct Flaky<'a> {
    remaining: &'a Cell<usize>,
}

impl Write for Flaky<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.remaining.get() == 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }

        #[expect(clippy::naive_bytecount, reason = "a test double is not worth a dependency")]
        let lines = buf.iter().filter(|byte| **byte == b'\n').count();

        self.remaining.set(self.remaining.get().saturating_sub(lines));

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`Host`] whose streams close after a given number of lines.
///
/// Console output is written a line at a time, and a host that fails on the very first one only
/// ever proves the first `?` works. Moving the failure along one line at a time is what reaches
/// the rest of them.
#[derive(Debug)]
pub struct FlakyHost {
    remaining: Cell<usize>,
}

impl FlakyHost {
    /// Creates a host that accepts `lines` lines on each stream before failing.
    #[must_use]
    pub const fn new(lines: usize) -> Self {
        Self {
            remaining: Cell::new(lines),
        }
    }
}

impl Host for FlakyHost {
    fn output(&mut self) -> impl Write {
        Flaky {
            remaining: &self.remaining,
        }
    }

    fn error(&mut self) -> impl Write {
        Flaky {
            remaining: &self.remaining,
        }
    }

    fn is_terminal(&self) -> bool {
        false
    }

    fn terminal_width(&self) -> Option<u16> {
        None
    }

    fn env(&self, _name: &str) -> Option<String> {
        None
    }
}

/// Asserts that `run` reports a closed pipe however many lines it managed to write first.
///
/// Every console line is a place the pipe can go away, and a `?` that is never taken is a `?` that
/// has never been shown to propagate rather than swallow.
pub fn fails_at_every_line<E: fmt::Display>(lines: usize, run: impl Fn(&mut FlakyHost) -> Result<(), E>) {
    for limit in 0..lines {
        let mut host = FlakyHost::new(limit);

        let error = run(&mut host).err().unwrap_or_else(|| panic!("line {limit} should have failed"));

        assert!(error.to_string().contains("broken pipe"), "line {limit}: {error}");
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {

    /// The watchdog returns what the closure returned, and does it on the closure's own thread.
    #[test]
    fn a_closure_that_finishes_gets_its_value_back() {
        let id = within(WATCHDOG, "reporting a thread id", || thread::current().id());

        assert_eq!(within(WATCHDOG, "adding", || 21 + 21), 42);
        assert_ne!(id, thread::current().id(), "the body must not run on the caller's thread");
    }

    /// A body that hangs fails the test rather than wedging the run.
    ///
    /// This is the whole point of the helper, so it is asserted directly: a body that never
    /// finishes has to come back as a panic on the caller's thread, and the panic has to name what
    /// hung, because a suite that stops without saying which test stopped it is a suite somebody
    /// debugs by bisection. A short budget is used here — the only place one is appropriate, since
    /// this body is *meant* to be late — and the hung thread is left running, which is exactly what
    /// the helper promises to do.
    #[test]
    fn a_body_that_never_finishes_is_reported_as_hung() {
        let (release, released) = mpsc::channel::<()>();
        let hung = panic::catch_unwind(|| {
            within(Duration::from_millis(50), "a deliberately hung body", move || {
                // Blocks until the sender is dropped, which happens when this test returns. The
                // wait is a real block rather than a sleep so that the body is genuinely still
                // running when the watchdog gives up on it, which a sleep could not guarantee.
                let _woken = released.recv();
            });
        })
        .expect_err("a hung body must fail the test");

        let message = hung
            .downcast_ref::<String>()
            .map_or_else(|| "not a string".to_owned(), Clone::clone);

        assert!(message.contains("a deliberately hung body"), "{message}");
        assert!(message.contains("is hung"), "{message}");

        drop(release);
    }

    /// A panic inside the body reaches the caller as that same panic.
    ///
    /// Otherwise a watchdog would convert every assertion failure inside it into "it is hung",
    /// which is the wrong diagnosis and sends the reader hunting for a deadlock that is not there.
    #[test]
    fn a_body_that_panics_fails_with_its_own_message_rather_than_as_a_hang() {
        let raised = panic::catch_unwind(|| {
            within(WATCHDOG, "a body that fails", || panic!("the assertion the body made"));
        })
        .expect_err("a panicking body must fail the test");

        let message = raised
            .downcast_ref::<&str>()
            .map_or_else(|| "not a string".to_owned(), |text| (*text).to_owned());

        assert_eq!(message, "the assertion the body made");
    }

    use super::*;

    /// A default sink looks like a redirected pipe and captures both streams.
    #[test]
    fn a_default_sink_captures_both_streams_and_reports_no_terminal() {
        let mut sink = Sink::default();

        write!(sink.output(), "result").expect("write");
        write!(sink.error(), "progress").expect("write");

        assert_eq!(sink.out(), "result");
        assert_eq!(sink.err(), "progress");
        assert!(!sink.is_terminal());
        assert_eq!(sink.terminal_width(), None);
        assert_eq!(sink.env("GAMMA_NOT_SET"), None);
    }

    /// The builders override the terminal and environment answers.
    #[test]
    fn the_builders_override_the_terminal_and_the_environment() {
        let sink = Sink::default().terminal(80).with_env("CI", "true");

        assert!(sink.is_terminal());
        assert_eq!(sink.terminal_width(), Some(80));
        assert_eq!(sink.env("CI").as_deref(), Some("true"));
        assert_eq!(sink.env("OTHER"), None);
    }

    /// Every stream of a broken host fails, on both write and flush.
    #[test]
    fn a_broken_host_fails_every_write_and_every_flush() {
        let mut host = BrokenHost;

        assert_eq!(host.output().write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.output().flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.error().write(b"x").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(host.error().flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(host.env("PATH"), None);
    }

    /// The flaky host lets exactly the budgeted number of lines through.
    #[test]
    fn a_flaky_host_fails_once_its_line_budget_is_spent() {
        let mut host = FlakyHost::new(2);

        writeln!(host.error(), "first").expect("first line");
        writeln!(host.error(), "second").expect("second line");

        assert_eq!(writeln!(host.output(), "third").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        host.error().flush().expect("flushing a live stream should succeed");
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(host.env("PATH"), None);
    }

    /// The sweep helper walks the failure across every line it is told about.
    #[test]
    fn the_sweep_helper_moves_the_failure_along_one_line_at_a_time() {
        fails_at_every_line(3, |host| {
            let mut stream = host.error();

            writeln!(stream, "one")?;
            writeln!(stream, "two")?;
            writeln!(stream, "three")
        });
    }

    /// The work directory lands under the workspace target directory so `cargo clean` sweeps it.
    #[test]
    fn a_work_directory_is_created_under_the_target_directory() {
        let dir = workdir("testing-workdir-");

        assert!(dir.path().is_dir());
        assert!(dir.path().to_string_lossy().contains("test-work"));
    }

    /// The portable helper compiles, runs, and does what its directives say — on every platform.
    ///
    /// Everything the verdict tests assert rests on this fixture behaving the way they describe,
    /// so it is proven directly rather than only through them: a helper that silently ignored a
    /// directive would turn tests about verdicts into tests that pass for the wrong reason.
    #[test]
    fn the_portable_helper_obeys_its_directives() {
        let script = [
            "--nocapture",
            "print:test a::b ... ok",
            "write-env:seen.txt|GAMMA_HELPER_UNSET",
            "when-env:PATH|print:seen the path",
            "when-env:GAMMA_HELPER_UNSET|exit:97",
            "exit:3",
        ];
        let (directory, _work) = helper_workspace("helper-self-", &script);

        let output = Command::new(helper().path.as_std_path())
            .args(script.iter().map(|step| {
                if step.starts_with("--") {
                    (*step).to_owned()
                } else {
                    directive(step)
                }
            }))
            .current_dir(directory.path())
            .output()
            .expect("the helper runs");

        assert_eq!(output.status.code(), Some(3));

        let text = String::from_utf8_lossy(&output.stdout);

        assert!(text.contains("test a::b ... ok"), "{text}");
        assert!(text.contains("seen the path"), "{text}");
        assert_eq!(
            fs::read_to_string(directory.path().join("seen.txt")).expect("the helper wrote the file"),
            ""
        );
    }

    /// A directive nobody implemented is loud, so a typo in a fixture cannot quietly pass.
    #[test]
    fn the_portable_helper_refuses_a_directive_it_does_not_know() {
        let status = Command::new(helper().path.as_std_path())
            .arg(directive("dance:jig"))
            .status()
            .expect("the helper runs");

        assert_eq!(status.code(), Some(97));
    }
}

/// Fixtures for the CI renderings, which all need a mutant with a known file, line and outcome.
pub mod ci_fixture {
    use camino::Utf8PathBuf;

    use crate::model::{Mutant, Outcome};
    use crate::ops::collect::Shape;

    pub fn mutant(file: &str, line: usize, mutator: &str, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("{file}:{line}:{mutator}").into(),
            ordinal: 0,
            file: (Utf8PathBuf::from(file)).into(),
            package: ("subject".to_owned()).into(),
            span: 0..1,
            line,
            end_line: line,
            column: 5,
            mutator: (mutator.to_owned()).into(),
            item_path: ("subject::f".to_owned()).into(),
            occurrence: 0,
            replacement_index: 0,
            original: "a > b".into(),
            replacement: "a >= b".into(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    pub fn root() -> Utf8PathBuf {
        Utf8PathBuf::from("/w")
    }
}

/// Fixtures for the diagnosis tests.
pub mod advise_fixture {
    use core::time::Duration;

    use camino::Utf8PathBuf;

    use crate::advise::{Finding, Timing};
    use crate::exec::TestBinary;
    use crate::model::{Mutant, Outcome};
    use crate::ops::collect::Shape;

    pub fn mutant(file: &str, mutator: &str, outcome: Outcome, ms: u64) -> Mutant {
        Mutant {
            id: format!("{file}{mutator}{ms}").into(),
            ordinal: 1,
            package: ("p".to_owned()).into(),
            mutator: (mutator.to_owned()).into(),
            file: (Utf8PathBuf::from(file)).into(),
            line: 1,
            end_line: 1,
            column: 1,
            span: 0..1,
            item_path: ("f".to_owned()).into(),
            occurrence: 0,
            replacement_index: 0,
            original: "a".into(),
            replacement: "b".into(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: ms,
            killed_by: None,
            note: None,
        }
    }

    pub fn timing(build: u64, baseline: u64, wall: u64) -> Timing {
        Timing {
            build: Duration::from_secs(build),
            baseline: Duration::from_secs(baseline),
            wall: Duration::from_secs(wall),
            jobs: 4,
        }
    }

    pub fn binary(package: &str, target: &str, baseline: u64, tests: Option<usize>) -> TestBinary {
        TestBinary {
            path: Utf8PathBuf::from(format!("/target/{target}")),
            package: package.to_owned(),
            package_id: format!("path+file:///w/{package}#0.0.0"),
            target: target.to_owned(),
            manifest_dir: Utf8PathBuf::from(format!("/w/{package}")),
            baseline: Duration::from_secs(baseline),
            tests,
            budget: None,
            peak: None,
            memory: None,
        }
    }

    pub fn find<'a>(findings: &'a [Finding], code: &str) -> Option<&'a Finding> {
        findings.iter().find(|finding| finding.code == code)
    }
}

/// Fixtures for the workspace-survey tests.
pub mod discover_fixture {
    use camino::Utf8PathBuf;

    use crate::model::{Mutant, Outcome};
    use crate::ops::collect;

    /// Builds a bare-minimum mutant for exercising `report_by_package`, which only looks at the
    /// package name.
    pub fn counting_mutant(package: &str) -> Mutant {
        Mutant {
            id: "aaaaaaaaaaaa".into(),
            ordinal: 1,
            file: (Utf8PathBuf::from("src/lib.rs")).into(),
            package: (package.to_owned()).into(),
            span: 0..1,
            line: 1,
            end_line: 1,
            column: 1,
            mutator: ("relational.gt_to_ge".to_owned()).into(),
            item_path: ("f".to_owned()).into(),
            occurrence: 0,
            replacement_index: 0,
            original: "a > b".into(),
            replacement: "a >= b".into(),
            shape: collect::Shape::Expr,
            outcome: Outcome::Survived,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }
}

/// Reduces arbitrary text to something that can sit inside one line of harness output.
///
/// The property tests over the output parsers splice a well-formed announcement into random
/// surrounding noise and assert it is still found. A generated name carrying a newline would split
/// the line it was spliced into, making the test a statement about a different input than the one
/// it claims to be about, so the alphabet is narrowed to what a test path can actually contain.
#[must_use]
pub fn token(text: &str) -> String {
    let kept: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect();

    if kept.is_empty() { "t".to_owned() } else { kept }
}

/// Splices `line` into `noise` at `at`, and returns the whole as one stream of output.
///
/// `at` is taken modulo the number of positions available, so a generator that yields any `usize`
/// still lands the line somewhere valid — including before everything and after everything, which
/// are the two positions most likely to catch an off-by-one in a reader.
#[must_use]
pub fn spliced(noise: &[String], line: &str, at: usize) -> String {
    let mut lines: Vec<&str> = noise.iter().map(String::as_str).collect();
    let at = at % lines.len().saturating_add(1);

    lines.insert(at, line);
    lines.join("\n")
}
