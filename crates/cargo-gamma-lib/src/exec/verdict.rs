// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Instant;
use std::{env, fmt, fs, thread};

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_process::{MemoryRequest, MemoryUsage, PlatformError, PreparedCommand, ProcessTree, SpawnedCommand, prepare};
use serde::Serialize;
use tick::{SimpleClock, Stopwatch};

mod hubs;

#[cfg(test)]
use cargo_gamma_process::faults::{self as process_faults, Fault as ProcessFault};
use hubs::Pulse;
#[doc(inline)]
pub use hubs::READERS;
#[cfg(test)]
#[cfg(not(loom))]
use hubs::Readers;
#[cfg(loom)]
pub(crate) use hubs::run_loom_models;

#[cfg(test)]
use super::faults::{self, Fault};
use super::harness_filters::HarnessFilters;
use super::loader::{Launch, STACK_VAR, UNDER_GAMMA_VAR, configure_loader};
use super::nextest;
use super::progress::{Progress, Watch, runtime_startup_failure};
use super::stall::Stall;
use super::test_binary::{TEST_THREADS_VAR, TestBinary};
use super::workspace::Workspace;

/// The variable Insta reads to decide whether to write snapshots it was asked to compare against.
const INSTA_UPDATE_VAR: &str = "INSTA_UPDATE";

/// The variable Insta reads to decide whether a mismatched snapshot still passes.
const INSTA_FORCE_PASS_VAR: &str = "INSTA_FORCE_PASS";

/// Nextest markers inherited from a runner that launched cargo-gamma rather than this test.
static INHERITED_NEXTEST_VARIABLES: OnceLock<Vec<OsString>> = OnceLock::new();

/// Which of a binary's tests a run is to execute.
///
/// A named test is a probe rather than a substitute for the binary: it is only ever used to check a
/// guess about which test catches a mutant, and a probe that does not convict is followed by the
/// ordinary whole-binary run. Nothing that reads this may draw a conclusion about the *absence* of
/// a failure, because a filtered run has not looked at the rest of the suite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Only<'name> {
    /// Everything the binary holds, which is what a verdict about a mutant is normally reached from.
    #[default]
    All,

    /// One test, named exactly as the harness names it.
    One(&'name str),
}

impl<'name> Only<'name> {
    /// The tests to run, empty when the whole binary runs.
    pub(super) fn names(&self) -> &[&'name str] {
        match self {
            Self::All => &[],
            Self::One(name) => core::slice::from_ref(name),
        }
    }
}

/// Everything about how one run of a test binary is to be performed, other than which binary.
///
/// Carried together because they are decided as a set and threaded unchanged through the launch,
/// the confirmation and the retries: which mutant is active decides what the run is evidence about,
/// the budget and the silence budget decide when it is cut short, the accounting decides what it is
/// held to, and the filter decides how much of the suite it looks at. Splitting them back out only
/// makes every function in the path take five more arguments and every retry a place to pass one of
/// them differently by accident.
#[derive(Debug, Clone, Copy)]
pub(super) struct Attempt<'name> {
    /// The mutant to switch on, or `None` for a run that is evidence about the suite itself.
    pub(super) active: Option<u32>,

    /// How long the binary may run before it is cut off, or `None` for no cutoff at all.
    ///
    /// `None` is what a run with no baseline gets: see [`TestBinary::budget`].
    ///
    /// [`TestBinary::budget`]: super::TestBinary::budget
    pub(super) timeout: Option<Duration>,

    /// How long it may go without saying anything before it is treated as stuck.
    pub(super) stall: Stall,

    /// What the run's memory is to be accounted for against.
    pub(super) request: MemoryRequest,

    /// How much of the binary's suite to run.
    pub(super) only: Only<'name>,
}

impl Attempt<'_> {
    /// The same attempt with no mutant active, to check a nextest enumeration failure.
    const fn exonerating(self) -> Self {
        Self { active: None, ..self }
    }

    /// The same attempt with several times the budget, which is how a suspected timeout is checked.
    #[cfg_attr(coverage_nightly, coverage(off))]
    const fn lengthened(self) -> Self {
        Self {
            timeout: match self.timeout {
                Some(timeout) => Some(timeout.saturating_mul(CONFIRM_FACTOR)),
                None => None,
            },
            ..self
        }
    }

    /// The same attempt with several times the silence budget, for a suspected stall.
    fn patient(self) -> Self {
        Self {
            stall: self.stall.scaled(CONFIRM_FACTOR),
            ..self
        }
    }
}

/// What running one test binary said.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Passed,

    /// A test failed, named when the harness said which.
    Failed(Option<String>),

    /// Nextest could not enumerate the selected tests while a mutant was active.
    ///
    /// This is only a suspicion until the identical selection succeeds with no mutant active.
    /// Carries nextest's output because enumeration failures usually explain themselves there —
    /// but that output comes from a test process and its inherited environment, so anything that
    /// turns this into a durable, published record must not repeat it verbatim; see
    /// `sweep::enumeration_note`, the one place that does.
    TestEnumerationFailed(String),

    /// The budget ran out while the binary was still making progress.
    TimedOut,

    /// The binary stopped reporting progress long before its budget ran out.
    ///
    /// Carries the last test the harness said anything about, which is a landmark rather than a
    /// diagnosis: libtest runs tests in parallel and names one only when it finishes, so the test
    /// that is spinning is precisely the one it has not named.
    Stalled(Option<String>),

    /// The kernel stopped the test workload for passing the memory ceiling this run installed.
    ///
    /// A separate verdict rather than a kind of failure or a kind of timeout, because it is
    /// neither: the tests did not notice anything, and nothing ran out of time. What happened is
    /// that the mutant made the workload allocate past a ceiling the same workload stayed under
    /// with no mutant active. Carries the peak the platform observed, when it could observe one,
    /// and the ceiling that fired, since a reader's first question is how far past it went.
    MemoryLimit {
        /// The highest aggregate memory the subtree reached, when the platform reported one.
        peak: Option<u64>,

        /// The ceiling that was installed for this binary.
        limit: u64,
    },

    /// A test failed during an unmutated nextest enumeration control.
    ///
    /// The control did not establish that the active mutant caused the enumeration failure.
    /// Carries the failed test, when known, rather than crediting the mutant with a kill.
    Flaky(Option<String>),

    /// An unmutated nextest control did not finish cleanly enough to attribute an enumeration failure.
    Unconfirmed(Option<String>, &'static str),

    /// The run-wide machinery could not continue producing trustworthy verdicts.
    ///
    /// Three shapes reach this: the accounting the run asked for could not be installed, so nothing
    /// was started at all; a run that started but could not be followed to a conclusion; and a run
    /// whose process-tree cleanup failed, leaving containment unproven. The last shape may occur
    /// after this mutant's verdict was settled, but later mutants cannot safely run beside
    /// descendants that may still hold scratch-tree locks or inherited pipes.
    ///
    /// Not a verdict about the mutant either way. It exists so that a failure of the machinery
    /// stops the run and says why, instead of quietly becoming an unprotected run or, worse, a
    /// detection the suite never made.
    Unmetered(String),

    /// This one run could not be performed, though the run as a whole is still sound.
    ///
    /// The machine was momentarily out of something a subprocess needs — descriptors, process
    /// slots, address space — or the child could not be asked about after it started. A sweep runs
    /// `jobs` processes at once, each with pipes of its own, so exhausting one of those tables is an
    /// ordinary consequence of the workload rather than a standing fact about the run: the mutants
    /// already judged are still judged, and the ones after it are still judgeable.
    ///
    /// Separate from [`Self::Unmetered`] because the response differs. An unmetered run says nothing
    /// further can be trusted and stops; this says one mutant went unjudged, records why against
    /// that mutant, and lets the sweep continue. Conflating them lets one transient `EMFILE` discard
    /// every verdict an hours-long run had already reached.
    Unjudged(String),
}

/// How a test process ended when the host supplied that information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub(super) enum Termination {
    ExitCode(i32),
    #[cfg(unix)]
    Signal(i32),
    Unknown,
}

impl fmt::Display for Termination {
    #[expect(clippy::renamed_function_params, reason = "`formatter` is clearer than `f`")]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitCode(code) => write!(formatter, "exit code {code}"),
            #[cfg(unix)]
            Self::Signal(signal) => write!(formatter, "signal {signal}"),
            Self::Unknown => formatter.write_str("an unknown process termination"),
        }
    }
}

/// Bounded process output retained only when an observation fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FailureEvidence {
    pub(super) termination: Option<Termination>,
    pub(super) failed_tests: Vec<String>,
    pub(super) stdout_tail: String,
    pub(super) stderr_tail: String,
    pub(super) output_truncated: bool,
}

/// Runs one test binary with an optional active mutant, under a wall-clock budget.
///
/// Test failures are accepted directly after baseline validation, without an unmutated rerun.
/// Resource suspicions still receive a second observation:
///
/// * A timeout or a stall is retried with the same mutant under a budget several times larger.
///   Both are verdicts a loaded machine can produce on its own — the budget is calibrated from a
///   baseline measured when nothing else was competing for cores, while mutants run many at a time
///   — so the first answer is a suspicion about the machine and the second is the finding. A
///   suspected stall keeps a looser silence budget rather than none at all, so a mutant that really
///   has hung is still cut off early instead of waiting out the whole timeout.
/// * A memory verdict is retried unchanged. The ceiling is not always enforced by something that
///   watched the allocation happen: on Windows the child is spawned and only afterwards assigned to
///   the job object, so the verdict is inferred from the job's accounting, and on the timeout path
///   it is inferred from what the accounting read when the run was cut short. An inference deserves
///   the same second look as a budget running out.
#[cfg(test)]
pub(super) fn run_binary(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> Verdict {
    run_binary_observed(work, binary, attempt).verdict
}

#[derive(Debug)]
pub(super) struct BinaryRun {
    pub(super) verdict: Verdict,
    pub(super) reach: ReachObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReachObservation {
    Reached,
    NotReached,
    Unknown,
}

pub(super) fn reach_hint_is_final(attempt: Attempt<'_>) -> bool {
    attempt.timeout.is_none() && attempt.stall.budget.is_none() && !attempt.request.meter
}

pub(super) fn run_binary_observed(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> BinaryRun {
    #[cfg(test)]
    if let Some(run) = SCRIPTED_RUNNER
        .with_borrow_mut(|runner| runner.as_mut().map(|runner| runner(binary, attempt)))
        .or_else(|| SHARED_RUNNER.with_borrow(|runner| runner.as_ref().map(|runner| runner(binary, attempt))))
    {
        return finish_binary_run(binary, attempt.only, run);
    }

    let _resources = work.acquire_resources(binary, attempt.only);
    let run = run_binary_with_observer(&SimpleClock::new_system(), attempt, |attempt| observe(work, binary, attempt));
    finish_binary_run(binary, attempt.only, run)
}

fn finish_binary_run(binary: &TestBinary, only: Only<'_>, run: BinaryRun) -> BinaryRun {
    super::sweep::record_observed_killer(binary, only, &run.verdict);
    run
}

fn run_binary_with_observer(clock: &SimpleClock, attempt: Attempt<'_>, mut observe: impl FnMut(Attempt<'_>) -> Observation) -> BinaryRun {
    let mut first = true;
    let mut observe = |attempt: Attempt<'_>| {
        super::sweep::record_binary_launch_attempt(!first);
        let started = Stopwatch::new(clock);
        let observed = observe(attempt);
        super::sweep::record_binary_execution(!first, started.elapsed(), &observed.verdict);
        first =
        // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
        false;
        observed
    };
    let mut reach = ReachObservation::Unknown;
    let confirmed = settle_suspicions(attempt, |attempt| {
        let observed = observe(attempt);
        if observed.reach == ReachObservation::Reached {
            reach = ReachObservation::Reached;
        } else if reach != ReachObservation::Reached && observed.reach == ReachObservation::NotReached {
            reach = ReachObservation::NotReached;
        }
        observed.verdict
    });

    let verdict = match confirmed {
        Verdict::TestEnumerationFailed(output) => confirm_enumeration_with(attempt, output, |attempt| observe(attempt).verdict),
        other => other,
    };

    BinaryRun { verdict, reach }
}

#[cfg(test)]
type ScriptedRunner = Box<dyn FnMut(&TestBinary, Attempt<'_>) -> BinaryRun>;

#[cfg(test)]
pub(super) type SharedRunner = Arc<dyn Fn(&TestBinary, Attempt<'_>) -> BinaryRun + Send + Sync>;

#[cfg(test)]
std::thread_local! {
    static SCRIPTED_RUNNER: std::cell::RefCell<Option<ScriptedRunner>> = const { std::cell::RefCell::new(None) };
    static SHARED_RUNNER: std::cell::RefCell<Option<SharedRunner>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn shared_runner() -> Option<SharedRunner> {
    SHARED_RUNNER.with_borrow(Clone::clone)
}

#[cfg(test)]
pub(super) fn set_shared_runner(runner: Option<SharedRunner>) {
    SHARED_RUNNER.with_borrow_mut(|slot| *slot = runner);
}

#[cfg(test)]
pub(super) fn with_shared_runner<T>(runner: SharedRunner, body: impl FnOnce() -> T) -> T {
    struct Reset(Option<SharedRunner>);
    impl Drop for Reset {
        fn drop(&mut self) {
            set_shared_runner(self.0.take());
        }
    }
    let _reset = Reset(shared_runner());
    set_shared_runner(Some(runner));
    body()
}

/// Supplies final verdict/reach results for caller ordering tests, bypassing resource acquisition,
/// execution, suspicion settlement and confirmation. Unlike the observer seam this replaces the
/// whole binary run. Scoped to the calling thread and reset on return or unwind.
#[cfg(test)]
pub(super) fn with_binary_runner<T>(runner: impl FnMut(&TestBinary, Attempt<'_>) -> BinaryRun + 'static, body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCRIPTED_RUNNER.with_borrow_mut(|runner| {
                let _ = runner.take();
            });
        }
    }
    SCRIPTED_RUNNER.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "a thread cannot install two binary runners simultaneously");
        *slot = Some(Box::new(runner));
    });
    let _reset = Reset;
    body()
}

/// How many times a spawn refused for want of a machine resource is attempted in all.
///
/// A sweep runs `jobs` subprocesses at once, each with pipes of its own, so running the descriptor
/// or process table dry is an ordinary consequence of the workload rather than a fault. It clears as
/// soon as one of the mutants already running finishes, which is what these attempts wait for.
const SPAWN_ATTEMPTS: usize = 4;

/// How long the first retry waits; each one after it waits twice as long.
///
/// Backed off rather than tried in a tight loop, because every worker meets the shortage at the same
/// moment and retrying in step would keep the table exhausted between them.
const SPAWN_BACKOFF: Duration = Duration::from_millis(50);

/// Starts the child, waiting out a refusal the machine will recover from on its own.
enum StartError {
    Containment(PlatformError),
    Spawn(io::Error),
}

fn spawn_patiently(command: Command, request: MemoryRequest) -> Result<SpawnedCommand, StartError> {
    spawn_patiently_with_backoff(command, request, PreparedCommand::backoff)
}

fn spawn_patiently_with_backoff(
    command: Command,
    request: MemoryRequest,
    mut backoff: impl FnMut(PreparedCommand, Duration) -> Result<PreparedCommand, PlatformError>,
) -> Result<SpawnedCommand, StartError> {
    let mut waited = SPAWN_BACKOFF;

    // Prepared once, outside the loop, and carried through every wait: a retry re-spawns the launch
    // it already has rather than building a second one, which is the only shape `prepare` supports.
    let mut prepared = prepare(command, request).map_err(StartError::Containment)?;

    for _attempt in 1..SPAWN_ATTEMPTS {
        match spawn_once(prepared) {
            Ok(spawned) => return Ok(spawned),
            Err(FailedSpawn { cause, prepared: returned }) if transient(&cause) => {
                prepared = backoff(*returned, waited).map_err(StartError::Containment)?;
            }
            Err(FailedSpawn { cause, .. }) => return Err(StartError::Spawn(cause)),
        }

        waited = waited.saturating_mul(2);
    }

    spawn_once(prepared).map_err(|failure| StartError::Spawn(failure.cause))
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn start_or_verdict(command: Command, binary: &TestBinary, request: MemoryRequest) -> Result<SpawnedCommand, (Verdict, MemoryUsage)> {
    start_or_verdict_with(command, binary, request, spawn_patiently)
}

fn start_or_verdict_with(
    command: Command,
    binary: &TestBinary,
    request: MemoryRequest,
    spawn: impl FnOnce(Command, MemoryRequest) -> Result<SpawnedCommand, StartError>,
) -> Result<SpawnedCommand, (Verdict, MemoryUsage)> {
    spawn(command, request).map_err(|reason| {
        // A spawn fails for reasons of the machine — descriptors, processes, address space, a
        // binary being written as it is read — and none of them is a test failing. Reporting one as
        // a failing suite would credit the tests with a kill they never made.
        let verdict = match reason {
            StartError::Containment(reason) => Verdict::Unmetered(reason.to_string()),
            StartError::Spawn(cause) => {
                let reason = if request.wanted() {
                    format!(
                        "`{}` could not be started inside its memory accounting boundary: {cause}",
                        binary.path
                    )
                } else {
                    format!("`{}` could not be started: {cause}", binary.path)
                };

                Verdict::Unjudged(reason)
            }
        };

        (verdict, MemoryUsage::default())
    })
}

struct FailedSpawn {
    cause: io::Error,
    prepared: Box<PreparedCommand>,
}

/// The spawn itself, named so the fault seam can stand in for the kernel's refusal.
fn spawn_once(prepared: PreparedCommand) -> Result<SpawnedCommand, FailedSpawn> {
    #[cfg(test)]
    if faults::fired(Fault::Spawn) {
        return Err(FailedSpawn {
            cause: io::Error::new(io::ErrorKind::WouldBlock, "the process table is full"),
            prepared: Box::new(prepared),
        });
    }

    prepared.spawn().map_err(|failure| {
        let (cause, prepared) = failure.into_parts();

        FailedSpawn {
            cause,
            prepared: Box::new(prepared),
        }
    })
}

/// Whether a spawn refusal is the machine being momentarily out of something.
///
/// Rust gives only `WouldBlock` a stable `ErrorKind`, so the rest are matched on the raw code:
/// `EAGAIN` for the process table, `EMFILE` and `ENFILE` for this process's and the system's
/// descriptor tables, `ENOMEM` for the address space a fork needs. Everything else — a binary that
/// is not there, one that is not executable — is a standing fact that no amount of waiting changes.
fn transient(cause: &io::Error) -> bool {
    if cause.kind() == io::ErrorKind::WouldBlock || cause.kind() == io::ErrorKind::Interrupted {
        return true;
    }

    #[cfg(unix)]
    {
        matches!(
            cause.raw_os_error(),
            Some(libc::EAGAIN | libc::EMFILE | libc::ENFILE | libc::ENOMEM)
        )
    }

    #[cfg(not(unix))]
    {
        false
    }
}

/// Which second look a verdict calls for, or `None` for one that is already evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suspicion {
    /// Retried under a budget several times larger.
    Timeout,

    /// Retried under a looser silence budget.
    Stall,

    /// Retried unchanged; the ceiling was inferred from accounting rather than watched.
    Memory,
}

impl Suspicion {
    /// The second look this verdict calls for.
    const fn of(verdict: &Verdict) -> Option<Self> {
        match verdict {
            Verdict::TimedOut => Some(Self::Timeout),
            Verdict::Stalled(_) => Some(Self::Stall),
            Verdict::MemoryLimit { .. } => Some(Self::Memory),
            _evidence => None,
        }
    }

    /// The attempt that checks it.
    fn checking(self, attempt: Attempt<'_>) -> Attempt<'_> {
        match self {
            Self::Timeout => attempt.lengthened(),
            Self::Stall => attempt.patient(),
            Self::Memory => attempt,
        }
    }
}

/// How many confirmations one verdict may cost before it is taken as it stands.
///
/// Two, because a confirmation can land in a *different* detection class than the one it was
/// checking — a lengthened re-run that hits the memory ceiling, a patient one that runs out of time
/// — and that class has then been observed exactly once, which is what the first confirmation
/// exists to refuse. Beyond two the classes are alternating rather than settling, and the run stops
/// paying for a second look it is not getting.
const CONFIRM_ROUNDS: usize = 2;

/// Runs the class-specific confirmations until the verdict is evidence, agreed with, or out of rounds.
///
/// A confirmation that reproduces the class it was checking is the agreement it asked for, and ends
/// the ladder — a third run would re-confirm something already seen twice.
///
/// `run` performs one attempt; production hands it [`observe`].
fn settle_suspicions(attempt: Attempt<'_>, mut run: impl FnMut(Attempt<'_>) -> Verdict) -> Verdict {
    let mut verdict = run(attempt);
    let mut checked = None;

    for _round in 0..CONFIRM_ROUNDS {
        let Some(suspicion) = Suspicion::of(&verdict) else {
            // #[gamma::skip(loop.break_to_continue, reason = "settle_suspicions uses a finite 0..CONFIRM_ROUNDS for-loop. When Suspicion::of(verdict) is None, continue only rechecks the same unchanged verdict on remaining rounds; the run callback and checked assignment below are never reached, so returned verdict and callback trace are unchanged.", tag = "equivalent")]
            break;
        };

        if checked == Some(suspicion) {
            // #[gamma::skip(loop.break_to_continue, reason = "settle_suspicions checks checked==Some(suspicion) before assigning checked or running a confirmation. Replacing this break by continue only repeats that same comparison over the remaining finite rounds with unchanged checked and verdict; no further confirmation callback can run.", tag = "equivalent")]
            break;
        }

        checked = Some(suspicion);
        verdict = run(suspicion.checking(attempt));
    }

    verdict
}

/// Confirms that nextest's inability to enumerate tests was caused by the active mutant.
#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
fn confirm_enumeration(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, output: String) -> Verdict {
    confirm_enumeration_with(attempt, output, |attempt| observe(work, binary, attempt).verdict)
}

fn confirm_enumeration_with(attempt: Attempt<'_>, output: String, observe: impl FnOnce(Attempt<'_>) -> Verdict) -> Verdict {
    match observe(attempt.exonerating()) {
        Verdict::Passed => Verdict::TestEnumerationFailed(output),
        Verdict::Failed(test) => Verdict::Flaky(test),
        Verdict::Unmetered(reason) => Verdict::Unmetered(reason),
        Verdict::Unjudged(reason) => Verdict::Unjudged(reason),

        // Timeout or memory-limit evidence from an exonerating run cannot be attributed to the
        // mutant because no mutant was active. Returning either verdict would record the suite as
        // having missed a mutant that was switched off for the run that produced the evidence.
        unestablished => unconfirmed(None, &unestablished),
    }
}

fn unconfirmed(test: Option<String>, verdict: &Verdict) -> Verdict {
    let reason = match verdict {
        Verdict::TimedOut => "exceeded its time budget",
        Verdict::Stalled(_) => "stopped reporting progress",
        Verdict::MemoryLimit { .. } => "exceeded its memory ceiling",
        _ => "did not establish a trustworthy passing result",
    };
    Verdict::Unconfirmed(test, reason)
}

/// Says on the diagnostic stream that a verdict was reached on less output than the binary produced.
///
/// Worth saying because the shortfall is invisible in the verdict itself: a truncated stream that
/// lost the failure announcement is indistinguishable from a suite that failed without naming a
/// test, and both come out as a kill with no killer. A reader chasing an unnamed kill would
/// otherwise spend the search on the suite rather than on the pipe.
fn announce_partial(binary: &TestBinary) {
    let message = format!(
        "the output of `{}` could not be read to the end, so the failing test may be named in text \
         this run never saw. The exit status still decided the verdict.",
        binary.path
    );
    // #[gamma::skip(call_result.default, reason = "announce returns unit and this mutator retains the diagnostic publication before replacing unit with unit")]
    announce(&message);
}

/// Writes one diagnostic line, for the running command to say through its own `Host`.
///
/// Raised as a note rather than written here: this is reached on a worker thread while the progress
/// display owns the terminal, so writing now would cut across a line somebody else is drawing.
fn announce(message: &str) {
    crate::notes::note(message.to_owned());
}

/// How much more room a suspected timeout or stall is given before it is believed.
///
/// Large enough that scheduling noise cannot survive it, and paid only by mutants that already
/// exhausted their budget — a small population, since a genuine hang is rare and a false one rarer
/// still.
pub(crate) const CONFIRM_FACTOR: u32 = 3;

/// Builds the command that runs one test binary, under whichever runner this run selected.
///
/// The two differ in more than the executable. Run directly, a binary takes the test arguments on
/// its own command line and has to be started in its package's directory, because that is where
/// `cargo test` would have started it. Run under nextest, both are nextest's business: it forwards
/// the arguments after `--` and sets each test's working directory to its own package root, so
/// imposing one here would make it resolve the whole workspace relative to a single package.
#[cfg_attr(coverage_nightly, coverage(off))]
fn launcher(work: &Workspace, binary: &TestBinary, only: Only<'_>, fail_fast: bool) -> Result<Command, Verdict> {
    let Some(harness) = work.runner() else {
        let mut command = Command::new(binary.path.as_std_path());
        let names = only.names();
        let resource_bearing = !work.required_resources(binary, only).is_empty();

        if names.is_empty() {
            // The whole binary runs, so the user's arguments are the whole selection and go through
            // exactly as written.
            if resource_bearing {
                let _ = command.args(HarnessFilters::parse(work.test_arguments()).without_test_threads());
            } else {
                let _ = command.args(work.test_arguments());
            }
        } else {
            // libtest matches a test that any *one* positional filter matches, so appending the
            // name this run chose to the user's own filters would widen the set rather than narrow
            // it — and a mutant could be convicted by a test the user deliberately excluded. The
            // intersection libtest cannot express is computed here instead: the user's filters
            // decide which of the chosen names may run, and only the survivors are passed.
            let user = HarnessFilters::parse(work.test_arguments());
            let allowed: Vec<&str> = names.iter().copied().filter(|name| user.admits(name)).collect();

            if allowed.is_empty() {
                return Err(Verdict::Unjudged(format!(
                    "the test selection this run made for `{}` names nothing the harness filters allow, \
                     so it was refused rather than run as a wider selection",
                    binary.path
                )));
            }

            // The user's positional filters are not repeated here: `admits` has already applied
            // them, and leaving them out is what lets `--exact` pin this run's own names without
            // also silently converting the user's substring filter into a whole-name match.
            if resource_bearing {
                let _ = command.args(user.flags_without_test_threads());
            } else {
                let _ = command.args(user.flags());
            }
            let _ = command.args(allowed).arg("--exact");
        }
        if resource_bearing {
            let _ = command.args(["--test-threads", "1"]);
        }
        let _ = command.current_dir(working_directory(work, binary).as_std_path());
        work.inherit_test_cache_home(&mut command);

        return Ok(command);
    };

    harness
        .command(work, binary, only.names(), fail_fast, true)
        .map_err(|cause| Verdict::Unmetered(cause.to_string()))
}

/// Turns a runner's non-zero exit into a verdict about the mutant.
///
/// A binary run directly says only that it failed, and libtest names the first failing test in its
/// output. Nextest distinguishes far more, and the distinction matters: a code saying the tests ran
/// and one failed convicts the mutant, whereas a code saying nextest matched no tests or could not
/// start is a fact about this run. Crediting the suite with a kill it never made would inflate the
/// score, so anything nextest does not describe as a test failure stops the run instead.
fn settle(
    under_nextest: bool,
    active: Option<u32>,
    code: Option<i32>,
    text: &[u8],
    authoritative: &[u8],
    usage: MemoryUsage,
) -> (Verdict, MemoryUsage) {
    if runtime_startup_failure(text) {
        return (
            Verdict::Unmetered(
                "the guard runtime could not acquire the process startup environment or install its startup selection".to_owned(),
            ),
            usage,
        );
    }

    let output = String::from_utf8_lossy(text);
    let authoritative = String::from_utf8_lossy(authoritative);

    if !under_nextest {
        return (Verdict::Failed(first_failure(&authoritative).map(str::to_owned)), usage);
    }

    match code {
        Some(nextest::TEST_RUN_FAILED) => (Verdict::Failed(nextest::first_failure(&authoritative).map(str::to_owned)), usage),

        // Nothing ran, so nothing decided anything. This is the filterset and the built tree
        // disagreeing about what exists, which is a fault in the run rather than in the mutant.
        Some(nextest::NO_TESTS_RUN) => (
            Verdict::Unmetered("`cargo nextest` matched no tests for a binary this run built".to_owned()),
            usage,
        ),

        Some(nextest::TEST_LIST_CREATION_FAILED) if active.is_some() => (Verdict::TestEnumerationFailed(output.trim().to_owned()), usage),

        // A signal, or a code nextest uses for its own failures. Either way it is not a verdict.
        other => (Verdict::Unmetered(nextest_runner_failure(other, &output)), usage),
    }
}

fn settle_collected(
    under_nextest: bool,
    active: Option<u32>,
    code: Option<i32>,
    collected: &Collected,
    usage: MemoryUsage,
) -> (Verdict, MemoryUsage) {
    settle(under_nextest, active, code, &collected.text, &collected.authoritative, usage)
}

/// Describes a nextest infrastructure failure without discarding its diagnostic output.
fn nextest_runner_failure(code: Option<i32>, output: &str) -> String {
    let mut reason = format!(
        "`cargo nextest` exited with {}, which does not describe a test run",
        code.map_or_else(|| "a signal".to_owned(), |code| format!("code {code}"))
    );
    let useful = output.trim();

    if !useful.is_empty() {
        reason.push_str(":\n");
        reason.push_str(tail(useful, DIAGNOSTIC_TAIL_LINES).as_ref());
    }

    reason
}

/// The controlled environment changes applied to a baseline process.
///
/// This deliberately excludes the inherited environment and the loader path's value. The latter
/// contains inherited path entries, while the fact that cargo-gamma configured it is enough to
/// diagnose a loader mismatch without copying ambient process data into an artifact.
pub(super) fn baseline_environment(work: &Workspace, binary: &TestBinary, only: Only<'_>) -> serde_json::Value {
    let launch = work.launch();
    let mut set = BTreeMap::from(
        // #[gamma::skip(collection.reverse_array, reason = "BTreeMap::from receives three distinct environment-variable keys. Reversing insertion order yields the identical sorted key/value map.", tag = "equivalent")]
        [
            (UNDER_GAMMA_VAR, "1".to_owned()),
            (INSTA_UPDATE_VAR, "no".to_owned()),
            (INSTA_FORCE_PASS_VAR, "0".to_owned()),
        ],
    );

    let threads = if work.required_resources(binary, only).is_empty() {
        work.harness_threads()
    } else {
        Some("1")
    };
    if let Some(threads) = threads {
        let _ = set.insert(TEST_THREADS_VAR, threads.to_owned());
    }

    serde_json::json!({
        "set": set,
        "removed": [gamma_rt::ACTIVE_VAR, gamma_rt::REACH_VAR, "NEXTEST", "NEXTEST_*"],
        "valuesOmitted": [STACK_VAR],
        "loaderPathConfigured": launch.loader.is_some(),
    })
}

/// Sets everything a test binary is run with, beyond the command itself.
///
/// Split out from [`run_with`] because it is a long, linear list of decisions that each carry
/// their own reason, and reading the run's control flow around them is otherwise impossible.
fn configure(
    command: &mut Command,
    binary: &TestBinary,
    launch: &Launch,
    threads: Option<&str>,
    active: Option<u32>,
    reach: Option<&Utf8Path>,
) {
    let _ = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A mutant that panics in a thousand tests would otherwise spend the whole budget
        // formatting backtraces nobody reads.
        .env("RUST_BACKTRACE", "0")
        // Every mutation site becomes a branch holding both the original and the replacement, so
        // an instrumented frame is larger than the one it stands in for. Deeply recursive code
        // that fits comfortably otherwise can exhaust a default 2 MiB test thread and abort the
        // process, which reads as a failure of the suite rather than of the mutant. Raising the
        // floor buys back the headroom instrumentation spent; an explicit setting still wins.
        .env(STACK_VAR, launch.stack.as_str())
        // Set for the baseline as well as for every mutant, so a test can branch on it without
        // knowing which phase it is in. A suite that shells out to cargo itself — this one
        // included — would otherwise run a nested build inside the scratch tree, failing for
        // reasons that have nothing to do with any mutant.
        .env(UNDER_GAMMA_VAR, "1")
        // Insta's own defaults are safe, but these are inherited, and a developer part-way
        // through `cargo insta review` has them set. Accepting a snapshot would let a mutant that
        // changed the snapshotted value pass, reporting it missed. One scratch tree serves many
        // mutants, so a single accepted write would go on judging every mutant after it against a
        // snapshot a mutant wrote.
        .env(INSTA_UPDATE_VAR, "no")
        .env(INSTA_FORCE_PASS_VAR, "0");

    // An outer nextest invocation marks this process and every subprocess it starts. Those markers
    // describe the outer test, not this launch: direct libtest must not see them, and a selected
    // nextest runner must establish fresh values for the test it actually runs.
    for name in INHERITED_NEXTEST_VARIABLES.get_or_init(|| {
        env::vars_os()
            .filter_map(|(name, _)| {
                let name_text = name.to_string_lossy();
                (name_text == "NEXTEST" || name_text.starts_with("NEXTEST_")).then_some(name)
            })
            .collect()
    }) {
        let _ = command.env_remove(name);
    }

    // Set per command rather than on this process, which the suite calls `run` from many threads
    // of at once. `None` means the caller already chose a width and it is left alone.
    if let Some(threads) = threads {
        let _ = command.env(TEST_THREADS_VAR, threads);
    }

    // Cargo sets this for every test it runs, and `env!("CARGO_MANIFEST_DIR")` is the usual way to
    // reach a fixture from a test. Left unset, the macro's compile-time value points into the
    // scratch tree's original location rather than the copy being tested.
    if !binary.manifest_dir.as_str().is_empty() {
        let _ = command.env("CARGO_MANIFEST_DIR", binary.manifest_dir.as_std_path());
    }

    match active {
        Some(ordinal) => {
            let _ = command.env(gamma_rt::ACTIVE_VAR, ordinal.to_string());
        }
        None => {
            let _ = command.env_remove(gamma_rt::ACTIVE_VAR);
        }
    }

    match reach {
        Some(path) => {
            let _ = command.env(gamma_rt::REACH_VAR, path.as_std_path());
        }
        None => {
            let _ = command.env_remove(gamma_rt::REACH_VAR);
        }
    }
}

fn configured_launcher(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> Result<Command, Verdict> {
    let mut command = launcher(work, binary, attempt.only, attempt.active.is_some())?;
    let launch = work.launch();
    configure_loader(&mut command, launch);
    work.configure_test_environment(&mut command, binary);
    let threads = if work.required_resources(binary, attempt.only).is_empty() {
        work.harness_threads()
    } else {
        Some("1")
    };
    // #[gamma::skip(call_result.default, reason = "configure returns unit and this mutator retains all command mutations before replacing unit with unit")]
    configure(&mut command, binary, launch, threads, attempt.active, None);
    Ok(command)
}

/// Runs one binary, publishing the harness's progress into `progress` as it goes.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn run_with(
    work: &Workspace,
    binary: &TestBinary,
    attempt: Attempt<'_>,
    progress: &Arc<Mutex<Progress>>,
    retain_failure: bool,
) -> (Verdict, MemoryUsage, Option<FailureEvidence>) {
    run_with_observation(work, binary, attempt, progress, retain_failure, None, ask_after)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn run_with_observation(
    work: &Workspace,
    binary: &TestBinary,
    attempt: Attempt<'_>,
    progress: &Arc<Mutex<Progress>>,
    retain_failure: bool,
    reach: Option<&Utf8Path>,
    mut observe_exit: impl FnMut(&mut ProcessTree) -> io::Result<Option<ExitStatus>>,
) -> (Verdict, MemoryUsage, Option<FailureEvidence>) {
    let (active, timeout, stall, request) = (attempt.active, attempt.timeout, attempt.stall, attempt.request);

    let mut command = match configured_launcher(work, binary, attempt) {
        Ok(command) => command,
        Err(verdict) => return (verdict, MemoryUsage::default(), None),
    };
    if let Some(path) = reach {
        let _ = command.env(gamma_rt::REACH_VAR, path.as_std_path());
    }

    // Nextest reports on stderr: the per-test `FAIL` lines that name what convicted a mutant, and
    // the progress that tells the stall detector the child is alive. Left discarded, every nextest
    // failure would be anonymous and a healthy run would look silent. A binary run directly says
    // all of that on stdout, and its stderr carries only panic noise that no verdict is read from.
    let under_nextest = work.runner().is_some();

    let spawned = match start_or_verdict(command, binary, request) {
        Ok(started) => started,
        Err((verdict, usage)) => return (verdict, usage, None),
    };

    // Adopted before anything is read from the child, so the typestate bundle cannot sit with its
    // interrupt window open while a grandchild starts outside the containment.
    let mut subtree = match ProcessTree::adopt(spawned) {
        Ok(subtree) => subtree,

        // The child is already live, so this is one run that cannot be accounted for rather than a
        // boundary the host will never provide — `prepare` above answers that question, and it
        // succeeded.
        Err(reason) => {
            return (Verdict::Unjudged(reason.to_string()), MemoryUsage::default(), None);
        }
    };

    let pulse = Arc::new(Pulse::default());
    let drained = match readers(&mut subtree, progress, &pulse, under_nextest, retain_failure) {
        Ok(drained) => drained,
        Err(cause) => return reader_failure(&mut subtree, request, binary, &cause),
    };

    let mut supervision = NativeSupervision {
        subtree: &mut subtree,
        drained: &drained,
        pulse: &pulse,
        binary,
        request,
        deadline: timeout.map(deadline_after),
        stall,
        observe_exit: &mut observe_exit,
        observation_attempted: std::cell::Cell::new(
            // #[gamma::skip(literal.bool_flip, reason = "The initial observation_attempted flag is overwritten by observe before expiration can inspect it. Only the later observe/expiration handshake enforces the lifecycle invariant.", tag = "equivalent")]
            false,
        ),
    };
    supervise(&mut supervision, binary, active, under_nextest, request, progress, retain_failure)
}

fn reader_failure(
    subtree: &mut impl ContainedProcess,
    request: MemoryRequest,
    binary: &TestBinary,
    cause: &ReaderStartError,
) -> (Verdict, MemoryUsage, Option<FailureEvidence>) {
    let (usage, _ceiling) = match subtree.cut_short(request, binary, false) {
        Ok(stopped) => stopped,
        Err((verdict, usage)) => return (verdict, usage, None),
    };
    (
        Verdict::Unjudged(format!("`{}` output could not be supervised: {cause}", binary.path)),
        usage,
        None,
    )
}

/// The native lifecycle/clock boundary; the verdict state machine consumes only these observations.
trait Supervision {
    fn observe(&mut self) -> io::Result<Option<ExitStatus>>;
    fn released(&self) -> bool;
    fn collect(&self) -> Collected;
    fn usage(&self) -> MemoryUsage;
    fn cut_short(&mut self, settled: bool) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)>;
    fn expiration(&self, progress: &Mutex<Progress>) -> (bool, bool);
    fn seen(&self) -> u64;
    fn wait(&self, seen: u64, progress: &Mutex<Progress>);
}

/// Verdict-aware containment policy over process release, usage and cleanup.
trait ContainedProcess {
    fn released(&self) -> bool;
    fn usage(&self) -> MemoryUsage;
    fn cut_short(
        &mut self,
        request: MemoryRequest,
        binary: &TestBinary,
        settled: bool,
    ) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)>;
}

/// Native cleanup primitives, separated from the verdict and memory-request policy.
trait ProcessCleanup {
    fn leader_released(&self) -> bool;
    fn snapshot_usage(&self) -> MemoryUsage;
    fn terminate_tree(&mut self) -> io::Result<()>;
}

impl ProcessCleanup for ProcessTree {
    // #[gamma::skip(fn_value.bool_true, reason = "This thin ProcessTree adapter reads native containment/reaping state. An owned fake bypasses this exact native adapter; deterministic tests cannot launch a child or mutate native process state.", tag = "native-boundary")]
    fn leader_released(&self) -> bool {
        Self::released(self)
    }

    fn snapshot_usage(&self) -> MemoryUsage {
        Self::usage(self)
    }

    fn terminate_tree(&mut self) -> io::Result<()> {
        Self::terminate(self).map(|_status| ())
    }
}

impl<T: ProcessCleanup> ContainedProcess for T {
    fn released(&self) -> bool {
        self.leader_released()
    }

    fn usage(&self) -> MemoryUsage {
        self.snapshot_usage()
    }

    fn cut_short(
        &mut self,
        request: MemoryRequest,
        binary: &TestBinary,
        settled: bool,
    ) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
        cut_short(self, request, binary, settled)
    }
}

struct NativeSupervision<'a, F, T = ProcessTree> {
    subtree: &'a mut T,
    drained: &'a Receiver<Drained>,
    pulse: &'a Pulse,
    binary: &'a TestBinary,
    request: MemoryRequest,
    deadline: Option<Instant>,
    stall: Stall,
    observe_exit: &'a mut F,
    observation_attempted: std::cell::Cell<bool>,
}

impl<T: ContainedProcess, F: FnMut(&mut T) -> io::Result<Option<ExitStatus>>> Supervision for NativeSupervision<'_, F, T> {
    fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        self.observation_attempted.set(true);
        (self.observe_exit)(self.subtree)
    }
    fn released(&self) -> bool {
        self.subtree.released()
    }
    fn collect(&self) -> Collected {
        collected(self.drained, DRAIN_GRACE)
    }
    fn usage(&self) -> MemoryUsage {
        self.subtree.usage()
    }
    fn cut_short(&mut self, settled: bool) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
        self.subtree.cut_short(self.request, self.binary, settled)
    }
    fn expiration(&self, progress: &Mutex<Progress>) -> (bool, bool) {
        assert!(
            self.observation_attempted.replace(false),
            "execution budgets are inspected only after attempting the current child observation"
        );
        (
            self.stall.exceeded(progress),
            self.deadline.is_some_and(|deadline| deadline_reached(deadline, Instant::now())),
        )
    }
    fn seen(&self) -> u64 {
        self.pulse.seen()
    }
    fn wait(&self, seen: u64, progress: &Mutex<Progress>) {
        let budget = WAIT_CAP
            .min(self.deadline.map_or(WAIT_CAP, remaining))
            .min(self.stall.slack(progress));
        debug_assert!(
            budget <= WAIT_CAP,
            "native supervision must return to child observation within its polling cap"
        );
        self.pulse.wait(seen, budget);
    }
}

fn supervise(
    supervision: &mut impl Supervision,
    binary: &TestBinary,
    active: Option<u32>,
    under_nextest: bool,
    request: MemoryRequest,
    progress: &Mutex<Progress>,
    retain_failure: bool,
) -> (Verdict, MemoryUsage, Option<FailureEvidence>) {
    macro_rules! cut_short {
        ($verdict_settled:expr) => {
            match supervision.cut_short($verdict_settled) {
                Ok(stopped) => stopped,
                Err((verdict, usage)) => return (verdict, usage, None),
            }
        };
    }

    loop {
        // Read before the child is checked, so a signal raised while this iteration is working is
        // not slept through.
        let seen = supervision.seen();

        match supervision.observe() {
            Ok(Some(status)) => {
                // `ProcessTree::observe` used a non-reaping observation, swept descendants, released
                // the interrupt slot, and only then reaped the leader.
                debug_assert!(supervision.released(), "the containment is released before the output is drained");

                let collected = supervision.collect();
                let usage = supervision.usage();
                let ceiling = request.limit.filter(|_limit| exhausted(&usage, status.success()));
                let code = status.code();
                let evidence = retain_failure.then(|| collected.failure_evidence(Some(termination(status)), announced_failures(progress)));

                if let Some(verdict) = environment_verdict(progress) {
                    return (verdict, usage, evidence);
                }

                if status.success() {
                    return (Verdict::Passed, usage, None);
                }

                // Said before the text is read rather than after, because what follows draws a
                // conclusion from an absence — no announced failure means the harness named no
                // test — and that reading is only sound if the text is all of it. The run is not
                // abandoned over it: the exit status still says the suite failed, which is the
                // verdict, and only the name is at risk.
                if !collected.whole {
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                    announce_partial(binary);
                }

                // Asked before the ceiling is turned into a verdict, because the harness's own
                // report is the more useful of the two: a named test is somewhere a reader can go,
                // and a memory verdict that pre-empted it would leave them with a number and no
                // idea which of the suite's tests to open.
                let (verdict, usage) = settle_collected(under_nextest, active, code, &collected, usage);

                return (prefer_named(verdict, usage.peak, ceiling), usage, evidence);
            }

            Ok(None) => {
                if let Some(verdict) = environment_verdict(progress) {
                    let (usage, _ceiling) = cut_short!(true);
                    let collected = supervision.collect();

                    return (
                        verdict,
                        usage,
                        retain_failure.then(|| collected.failure_evidence(None, announced_failures(progress))),
                    );
                }

                // A direct libtest failure settles a mutant verdict, so every test after it would
                // be paid for to learn nothing. A baseline instead completes to collect every
                // failure and libtest's trailing diagnostics. Nextest prints its FAIL line before
                // replaying the failed child's captured output, which may hold the guard runtime's
                // environment-error marker, so it is never cut here.
                if !retain_failure && let Some(name) = failure_to_cut_short(under_nextest, progress) {
                    let (usage, _ceiling) = cut_short!(true);
                    let _collected = supervision.collect();

                    return (cut_by_named_failure(name), usage, None);
                }

                let (stalled, expired) = supervision.expiration(progress);

                if stalled || expired {
                    let (usage, ceiling) = cut_short!(true);
                    return settle_budget_with(under_nextest, progress, stalled, retain_failure, || {
                        (usage, ceiling, supervision.collect())
                    });
                }

                // Bounded by whichever comes first: the deadline, the earliest moment silence
                // could be called a stall, and the backstop. Sleeping past any of the three would
                // let the run overshoot a limit it promised to keep.
                supervision.wait(seen, progress);
            }

            // A non-reaping exit observation reports an error only when the child can no longer
            // be observed. No host arranges that on request, so it is asked for through the fault
            // seam instead — see `ask_after`.
            //
            // Nothing is known about the mutant at this point: the child is still running, and the
            // one question that would have settled it cannot be asked again. Reporting a failing
            // suite here would credit the tests with a kill they never made, so the run says it
            // could not be measured instead, which is not a verdict about the mutant and is not
            // scored as one. The child is not simply dropped either — everything it spawned is
            // ended and reaped first, since an orphan holds scratch-tree locks and the pipes the
            // readers below are waiting on, and both outlive this function into the next mutant.
            Err(cause) => {
                let stopped = supervision.cut_short(false);

                debug_assert!(supervision.released(), "the containment is released before the output is drained");

                // Discarded, since there is no verdict to read out of it: the drain is here so the
                // reader threads reach end of file and end, rather than being left holding pipes
                // for the rest of the run. Whether it was complete is discarded with it — nothing
                // was going to be concluded from the text either way.
                let collected = supervision.collect();

                let (usage, _ceiling) = match stopped {
                    Ok(stopped) => stopped,
                    Err((verdict, usage)) => {
                        return (
                            verdict,
                            usage,
                            retain_failure.then(|| collected.failure_evidence(None, announced_failures(progress))),
                        );
                    }
                };

                return (
                    Verdict::Unjudged(format!("`{}` could not be asked whether it had finished: {cause}", binary.path)),
                    usage,
                    retain_failure.then(|| collected.failure_evidence(None, announced_failures(progress))),
                );
            }
        }
    }
}

fn deadline_reached(deadline: Instant, now: Instant) -> bool {
    now >= deadline
}

fn settle_budget_with(
    under_nextest: bool,
    progress: &Mutex<Progress>,
    stalled: bool,
    retain_failure: bool,
    observe: impl FnOnce() -> (MemoryUsage, Option<u64>, Collected),
) -> (Verdict, MemoryUsage, Option<FailureEvidence>) {
    let (usage, ceiling, collected) = observe();
    let evidence = retain_failure.then(|| collected.failure_evidence(None, announced_failures(progress)));
    let verdict = if let Some(verdict) = unfinished_nextest_failure(under_nextest, progress) {
        verdict
    } else if let Some(limit) = ceiling {
        Verdict::MemoryLimit { peak: usage.peak, limit }
    } else if stalled {
        Verdict::Stalled(last_test(progress))
    } else {
        Verdict::TimedOut
    };

    (verdict, usage, evidence)
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    let timeout = timeout.min(Duration::from_secs(crate::bounds::MOST_SECONDS));

    now.checked_add(timeout).unwrap_or(now)
}

/// Gathers whatever the readers have handed back, and whether it is all of it.
///
/// Both streams are wanted and either may arrive first, so this waits out the grace period rather
/// than taking the first chunk. Their combined text remains available for environment and runner
/// diagnostics, while the authoritative stream is retained separately for failure-name parsing.
/// A stream that ended in a read failure makes the whole collection partial.
///
/// A reader that never returns is the same loss and is reported the same way. Only a disconnect
/// says every reader has sent and dropped its sender; running out of grace means one of them is
/// still holding a pipe open — a descendant that escaped the containment inherited the write end —
/// and its stream is missing entirely. Reading that as a complete collection would let a run whose
/// `FAIL` line never arrived convict a mutant with no killer named and warn about nothing.
struct Drained {
    text: Vec<u8>,
    tail: Vec<u8>,
    tail_truncated: bool,
    complete: bool,
    authoritative: bool,
    stream: ReaderStream,
}

struct Collected {
    text: Vec<u8>,
    authoritative: Vec<u8>,
    stdout_tail: Vec<u8>,
    stderr_tail: Vec<u8>,
    tail_truncated: bool,
    whole: bool,
}

/// Receives output under the caller's bounded grace period.
trait CollectionReceiver {
    fn receive(&self, grace: Duration) -> Result<Drained, RecvTimeoutError>;
}

impl CollectionReceiver for Receiver<Drained> {
    fn receive(&self, grace: Duration) -> Result<Drained, RecvTimeoutError> {
        self.recv_timeout(grace)
    }
}

/// Supplies a finite response trace and rejects reads beyond its scripted terminal response.
#[cfg(test)]
struct ScriptedCollection {
    grace: Duration,
    replies: core::cell::RefCell<VecDeque<Result<Drained, RecvTimeoutError>>>,
}

#[cfg(test)]
impl CollectionReceiver for ScriptedCollection {
    fn receive(&self, grace: Duration) -> Result<Drained, RecvTimeoutError> {
        assert_eq!(grace, self.grace, "collection must preserve its bounded receive policy");
        self.replies
            .borrow_mut()
            .pop_front()
            .expect("collection must stop at the scripted terminal reply")
    }
}

fn collected(drained: &impl CollectionReceiver, grace: Duration) -> Collected {
    let mut text = Vec::new();
    let mut authoritative = Vec::new();
    let mut stdout_tail = Vec::new();
    let mut stderr_tail = Vec::new();
    let mut tail_truncated = false;
    let mut whole = true;

    loop {
        let received = drained.receive(grace);
        match received {
            Ok(chunk) => {
                if chunk.authoritative {
                    let complete_prefix = if chunk.complete {
                        chunk.text.len()
                    } else {
                        chunk.text.iter().rposition(|byte| *byte == b'\n').map_or(0, |last| last + 1)
                    };
                    authoritative.extend_from_slice(&chunk.text[..complete_prefix]);
                }
                text.extend_from_slice(&chunk.text);
                match chunk.stream {
                    ReaderStream::Stdout => stdout_tail = chunk.tail,
                    ReaderStream::Stderr => stderr_tail = chunk.tail,
                }
                tail_truncated |= chunk.tail_truncated;
                whole &= chunk.complete;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Collected {
                    text,
                    authoritative,
                    stdout_tail,
                    stderr_tail,
                    tail_truncated,
                    whole,
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                return Collected {
                    text,
                    authoritative,
                    stdout_tail,
                    stderr_tail,
                    tail_truncated,
                    whole: false,
                };
            }
        }
    }
}

impl Collected {
    fn failure_evidence(&self, termination: Option<Termination>, failed_tests: Vec<String>) -> FailureEvidence {
        let (stdout_tail, stdout_lines_truncated) = diagnostic_tail(&self.stdout_tail);
        let (stderr_tail, stderr_lines_truncated) = diagnostic_tail(&self.stderr_tail);

        FailureEvidence {
            termination,
            failed_tests,
            stdout_tail,
            stderr_tail,
            output_truncated: !self.whole || self.tail_truncated || stdout_lines_truncated || stderr_lines_truncated,
        }
    }
}

fn diagnostic_tail(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    let truncated = text.lines().count() > OUTPUT_TAIL_LINES;

    (tail(&text, OUTPUT_TAIL_LINES).into_owned(), truncated)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn termination(status: ExitStatus) -> Termination {
    if let Some(code) = status.code() {
        return Termination::ExitCode(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(signal) = status.signal() {
            return Termination::Signal(signal);
        }
    }

    Termination::Unknown
}

/// Asks whether the child has finished, without giving cleanup a reused group id.
///
/// Named rather than called inline only so that the error arm below it can be reached from a test.
/// Outside test builds, [`ProcessTree::observe`] performs Unix's non-reaping observation and owns the
/// sweep, release, and final reap as one lifecycle.
// #[gamma::skip(fn_value.ok, tag = "timeout", reason = "always reporting no process exit prevents supervision from observing completion")]
fn ask_after(subtree: &mut ProcessTree) -> io::Result<Option<ExitStatus>> {
    #[cfg(test)]
    if faults::fired(Fault::Wait) {
        return Err(io::Error::other("the wait a test asked to fail"));
    }

    subtree.observe()
}

/// Chooses between a failure the harness named and a ceiling the same run also crossed.
///
/// Both can be true at once, and the name is the more useful of the two: it is somewhere a reader
/// can go, whereas a memory verdict leaves them with a number and no idea which test to open. The
/// named failure therefore takes precedence.
///
/// With nothing named there is only the ceiling to report, and reporting it matters: without it the
/// run would say the suite failed, which sends a reader looking for an assertion that never was.
fn prefer_named(verdict: Verdict, peak: Option<u64>, ceiling: Option<u64>) -> Verdict {
    let Some(limit) = ceiling else {
        return verdict;
    };

    match verdict {
        Verdict::Failed(None) => Verdict::MemoryLimit { peak, limit },

        // Not a verdict about the mutant at all, and a ceiling does not make it one.
        other => other,
    }
}

/// The verdict a run cut short by a failure the harness announced reaches.
///
/// Stated once, and in the same terms as the ordinary exit path, because the two are the same run
/// seen from either side of one race: whether the reader publishes the failure before the exit
/// observation cleans up the child decides which of them is taken, and nothing orders those two events. A policy
/// spelled out twice would make the verdict a statement about that ordering — the name kept on a
/// machine where the reader wins and replaced by a byte count where the reaper does.
///
/// [`cut_short`] has already ended the run by the time this is asked, so both facts are final.
fn cut_by_named_failure(name: String) -> Verdict {
    Verdict::Failed(Some(name))
}

/// Ends a run early, whether before or after its verdict is settled.
///
/// Everything the workload spawned goes with it. An orphan holds locks in the scratch tree, which
/// fails the next run, and an inherited pipe handle, which keeps whoever is reading this run's
/// output from ever seeing end of file.
///
/// The ceiling comes back as the limit that fired rather than as a verdict, because what to make of
/// it differs by caller and only the caller knows which facts it also holds. A run cut short with a
/// test already named has two true facts and prefers the name; a run cut short by silence or by its
/// budget has only the ceiling, and the memory is the cause of the overrun rather than a second
/// symptom of it — reporting the stall instead would send the reader looking for a hang that is not
/// there. `verdict_settled` affects only the cleanup-failure diagnostic: pre-verdict reader and
/// observation failures must not claim that a verdict existed.
#[cfg_attr(coverage_nightly, coverage(off))]
fn cut_short(
    subtree: &mut impl ProcessCleanup,
    request: MemoryRequest,
    binary: &TestBinary,
    verdict_settled: bool,
) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
    cut_short_with_usage(subtree, request, binary, verdict_settled, ProcessCleanup::snapshot_usage)
}

fn cut_short_with_usage<T: ProcessCleanup>(
    subtree: &mut T,
    request: MemoryRequest,
    binary: &TestBinary,
    verdict_settled: bool,
    observe_usage: impl FnOnce(&T) -> MemoryUsage,
) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
    // Only Unix has a numeric watch slot that can be released too early. On Windows `released`
    // necessarily returns true because the job handle itself remains the authority over the child.
    #[cfg(unix)]
    debug_assert!(
        !subtree.leader_released(),
        "the subtree is signalled while it still holds its leader and its watch slot"
    );

    let stopped = subtree.terminate_tree();
    let usage = observe_usage(subtree);

    if let Err(cause) = stopped {
        let point = if verdict_settled {
            "after its verdict was settled"
        } else {
            "after execution began"
        };

        return Err((
            Verdict::Unmetered(format!(
                "`{}` and its descendants could not be terminated {point}; containment is unproven: {cause}",
                binary.path,
            )),
            usage,
        ));
    }

    let ceiling = request.limit.filter(|_limit| exhausted(&usage, false));

    Ok((usage, ceiling))
}

/// Whether a finished run should be read as having been stopped by its memory ceiling.
///
/// The platform's own report is the whole of the authority: on Linux an `oom` or `oom_kill` event
/// recorded against this invocation's cgroup, on Windows the job's accounting reaching the limit
/// set for it. A peak that merely touched the ceiling says nothing on its own, because reclaim may
/// have succeeded — a workload that filled the page cache, had it reclaimed, and then failed for a
/// reason of its own would be convicted of running out of memory it never ran out of, and the
/// reader would be sent to raise a ceiling that was never the problem.
///
/// A workload that succeeded is never convicted however close it came, since a suite that passed
/// detected nothing.
const fn exhausted(usage: &MemoryUsage, succeeded: bool) -> bool {
    !succeeded && usage.exhausted
}

/// How long to wait for the reader to finish once the child has exited.
///
/// Normally the pipe reaches end of file the instant the child does, and the text is already in
/// hand. A wait this long is only ever reached when something the test spawned outlived it and
/// still holds the write end, in which case the text will never arrive and the alternative to
/// giving up is hanging.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// What one observed run of a test binary produced.
#[derive(Debug)]
pub(super) struct Observation {
    pub(super) verdict: Verdict,

    /// Whether the active mutation guard was observed during this same subprocess.
    pub(super) reach: ReachObservation,

    /// Process termination and bounded output, retained only for a failed observation.
    pub(super) failure: Option<FailureEvidence>,

    /// The longest the harness went quiet, for calibrating later runs.
    pub(super) quiet: Duration,

    /// How many tests the harness announced, or `None` if it announced nothing.
    pub(super) tests: Option<usize>,

    /// The highest aggregate memory the subtree reached, when the run asked for a measurement and
    /// the platform could supply one.
    pub(super) peak: Option<u64>,
}

/// The directory a test binary is launched from.
///
/// `cargo test` runs each binary with the working directory set to its package root, and tests
/// rely on it: a fixture opened as `tests/data/input.json` resolves from there and nowhere else. In
/// a single-package workspace the two are the same directory, which is why running everything from
/// the workspace root works until the day someone adds a second crate — and then every test that
/// touches a file fails identically with and without a mutant active, so every mutant in that
/// package is scored as a survivor.
///
/// Falls back to the workspace root when cargo did not say where the manifest was, which is the
/// behaviour that was always there.
pub(super) fn working_directory<'work>(work: &'work Workspace, binary: &'work TestBinary) -> &'work Utf8Path {
    if binary.manifest_dir.as_str().is_empty() {
        &work.root
    } else {
        &binary.manifest_dir
    }
}

/// Runs one binary and reports what the harness said as well as how it ended.
pub(super) fn observe(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> Observation {
    observe_with(work, binary, attempt, false)
}

/// Runs one baseline binary while retaining bounded diagnostics if it fails.
pub(super) fn observe_baseline(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> Observation {
    observe_with(work, binary, attempt, true)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn observe_with(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, retain_failure: bool) -> Observation {
    let capture = ReachCapture::new(work, attempt);
    let progress = Arc::new(Mutex::new(Progress::new(watch(work))));
    let (verdict, usage, failure) = run_with_observation(
        work,
        binary,
        attempt,
        &progress,
        retain_failure,
        capture.as_ref().map(|capture| capture.path.as_path()),
        ask_after,
    );
    let reach = capture.map_or(ReachObservation::Unknown, ReachCapture::finish);
    let quiet = quiet_of(&progress);

    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    let tests = progress.lock().unwrap().tests;

    Observation {
        verdict,
        reach,
        failure,
        quiet,
        tests,
        peak: usage.peak,
    }
}

static NEXT_REACH_CAPTURE: AtomicUsize = AtomicUsize::new(0);

struct ReachCapture {
    path: Utf8PathBuf,
    ordinal: u32,
}

impl ReachCapture {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn new(work: &Workspace, attempt: Attempt<'_>) -> Option<Self> {
        Self::new_with_sequence(work, attempt, &NEXT_REACH_CAPTURE)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn new_with_sequence(work: &Workspace, attempt: Attempt<'_>, next: &AtomicUsize) -> Option<Self> {
        let ordinal = attempt.active?;
        let sequence = next.fetch_add(1, Ordering::Relaxed);
        let path = work.target.join(format!(".gamma-reach-{}-{sequence}.bin", std::process::id()));
        let _created = fs::create_dir_all(work.target.as_std_path());
        let _removed = fs::remove_file(path.as_std_path());

        Some(Self { path, ordinal })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn finish(self) -> ReachObservation {
        let bytes = fs::read(self.path.as_std_path());
        let _removed = fs::remove_file(self.path.as_std_path());
        let Ok(bytes) = bytes else {
            return ReachObservation::Unknown;
        };

        reach_from_records(&bytes, self.ordinal)
    }
}

/// Positive reach evidence wins; a negative proof requires complete, sealed records without overflow.
fn reach_from_records(bytes: &[u8], ordinal: u32) -> ReachObservation {
    let mut sealed = false;
    let mut reached = false;
    for record in bytes.chunks_exact(core::mem::size_of::<u32>()) {
        let value = u32::from_le_bytes(record.try_into().expect("chunks_exact yields exactly one runtime reach record"));
        if value == ordinal {
            reached = true;
        }
        if value == gamma_rt::OVERFLOW {
            sealed =
            // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
            false;
            break;
        }
        if value == gamma_rt::SEAL {
            if sealed {
                sealed =
                // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
                false;
                break;
            }
            sealed = true;
        } else if sealed {
            sealed =
            // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
            false;
            break;
        }
    }

    if reached {
        ReachObservation::Reached
    } else if sealed && bytes.len().is_multiple_of(core::mem::size_of::<u32>()) {
        ReachObservation::NotReached
    } else {
        ReachObservation::Unknown
    }
}

/// Whether this run's output can be trusted to announce a failure, and in whose format.
///
/// A mutant is usually caught by one test, and reading the announcement as it arrives ends the run
/// there instead of paying for every test after it. The saving is the whole tail of the binary.
///
/// It is only sound while the harness is the only thing writing to the stream being read. libtest
/// captures each test's output by default and replays it after every test has run, so a line
/// announcing a failure during the run is libtest's own. Under `--nocapture` or `--show-output`
/// that stops being true: a test's own writing lands among the harness's, and a test that prints
/// something shaped like a failure would convict a mutant the suite had not caught — inflating the
/// score, which is the worst direction to be wrong in. So the whole optimization is given up there.
fn watch(work: &Workspace) -> Watch {
    if work.runner().is_some() {
        return Watch::Nextest;
    }

    let interleaved = work
        .test_arguments()
        .iter()
        .any(|argument| argument == "--nocapture" || argument == "--show-output");

    // The environment variable is the same setting by another name, and it is inherited.
    if interleaved || std::env::var_os("RUST_TEST_NOCAPTURE").is_some() {
        return Watch::Off;
    }

    Watch::Libtest
}

/// How much of a deadline is left, saturating at zero once it has passed.
fn remaining(deadline: Instant) -> Duration {
    let now = Instant::now();
    #[cfg(test)]
    let now = REMAINING_NOW.get().unwrap_or(now);
    remaining_at(deadline, now)
}

#[cfg(test)]
thread_local! {
    static REMAINING_NOW: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

fn remaining_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

/// The longest the wait loop will sleep without being woken.
///
/// A backstop, not the mechanism. Everything the loop reacts to signals it — a child that exits
/// closes its pipes, and a harness that announces a failure is read from them — so this bounds only
/// the case where a signal was never sent at all, and the case the loop must poll for because
/// nothing can announce it: silence.
const WAIT_CAP: Duration = Duration::from_millis(50);

#[cfg(all(test, not(loom), not(miri)))]
mod pulse_wait_regression_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn recovered_collection_retains_exact_complete_authoritative_prefixes_and_tail_flags() {
        for bytes in [b"a\nb\npartial".as_slice(), b"partial".as_slice()] {
            for complete in [false, true] {
                for authoritative in [false, true] {
                    let (sender, receiver) = mpsc::channel();
                    sender
                        .send(Drained {
                            text: bytes.to_vec(),
                            tail: b"tail".to_vec(),
                            tail_truncated: false,
                            complete,
                            authoritative,
                            stream: ReaderStream::Stdout,
                        })
                        .unwrap();
                    drop(sender);
                    let collected = collected(&receiver, Duration::ZERO);
                    let expected = if !authoritative {
                        b"".as_slice()
                    } else if complete {
                        bytes
                    } else if bytes.starts_with(b"a\n") {
                        b"a\nb\n".as_slice()
                    } else {
                        b"".as_slice()
                    };
                    assert_eq!(collected.authoritative, expected);
                    assert_eq!(collected.text, bytes);
                    assert_eq!(collected.stdout_tail, b"tail");
                    assert!(collected.stderr_tail.is_empty());
                    assert_eq!(collected.whole, complete);
                    assert!(!collected.tail_truncated);
                }
            }
        }
    }

    #[test]
    fn recovered_output_cap_equality_is_a_whole_complete_line() {
        let pipe = io::repeat(b'x')
            .take(u64::try_from(OUTPUT_CAP - 1).unwrap())
            .chain(io::Cursor::new(b"\n"));
        let progress = Mutex::new(Progress::new(Watch::Off));
        let (kept, tail, truncated, whole) = drain(pipe, &progress, &Pulse::default(), false, false);
        assert_eq!(kept.len(), OUTPUT_CAP);
        assert_eq!(kept.last(), Some(&b'\n'));
        assert!(tail.is_empty());
        assert!(!truncated);
        assert!(whole);
    }

    #[test]
    fn oct25_collection_preserves_zero_grace_and_stops_on_the_first_timeout() {
        let receiver = ScriptedCollection {
            grace: Duration::ZERO,
            replies: core::cell::RefCell::new(VecDeque::from([Err(RecvTimeoutError::Timeout)])),
        };
        let collected = collected(&receiver, Duration::ZERO);
        assert!(receiver.replies.borrow().is_empty());
        assert!(collected.text.is_empty());
        assert!(collected.authoritative.is_empty());
        assert!(collected.stdout_tail.is_empty());
        assert!(collected.stderr_tail.is_empty());
        assert!(!collected.tail_truncated);
        assert!(!collected.whole);
    }

    #[test]
    fn oct26_collection_forwards_a_nonzero_bounded_grace_without_native_waiting() {
        let grace = Duration::from_millis(17);
        let receiver = ScriptedCollection {
            grace,
            replies: core::cell::RefCell::new(VecDeque::from([Err(RecvTimeoutError::Timeout)])),
        };
        let output = collected(&receiver, grace);
        assert!(receiver.replies.borrow().is_empty());
        assert_eq!(output.text, Vec::<u8>::new());
        assert_eq!(output.authoritative, Vec::<u8>::new());
        assert_eq!(output.stdout_tail, Vec::<u8>::new());
        assert_eq!(output.stderr_tail, Vec::<u8>::new());
        assert!(!output.tail_truncated);
        assert!(!output.whole);
    }

    #[test]
    fn recovered_startup_marker_overlap_keeps_every_byte_before_a_one_byte_suffix() {
        for marker in [gamma_rt::ENVIRONMENT_ERROR_MARKER, gamma_rt::PRE_INSTALL_ERROR_MARKER] {
            let prefix = OUTPUT_CAP + 1 - (marker.len() - 1);
            let pipe = io::repeat(b'x')
                .take(u64::try_from(prefix).unwrap())
                .chain(io::Cursor::new(marker))
                .chain(io::Cursor::new(b"\n"));
            let progress = Mutex::new(Progress::new(Watch::Off));
            let (_, _, _, whole) = drain(pipe, &progress, &Pulse::default(), false, false);
            assert!(!whole);
            assert!(environment_failure(&progress));
        }
    }

    #[test]
    fn recovered_diagnostic_markers_beyond_the_overlap_prefix_still_publish_startup_failure() {
        for marker in [gamma_rt::ENVIRONMENT_ERROR_MARKER, gamma_rt::PRE_INSTALL_ERROR_MARKER] {
            let bytes = [
                b"ordinary prefix that exceeds the marker overlap buffer: ".as_slice(),
                marker,
                b"\n",
            ]
            .concat();
            let progress = Mutex::new(Progress::new(Watch::Off));
            let pulse = Pulse::default();
            let (kept, _, _, whole) = drain(io::Cursor::new(&bytes), &progress, &pulse, false, false);
            assert_eq!(kept, bytes);
            assert!(whole);
            assert!(environment_failure(&progress));
            assert_eq!(pulse.seen(), 1);
        }
    }

    #[test]
    fn real_supervision_enters_the_actual_pulse_wait_before_rechecking_a_contained_child() {
        let (_directory, work) = crate::testing::helper_workspace("supervisor-pulse-wait-", &["exit:0"]);
        let binary = crate::testing::helper();
        let waits = Rc::new(Cell::new(0_usize));
        let observed_waits = Rc::clone(&waits);
        let mut observations = 0_usize;
        let attempt = Attempt {
            active: None,
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        let (verdict, _usage, evidence) = Pulse::with_wait_observer(
            move |pulse, _seen, upto| {
                observed_waits.set(observed_waits.get() + 1);
                assert!(upto <= WAIT_CAP, "the real wait must retain the supervision backstop");
                if observed_waits.get() == 1 {
                    pulse.signal();
                }
            },
            || {
                run_with_observation(
                    &work,
                    &binary,
                    attempt,
                    &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
                    false,
                    None,
                    |subtree| {
                        observations += 1;
                        if observations == 1 { Ok(None) } else { ask_after(subtree) }
                    },
                )
            },
        );
        assert_eq!(verdict, Verdict::Passed);
        assert!(evidence.is_none());
        assert!(observations >= 2, "the real contained child must be observed through completion");
        assert!(waits.get() >= 1, "every forced live observation must enter the actual Pulse::wait");
    }
}

/// Returns a live failure only when its announcement cannot precede a startup-error marker.
fn failure_to_cut_short(under_nextest: bool, progress: &Mutex<Progress>) -> Option<String> {
    (!under_nextest).then(|| announced_failure(progress)).flatten()
}

/// The first failure announcement, without treating it as a completed verdict.
fn announced_failure(progress: &Mutex<Progress>) -> Option<String> {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().failed.clone()
}

fn announced_failures(progress: &Mutex<Progress>) -> Vec<String> {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().failures.clone()
}

/// Whether the runtime has independently disqualified this test process as evidence.
fn environment_failure(progress: &Mutex<Progress>) -> bool {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().environment_error
}

fn environment_verdict(progress: &Mutex<Progress>) -> Option<Verdict> {
    environment_failure(progress).then(|| {
        Verdict::Unmetered(
            "the guard runtime could not acquire the process startup environment or install its startup selection".to_owned(),
        )
    })
}

fn unfinished_nextest_failure(under_nextest: bool, progress: &Mutex<Progress>) -> Option<Verdict> {
    (under_nextest && announced_failure(progress).is_some()).then(|| {
        Verdict::Unmetered(
            "`cargo nextest` announced a failure but did not finish before the run budget, \
             so its captured startup output could not be classified"
                .to_owned(),
        )
    })
}

/// The last test the harness named, if any.
fn last_test(progress: &Mutex<Progress>) -> Option<String> {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().test.clone()
}

/// The longest silence a binary produced, for calibrating later runs.
fn quiet_of(progress: &Mutex<Progress>) -> Duration {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    let progress = progress.lock().unwrap();

    progress.quiet.max(Instant::now().saturating_duration_since(progress.heard))
}

/// How much of a test binary's output is worth keeping.
///
/// Only the first failure is ever read out of it, and libtest prints that early. A binary
/// producing more than this is extremely chatty or looping, and buffering it all would turn one
/// runaway mutant into an out-of-memory kill of the whole run.
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

/// Which of a child's two piped streams a reader is draining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderStream {
    Stdout,
    Stderr,
}

impl fmt::Display for ReaderStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdout => f.write_str("standard output"),
            Self::Stderr => f.write_str("standard error"),
        }
    }
}

#[derive(Debug)]
struct ReaderStartError {
    stream: ReaderStream,
    cause: io::Error,
}

impl fmt::Display for ReaderStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} reader could not be started: {}", self.stream, self.cause)
    }
}

impl std::error::Error for ReaderStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Starts a reader for each of the child's piped streams, and yields what they collect.
///
/// A pipe must be drained by somebody other than the thread waiting for the child to exit. A pipe
/// holds about 64 KB; a test binary that prints more blocks forever in `write` while the waiting
/// thread sees a process that never finishes — turning a mutant that should time out into one
/// recorded as timed out for the wrong reason, or the baseline into a false ten-minute stall.
///
/// The readers hand their text back over a channel rather than through a join, because the child
/// exiting does not guarantee the pipes are closed: anything the test spawned inherited the write
/// ends, and a grandchild that outlives it holds them open. Every exit now sweeps the subtree
/// before draining, which closes those ends in the ordinary case — but a descendant that escaped
/// the containment entirely is still possible, so the wait stays bounded and the readers stay
/// abandonable rather than joined.
///
/// Two threads and two buffers per child, rather than one multiplexing supervisor. Substituting one
/// would mean redesigning the abandonable-reader and authoritative/diagnostic split above against a
/// ceiling nobody has measured, trading a known-correct shape for an unmeasured one. The targeted
/// process-launch measurement that would justify or rule it out — across quiet, chatty,
/// mixed-stream, capped, inherited-pipe, and fast-exit children — remains outstanding.
fn readers(
    subtree: &mut ProcessTree,
    progress: &Arc<Mutex<Progress>>,
    pulse: &Arc<Pulse>,
    under_nextest: bool,
    retain_tail: bool,
) -> Result<Receiver<Drained>, ReaderStartError> {
    let (sink, drained) = mpsc::channel::<Drained>();

    // Only the harness's authoritative stream may settle a verdict. The other stream still keeps
    // the stall clock alive and can carry the runtime's startup-error marker.
    let pipes = [
        subtree
            .take_stdout()
            .map(|pipe| (Either::Out(pipe), !under_nextest, ReaderStream::Stdout)),
        subtree
            .take_stderr()
            .map(|pipe| (Either::Err(pipe), under_nextest, ReaderStream::Stderr)),
    ];

    for (pipe, authoritative, stream) in pipes.into_iter().flatten() {
        let published = Arc::clone(progress);
        let pulse = Arc::clone(pulse);
        let sink = sink.clone();

        #[cfg(test)]
        let refused = faults::fired(Fault::Thread);
        #[cfg(not(test))]
        let refused = false;

        let spawned = if refused {
            Err(io::Error::other("the reader thread a test asked to fail"))
        } else {
            reader_thread().spawn(move || {
                READERS.started();

                let (text, tail, tail_truncated, complete) = match pipe {
                    Either::Out(pipe) => drain(pipe, &published, &pulse, authoritative, retain_tail),
                    Either::Err(pipe) => drain(pipe, &published, &pulse, authoritative, retain_tail),
                };

                let _sent = sink.send(Drained {
                    text,
                    tail,
                    tail_truncated,
                    complete,
                    authoritative,
                    stream,
                });

                // End of stream. A child that exits closes its pipes, so this is how the waiting thread
                // learns the run is over without having to ask again on a timer. It is sent even when
                // the stream ended for some other reason, because the waiter re-checks the child
                // itself and a spurious wakeup costs one `try_wait`.
                pulse.signal();

                // Reached whether this reader was waited for or abandoned — an abandoned one runs on
                // until its pipe finally closes, and this is where it stops being counted.
                READERS.finished();
            })
        };

        let _handle = spawned.map_err(|cause| ReaderStartError { stream, cause })?;
    }

    // Dropped so the receiver sees the readers' senders as the only ones left, and disconnects as
    // soon as they finish rather than waiting out the grace period on a sender nobody is using.
    drop(sink);

    Ok(drained)
}

fn reader_thread() -> thread::Builder {
    thread::Builder::new().name("cargo-gamma-output".to_owned())
}

/// One of the two streams a child may be read from, so both can be started by the same loop.
enum Either {
    Out(ChildStdout),
    Err(ChildStderr),
}

/// Reads a child's output to exhaustion, keeping at most [`OUTPUT_CAP`] of it.
///
/// Reading continues past the cap even though the excess is discarded: the point is to keep the
/// pipe empty so the child can run to completion, not to collect the text.
///
/// The final value says whether the stream really ended. A failed read leaves a prefix
/// of what the binary said, and a prefix reads exactly like the whole of a quiet suite: the
/// first-failure scan finds no announcement and the mutant is recorded as killed by a test nobody
/// can name. This loop is the only place that can still tell the two apart, so it says so, and the
/// caller reports the gap rather than presenting the shortfall as the binary having stayed silent.
///
/// An interrupted read is not truncation and does not reach the failing arm: `read_until` retries
/// `EINTR` itself, and nothing was taken out of the pipe when it fired.
fn drain<R: Read>(
    pipe: R,
    progress: &Mutex<Progress>,
    pulse: &Pulse,
    authoritative: bool,
    retain_tail: bool,
) -> (Vec<u8>, Vec<u8>, bool, bool) {
    let mut reader = BufReader::new(pipe);
    let mut kept = Vec::new();
    let mut tail = VecDeque::new();
    let mut tail_truncated = false;
    let mut line = Vec::new();
    let mut whole = true;
    let mut continuing = false;
    let marker_overlap_cap = gamma_rt::ENVIRONMENT_ERROR_MARKER
        .len()
        .max(gamma_rt::PRE_INSTALL_ERROR_MARKER.len())
        .saturating_sub(
            // #[gamma::skip(literal.int_decrement, reason = "Changing overlap retention max_marker_len-1 to max_marker_len adds one prior byte only. Every new marker crossing a fragment boundary requires at most len-1 previous bytes, so original already detects all crossings. Any extra wholly-prior marker was already reflected in the idempotent environment-error Boolean; no counter/wakeup changes.", tag = "equivalent")]
            1,
        );
    let mut marker_overlap = Vec::new();

    loop {
        match read_output_fragment(&mut reader, &mut line) {
            Ok(0) => return (kept, tail.into_iter().collect(), tail_truncated, whole),

            // The partial line this read was building goes with the bytes it lost: half a line is
            // not something the watcher should be shown, and it could as easily be half an
            // announcement as half a progress bar.
            Err(_truncated) => return (kept, tail.into_iter().collect(), tail_truncated, false),

            Ok(_read) => {
                let fragmented = continuing || line.len() > OUTPUT_CAP;
                continuing =
                // #[gamma::skip(logical.and_remove_left, reason = "drain read_output_fragment uses take(OUTPUT_CAP+1).read_until(newline). A nonempty fragment <=OUTPUT_CAP without newline can only be its terminal EOF remainder; otherwise it contains newline or is oversized. Removing fragmented&& only makes continuing true for that terminal remainder; the next read is EOF and returns before continuing is observed.", tag = "equivalent")]
                fragmented && line.last() != Some(&b'\n');

                // #[gamma::skip(assign.and_to_or, stmt.delete_assign, reason = "The whole &= !fragmented update is redundant with the later kept_now<line.len() => whole=false. First fragmented input exceeds OUTPUT_CAP, truncates, and sets false at the bottom. A continuing fragment inherits prior truncation; later nonempty lines have room=0 and reset false before EOF even if OR briefly sets true. Before truncation whole is true. Error returns false independently, so every return preserves the same whole value.", tag = "equivalent")]
                whole &= !fragmented;
                marker_overlap.extend_from_slice(&line[..line.len().min(marker_overlap_cap)]);
                let boundary_error = runtime_startup_failure(&marker_overlap);
                marker_overlap.clear();
                if
                // #[gamma::skip(cond.always_true, reason = "When continuing was false after a line read, the fragment either ended in newline or was the final short EOF fragment. Retaining its extra suffix cannot change a future marker match: EOF has no next bytes, and each runtime marker contains its only newline at its terminus. A match cannot extend across that already-retained newline; any complete marker wholly inside the old suffix was already observed in that line, and any marker wholly inside the next prefix is observed directly there. environment_error is monotone OR. continuing/fragmented/kept/tail/whole are otherwise untouched.", tag = "equivalent")]
                continuing {
                    marker_overlap.extend_from_slice(&line[line.len().saturating_sub(marker_overlap_cap)..]);
                }

                // Published before the text is kept, so a binary past the cap still counts as
                // making progress. Silence is the signal, not volume.
                #[expect(clippy::unwrap_used, reason = "the watcher only panics if the process is unwinding")]
                let decisive = {
                    let mut progress = progress.lock().unwrap();
                    let before = progress.failed.is_some();
                    let environment_error = progress.environment_error;

                    let line = String::from_utf8_lossy(&line);
                    progress.environment_error |= boundary_error;
                    if authoritative && !fragmented {
                        progress.heard(&line);
                    } else {
                        progress.heard_diagnostic(&line);
                    }

                    (!before && progress.failed.is_some()) || (!environment_error && progress.environment_error)
                };

                // Only evidence that can settle the run wakes the waiter. Waking on every line
                // would turn a chatty binary into a spin.
                if decisive {
                    pulse.signal();
                }

                let room = OUTPUT_CAP.saturating_sub(kept.len());

                let kept_now = line.len().min(room);
                kept.extend_from_slice(&line[..kept_now]);
                if retain_tail {
                    tail_truncated |= retain_output_tail(&mut tail, &line);
                }

                if kept_now < line.len() {
                    whole =
                    // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
                    false;
                }
            }
        }
    }
}

fn read_output_fragment(reader: &mut impl io::BufRead, line: &mut Vec<u8>) -> io::Result<usize> {
    line.clear();
    reader.take(OUTPUT_CAP as u64 + 1).read_until(b'\n', line)
}

/// Bytes retained from the end of each test-process stream for failure diagnostics.
const OUTPUT_TAIL_CAP: usize = 64 * 1024;

/// Lines retained from the byte-bounded tail for failure diagnostics.
///
/// A shared fixture failure can make every test report a short secondary
/// poison panic. Keep enough lines for the original panic to survive alongside
/// those follow-on failures; the 64 KiB byte cap remains the hard bound.
const OUTPUT_TAIL_LINES: usize = 2_000;

fn retain_output_tail(tail: &mut VecDeque<u8>, bytes: &[u8]) -> bool {
    if bytes.len() >= OUTPUT_TAIL_CAP {
        tail.clear();
        tail.extend(bytes[bytes.len() - OUTPUT_TAIL_CAP..].iter().copied());
        return true;
    }

    let excess = tail.len().saturating_add(bytes.len()).saturating_sub(OUTPUT_TAIL_CAP);
    if
    // #[gamma::skip(cond.always_true, relational.gt_to_ge, reason = "excess==0 makes drain(..excess) drain an empty range. Executing that branch at zero has no effect on the output buffer or progress state.", tag = "equivalent")]
    excess > 0 {
        tail.drain(..excess);
    }

    tail.extend(bytes.iter().copied());

    excess > 0
}

/// Extracts the name of the first failing test from libtest's output.
///
/// Borrowed from `output`, so that scanning costs nothing until a name is actually kept.
fn first_failure(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        let rest = trimmed.strip_prefix("test ")?;
        let name = rest.strip_suffix(" ... FAILED")?;

        Some(name.trim())
    })
}

/// Number of trailing lines retained from verbose nextest diagnostics.
///
/// This keeps the summary and nearby cause visible while bounding console noise. Enumeration and
/// runner failures deliberately use the same local-diagnostic policy.
pub(super) const DIAGNOSTIC_TAIL_LINES: usize = 20;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod final_survivor_tests {
    use std::ffi::OsStr;

    use super::ReachObservation::{NotReached, Reached, Unknown};
    use super::*;
    use crate::exec::resources::Resources;

    #[test]
    #[cfg(not(loom))]
    fn oct25_native_supervision_preserves_observations_and_waits_only_until_the_nearer_deadline() {
        #[derive(Default)]
        struct Process {
            released: bool,
            observations: usize,
            stops: Vec<(bool, bool, Option<u64>, Utf8PathBuf)>,
        }
        impl ContainedProcess for Process {
            fn released(&self) -> bool {
                self.released
            }
            fn usage(&self) -> MemoryUsage {
                MemoryUsage::default()
            }
            fn cut_short(
                &mut self,
                request: MemoryRequest,
                binary: &TestBinary,
                settled: bool,
            ) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
                self.stops.push((settled, request.meter, request.limit, binary.path.clone()));
                Ok((MemoryUsage::default(), Some(73)))
            }
        }
        struct ResetNow(Option<Instant>);
        impl Drop for ResetNow {
            fn drop(&mut self) {
                REMAINING_NOW.set(self.0);
            }
        }
        let mut process = Process::default();
        let (_sender, drained) = mpsc::channel();
        let pulse = Pulse::default();
        let binary = crate::testing::test_binary("never-launched-native-supervision");
        let mut observe = |process: &mut Process| {
            process.observations += 1;
            Ok((process.observations == 1).then(|| exit_status(17)))
        };
        let mut supervision = NativeSupervision {
            subtree: &mut process,
            drained: &drained,
            pulse: &pulse,
            binary: &binary,
            request: MemoryRequest {
                calibrating: false,
                meter: true,
                limit: Some(4096),
            },
            deadline: None,
            stall: Stall::NONE,
            observe_exit: &mut observe,
            observation_attempted: std::cell::Cell::new(false),
        };
        assert_eq!(supervision.observe().unwrap(), Some(exit_status(17)));
        assert_eq!(supervision.observe().unwrap(), None);
        assert_eq!(supervision.subtree.observations, 2);
        assert!(!supervision.released());
        supervision.subtree.released = true;
        assert!(supervision.released());
        for settled in [true, false] {
            assert_eq!(supervision.cut_short(settled).unwrap().1, Some(73));
        }
        assert_eq!(
            supervision.subtree.stops,
            [
                (true, true, Some(4096), binary.path.clone()),
                (false, true, Some(4096), binary.path.clone())
            ]
        );
        assert_eq!(supervision.seen(), 0);
        (0..3).for_each(|_| pulse.signal());
        assert_eq!(supervision.seen(), 3);
        let progress = Mutex::new(Progress::new(Watch::Off));
        assert_eq!(supervision.expiration(&progress), (false, false));
        let waits = std::rc::Rc::new(core::cell::Cell::new(0));
        Pulse::with_wait_observer(
            {
                let waits = std::rc::Rc::clone(&waits);
                move |pulse, seen, upto| {
                    assert_eq!(seen, 3);
                    assert_eq!(upto, WAIT_CAP);
                    waits.set(waits.get() + 1);
                    pulse.signal();
                }
            },
            || supervision.wait(3, &progress),
        );
        assert_eq!(waits.get(), 1);
        assert_eq!(pulse.seen(), 4);
        let now = Instant::now();
        let _reset = ResetNow(REMAINING_NOW.replace(Some(now)));
        supervision.deadline = Some(now + Duration::from_millis(5));
        Pulse::with_wait_observer(
            |pulse, seen, upto| {
                assert_eq!(seen, 4);
                assert_eq!(upto, Duration::from_millis(5));
                pulse.signal();
            },
            || supervision.wait(4, &progress),
        );
    }

    #[test]
    fn oct25_native_cleanup_adapter_preserves_whether_a_verdict_preceded_termination_failure() {
        struct Cleanup {
            stopped: bool,
        }

        impl ProcessCleanup for Cleanup {
            fn leader_released(&self) -> bool {
                false
            }

            fn snapshot_usage(&self) -> MemoryUsage {
                assert!(self.stopped, "the native cleanup request must precede the usage snapshot");
                MemoryUsage {
                    peak: Some(73),
                    exhausted: false,
                }
            }

            fn terminate_tree(&mut self) -> io::Result<()> {
                assert!(!self.stopped, "only one native cleanup request is expected");
                self.stopped = true;
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "owned termination refusal"))
            }
        }
        let binary = crate::testing::test_binary("never-launched-native-cleanup");
        for (settled, point) in [(false, "after execution began"), (true, "after its verdict was settled")] {
            let mut cleanup = Cleanup { stopped: false };
            let (verdict, usage) = ContainedProcess::cut_short(&mut cleanup, MemoryRequest::default(), &binary, settled).unwrap_err();
            let Verdict::Unmetered(message) = verdict else {
                panic!("a native cleanup failure must invalidate containment");
            };
            assert_eq!(
                message,
                format!(
                    "`{}` and its descendants could not be terminated {point}; containment is unproven: owned termination refusal",
                    binary.path
                )
            );
            assert_eq!(usage.peak, Some(73));
            assert!(!usage.exhausted);
            assert!(cleanup.stopped);
        }
    }

    #[test]
    fn oct26_contained_release_adapter_preserves_both_owned_cleanup_states() {
        struct Cleanup(bool);
        impl ProcessCleanup for Cleanup {
            fn leader_released(&self) -> bool {
                self.0
            }
            fn snapshot_usage(&self) -> MemoryUsage {
                panic!("release inspection must not inspect usage")
            }
            fn terminate_tree(&mut self) -> io::Result<()> {
                panic!("release inspection must not terminate the process")
            }
        }
        for released in [false, true] {
            assert_eq!(ContainedProcess::released(&Cleanup(released)), released);
        }
    }

    fn policy_workspace() -> Workspace {
        let root = Utf8PathBuf::from(if cfg!(windows) { "C:/verdict-policy" } else { "/verdict-policy" });
        let mut work = Workspace::adopt(root.clone(), root.join("target"));
        work.set_launch(Launch {
            loader: None,
            stack: "16777216".into(),
        });
        work
    }

    fn baseline_attempt() -> Attempt<'static> {
        Attempt {
            active: None,
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        }
    }

    #[test]
    fn a_kill_records_one_launch_without_an_unmutated_confirmation() {
        let control = tick::ClockControl::new();
        let clock = control.to_simple_clock();
        let mut verdicts = std::iter::once(Verdict::Failed(Some("killer".into())));
        super::super::sweep::begin_selection_trace();
        let events = super::super::sweep::capture_launch_costs_for_test(|| {
            let run = run_binary_with_observer(
                &clock,
                Attempt {
                    active: Some(37),
                    ..baseline_attempt()
                },
                |attempt| {
                    assert_eq!(attempt.active, Some(37));
                    assert!(matches!(attempt.only, Only::All));
                    control.advance_millis(7);
                    Observation {
                        verdict: verdicts.next().expect("a kill must execute exactly once"),
                        reach: ReachObservation::Reached,
                        failure: None,
                        quiet: Duration::ZERO,
                        tests: Some(1),
                        peak: None,
                    }
                },
            );
            assert!(matches!(run.verdict, Verdict::Failed(Some(ref name)) if name == "killer"));
            assert_eq!(run.reach, ReachObservation::Reached);
        });
        assert!(verdicts.next().is_none());
        assert_eq!(events.len(), 2);
        let exact = events
            .iter()
            .map(|(_tier, confirmation, cost)| {
                (
                    *confirmation,
                    cost.launches,
                    cost.execution_ms,
                    cost.hits,
                    cost.misses,
                    cost.inconclusive,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(exact, [(false, 1, 0, 0, 0, 0), (false, 0, 7, 0, 0, 0)]);
        let _costs = super::super::sweep::take_execution_costs();
        assert!(super::super::sweep::take_selection_trace().is_empty());
    }

    #[test]
    fn kill_evidence_never_launches_an_unmutated_control() {
        let control = tick::ClockControl::new();
        let clock = control.to_simple_clock();
        for active in [None, Some(37)] {
            for only in [Only::All, Only::One("hinted::killer")] {
                for test in [None, Some("reported::killer".to_owned())] {
                    let mut launches = 0;
                    let attempt = Attempt {
                        active,
                        only,
                        timeout: Some(Duration::from_secs(3)),
                        stall: Stall {
                            budget: Some(Duration::from_secs(1)),
                        },
                        request: MemoryRequest {
                            calibrating: false,
                            meter: true,
                            limit: Some(4096),
                        },
                    };
                    let result = run_binary_with_observer(&clock, attempt, |actual| {
                        launches += 1;
                        assert_eq!(launches, 1, "an assertion failure must not launch a control run");
                        assert_eq!(actual.active, active);
                        assert_eq!(actual.only, only);
                        assert_eq!(actual.timeout, attempt.timeout);
                        assert_eq!(actual.stall.budget, attempt.stall.budget);
                        assert_eq!(actual.request, attempt.request);
                        Observation {
                            verdict: Verdict::Failed(test.clone()),
                            reach: ReachObservation::Reached,
                            failure: None,
                            quiet: Duration::ZERO,
                            tests: Some(1),
                            peak: None,
                        }
                    });
                    assert_eq!(launches, 1);
                    assert_eq!(result.verdict, Verdict::Failed(test));
                    assert_eq!(result.reach, ReachObservation::Reached);
                    let _costs = super::super::sweep::take_execution_costs();
                }
            }
        }
    }

    #[test]
    fn telemetry_pre_spawn_refusal_is_reported_as_an_inconclusive_runner_launch_attempt() {
        let control = tick::ClockControl::new();
        let clock = control.to_simple_clock();
        super::super::sweep::begin_selection_trace();
        let run = run_binary_with_observer(
            &clock,
            Attempt {
                active: Some(37),
                ..baseline_attempt()
            },
            |_| {
                control.advance_millis(17);
                Observation {
                    verdict: Verdict::Unmetered("runner unavailable; no child spawned".into()),
                    reach: ReachObservation::Unknown,
                    failure: None,
                    quiet: Duration::ZERO,
                    tests: None,
                    peak: None,
                }
            },
        );
        assert!(matches!(run.verdict, Verdict::Unmetered(_)));
        let costs = super::super::sweep::take_execution_costs();
        let [(confirmation, cost)] = costs.as_slice() else {
            panic!("exactly one runner invocation was attempted")
        };
        assert!(!confirmation);
        assert_eq!((cost.launches, cost.hits, cost.misses, cost.inconclusive), (1, 0, 0, 1));
        assert_eq!(cost.execution_ms, 17);
        let mut json = serde_json::to_value(cost).unwrap();
        assert_eq!(json["launchAttempts"], 1);
        assert!(
            json.get("launches").is_none(),
            "runner invocations must never be labelled physical process launches"
        );
        let attempts = json.as_object_mut().unwrap().remove("launchAttempts").unwrap();
        json["launches"] = attempts;
        let legacy: super::super::session::AttemptCost = serde_json::from_value(json).unwrap();
        assert_eq!(&legacy, cost, "previously named attempt counters remain readable");
        assert!(super::super::sweep::take_selection_trace().is_empty());
    }

    #[test]
    fn telemetry_records_each_confirmation_observation_once_without_launching_processes() {
        for verdicts in [
            vec![Verdict::Failed(Some("killer".into()))],
            vec![Verdict::TimedOut, Verdict::TimedOut],
            vec![Verdict::TestEnumerationFailed("diagnostic".into()), Verdict::Passed],
            vec![Verdict::TimedOut, Verdict::Failed(Some("killer".into()))],
        ] {
            let expected = verdicts.len();
            let mut verdicts = verdicts.into_iter().enumerate();
            let control = tick::ClockControl::new();
            let clock = control.to_simple_clock();
            super::super::sweep::begin_selection_trace();
            let _run = run_binary_with_observer(
                &clock,
                Attempt {
                    active: Some(37),
                    ..baseline_attempt()
                },
                |_| {
                    let (index, verdict) = verdicts.next().expect("only the scripted observations may be executed");
                    control.advance(Duration::from_millis(u64::try_from(index + 1).unwrap() * 7));
                    Observation {
                        verdict,
                        reach: ReachObservation::Unknown,
                        failure: None,
                        quiet: Duration::ZERO,
                        tests: Some(1),
                        peak: None,
                    }
                },
            );
            assert!(verdicts.next().is_none());
            let costs = super::super::sweep::take_execution_costs();
            assert_eq!(costs.len(), expected);
            for (index, (_, cost)) in costs.iter().enumerate() {
                assert_eq!(cost.execution_ms, u64::try_from(index + 1).unwrap() * 7);
            }
            assert!(!costs[0].0, "only the first observation is a primary execution");
            assert!(costs[1..].iter().all(|(confirmation, cost)| *confirmation && cost.launches == 1));
            assert_eq!(costs.iter().map(|(_, cost)| cost.launches).sum::<usize>(), expected);
            assert!(super::super::sweep::take_selection_trace().is_empty());
        }
    }

    #[test]
    fn filtered_selection_refusal_is_inconclusive_before_any_process_start() {
        let mut work = policy_workspace();
        work.set_test_args(vec!["allowed".to_owned()]);
        let binary = crate::testing::test_binary("never-started");
        let (verdict, _usage, evidence) = run_with_observation(
            &work,
            &binary,
            Attempt {
                active: Some(7),
                only: Only::One("excluded"),
                ..baseline_attempt()
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Off))),
            false,
            None,
            |_subtree| panic!("an empty filter intersection must return before starting a process"),
        );
        assert!(
            matches!(verdict, Verdict::Unjudged(ref reason) if reason.contains("harness filters allow")),
            "{verdict:?}"
        );
        assert!(evidence.is_none());
    }

    #[test]
    fn repeated_observations_preserve_positive_reach_and_do_not_invent_negative_evidence() {
        for (first, second, expected) in [
            (Reached, NotReached, Reached),
            (Reached, Unknown, Reached),
            (NotReached, Unknown, NotReached),
            (Unknown, Unknown, Unknown),
            (Unknown, NotReached, NotReached),
        ] {
            let mut results = [(Verdict::TimedOut, first), (Verdict::Passed, second)].into_iter();
            let result = run_binary_with_observer(
                &SimpleClock::new_system(),
                Attempt {
                    active: Some(37),
                    timeout: Some(Duration::from_secs(1)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                },
                |_| {
                    let (verdict, reach) = results.next().expect("one initial observation and one timeout confirmation");
                    Observation {
                        verdict,
                        reach,
                        failure: None,
                        quiet: Duration::ZERO,
                        tests: Some(1),
                        peak: None,
                    }
                },
            );
            assert_eq!(result.verdict, Verdict::Passed);
            assert_eq!(result.reach, expected, "{first:?}/{second:?}");
            assert!(results.next().is_none());
        }
    }

    #[test]
    fn enumeration_suspicions_still_require_an_unmutated_check() {
        let mut calls = Vec::new();
        let result = run_binary_with_observer(
            &SimpleClock::new_system(),
            Attempt {
                active: Some(37),
                timeout: None,
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
            |attempt| {
                calls.push(attempt.active);
                Observation {
                    verdict: if attempt.active.is_some() {
                        Verdict::TestEnumerationFailed("original enumeration error".into())
                    } else {
                        Verdict::Passed
                    },
                    reach: ReachObservation::Unknown,
                    failure: None,
                    quiet: Duration::ZERO,
                    tests: None,
                    peak: None,
                }
            },
        );
        assert_eq!(calls, [Some(37), None]);
        assert_eq!(result.verdict, Verdict::TestEnumerationFailed("original enumeration error".into()));
    }

    #[test]
    fn native_spawn_retries_double_each_backoff_and_never_retry_a_permanent_refusal() {
        let (_directory, work) = crate::testing::helper_workspace("spawn-backoff-contract-", &["exit:0"]);
        let binary = crate::testing::helper();
        let command = launcher(&work, &binary, Only::All, false).unwrap();
        let refusals: Vec<_> = (0..3).map(|_| faults::arm(Fault::Spawn)).collect();
        let mut waits = Vec::new();
        let spawned = spawn_patiently_with_backoff(command, MemoryRequest::default(), |prepared, waited| {
            waits.push(waited);
            prepared.backoff(waited)
        });
        drop(refusals);
        let spawned = spawned.unwrap_or_else(|error| match error {
            StartError::Containment(reason) => panic!("the fourth helper spawn must establish native containment: {reason}"),
            StartError::Spawn(reason) => panic!("the fourth helper spawn must succeed after three transient refusals: {reason}"),
        });
        let mut subtree = ProcessTree::adopt(spawned).unwrap();
        subtree.terminate().unwrap();
        assert_eq!(
            waits,
            [Duration::from_millis(50), Duration::from_millis(100), Duration::from_millis(200)]
        );
        let missing = work.root.join("does-not-exist");
        let result = spawn_patiently_with_backoff(Command::new(missing), MemoryRequest::default(), |_, _| {
            panic!("a missing executable is a permanent refusal and must never enter backoff")
        });
        assert!(matches!(result, Err(StartError::Spawn(error)) if error.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn spawn_failures_name_a_memory_boundary_only_when_the_request_asked_for_one() {
        let binary = crate::testing::test_binary("selected-test");
        for meter in [false, true] {
            let cause = io::Error::from(io::ErrorKind::NotFound);
            let detail = cause.to_string();
            let request = MemoryRequest {
                calibrating: false,
                meter,
                limit: None,
            };
            let result = start_or_verdict_with(Command::new("not-started"), &binary, request, |_, actual| {
                assert_eq!(actual.meter, meter);
                Err(StartError::Spawn(cause))
            });
            let Err((Verdict::Unjudged(reason), usage)) = result else {
                panic!("a spawn error must remain an unjudged run");
            };
            let boundary = if meter { " inside its memory accounting boundary" } else { "" };
            assert_eq!(reason, format!("`selected-test` could not be started{boundary}: {detail}"));
            assert_eq!(usage.peak, None);
        }
    }

    #[test]
    fn cleanup_failures_preserve_whether_the_verdict_was_already_settled() {
        for settled in [false, true] {
            let (_directory, work) = crate::testing::helper_workspace("cleanup-diagnostic-contract-", &["sleep:30000"]);
            let binary = crate::testing::helper();
            let mut command = launcher(&work, &binary, Only::All, false).unwrap();
            configure(&mut command, &binary, work.launch(), None, None, None);
            let spawned = prepare(command, MemoryRequest::default())
                .unwrap()
                .spawn()
                .unwrap_or_else(|_| panic!("the owned helper process must spawn before its cleanup fault is armed"));
            let mut subtree = ProcessTree::adopt(spawned).unwrap();
            let _fault = process_faults::arm(ProcessFault::Terminate);
            let Err((Verdict::Unmetered(reason), _usage)) = cut_short(&mut subtree, MemoryRequest::default(), &binary, settled) else {
                panic!("a failed subtree termination must reject further uncontained execution");
            };
            let point = if settled {
                "after its verdict was settled"
            } else {
                "after execution began"
            };
            assert!(
                reason.starts_with(&format!(
                    "`{}` and its descendants could not be terminated {point}; containment is unproven: ",
                    binary.path
                )),
                "{reason}"
            );
        }
    }

    #[test]
    fn cutoff_retains_a_requested_ceiling_only_when_platform_accounting_reports_exhaustion() {
        for exhausted in [false, true] {
            for limit in [None, Some(4096)] {
                let (_directory, work) = crate::testing::helper_workspace("cutoff-accounting-contract-", &["sleep:30000"]);
                let binary = crate::testing::helper();
                let mut command = launcher(&work, &binary, Only::All, false).unwrap();
                configure(&mut command, &binary, work.launch(), None, None, None);
                let spawned = prepare(command, MemoryRequest::default())
                    .unwrap()
                    .spawn()
                    .unwrap_or_else(|_| panic!("the contained helper must spawn before accounting is observed"));
                let mut subtree = ProcessTree::adopt(spawned).unwrap();
                let (usage, ceiling) = cut_short_with_usage(
                    &mut subtree,
                    MemoryRequest {
                        calibrating: false,
                        meter: true,
                        limit,
                    },
                    &binary,
                    false,
                    |_| MemoryUsage {
                        peak: Some(123),
                        exhausted,
                    },
                )
                .unwrap();
                assert_eq!(usage.peak, Some(123));
                assert_eq!(usage.exhausted, exhausted);
                assert_eq!(ceiling, limit.filter(|_| exhausted));
            }
        }
    }

    fn recorded_failures(environment_error: bool) -> Arc<Mutex<Progress>> {
        let mut progress = Progress::new(Watch::Libtest);
        progress.heard("running 2 tests");
        progress.heard("test alpha ... FAILED");
        progress.heard("test beta ... FAILED");
        if environment_error {
            progress.heard(core::str::from_utf8(gamma_rt::ENVIRONMENT_ERROR_MARKER).unwrap());
        }
        Arc::new(Mutex::new(progress))
    }

    fn empty_collected() -> Collected {
        Collected {
            text: Vec::new(),
            authoritative: Vec::new(),
            stdout_tail: Vec::new(),
            stderr_tail: Vec::new(),
            tail_truncated: false,
            whole: true,
        }
    }

    struct ScriptedSupervision {
        observation: Option<io::Result<Option<ExitStatus>>>,
        expiration: (bool, bool),
        stopped: Option<bool>,
        collected: std::cell::Cell<bool>,
        stop_error: Option<String>,
    }

    impl ScriptedSupervision {
        fn new(observation: io::Result<Option<ExitStatus>>, expiration: (bool, bool)) -> Self {
            Self {
                observation: Some(observation),
                expiration,
                stopped: None,
                collected: std::cell::Cell::new(false),
                stop_error: None,
            }
        }
    }

    impl Supervision for ScriptedSupervision {
        fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
            self.observation.take().expect("only one completion observation is scripted")
        }
        fn released(&self) -> bool {
            assert!(
                self.observation.is_none(),
                "completion must be observed before release is inspected"
            );
            true
        }
        fn collect(&self) -> Collected {
            assert!(!self.collected.replace(true), "only one output collection is scripted");
            empty_collected()
        }
        fn usage(&self) -> MemoryUsage {
            assert!(self.collected.get(), "completed output is collected before usage");
            MemoryUsage::default()
        }
        fn cut_short(&mut self, settled: bool) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
            assert!(self.stopped.replace(settled).is_none(), "only one termination is scripted");
            if let Some(reason) = self.stop_error.take() {
                return Err((Verdict::Unmetered(reason), MemoryUsage::default()));
            }
            Ok((MemoryUsage::default(), None))
        }
        fn expiration(&self, _progress: &Mutex<Progress>) -> (bool, bool) {
            self.expiration
        }
        fn seen(&self) -> u64 {
            0
        }
        fn wait(&self, _seen: u64, _progress: &Mutex<Progress>) {
            panic!("the supplied terminal observation must settle without waiting")
        }
    }

    fn exit_status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt as _;
            ExitStatus::from_raw(u32::try_from(code).unwrap())
        }
    }

    #[test]
    fn recovered_completed_supervision_retains_active_nextest_enumeration_identity() {
        let binary = crate::testing::test_binary("never-launched-supervision");
        for active in [None, Some(37)] {
            for under_nextest in [false, true] {
                let progress = Mutex::new(Progress::new(Watch::Off));
                let mut supervision = ScriptedSupervision::new(Ok(Some(exit_status(nextest::TEST_LIST_CREATION_FAILED))), (false, false));
                let (verdict, usage, evidence) = supervise(
                    &mut supervision,
                    &binary,
                    active,
                    under_nextest,
                    MemoryRequest::default(),
                    &progress,
                    false,
                );
                match (under_nextest, active) {
                    (true, Some(_)) => {
                        let Verdict::TestEnumerationFailed(output) = verdict else {
                            panic!("an active nextest list failure must remain an enumeration failure: {verdict:?}");
                        };
                        assert_eq!(output, "");
                    }
                    (true, None) => {
                        let Verdict::Unmetered(reason) = verdict else {
                            panic!("a baseline list failure is infrastructure failure: {verdict:?}");
                        };
                        assert_eq!(
                            reason,
                            format!(
                                "`cargo nextest` exited with code {}, which does not describe a test run",
                                nextest::TEST_LIST_CREATION_FAILED
                            )
                        );
                    }
                    (false, _) => assert!(matches!(verdict, Verdict::Failed(None))),
                }
                assert_eq!(usage.peak, None);
                assert!(!usage.exhausted);
                assert!(evidence.is_none());
                assert!(supervision.collected.get());
                assert_eq!(supervision.stopped, None);
            }
        }
    }

    #[test]
    fn completed_and_running_startup_errors_keep_all_named_failures() {
        for completed in [false, true] {
            let binary = crate::testing::test_binary("startup-evidence");
            let progress = recorded_failures(true);
            let mut supervision = ScriptedSupervision::new(Ok(completed.then(|| exit_status(7))), (false, false));
            let (verdict, _usage, evidence) = supervise(&mut supervision, &binary, None, false, MemoryRequest::default(), &progress, true);
            let Verdict::Unmetered(reason) = verdict else {
                panic!("startup evidence must take precedence over a native exit or a pending process: {verdict:?}");
            };
            assert_eq!(
                reason,
                "the guard runtime could not acquire the process startup environment or install its startup selection"
            );
            let evidence = evidence.expect("requested startup diagnostics must retain their announced failures");
            assert_eq!(evidence.failed_tests, ["alpha", "beta"]);
            assert_eq!(evidence.termination, completed.then_some(Termination::ExitCode(7)));
            assert_eq!(supervision.stopped, (!completed).then_some(true));
            assert!(supervision.collected.get());
        }
    }

    #[test]
    fn supervision_errors_retain_failures_and_report_unproven_cleanup_at_the_right_stage() {
        for refuse_cleanup in [false, true] {
            let binary = crate::testing::test_binary("wait-evidence");
            let cleanup_error = "controlled cleanup refusal after execution began; containment is unproven: controlled failure";
            let mut supervision = ScriptedSupervision::new(Err(io::Error::other("controlled observation refusal")), (false, false));
            supervision.stop_error = refuse_cleanup.then(|| cleanup_error.to_owned());
            let (verdict, _usage, evidence) = supervise(
                &mut supervision,
                &binary,
                None,
                false,
                MemoryRequest::default(),
                &recorded_failures(false),
                true,
            );
            assert_eq!(
                verdict,
                if refuse_cleanup {
                    Verdict::Unmetered(cleanup_error.to_owned())
                } else {
                    Verdict::Unjudged(
                        "`wait-evidence` could not be asked whether it had finished: controlled observation refusal".to_owned(),
                    )
                }
            );
            let evidence = evidence.expect("supervision errors must retain requested failure evidence");
            assert_eq!(evidence.failed_tests, ["alpha", "beta"]);
            assert_eq!(evidence.termination, None);
            assert_eq!(supervision.stopped, Some(false));
            assert!(supervision.collected.get());
        }
    }

    #[test]
    fn wall_and_silence_deadlines_keep_distinct_verdicts_and_all_failure_names() {
        for stalled in [false, true] {
            let binary = crate::testing::test_binary("deadline-evidence");
            let mut supervision = ScriptedSupervision::new(Ok(None), (stalled, !stalled));
            let (verdict, _usage, evidence) = supervise(
                &mut supervision,
                &binary,
                None,
                false,
                MemoryRequest::default(),
                &recorded_failures(false),
                true,
            );
            if stalled {
                assert_eq!(verdict, Verdict::Stalled(Some("beta".to_owned())));
            } else {
                assert_eq!(verdict, Verdict::TimedOut);
            }
            let evidence = evidence.expect("deadline exits must preserve requested failure evidence");
            assert_eq!(evidence.failed_tests, ["alpha", "beta"]);
            assert_eq!(evidence.termination, None);
            assert_eq!(supervision.stopped, Some(true));
            assert!(supervision.collected.get());
        }
        let now = Instant::now();
        let later = now.checked_add(Duration::from_nanos(1)).unwrap();
        assert!(!deadline_reached(later, now));
        assert!(deadline_reached(now, now));
        assert!(deadline_reached(now, later));
    }

    #[test]
    fn launch_preparation_applies_loader_and_resource_policies_before_spawning() {
        let mut work = policy_workspace();
        let binary = crate::testing::test_binary("launch-preparation");
        work.set_launch(Launch {
            loader: Some("controlled-loader-path".into()),
            stack: "16777216".into(),
        });
        work.set_test_environment(&binary.package, "CARGO_GAMMA_CONTROLLED_ENVIRONMENT", "wired");
        work.set_harness_threads("4");
        for serialized in [false, true] {
            if serialized {
                work.set_resources(Resources::fake_binary(&binary));
            }
            let command = configured_launcher(&work, &binary, baseline_attempt()).unwrap();
            let actual: BTreeMap<_, _> = command.get_envs().collect();
            assert_eq!(
                actual.get(OsStr::new("CARGO_GAMMA_CONTROLLED_ENVIRONMENT")).copied().flatten(),
                Some(OsStr::new("wired"))
            );
            assert_eq!(
                actual.get(OsStr::new("RUST_TEST_THREADS")).copied().flatten(),
                if serialized {
                    Some(OsStr::new("1"))
                } else {
                    work.harness_threads().map(OsStr::new)
                }
            );
            let mut loader = Command::new(binary.path.as_std_path());
            configure_loader(&mut loader, work.launch());
            let loader_environment: Vec<_> = loader.get_envs().collect();
            assert!(
                !loader_environment.is_empty(),
                "the real workspace launch policy must contain a loader path"
            );
            for (key, value) in loader_environment {
                assert_eq!(actual.get(key).copied(), Some(value), "loader variable {key:?} was not applied");
            }
        }
    }

    fn reader_helper_process() -> ProcessTree {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let _ = command
            .args([
                "--exact",
                "exec::verdict::final_survivor_tests::reader_contract_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("CARGO_GAMMA_READER_CONTRACT_HELPER", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        let spawned = prepare(command, MemoryRequest::default())
            .unwrap()
            .spawn()
            .unwrap_or_else(|_| panic!("the owned reader helper must spawn"));
        ProcessTree::adopt(spawned).unwrap()
    }

    #[test]
    #[cfg(feature = "system-tests")]
    #[cfg_attr(miri, ignore = "native pipe acceptance launches a child and reads its operating-system pipes")]
    fn real_pipe_readers_preserve_stream_authority_tails_and_completion_wakeups() {
        for under_nextest in [false, true] {
            for retain_tail in [false, true] {
                let mut subtree = reader_helper_process();
                let progress = Arc::new(Mutex::new(Progress::new(Watch::Libtest)));
                let pulse = Arc::new(Pulse::default());
                let before = pulse.seen();
                let received = readers(&mut subtree, &progress, &pulse, under_nextest, retain_tail).unwrap();
                let output = collected(&received, Duration::from_secs(30));
                while ask_after(&mut subtree).unwrap().is_none() {
                    thread::yield_now();
                }
                assert!(output.whole);
                assert!(String::from_utf8_lossy(&output.text).contains("stdout-marker"));
                assert!(String::from_utf8_lossy(&output.text).contains("stderr-marker"));
                let (selected, rejected) = if under_nextest {
                    ("stderr_case", "stdout_case")
                } else {
                    ("stdout_case", "stderr_case")
                };
                assert_eq!(announced_failure(&progress).as_deref(), Some(selected));
                let authoritative = String::from_utf8_lossy(&output.authoritative);
                assert!(authoritative.contains(selected), "{authoritative}");
                assert!(!authoritative.contains(rejected), "{authoritative}");
                if retain_tail {
                    assert!(String::from_utf8_lossy(&output.stdout_tail).contains("stdout-marker"));
                    assert!(String::from_utf8_lossy(&output.stderr_tail).contains("stderr-marker"));
                } else {
                    assert!(output.stdout_tail.is_empty());
                    assert!(output.stderr_tail.is_empty());
                }
                assert_eq!(
                    pulse.seen() - before,
                    3,
                    "one decisive failure and two completed streams wake supervision"
                );
            }
        }
    }

    #[test]
    fn reader_threads_have_a_diagnostic_name_and_refusals_identify_the_first_stream() {
        assert_eq!(
            reader_thread()
                .spawn(|| thread::current().name().unwrap().to_owned())
                .unwrap()
                .join()
                .unwrap(),
            "cargo-gamma-output"
        );
        let mut subtree = reader_helper_process();
        let _fault = faults::arm(Fault::Thread);
        let error = readers(
            &mut subtree,
            &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
            &Arc::new(Pulse::default()),
            false,
            true,
        )
        .expect_err("the armed native reader-start fault must be returned");
        subtree.terminate().unwrap();
        assert!(matches!(error.stream, ReaderStream::Stdout));
        assert_eq!(error.cause.to_string(), "the reader thread a test asked to fail");
    }

    #[test]
    fn selected_resource_launches_strip_conflicting_thread_flags() {
        for arguments in [vec!["--test-threads=8"], vec!["--test-threads", "8"]] {
            let mut work = policy_workspace();
            let binary = crate::testing::test_binary("selected-resource");
            work.set_test_args(arguments.into_iter().map(str::to_owned).collect());
            work.set_resources(Resources::fake_binary(&binary));
            let command = launcher(&work, &binary, Only::One("chosen"), true).unwrap();
            let arguments: Vec<_> = command.get_args().map(|argument| argument.to_string_lossy().into_owned()).collect();
            assert_eq!(arguments, ["chosen", "--exact", "--test-threads", "1"]);
        }
    }

    #[test]
    fn nextest_launch_preparation_forwards_active_mutants_and_serializes_each_mutant() {
        for resource in [false, true] {
            let mut work = policy_workspace();
            let binary = crate::testing::test_binary("/t/deps/nxspike-abc");
            work.set_runner(nextest::Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]));
            if resource {
                work.set_resources(Resources::fake_binary(&binary));
            }
            for active in [None, Some(37)] {
                let command = configured_launcher(
                    &work,
                    &binary,
                    Attempt {
                        active,
                        ..baseline_attempt()
                    },
                )
                .unwrap();
                let arguments: Vec<_> = command.get_args().map(|argument| argument.to_string_lossy().into_owned()).collect();
                assert_eq!(
                    arguments.iter().any(|argument| argument == "--fail-fast"),
                    active.is_some(),
                    "{arguments:?}"
                );
                assert!(arguments.windows(2).any(|pair| pair == ["--test-threads", "1"]), "{arguments:?}");
            }
        }
    }

    #[test]
    fn named_failures_outrank_memory_ceiling_reports_and_direct_failures_are_not_partial_nextest_runs() {
        assert_eq!(
            prefer_named(Verdict::Failed(Some("named".to_owned())), Some(7), Some(5)),
            Verdict::Failed(Some("named".to_owned()))
        );
        let progress = recorded_failures(false);
        assert_eq!(unfinished_nextest_failure(false, &progress), None);
        assert!(matches!(unfinished_nextest_failure(true, &progress), Some(Verdict::Unmetered(_))));
    }

    #[test]
    fn failing_native_children_warn_only_when_their_output_was_actually_truncated() {
        for oversized in [false, true] {
            let amount = if oversized { OUTPUT_CAP + 1 } else { 16 };
            let flood = format!("flood:{amount}");
            let (_directory, work) = crate::testing::helper_workspace("partial-output-diagnostic-", &[&flood, "exit:1"]);
            let binary = crate::testing::helper();
            let notes = crate::notes::Run::new();
            let _scope = crate::notes::enter(Some(&notes));
            let (verdict, _usage, _failure) = run_with(
                &work,
                &binary,
                baseline_attempt(),
                &Arc::new(Mutex::new(Progress::new(Watch::Off))),
                false,
            );
            assert_eq!(verdict, Verdict::Failed(None));
            let messages = crate::notes::drain();
            let expected = format!(
                "the output of `{}` could not be read to the end, so the failing test may be named in text \
                 this run never saw. The exit status still decided the verdict.",
                binary.path
            );
            assert_eq!(messages, if oversized { vec![expected] } else { Vec::new() });
        }
    }

    #[test]
    fn an_unfinished_nextest_failure_is_not_reclassified_as_a_mutant_timeout() {
        let mut progress = Progress::new(Watch::Nextest);
        progress.heard("        FAIL [   0.024s] (1/1) helper named");
        let progress = Arc::new(Mutex::new(progress));
        assert_eq!(announced_failure(&progress).as_deref(), Some("named"));
        let observed = std::cell::Cell::new(0);
        let (verdict, _usage, evidence) = settle_budget_with(true, &progress, false, true, || {
            observed.set(observed.get() + 1);
            (
                MemoryUsage::default(),
                None,
                Collected {
                    text: Vec::new(),
                    authoritative: Vec::new(),
                    stdout_tail: Vec::new(),
                    stderr_tail: Vec::new(),
                    tail_truncated: false,
                    whole: true,
                },
            )
        });
        assert_eq!(observed.get(), 1);
        assert_eq!(
            verdict,
            Verdict::Unmetered(
                "`cargo nextest` announced a failure but did not finish before the run budget, so its captured startup output could not be classified"
                    .to_owned()
            )
        );
        assert_eq!(
            evidence
                .expect("an unfinished runner must preserve its announced failure")
                .failed_tests,
            ["named"]
        );
    }

    #[test]
    #[ignore = "subprocess fixture launched by the real-pipe reader regression"]
    fn reader_contract_helper() {
        assert_eq!(std::env::var("CARGO_GAMMA_READER_CONTRACT_HELPER").unwrap(), "1");
        println!("\ntest stdout_case ... FAILED\nstdout-marker");
        eprintln!("\nrunning 1 test\ntest stderr_case ... FAILED\nstderr-marker");
    }

    #[test]
    fn environment_markers_wake_the_drain_once_even_when_repeated_on_a_diagnostic_stream() {
        let marker = core::str::from_utf8(gamma_rt::ENVIRONMENT_ERROR_MARKER).unwrap();
        for authoritative in [false, true] {
            for text in [format!("{marker}\n"), format!("{marker}\n{marker}\n")] {
                let progress = Mutex::new(Progress::new(Watch::Libtest));
                let pulse = Pulse::default();
                let before = pulse.seen();
                let (kept, _, _, complete) = drain(text.as_bytes(), &progress, &pulse, authoritative, false);
                assert_eq!(kept, text.as_bytes());
                assert!(complete);
                let environment_error = progress
                    .lock()
                    .expect("this local mutex is used only by synchronous drain, which returned without unwinding while locked")
                    .environment_error;
                assert!(environment_error);
                assert_eq!(pulse.seen() - before, 1);
            }
        }
    }

    #[test]
    fn an_open_output_sender_is_partial_but_a_disconnected_complete_channel_is_not() {
        let (sender, receiver) = mpsc::channel();
        assert!(!collected(&receiver, Duration::ZERO).whole);
        drop(sender);
        assert!(collected(&receiver, Duration::ZERO).whole);
    }

    #[test]
    fn remaining_deadlines_are_positive_before_the_deadline_and_saturate_after_it() {
        let now = Instant::now();
        let future = now.checked_add(Duration::from_secs(30)).unwrap();
        assert_eq!(remaining_at(future, now), Duration::from_secs(30));
        assert_eq!(remaining_at(now, now), Duration::ZERO);
        assert_eq!(remaining_at(now, future), Duration::ZERO);
    }

    #[test]
    fn normal_binary_environments_keep_calibrated_threads_and_leave_an_empty_manifest_unset() {
        let mut work = policy_workspace();
        work.set_harness_threads("3");
        let binary = TestBinary {
            manifest_dir: Utf8PathBuf::new(),
            ..crate::testing::test_binary("environment-policy")
        };
        assert_eq!(
            baseline_environment(&work, &binary, Only::All)["set"][TEST_THREADS_VAR].as_str(),
            work.harness_threads()
        );
        let mut command = Command::new("not-started");
        let _inherited = command.env("CARGO_MANIFEST_DIR", "original-manifest");
        configure(&mut command, &binary, &Launch::derive(&[]), None, None, None);
        let value = command
            .get_envs()
            .find(|(key, _)| *key == "CARGO_MANIFEST_DIR")
            .and_then(|(_, value)| value);
        assert_eq!(value, Some(std::ffi::OsStr::new("original-manifest")));
    }

    #[test]
    fn inherited_nextest_and_nocapture_settings_are_checked_in_an_isolated_process() {
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "exec::verdict::final_survivor_tests::inherited_environment_helper",
                "--nocapture",
            ])
            .env("CARGO_GAMMA_ENVIRONMENT_CONTRACT_HELPER", "1")
            .env("NEXTEST", "1")
            .env("NEXTEST_CONTRACT_MARKER", "1")
            .env("CONTRACT_NEXTEST_", "1")
            .env("RUST_TEST_NOCAPTURE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated child process for inherited_environment_helper"]
    fn inherited_environment_helper() {
        assert_eq!(env::var("CARGO_GAMMA_ENVIRONMENT_CONTRACT_HELPER").as_deref(), Ok("1"));
        let (_directory, work) = crate::testing::helper_workspace("inherited-environment-contract-", &[]);
        assert!(matches!(watch(&work), Watch::Off));
        let mut command = Command::new("not-started");
        configure(&mut command, &crate::testing::helper(), &Launch::derive(&[]), None, None, None);
        let changes: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(changes.get(std::ffi::OsStr::new("NEXTEST")), Some(&None));
        assert_eq!(changes.get(std::ffi::OsStr::new("NEXTEST_CONTRACT_MARKER")), Some(&None));
        assert!(!changes.contains_key(std::ffi::OsStr::new("CONTRACT_NEXTEST_")));
    }

    #[test]
    fn collected_verdicts_keep_the_runner_selection_and_active_mutant_at_the_adapter_boundary() {
        let collected = Collected {
            text: b"  enumeration diagnostic \n".to_vec(),
            authoritative: b"test observed ... FAILED\n".to_vec(),
            stdout_tail: Vec::new(),
            stderr_tail: Vec::new(),
            tail_truncated: false,
            whole: true,
        };
        for (nextest, active, expected) in [
            (true, Some(37), Verdict::TestEnumerationFailed("enumeration diagnostic".into())),
            (false, Some(37), Verdict::Failed(Some("observed".into()))),
        ] {
            let (actual, retained) = settle_collected(
                nextest,
                active,
                Some(nextest::TEST_LIST_CREATION_FAILED),
                &collected,
                MemoryUsage {
                    peak: Some(19),
                    exhausted: true,
                },
            );
            assert_eq!(actual, expected);
            assert_eq!(retained.peak, Some(19));
            assert!(retained.exhausted);
        }
        let (actual, _) = settle_collected(
            true,
            None,
            Some(nextest::TEST_LIST_CREATION_FAILED),
            &collected,
            MemoryUsage::default(),
        );
        assert!(matches!(actual, Verdict::Unmetered(_)), "{actual:?}");
    }

    #[test]
    fn simultaneous_reach_captures_never_remove_each_others_records() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let attempt = Attempt {
            active: Some(37),
            timeout: None,
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        let first = ReachCapture::new(&work, attempt).expect("an active mutant run needs its own capture");
        fs::write(&first.path, 37_u32.to_le_bytes()).unwrap();
        let second = ReachCapture::new(&work, attempt).expect("a second in-flight run needs an independent capture");
        assert_ne!(first.path, second.path);
        assert_eq!(fs::read(&first.path).unwrap(), 37_u32.to_le_bytes());
        fs::write(&second.path, gamma_rt::SEAL.to_le_bytes()).unwrap();
        assert_eq!(first.finish(), ReachObservation::Reached);
        assert_eq!(second.finish(), ReachObservation::NotReached);
        assert!(ReachCapture::new(&work, Attempt { active: None, ..attempt }).is_none());
    }

    #[test]
    fn a_wrapping_reach_sequence_does_not_skip_into_the_next_live_capture_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let attempt = Attempt {
            active: Some(37),
            timeout: None,
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        let previous = AtomicUsize::new(1);
        let live = ReachCapture::new_with_sequence(&work, attempt, &previous).unwrap();
        fs::write(&live.path, 37_u32.to_le_bytes()).unwrap();
        let next = AtomicUsize::new(usize::MAX);
        let last = ReachCapture::new_with_sequence(&work, attempt, &next).unwrap();
        let wrapped = ReachCapture::new_with_sequence(&work, attempt, &next).unwrap();
        assert_ne!(wrapped.path, live.path);
        assert_ne!(last.path, wrapped.path);
        assert_eq!(live.finish(), ReachObservation::Reached);
        assert_eq!(last.finish(), ReachObservation::Unknown);
        assert_eq!(wrapped.finish(), ReachObservation::Unknown);
    }

    #[test]
    fn finite_output_drain_wakes_only_for_new_decisive_evidence_and_retains_the_requested_tail() {
        for authoritative in [false, true] {
            for retain_tail in [false, true] {
                for (text, decisive) in [
                    ("ordinary diagnostic\n", 0),
                    ("test named_failure ... FAILED\n", usize::from(authoritative)),
                    (
                        "test named_failure ... FAILED\ntest named_failure ... FAILED\n",
                        usize::from(authoritative),
                    ),
                ] {
                    let progress = Mutex::new(Progress::new(Watch::Libtest));
                    progress
                        .lock()
                        .expect("this newly created local progress mutex has not yet been locked")
                        .heard("running 1 test");
                    let pulse = Pulse::default();
                    let before = pulse.seen();
                    let (kept, tail, truncated, whole) =
                        drain(io::Cursor::new(text.as_bytes()), &progress, &pulse, authoritative, retain_tail);
                    assert_eq!(kept, text.as_bytes());
                    assert_eq!(tail, if retain_tail { text.as_bytes() } else { b"" });
                    assert!(!truncated);
                    assert!(whole);
                    assert_eq!(pulse.seen() - before, decisive as u64);
                    let failed = progress
                        .lock()
                        .expect("local heard and synchronous drain returned without unwinding while holding this mutex")
                        .failed
                        .is_some();
                    assert_eq!(failed, decisive != 0);
                }
            }
        }
    }

    #[test]
    fn failure_evidence_marks_each_independent_source_of_truncation() {
        for (whole, bytes_truncated, stdout_lines, stderr_lines) in [
            (true, false, 1, 1),
            (false, false, 1, 1),
            (true, true, 1, 1),
            (true, false, OUTPUT_TAIL_LINES + 1, 1),
            (true, false, 1, OUTPUT_TAIL_LINES + 1),
        ] {
            let collected = Collected {
                text: Vec::new(),
                authoritative: Vec::new(),
                stdout_tail: "out\n".repeat(stdout_lines).into_bytes(),
                stderr_tail: "err\n".repeat(stderr_lines).into_bytes(),
                tail_truncated: bytes_truncated,
                whole,
            };
            let evidence = collected.failure_evidence(None, vec!["named".into()]);
            assert_eq!(
                evidence.output_truncated,
                !whole || bytes_truncated || stdout_lines > OUTPUT_TAIL_LINES || stderr_lines > OUTPUT_TAIL_LINES
            );
            assert_eq!(evidence.stdout_tail, vec!["out"; stdout_lines.min(OUTPUT_TAIL_LINES)].join("\n"));
            assert_eq!(evidence.stderr_tail, vec!["err"; stderr_lines.min(OUTPUT_TAIL_LINES)].join("\n"));
            assert_eq!(evidence.failed_tests, ["named"]);
        }
    }

    #[test]
    fn output_tail_reports_the_first_excess_byte_not_exact_capacity() {
        let mut bytes = VecDeque::new();
        assert!(!retain_output_tail(&mut bytes, b"one"));
        assert!(!retain_output_tail(&mut bytes, &vec![b'x'; OUTPUT_TAIL_CAP - 3]));
        assert_eq!(bytes.len(), OUTPUT_TAIL_CAP);
        assert!(retain_output_tail(&mut bytes, b"y"));
        assert_eq!(bytes.len(), OUTPUT_TAIL_CAP);
        assert_eq!(bytes.front(), Some(&b'n'));
        assert_eq!(bytes.back(), Some(&b'y'));
        assert!(matches!(tail("a\nb\n", 2), Cow::Borrowed("a\nb")));
        assert_eq!(tail("a\r\nb\r\n", 2), "a\nb");
    }

    #[test]
    fn reach_capture_is_removed_for_valid_and_invalid_runtime_records() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        for (index, records, fragment, expected) in [
            (0, vec![7, gamma_rt::SEAL], false, ReachObservation::Reached),
            (1, vec![8, gamma_rt::SEAL], false, ReachObservation::NotReached),
            (2, vec![8], false, ReachObservation::Unknown),
            (3, vec![gamma_rt::SEAL, gamma_rt::SEAL], false, ReachObservation::Unknown),
            (4, vec![gamma_rt::SEAL, 8], false, ReachObservation::Unknown),
            (5, vec![gamma_rt::OVERFLOW], false, ReachObservation::Unknown),
            (6, vec![gamma_rt::SEAL], true, ReachObservation::Unknown),
        ] {
            let path = root.join(format!("capture-{index}"));
            let mut bytes = records.into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>();
            if fragment {
                bytes.push(1);
            }
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                ReachCapture {
                    path: path.clone(),
                    ordinal: 7
                }
                .finish(),
                expected
            );
            assert!(!path.exists());
        }
    }
}

/// Returns the last `count` lines without indexing the complete input.
pub(super) fn tail(text: &str, count: usize) -> Cow<'_, str> {
    if
    // #[gamma::skip(logical.or_remove_right, reason = "For empty text with count>0, stripping suffixes and iterating match_indices do nothing; text.get(0..) is empty and contains no CR, so normal path returns Cow::Borrowed(\"\") exactly as the removed early return. count0 keeps its original guard.", tag = "equivalent")]
    count == 0 || text.is_empty() {
        return Cow::Borrowed("");
    }

    let text = text.strip_suffix('\n').map_or(text, |without_newline| {
        without_newline.strip_suffix('\r').unwrap_or(without_newline)
    });
    let mut remaining = count;
    let mut start = 0;

    for (index, _) in text.match_indices('\n').rev() {
        if remaining == 1 {
            start = index + 1;
            break;
        }

        remaining -= 1;
    }

    let selected = text.get(start..).unwrap_or_default();

    if selected.contains('\r') {
        Cow::Owned(selected.lines().collect::<Vec<_>>().join("\n"))
    } else {
        Cow::Borrowed(selected)
    }
}

#[cfg(all(test, not(loom), not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod fuzz {
    use super::super::progress::{Progress, Watch};
    use super::first_failure;
    use crate::testing::{spliced, token};

    /// A libtest failure announcement is found whatever surrounds it.
    ///
    /// The asymmetry is why this is fuzzed rather than sampled. A misparse that invents a failure
    /// costs one survivor a second look and is visible in the report. A misparse that loses one
    /// reports a mutant as killed by a test that never failed, which raises the score and says
    /// nothing at all while doing it — so the direction worth spending machine time on is that no
    /// input, however strange the surrounding output, makes an announced failure disappear.
    #[test]
    fn a_libtest_failure_is_never_lost_among_arbitrary_output() {
        bolero::check!()
            .with_type::<(Vec<String>, String, usize)>()
            .for_each(|(noise, name, at)| {
                let output = spliced(noise, &format!("test {} ... FAILED", token(name)), *at);

                assert!(first_failure(&output).is_some(), "the failure was lost in {output:?}");
            });
    }

    /// Arbitrary output never panics the reader and never names an empty test.
    ///
    /// A test binary is an arbitrary program: it can print anything, including partial lines and
    /// invalid-looking announcements, and a panic here would take down the run that was measuring
    /// it rather than the mutant.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn arbitrary_output_is_read_without_panicking() {
        bolero::check!().with_type::<String>().for_each(|output| {
            if let Some(name) = first_failure(output) {
                assert!(!name.contains('\n'), "a test name spans lines: {name:?}");
            }
        });
    }

    /// The streaming reader agrees with the batch reader that a failure happened.
    ///
    /// These two read the same harness's output for the same fact, one line at a time as it
    /// arrives and one over the whole buffer at the end, and only the first can stop a binary
    /// early. If they drift apart, a mutant is convicted by one and acquitted by the other
    /// depending only on how the run happened to be watched, which is not a property of the code
    /// under test at all.
    ///
    /// Only the canonical spelling is asserted on. The two genuinely differ on an indented
    /// announcement — the batch reader trims the line first and the streaming one does not — and
    /// libtest does not indent, so pinning that difference would be pinning an accident.
    #[test]
    fn the_streaming_reader_sees_the_failure_the_batch_reader_sees() {
        bolero::check!()
            .with_type::<(Vec<String>, String, usize)>()
            .for_each(|(noise, name, at)| {
                let line = format!("test {} ... FAILED", token(name));
                let output = spliced(noise, &line, *at);

                let mut progress = Progress::new(Watch::Libtest);

                // The streaming reader ignores verdicts until a suite has announced itself, because a
                // `harness = false` target prints whatever it likes.
                progress.heard("running 1 test");

                for line in output.lines() {
                    progress.heard(line);
                }

                assert!(progress.failed.is_some(), "the streaming reader lost the failure in {output:?}");
            });
    }
}

#[cfg(all(test, not(loom), not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn recovered_reader_start_failure_stops_unsettled_and_preserves_termination_precedence_and_usage() {
        struct Process {
            binary: Utf8PathBuf,
            calls: usize,
            fails: bool,
        }
        impl ContainedProcess for Process {
            fn released(&self) -> bool {
                panic!("reader startup failure needs no release query")
            }
            fn usage(&self) -> MemoryUsage {
                panic!("termination supplies the final usage")
            }
            fn cut_short(
                &mut self,
                request: MemoryRequest,
                binary: &TestBinary,
                settled: bool,
            ) -> Result<(MemoryUsage, Option<u64>), (Verdict, MemoryUsage)> {
                self.calls += 1;
                assert_eq!((request.meter, request.limit, settled), (true, Some(41), false));
                assert_eq!(binary.path, self.binary);
                let usage = MemoryUsage {
                    peak: Some(19),
                    exhausted: false,
                };
                if self.fails {
                    Err((Verdict::Unmetered("named stop refusal".into()), usage))
                } else {
                    Ok((usage, Some(41)))
                }
            }
        }
        let binary = crate::testing::test_binary("never-launched-reader-failure");
        for fails in [false, true] {
            let mut process = Process {
                binary: binary.path.clone(),
                calls: 0,
                fails,
            };
            let (verdict, usage, evidence) = reader_failure(
                &mut process,
                MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: Some(41),
                },
                &binary,
                &ReaderStartError {
                    stream: ReaderStream::Stderr,
                    cause: io::Error::other("named reader startup refusal"),
                },
            );
            let expected = if fails {
                Verdict::Unmetered("named stop refusal".into())
            } else {
                Verdict::Unjudged(format!(
                    "`{}` output could not be supervised: the standard error reader could not be started: named reader startup refusal",
                    binary.path
                ))
            };
            assert_eq!(verdict, expected);
            assert_eq!((usage.peak, usage.exhausted), (Some(19), false));
            assert!(evidence.is_none());
            assert_eq!(process.calls, 1);
        }
    }

    #[test]
    fn recovered_native_remaining_time_uses_the_exact_injected_clock_before_at_and_after_the_deadline() {
        struct Reset(Option<Instant>);
        impl Drop for Reset {
            fn drop(&mut self) {
                REMAINING_NOW.set(self.0);
            }
        }
        let now = Instant::now();
        let _reset = Reset(REMAINING_NOW.replace(Some(now)));
        assert_eq!(remaining(now + Duration::from_millis(37)), Duration::from_millis(37));
        assert_eq!(remaining(now), Duration::ZERO);
        assert_eq!(remaining(now.checked_sub(Duration::from_millis(1)).unwrap()), Duration::ZERO);
    }

    #[test]
    fn recovered_named_resource_free_launcher_preserves_explicit_test_threads_and_other_flags() {
        let mut work = Workspace::adopt(
            "never-launched-resource-free-root".into(),
            "never-written-resource-free-target".into(),
        );
        work.set_test_args(vec!["--test-threads".into(), "7".into(), "chosen".into(), "--nocapture".into()]);
        let binary = crate::testing::test_binary("never-launched-resource-free-binary");
        let command = launcher(&work, &binary, Only::One("chosen_test"), false).unwrap();
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--test-threads", "7", "--nocapture", "chosen_test", "--exact"].map(std::ffi::OsStr::new)
        );
        assert_eq!(command.get_program(), binary.path.as_std_path());
    }

    #[test]
    fn recovered_reach_records_stop_at_the_first_invalid_seal_and_never_reseal_overflow() {
        for (records, expected) in [
            (vec![gamma_rt::SEAL], ReachObservation::NotReached),
            (vec![9, gamma_rt::SEAL], ReachObservation::Reached),
            (vec![gamma_rt::SEAL, gamma_rt::OVERFLOW], ReachObservation::Unknown),
            (vec![gamma_rt::SEAL, gamma_rt::SEAL, gamma_rt::SEAL], ReachObservation::Unknown),
            (vec![gamma_rt::SEAL, 3, gamma_rt::SEAL], ReachObservation::Unknown),
            (vec![gamma_rt::SEAL, 3, 9], ReachObservation::Unknown),
        ] {
            let bytes = records.iter().flat_map(|record| record.to_le_bytes()).collect::<Vec<_>>();
            assert_eq!(reach_from_records(&bytes, 9), expected, "{records:?}");
        }
        let mut partial = gamma_rt::SEAL.to_le_bytes().to_vec();
        partial.push(1);
        assert_eq!(reach_from_records(&partial, 9), ReachObservation::Unknown);
    }

    #[test]
    fn attempts_and_diagnostic_errors_preserve_their_user_visible_details() {
        let attempt = Attempt {
            active: Some(7),
            timeout: None,
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        assert_eq!(attempt.patient().timeout, None);

        assert_eq!(Termination::ExitCode(17).to_string(), "exit code 17");
        assert_eq!(Termination::Unknown.to_string(), "an unknown process termination");
        assert_eq!(ReaderStream::Stderr.to_string(), "standard error");

        let error = ReaderStartError {
            stream: ReaderStream::Stderr,
            cause: io::Error::other("reader refused"),
        };
        assert_eq!(
            std::error::Error::source(&error).map(ToString::to_string).as_deref(),
            Some("reader refused")
        );
    }

    #[test]
    fn a_runtime_environment_marker_has_a_standalone_verdict() {
        let progress = Mutex::new(Progress::new(Watch::Libtest));
        assert_eq!(environment_verdict(&progress), None);

        progress
            .lock()
            .expect("the progress lock is not poisoned")
            .heard(core::str::from_utf8(gamma_rt::ENVIRONMENT_ERROR_MARKER).expect("the marker is ASCII"));

        assert!(matches!(environment_verdict(&progress), Some(Verdict::Unmetered(reason)) if reason.contains("startup")));
    }

    fn reach_fixture(records: &[u32], ordinal: u32) -> ReachObservation {
        let directory = tempfile::tempdir().expect("the reach fixture directory is created");
        let path = Utf8PathBuf::from_path_buf(directory.path().join("reach.bin")).expect("the fixture path is UTF-8");
        let bytes: Vec<u8> = records.iter().flat_map(|record| record.to_le_bytes()).collect();
        fs::write(path.as_std_path(), bytes).expect("the reach fixture is written");

        ReachCapture { path, ordinal }.finish()
    }

    #[test]
    fn positive_reach_survives_an_incomplete_observation_but_negative_reach_does_not() {
        assert_eq!(reach_fixture(&[7], 7), ReachObservation::Reached);
        assert_eq!(reach_fixture(&[gamma_rt::SEAL], 7), ReachObservation::NotReached);
        assert_eq!(reach_fixture(&[], 7), ReachObservation::Unknown);
        assert_eq!(reach_fixture(&[gamma_rt::OVERFLOW, gamma_rt::SEAL], 7), ReachObservation::Unknown);
    }

    #[test]
    fn reach_hints_are_final_only_without_canonical_precedence_policies() {
        let base = Attempt {
            active: Some(1),
            timeout: None,
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        assert!(reach_hint_is_final(base));
        assert!(!reach_hint_is_final(Attempt {
            timeout: Some(Duration::from_secs(1)),
            ..base
        }));
        assert!(!reach_hint_is_final(Attempt {
            stall: Stall {
                budget: Some(Duration::from_secs(1)),
            },
            ..base
        }));
        assert!(!reach_hint_is_final(Attempt {
            request: MemoryRequest {
                calibrating: false,
                meter: true,
                limit: None
            },
            ..base
        }));
    }

    #[test]
    fn an_ordinary_mutant_run_harvests_reach_without_an_extra_launch() {
        let directive = format!("write-le:{}|7|{}", gamma_rt::REACH_VAR, gamma_rt::SEAL);
        let (_directory, work) = crate::testing::helper_workspace("reach-observation", &[&directive]);
        let binary = crate::testing::helper();

        let run = run_binary_observed(
            &work,
            &binary,
            Attempt {
                active: Some(7),
                timeout: None,
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        assert_eq!(run.verdict, Verdict::Passed);
        assert_eq!(run.reach, ReachObservation::Reached);
    }

    #[test]
    fn malformed_reach_streams_are_never_negative_evidence() {
        assert_eq!(reach_fixture(&[gamma_rt::SEAL, gamma_rt::SEAL], 7), ReachObservation::Unknown);
        assert_eq!(reach_fixture(&[gamma_rt::SEAL, 9], 7), ReachObservation::Unknown);
        assert_eq!(reach_fixture(&[9, gamma_rt::SEAL], 7), ReachObservation::NotReached);
        assert_eq!(reach_fixture(&[9, gamma_rt::OVERFLOW], 7), ReachObservation::Unknown);
    }

    #[test]
    fn collection_preserves_each_stream_and_every_completeness_flag() {
        let (sink, drained) = mpsc::channel();
        sink.send(Drained {
            text: b"stdout".to_vec(),
            tail: b"stdout-tail".to_vec(),
            tail_truncated: true,
            complete: true,
            authoritative: true,
            stream: ReaderStream::Stdout,
        })
        .expect("stdout record");
        sink.send(Drained {
            text: b"stderr".to_vec(),
            tail: b"stderr-tail".to_vec(),
            tail_truncated: false,
            complete: false,
            authoritative: false,
            stream: ReaderStream::Stderr,
        })
        .expect("stderr record");
        drop(sink);

        let collected = collected(&drained, Duration::from_secs(1));
        assert_eq!(collected.text, b"stdoutstderr");
        assert_eq!(collected.authoritative, b"stdout");
        assert_eq!(collected.stdout_tail, b"stdout-tail");
        assert_eq!(collected.stderr_tail, b"stderr-tail");
        assert!(collected.tail_truncated);
        assert!(!collected.whole);

        let evidence = collected.failure_evidence(Some(Termination::ExitCode(9)), Vec::new());
        assert_eq!(evidence.termination, Some(Termination::ExitCode(9)));
        assert!(evidence.output_truncated);
    }

    #[test]
    fn diagnostic_tail_has_an_exact_line_boundary() {
        let exact = "line\n".repeat(OUTPUT_TAIL_LINES);
        let expected = exact.strip_suffix('\n').expect("the fixture ends in a newline");
        let (text, truncated) = diagnostic_tail(exact.as_bytes());
        assert_eq!(text, expected);
        assert!(!truncated);

        let over = format!("discarded\n{exact}");
        let (text, truncated) = diagnostic_tail(over.as_bytes());
        assert_eq!(text, expected);
        assert!(truncated);
    }

    #[test]
    fn bounded_tail_retains_the_exact_suffix_and_reports_eviction() {
        let mut tail = VecDeque::from(vec![b'a'; OUTPUT_TAIL_CAP - 2]);
        assert!(!retain_output_tail(&mut tail, b"bc"));
        assert_eq!(tail.len(), OUTPUT_TAIL_CAP);
        assert_eq!(tail.back(), Some(&b'c'));

        assert!(retain_output_tail(&mut tail, b"def"));
        assert_eq!(tail.len(), OUTPUT_TAIL_CAP);
        assert_eq!(tail.iter().rev().take(3).copied().collect::<Vec<_>>(), b"fed");

        let replacement = vec![b'z'; OUTPUT_TAIL_CAP + 1];
        assert!(retain_output_tail(&mut tail, &replacement));
        assert_eq!(tail, VecDeque::from(vec![b'z'; OUTPUT_TAIL_CAP]));
    }

    #[test]
    fn nextest_failures_keep_their_classification_and_diagnostic() {
        let usage = MemoryUsage::default();
        let (verdict, _) = settle(true, Some(7), Some(nextest::NO_TESTS_RUN), b"ignored", b"", usage);
        assert!(matches!(verdict, Verdict::Unmetered(reason) if reason == "`cargo nextest` matched no tests for a binary this run built"));

        let reason = nextest_runner_failure(Some(17), "  useful detail  ");
        assert_eq!(
            reason,
            "`cargo nextest` exited with code 17, which does not describe a test run:\nuseful detail"
        );
        assert_eq!(
            nextest_runner_failure(None, ""),
            "`cargo nextest` exited with a signal, which does not describe a test run"
        );
    }

    #[test]
    fn baseline_and_binary_environments_record_every_controlled_value() {
        let (_directory, work) = crate::testing::helper_workspace("verdict-environment", &[]);
        work.calibrate_harness(1);
        let binary = crate::testing::helper();
        let environment = baseline_environment(&work, &binary, Only::All);
        let set = environment["set"].as_object().expect("the set is an object");

        assert_eq!(set[UNDER_GAMMA_VAR], "1");
        assert_eq!(set[INSTA_UPDATE_VAR], "no");
        assert_eq!(set[INSTA_FORCE_PASS_VAR], "0");
        assert_eq!(
            environment["removed"],
            serde_json::json!([gamma_rt::ACTIVE_VAR, gamma_rt::REACH_VAR, "NEXTEST", "NEXTEST_*"])
        );
        assert_eq!(environment["valuesOmitted"], serde_json::json!([STACK_VAR]));

        let binary = TestBinary {
            manifest_dir: Utf8PathBuf::from("manifest"),
            ..crate::testing::helper()
        };
        let launch = Launch::derive(&[]);
        let mut command = Command::new("test");
        configure(
            &mut command,
            &binary,
            &launch,
            Some("3"),
            Some(11),
            Some(Utf8Path::new("reach.bin")),
        );
        let env: BTreeMap<_, _> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        assert_eq!(env["RUST_BACKTRACE"].as_deref(), Some("0"));
        assert_eq!(env[UNDER_GAMMA_VAR].as_deref(), Some("1"));
        assert_eq!(env[INSTA_UPDATE_VAR].as_deref(), Some("no"));
        assert_eq!(env[INSTA_FORCE_PASS_VAR].as_deref(), Some("0"));
        assert_eq!(env[TEST_THREADS_VAR].as_deref(), Some("3"));
        assert_eq!(env[gamma_rt::ACTIVE_VAR].as_deref(), Some("11"));
        assert_eq!(env[gamma_rt::REACH_VAR].as_deref(), Some("reach.bin"));
        assert_eq!(env["CARGO_MANIFEST_DIR"].as_deref(), Some("manifest"));
    }

    #[test]
    fn baseline_environment_records_the_resource_thread_override() {
        let (_directory, mut work) = crate::testing::helper_workspace("verdict-resource-environment", &[]);
        work.calibrate_harness(8);
        let binary = crate::testing::helper();
        work.set_resources(super::super::resources::Resources::fake_binary(&binary));

        let environment = baseline_environment(&work, &binary, Only::All);

        assert_eq!(environment["set"][TEST_THREADS_VAR], "1");
    }

    #[test]
    fn output_reader_diagnostics_name_the_stream_and_cause() {
        assert_eq!(ReaderStream::Stdout.to_string(), "standard output");
        let error = ReaderStartError {
            stream: ReaderStream::Stdout,
            cause: io::Error::other("refused"),
        };
        assert_eq!(error.to_string(), "the standard output reader could not be started: refused");
    }

    #[test]
    fn suspicion_attempts_scale_only_the_budget_they_are_confirming() {
        let base = Attempt {
            active: Some(7),
            timeout: Some(Duration::from_secs(2)),
            stall: Stall {
                budget: Some(Duration::from_secs(3)),
            },
            request: MemoryRequest {
                calibrating: false,
                meter: true,
                limit: Some(100),
            },
            only: Only::All,
        };

        let patient = base.patient();
        assert_eq!(patient.timeout, Some(Duration::from_secs(2)));
        assert_eq!(patient.stall.budget, Some(Duration::from_secs(9)));
        assert_eq!(patient.active, Some(7));
        assert_eq!(patient.request.limit, Some(100));
    }

    #[test]
    fn progress_helpers_distinguish_nextest_partial_failure_and_last_test() {
        let progress = Mutex::new(Progress::new(Watch::Nextest));
        assert_eq!(unfinished_nextest_failure(false, &progress), None);
        assert_eq!(unfinished_nextest_failure(true, &progress), None);

        {
            let mut progress = progress.lock().expect("progress lock");
            progress.failed = Some("module::failed".to_owned());
            progress.test = Some("module::running".to_owned());
        }

        assert!(
            matches!(unfinished_nextest_failure(true, &progress), Some(Verdict::Unmetered(reason)) if reason.contains("did not finish"))
        );
        assert_eq!(last_test(&progress).as_deref(), Some("module::running"));
    }

    #[test]
    fn collection_timeout_is_partial_even_when_every_received_chunk_was_complete() {
        let chunk = Drained {
            text: b"prefix".to_vec(),
            tail: b"tail".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: true,
            stream: ReaderStream::Stdout,
        };
        let drained = ScriptedCollection {
            grace: Duration::from_millis(1),
            replies: core::cell::RefCell::new(VecDeque::from([Ok(chunk), Err(RecvTimeoutError::Timeout)])),
        };

        let collected = collected(&drained, Duration::from_millis(1));
        assert_eq!(collected.text, b"prefix");
        assert!(!collected.whole);
        assert!(drained.replies.borrow().is_empty());
    }

    /// A pipe that fails part way through is not reported as a stream that simply ended.
    ///
    /// The two are indistinguishable downstream: a prefix that stops before the harness announced
    /// anything looks exactly like a suite that failed without naming a test, and the verdict then
    /// carries no killer at all. The reader is the last place that can still tell them apart, so it
    /// is where the difference has to be recorded — and the bytes it did get are kept, because a
    /// truncated failure list is still worth showing.
    #[test]
    fn a_pipe_that_fails_part_way_is_not_read_as_the_end_of_the_stream() {
        /// Yields one line and then fails, the way a pipe whose writer died mid-stream does.
        struct Faltering(bool);

        impl Read for Faltering {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("the read a test asked to fail"));
                }

                self.0 = true;

                let line = b"running 1 test\n";

                buf[..line.len()].copy_from_slice(line);

                Ok(line.len())
            }
        }

        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let pulse = Pulse::default();

        let (kept, _tail, _tail_truncated, whole) = drain(Faltering(false), &progress, &pulse, true, false);

        assert!(!whole, "a severed pipe was reported as the end of the stream");
        assert_eq!(String::from_utf8_lossy(&kept), "running 1 test\n");
    }

    #[test]
    fn output_discarded_past_the_cap_marks_the_stream_incomplete() {
        let mut output = vec![b'x'; OUTPUT_CAP];
        output.extend_from_slice(b"\ntest meaningful::killer ... FAILED\n");
        output.extend_from_slice(gamma_rt::ENVIRONMENT_ERROR_MARKER);
        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let pulse = Pulse::default();

        let (kept, tail, tail_truncated, whole) = drain(io::Cursor::new(output), &progress, &pulse, true, true);

        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(!whole, "discarding bytes past the cap was reported as a whole stream");
        assert!(tail_truncated, "the bounded diagnostic tail was reported as complete");
        assert!(
            String::from_utf8_lossy(&tail).contains("test meaningful::killer ... FAILED"),
            "the diagnostic tail did not retain the evidence discarded from the parsing prefix"
        );
        assert!(environment_failure(&progress), "the marker remains visible outside the output cap");
    }

    #[test]
    fn unterminated_output_is_read_in_bounded_fragments_through_newline_and_eof() {
        for ending in [b"".as_slice(), b"\nfollowing line\n".as_slice()] {
            let bytes = OUTPUT_CAP * 3 + 17;
            let pipe = io::repeat(b'x').take(bytes as u64).chain(io::Cursor::new(ending));
            let mut reader = BufReader::new(pipe);
            let mut fragment = Vec::new();
            let mut observed = 0;
            let mut largest = 0;
            loop {
                let read = read_output_fragment(&mut reader, &mut fragment).unwrap();
                assert!(fragment.len() <= OUTPUT_CAP + 1);
                assert!(fragment.capacity() <= (OUTPUT_CAP + 1) * 2);
                largest = largest.max(fragment.len());
                observed += read;
                if read == 0 {
                    break;
                }
            }
            assert_eq!(largest, OUTPUT_CAP + 1);
            assert_eq!(observed, bytes + ending.len());
        }
    }

    #[test]
    fn overlong_line_fragments_cannot_forge_failures_and_later_lines_still_count() {
        let pipe = io::Cursor::new(b"running 1 test\n")
            .chain(io::repeat(b'x').take(OUTPUT_CAP as u64 + 1))
            .chain(io::Cursor::new(b"test forged::failure ... FAILED\ntest real::killer ... FAILED\n"));
        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let (kept, tail, truncated, whole) = drain(pipe, &progress, &Pulse::default(), true, true);
        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(tail.len() <= OUTPUT_TAIL_CAP);
        assert!(truncated);
        assert!(!whole);
        let progress = progress.lock().unwrap();
        assert_eq!(progress.failed.as_deref(), Some("real::killer"));
        assert_eq!(progress.failures, ["real::killer"]);
    }

    #[test]
    fn startup_markers_split_across_overlong_fragments_remain_diagnostic_evidence() {
        for marker in [gamma_rt::ENVIRONMENT_ERROR_MARKER, gamma_rt::PRE_INSTALL_ERROR_MARKER] {
            let pipe = io::repeat(b'x')
                .take((OUTPUT_CAP + 1 - marker.len() / 2) as u64)
                .chain(io::Cursor::new(marker))
                .chain(io::Cursor::new(b"\n"));
            let progress = Mutex::new(Progress::new(Watch::Libtest));
            let (_kept, _tail, _truncated, whole) = drain(pipe, &progress, &Pulse::default(), true, false);
            assert!(!whole);
            assert!(environment_failure(&progress));
            assert!(progress.lock().unwrap().failed.is_none());
        }
    }

    #[test]
    fn truncated_output_prefixes_never_supply_a_partial_final_failure_line() {
        for complete in [false, true] {
            let (sink, drained) = mpsc::channel();
            sink.send(Drained {
                text: b"running 1 test\ntest forged::failure ... FAILED".to_vec(),
                tail: Vec::new(),
                tail_truncated: false,
                complete,
                authoritative: true,
                stream: ReaderStream::Stdout,
            })
            .unwrap();
            drop(sink);
            let output = collected(&drained, Duration::from_secs(1));
            assert_eq!(
                first_failure(&String::from_utf8_lossy(&output.authoritative)),
                complete.then_some("forged::failure")
            );
            assert!(String::from_utf8_lossy(&output.text).contains("forged::failure"));
        }
    }

    #[test]
    fn diagnostic_line_limit_marks_output_truncated() {
        let output = format!("discarded\n{}", "kept\n".repeat(OUTPUT_TAIL_LINES));
        let collected = Collected {
            text: Vec::new(),
            authoritative: Vec::new(),
            stdout_tail: output.into_bytes(),
            stderr_tail: Vec::new(),
            tail_truncated: false,
            whole: true,
        };

        let evidence = collected.failure_evidence(None, Vec::new());

        assert!(evidence.output_truncated);
        assert!(!evidence.stdout_tail.contains("discarded"));
        assert!(evidence.stdout_tail.starts_with("kept\n"));
    }

    #[test]
    fn diagnostic_output_cannot_forge_a_libtest_failure() {
        let output = b"running 1 test\ntest forged::failure ... FAILED\n";
        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let pulse = Pulse::default();

        let (_kept, _tail, _tail_truncated, whole) = drain(io::Cursor::new(output), &progress, &pulse, false, false);

        assert!(whole);
        assert_eq!(announced_failure(&progress), None);
        assert_eq!(last_test(&progress), None);
    }

    /// A confirmation landing in a different detection class is itself confirmed.
    ///
    /// A lengthened re-run that hits the memory ceiling has been observed once, and one observation
    /// of a detection is exactly what the confirmation exists to refuse: a machine loaded by the
    /// sweep produces both classes on its own. Believing it credits the suite with a kill it never
    /// made, which is the one direction this tool must not be wrong in.
    #[test]
    fn a_confirmation_that_crosses_into_another_detection_class_is_confirmed_in_turn() {
        let mut issued = 0_usize;

        let verdict = settle_suspicions(attempt_for_confirmation(), |_attempt| {
            issued += 1;

            match issued {
                1 => Verdict::TimedOut,
                2 => Verdict::MemoryLimit { peak: Some(9), limit: 8 },
                _settled => Verdict::Passed,
            }
        });

        assert_eq!(issued, 3, "the memory verdict the confirmation produced was believed unchecked");
        assert_eq!(verdict, Verdict::Passed);
    }

    /// A confirmation that reproduces the class it was checking is the agreement it asked for.
    ///
    /// Without this the ladder would pay for a third run of every genuine timeout in the population,
    /// which is the most expensive verdict class there is.
    #[test]
    fn a_confirmation_that_agrees_ends_the_ladder() {
        let mut issued = 0_usize;

        let verdict = settle_suspicions(attempt_for_confirmation(), |_attempt| {
            issued += 1;

            Verdict::TimedOut
        });

        assert_eq!(issued, 2, "a reproduced timeout was asked about again");
        assert_eq!(verdict, Verdict::TimedOut);
    }

    /// Alternating classes stop at the bound rather than running forever.
    #[test]
    fn a_verdict_that_never_settles_stops_at_the_bound() {
        let mut issued = 0_usize;

        let _verdict = settle_suspicions(attempt_for_confirmation(), |_attempt| {
            issued += 1;

            if issued.is_multiple_of(2) {
                Verdict::MemoryLimit { peak: Some(9), limit: 8 }
            } else {
                Verdict::TimedOut
            }
        });

        assert_eq!(issued, 1 + CONFIRM_ROUNDS, "the ladder is not bounded");
    }

    /// An assertion failure needs no resource confirmation.
    #[test]
    fn a_settled_verdict_is_not_re_run() {
        let mut issued = 0_usize;

        let verdict = settle_suspicions(attempt_for_confirmation(), |_attempt| {
            issued += 1;

            Verdict::Failed(Some("a::b".to_owned()))
        });

        assert_eq!(issued, 1);
        assert_eq!(verdict, Verdict::Failed(Some("a::b".to_owned())));
    }

    /// The attempt the ladder tests are run against; none of them reads it.
    fn attempt_for_confirmation() -> Attempt<'static> {
        Attempt {
            active: Some(1),
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        }
    }

    /// A reader that never returns leaves the collection partial, exactly as a severed pipe does.
    ///
    /// A grandchild that escaped the containment inherited the write end of a pipe, so its reader
    /// is still blocked when the grace runs out and one whole stream is missing. Reading that as
    /// complete lets a run whose `FAIL` line was on the missing stream convict a mutant with no
    /// killer named, and suppresses the warning that would have said so.
    #[test]
    fn a_reader_that_never_returns_leaves_the_collection_partial() {
        let chunk = Drained {
            text: b"running 1 test\n".to_vec(),
            tail: b"running 1 test\n".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: true,
            stream: ReaderStream::Stdout,
        };
        let drained = ScriptedCollection {
            grace: Duration::from_millis(50),
            replies: core::cell::RefCell::new(VecDeque::from([Ok(chunk), Err(RecvTimeoutError::Timeout)])),
        };

        let Collected { text, whole, .. } = collected(&drained, Duration::from_millis(50));

        assert!(!whole, "an absent reader was read as the end of the output");
        assert_eq!(String::from_utf8_lossy(&text), "running 1 test\n");
        assert!(drained.replies.borrow().is_empty());
    }

    /// Every reader having sent and dropped its sender is the whole of the output, and the ordinary
    /// case must not be reported as partial or every run would warn about nothing.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn readers_that_all_returned_leave_the_collection_whole() {
        let (sink, drained) = mpsc::channel::<Drained>();

        let _sent = sink.send(Drained {
            text: b"running 1 test\n".to_vec(),
            tail: b"running 1 test\n".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: true,
            stream: ReaderStream::Stdout,
        });
        let _also = sink.send(Drained {
            text: b"test a::b ... FAILED\n".to_vec(),
            tail: b"test a::b ... FAILED\n".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: false,
            stream: ReaderStream::Stderr,
        });

        drop(sink);

        let Collected {
            text,
            authoritative,
            whole,
            ..
        } = collected(&drained, Duration::from_secs(30));

        assert!(whole, "a complete collection was reported as partial");
        assert_eq!(String::from_utf8_lossy(&authoritative), "running 1 test\n");
        assert!(
            String::from_utf8_lossy(&text).contains("FAILED"),
            "{:?}",
            String::from_utf8_lossy(&text)
        );
    }

    /// Final verdict parsing retains the same authoritative-stream boundary as live watching.
    #[test]
    fn diagnostic_output_cannot_forge_a_final_libtest_failure_name() {
        let (sink, drained) = mpsc::channel::<Drained>();
        let _diagnostic = sink.send(Drained {
            text: b"test forged::failure ... FAILED\n".to_vec(),
            tail: b"test forged::failure ... FAILED\n".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: false,
            stream: ReaderStream::Stderr,
        });
        let _authoritative = sink.send(Drained {
            text: b"running 1 test\ncustom harness failure\n".to_vec(),
            tail: b"running 1 test\ncustom harness failure\n".to_vec(),
            tail_truncated: false,
            complete: true,
            authoritative: true,
            stream: ReaderStream::Stdout,
        });
        drop(sink);

        let collected = collected(&drained, Duration::from_secs(30));
        let (verdict, _usage) = settle(
            false,
            Some(7),
            Some(101),
            &collected.text,
            &collected.authoritative,
            MemoryUsage::default(),
        );

        assert_eq!(verdict, Verdict::Failed(None));
    }

    /// A read interrupted by a signal is retried rather than treated as truncation.
    ///
    /// `EINTR` has taken nothing out of the pipe, so the next read gets it, and the retry comes
    /// from `read_until` rather than from this module. Pinned anyway because the truncation rule
    /// above it is what makes the distinction matter: were the interruption to start counting as a
    /// lost stream, every run the user so much as resizes a terminal during would report its
    /// verdicts as reached on partial text.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_read_interrupted_by_a_signal_is_retried_rather_than_cut_short() {
        /// Interrupts once between two lines, then ends the stream.
        struct Interrupted(u8);

        impl Read for Interrupted {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0 = self.0.saturating_add(1);

                let line: &[u8] = match self.0 {
                    1 => b"running 1 test\n",
                    2 => return Err(io::Error::from(io::ErrorKind::Interrupted)),
                    3 => b"test a::b ... FAILED\n",
                    _ => return Ok(0),
                };

                buf[..line.len()].copy_from_slice(line);

                Ok(line.len())
            }
        }

        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let pulse = Pulse::default();

        let (kept, _tail, _tail_truncated, whole) = drain(Interrupted(0), &progress, &pulse, true, false);

        assert!(whole, "an interrupted read was reported as truncation");
        assert!(
            String::from_utf8_lossy(&kept).contains("test a::b ... FAILED"),
            "the text after the interruption was lost: {:?}",
            String::from_utf8_lossy(&kept)
        );
    }

    /// A verdict reached on a truncated stream says so, since the shortfall is invisible in the
    /// verdict itself.
    ///
    /// A kill with no killer is exactly what a lost announcement looks like, so without this a
    /// reader would go looking through the suite for a failure that was never in the text they can
    /// see.
    #[test]
    fn a_verdict_reached_on_a_truncated_stream_says_the_text_was_partial() {
        crate::notes::alone(|| {
            let binary = crate::testing::helper();

            announce_partial(&binary);

            let raised = crate::notes::drain();

            assert_eq!(raised.len(), 1, "{raised:?}");
            assert!(raised[0].contains(binary.path.as_str()), "the binary is not named: {raised:?}");
        });
    }

    /// A workspace whose test binary is the portable helper, running the given script.
    ///
    /// Portable rather than a shell script because everything below is a statement about the
    /// verdict machinery rather than about Unix, and a fixture that only exists on one platform
    /// leaves the machinery unproven — and uncompiled — on the other.
    fn scripted(script: &[&str]) -> (tempfile::TempDir, Workspace) {
        crate::testing::helper_workspace("verdict", script)
    }

    /// A workspace whose test binaries are `/bin/sh`, running the given script.
    ///
    /// Kept for the one test that needs a survivor whose process id is written down for the test to
    /// check afterwards, which the portable helper deliberately does not offer.
    #[cfg(unix)]
    fn shell(body: &str) -> (tempfile::TempDir, Workspace) {
        crate::testing::shell_workspace("verdict", body)
    }

    /// A test binary exiting cleanly says nothing about what it left running.
    ///
    /// This is the guarantee the containment machinery exists for. A survivor holds ports and
    /// locks in the scratch tree, and the mutant that inherits them is convicted of a failure the
    /// mutant before it caused — which makes a score depend on the order the run happened to
    /// choose. It also holds the write end of a pipe this run is reading, stranding the reader.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_exits_normally_still_has_its_survivors_killed() {
        // Run under the watchdog: everything here waits on a child, and a child that never reports
        // would otherwise stop the suite instead of failing this test. The budget is not the
        // convergence deadline below — that one asks how long a `SIGKILL` may take to land, which
        // is a real question with a real answer; this one only asks whether anything is moving.
        let alive = crate::testing::within(crate::testing::WATCHDOG, "sweeping a leaked grandchild", || {
            let marker = std::env::temp_dir().join(format!("gamma-verdict-survivor.{}", std::process::id()));
            let _removed = std::fs::remove_file(&marker);
            let (_directory, work) = shell(&format!("sleep 300 & echo $! > {}; exit 0", marker.display()));
            let leaky = crate::testing::test_binary("/bin/sh");

            let verdict = run_with(
                &work,
                &leaky,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                },
                &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
                false,
            )
            .0;

            // The sweep is housekeeping, not judgement: the tests passed, so the mutant survived.
            assert!(matches!(verdict, Verdict::Passed), "{verdict:?}");

            let recorded = std::fs::read_to_string(&marker).expect("the fixture records its background child");
            let pid: i32 = recorded.trim().parse().expect("the fixture writes a pid");
            let _removed = std::fs::remove_file(&marker);

            // `SIGKILL` is delivered rather than awaited, so the process is gone shortly after
            // `run_with` returns rather than exactly when it does.
            let deadline = Instant::now() + Duration::from_secs(10);

            loop {
                if !cargo_gamma_unsafe::group::exists(pid) {
                    break false;
                }

                if Instant::now() >= deadline {
                    break true;
                }

                thread::sleep(Duration::from_millis(20));
            }
        });

        assert!(!alive, "the grandchild outlived the test binary that spawned it");
    }

    /// A binary that cannot be started said nothing about the mutant, whatever the run asked for.
    ///
    /// A spawn fails for reasons of the machine — descriptors, processes, address space — and none
    /// of them is a test failing. Reading one as a failing suite credits the tests with a kill they
    /// never made, and the confirmation pass does not shield it: a transient refusal clears on the
    /// second attempt and is then believed. The reason travels so the run can say what happened
    /// rather than reporting a mutant killed by a test it cannot name.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_cannot_be_spawned_is_unjudged_rather_than_a_kill() {
        let (_directory, work) = scripted(&["exit:0"]);
        let missing = crate::testing::test_binary(work.root.join("no-such-binary").as_str());

        // Asked for with no accounting at all, which is the state every host without a delegated
        // cgroup is in, and the state `--memory off` chooses.
        let verdict = run_binary(
            &work,
            &missing,
            Attempt {
                active: None,
                timeout: Some(Duration::from_secs(1)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: false,
                    limit: None,
                },
                only: Only::All,
            },
        );

        match verdict {
            Verdict::Unjudged(reason) => assert!(reason.contains("no-such-binary"), "{reason}"),
            other => panic!("expected an unjudged run, got {other:?}"),
        }
    }

    /// A binary that cannot be spawned inside a memory accounting boundary this run asked for is
    /// reported as unjudged, not as a plain test failure.
    ///
    /// A run that asked to be protected has to know the protection never took effect, or a mutant
    /// would be scored by a spawn failure that has nothing to do with anything it changed.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_cannot_be_spawned_inside_its_memory_boundary_is_unjudged() {
        if crate::testing::without_memory_support("a run reporting the memory a mutant used") {
            return;
        }

        let (_directory, work) = scripted(&["exit:0"]);
        let missing = crate::testing::test_binary(work.root.join("no-such-binary").as_str());

        let verdict = run_binary(
            &work,
            &missing,
            Attempt {
                active: None,
                timeout: Some(Duration::from_secs(1)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: None,
                },
                only: Only::All,
            },
        );

        assert!(matches!(verdict, Verdict::Unjudged(_)), "{verdict:?}");
    }

    /// A spawn the machine refuses for want of a resource is tried again, and the run goes ahead.
    ///
    /// A sweep runs `jobs` subprocesses at once, each with pipes of its own, so meeting a full
    /// descriptor or process table is an ordinary consequence of the workload and clears as soon as
    /// one of the other workers finishes. Giving up at the first refusal would leave a mutant
    /// unjudged for a shortage that lasted milliseconds.
    #[test]
    fn a_spawn_the_machine_refuses_is_tried_again() {
        let (_directory, work) = scripted(&["exit:0"]);
        let binary = crate::testing::helper();
        let _refused = faults::arm(Fault::Spawn);

        let verdict = run_binary(&work, &binary, plain_attempt());

        assert_eq!(verdict, Verdict::Passed, "the retry must let the run go ahead");
    }

    /// A machine that refuses every attempt leaves the one mutant unjudged, not the run abandoned.
    ///
    /// The distinction is the whole point of the separate verdict: an hours-long sweep that meets a
    /// persistent shortage on one mutant keeps every verdict it has already reached and records why
    /// that one has none, rather than discarding the lot.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_spawn_refused_every_time_leaves_the_mutant_unjudged() {
        let (_directory, work) = scripted(&["exit:0"]);
        let binary = crate::testing::helper();
        let _refusals: Vec<_> = (0..SPAWN_ATTEMPTS).map(|_round| faults::arm(Fault::Spawn)).collect();

        let verdict = run_binary(&work, &binary, plain_attempt());

        match verdict {
            Verdict::Unjudged(reason) => assert!(reason.contains("could not be started"), "{reason}"),
            other => panic!("expected an unjudged run, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_backoff_closes_the_interrupt_window_before_sleeping() {
        let (_directory, work) = scripted(&["exit:0"]);
        let binary = crate::testing::helper();
        let _refused = faults::arm(Fault::Spawn);
        let _interrupted = process_faults::arm_late(ProcessFault::Window, Duration::from_millis(10));
        let started = Instant::now();

        let verdict = run_binary(&work, &binary, plain_attempt());

        assert!(matches!(verdict, Verdict::Unmetered(_)), "{verdict:?}");
        assert!(started.elapsed() < SPAWN_BACKOFF.saturating_mul(2), "{:?}", started.elapsed());
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn reader_thread_creation_failure_leaves_the_attempt_unjudged() {
        let (_directory, work) = scripted(&["flood:5000000", "exit:1"]);
        let binary = crate::testing::helper();
        let _refused = faults::arm(Fault::Thread);

        let verdict = run_binary(&work, &binary, plain_attempt());

        match verdict {
            Verdict::Unjudged(reason) => assert!(reason.contains("output could not be supervised"), "{reason}"),
            other => panic!("expected an unjudged attempt, got {other:?}"),
        }
    }

    /// Only the refusals the machine recovers from on its own are waited out.
    ///
    /// A binary that is not there is not there however long the wait, and retrying it would multiply
    /// the delay on the one shape of failure that is certain to be permanent.
    #[test]
    fn only_a_transient_refusal_is_waited_out() {
        assert!(transient(&io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(transient(&io::Error::from(io::ErrorKind::Interrupted)));
        assert!(!transient(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!transient(&io::Error::from(io::ErrorKind::PermissionDenied)));

        #[cfg(unix)]
        {
            assert!(transient(&io::Error::from_raw_os_error(libc::EMFILE)));
            assert!(transient(&io::Error::from_raw_os_error(libc::ENFILE)));
            assert!(transient(&io::Error::from_raw_os_error(libc::ENOMEM)));
            assert!(!transient(&io::Error::from_raw_os_error(libc::ENOEXEC)));
        }
    }

    /// An attempt asking for nothing beyond running the binary, which the spawn tests all share.
    fn plain_attempt() -> Attempt<'static> {
        Attempt {
            active: None,
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest {
                calibrating: false,
                meter: false,
                limit: None,
            },
            only: Only::All,
        }
    }

    /// A host that cannot install the memory accounting a metered run asked for says so before
    /// anything is spawned, rather than running the binary unprotected.
    ///
    /// Spawning anyway would mean every verdict that follows is compared against a run the ceiling
    /// never actually watched, and a mutant that genuinely exhausted memory would simply be scored
    /// as an ordinary pass or failure with nothing left to explain why.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_undelegated_host_says_what_is_missing_before_anything_is_spawned() {
        if super::super::memory::support().is_ok() {
            return;
        }

        let (_directory, work) = scripted(&["exit:0"]);
        let ok = crate::testing::helper();

        let verdict = run_binary(
            &work,
            &ok,
            Attempt {
                active: None,
                timeout: Some(Duration::from_secs(1)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: None,
                },
                only: Only::All,
            },
        );

        match verdict {
            Verdict::Unmetered(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
            other => panic!("expected an unmetered refusal, got {other:?}"),
        }
    }

    /// A run whose containment cannot be installed says so, and never spawns the binary.
    ///
    /// The host here can install it perfectly well; the refusal is asked for through the fault seam,
    /// because the real causes — a cgroup controller that is not delegated, a job object the system
    /// will not make — need a differently configured machine rather than a differently written test.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn containment_that_cannot_be_installed_is_reported_rather_than_run_without() {
        let (_directory, work) = scripted(&["exit:0"]);
        let ok = crate::testing::helper();
        let _armed = process_faults::arm(ProcessFault::Prepare);

        let observed = observe(
            &work,
            &ok,
            Attempt {
                active: None,
                timeout: Some(Duration::from_secs(10)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        // Not `Passed`: the binary exits zero, so a run that spawned it anyway would say so.
        match observed.verdict {
            Verdict::Unmetered(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
            other => panic!("expected an unmetered refusal, got {other:?}"),
        }
    }

    /// A child that cannot be moved into its accounting boundary is killed, not left running.
    ///
    /// The window this covers is the one the containment exists to close. The child is already
    /// alive when the move is refused, so leaving it would mean a test binary running outside every
    /// bound the run believes it is inside — and, on Windows, one suspended forever waiting for a
    /// job it never entered. Nothing but a seam can ask for this: it needs a cgroup removed, or a
    /// job object refused, between the spawn and the move.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_child_that_cannot_be_adopted_is_ended_rather_than_left_outside_the_boundary() {
        crate::testing::within(crate::testing::WATCHDOG, "ending an unadoptable child", || {
            let started = crate::testing::workdir("verdict-adopt");
            let root = camino::Utf8PathBuf::from_path_buf(started.path().to_path_buf()).expect("a UTF-8 scratch path");
            let marker = root.join("ran");

            // Long enough that a child left alive would still be alive when this test looks, and
            // short enough that it cannot outlast the watchdog if the kill never happens.
            let (_directory, work) =
                crate::testing::helper_workspace("verdict-adopt", &["sleep:250", &format!("touch:{marker}"), "exit:0"]);
            let ok = crate::testing::helper();
            let _armed = process_faults::arm(ProcessFault::Adopt);

            let observed = observe(
                &work,
                &ok,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(10)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                },
            );

            match observed.verdict {
                Verdict::Unjudged(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
                other => panic!("expected an unjudged refusal, got {other:?}"),
            }

            // Well past the sleep the child would have finished, had it been left to run.
            thread::sleep(Duration::from_millis(1500));

            assert!(
                !marker.as_std_path().exists(),
                "a child that could not be adopted has to be ended, not left running outside the boundary"
            );
        });
    }

    /// A child this run cannot ask after is a mutant it cannot judge, not one the suite caught.
    ///
    /// `waitpid` fails only when the handle itself is gone, which for a child this process spawned
    /// and has not reaped no host will arrange. What matters is which way the run is wrong when it
    /// happens: a failing verdict here is read as a detection all the way out to the score, so the
    /// suite would be credited with a kill it never made on the strength of a broken wait.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_child_that_cannot_be_asked_after_is_unjudged_rather_than_a_failure() {
        crate::testing::within(crate::testing::WATCHDOG, "a wait that fails", || {
            let (_directory, work) = scripted(&["sleep:200", "exit:0"]);
            let ok = crate::testing::helper();
            let _armed = faults::arm(Fault::Wait);

            let observed = observe(
                &work,
                &ok,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(10)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                },
            );

            // Neither `Passed`, which is what the binary would otherwise have earned, nor a
            // failure, which is what a killed mutant looks like, nor a panic.
            match observed.verdict {
                Verdict::Unjudged(reason) => assert!(reason.contains("finished"), "{reason}"),
                other => panic!("expected an unjudged run, got {other:?}"),
            }
        });
    }

    /// A wait that fails still takes everything the child spawned with it.
    ///
    /// The arm has no verdict to report and nothing further to ask the child, and the temptation is
    /// to return from it — leaving a live process group holding scratch-tree locks, the write ends
    /// of the pipes this run's readers are blocked on, and whatever memory it was using, all of it
    /// into the next mutant's run. The fault is delayed rather than immediate so that the child has
    /// reached the point of having a descendant at all; without that this test would be watching an
    /// empty subtree and would pass whatever the arm did.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_wait_that_fails_still_takes_the_subtree_with_it() {
        crate::testing::within(crate::testing::WATCHDOG, "a wait that fails mid-run", || {
            let started = crate::testing::workdir("verdict-wait");
            let root = camino::Utf8PathBuf::from_path_buf(started.path().to_path_buf()).expect("a UTF-8 scratch path");
            let (running, survived) = (root.join("running"), root.join("survived"));

            // The descendant announces itself, waits out anything the kill has to do, and only then
            // writes the file this test is looking for. A subtree that was killed cannot reach the
            // second write; one that was merely abandoned does, and says so.
            let (_directory, work) = scripted(&[&format!("spawn:touch:{running}|sleep:2000|touch:{survived}"), "sleep:30000"]);
            let ok = crate::testing::helper();
            let _armed = faults::arm_late(Fault::Wait, Duration::from_millis(750));

            let observed = observe(
                &work,
                &ok,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                },
            );

            match observed.verdict {
                Verdict::Unjudged(reason) => assert!(!reason.is_empty(), "an unjudged run has to say why"),
                other => panic!("expected an unjudged run, got {other:?}"),
            }

            assert!(
                running.as_std_path().exists(),
                "the descendant never started, so this test proves nothing about killing it"
            );

            // Past the descendant's sleep, so a survivor has had every chance to write.
            thread::sleep(Duration::from_millis(2500));

            assert!(
                !survived.as_std_path().exists(),
                "a wait that failed has to take the whole subtree with it, not just the child"
            );
        });
    }

    #[test]
    fn an_unrepresentable_deadline_is_clamped_without_panicking() {
        let before = Instant::now();
        let deadline = deadline_after(Duration::MAX);
        let latest = Instant::now()
            .checked_add(Duration::from_secs(crate::bounds::MOST_SECONDS))
            .expect("one year is a representable deadline");

        assert!(deadline >= before);
        assert!(deadline <= latest);
    }

    /// A binary that outlives its budget is timed out, and the verdict survives confirmation.
    #[test]
    fn a_binary_that_outlives_its_budget_times_out() {
        let (_directory, work) = scripted(&["sleep:30000"]);
        let sleeper = crate::testing::helper();

        // A genuine hang has to survive the confirmation run as well as the first one, so this
        // exercises both passes: the suspicion and the finding.
        assert_eq!(
            run_binary(
                &work,
                &sleeper,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_millis(50)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::TimedOut
        );
    }

    #[test]
    fn a_termination_failure_abandons_the_run() {
        let (_directory, work) = scripted(&["sleep:30000"]);
        let sleeper = crate::testing::helper();
        let _failed = process_faults::arm(ProcessFault::Terminate);

        let (verdict, _usage, _failure) = run_with(
            &work,
            &sleeper,
            Attempt {
                active: Some(1),
                timeout: Some(Duration::from_millis(50)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: false,
                    limit: None,
                },
                only: Only::All,
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Off))),
            false,
        );

        assert!(
            matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("could not be terminated")),
            "{verdict:?}"
        );
    }

    /// A binary that goes quiet for longer than its stall budget is cut off early.
    #[test]
    fn a_binary_that_goes_quiet_is_stalled_at_the_last_test_it_named() {
        let (_directory, work) = scripted(&["print:test slow::case ... ", "sleep:30000"]);
        let hanger = crate::testing::helper();

        // The point of stall detection is to cut a hang off long before the full budget, and to
        // say which test was running when the silence began.
        let verdict = run_binary(
            &work,
            &hanger,
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(1)),
                stall: Stall {
                    budget: Some(Duration::from_millis(50)),
                },
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        assert!(matches!(verdict, Verdict::Stalled(_)), "{verdict:?}");
    }

    #[test]
    fn startup_silence_measured_by_the_baseline_does_not_stall_the_same_binary() {
        let (_directory, work) =
            crate::testing::helper_workspace("verdict-startup-silence-", &["sleep:100", "print:running 0 tests", "exit:0"]);
        let binary = crate::testing::helper();
        let baseline = observe(
            &work,
            &binary,
            Attempt {
                active: None,
                timeout: Some(Duration::from_secs(5)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        let measured = baseline.quiet;
        let observed = observe(
            &work,
            &binary,
            Attempt {
                active: Some(1),
                timeout: Some(Duration::from_secs(5)),
                stall: Stall {
                    budget: Some(measured.saturating_mul(4)),
                },
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        assert!(measured >= Duration::from_millis(80), "{measured:?}");
        assert_eq!(observed.verdict, Verdict::Passed);
    }

    /// A binary runs from its own package's root, the way cargo would run it.
    #[test]
    fn a_binary_runs_from_its_package_root() {
        // A test that opens a fixture by relative path only finds it from the package root. Run
        // from the workspace root it fails identically with and without a mutant active, so every
        // mutant in the package is scored as a survivor.
        let (_directory, work) = scripted(&["require-file:marker.txt"]);
        let package = work.root.join("crates").join("subject");

        std::fs::create_dir_all(package.as_std_path()).expect("the package directory is created");
        std::fs::write(package.join("marker.txt").as_std_path(), "here").expect("the marker is written");

        let binary = TestBinary {
            manifest_dir: package,
            ..crate::testing::helper()
        };

        assert_eq!(
            run_binary(
                &work,
                &binary,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// A binary that exits cleanly passes, and its output is read back.
    #[test]
    fn a_binary_that_exits_cleanly_passes() {
        let (_directory, work) = scripted(&["print:test a::b ... ok", "exit:0"]);
        let ok = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &ok,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// A mutant caught by an early test is convicted there, not after every later test has run.
    ///
    /// This is the whole value of watching the output: the binary below would take half a minute to
    /// finish, and the verdict is settled a fraction of a second in.
    #[test]
    fn a_failure_convicts_a_mutant_without_waiting_for_the_rest_of_the_binary() {
        let (_directory, work) = scripted(&["print:running 2 tests", "print:test a::b ... FAILED", "sleep:30000", "exit:101"]);
        let binary = crate::testing::helper();
        let started = Instant::now();

        let verdict = run_binary(
            &work,
            &binary,
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(2)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );
        let took = started.elapsed();

        assert_eq!(verdict, Verdict::Failed(Some("a::b".to_owned())));
        assert!(took < Duration::from_secs(15), "the run waited for the whole binary: {took:?}");
    }

    /// A baseline waits for libtest's trailing diagnostics and retains every announced failure.
    #[test]
    fn a_baseline_failure_waits_for_complete_evidence() {
        let (_directory, work) = scripted(&[
            "print:running 3 tests",
            "print:test a::b ... FAILED",
            "print:test a::later ... FAILED",
            "print:failure detail",
            "exit:101",
        ]);
        let binary = crate::testing::helper();

        let observed = observe_baseline(
            &work,
            &binary,
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(2)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        assert_eq!(observed.verdict, Verdict::Failed(Some("a::b".to_owned())));
        let evidence = observed.failure.expect("a failed baseline retains evidence");
        assert_eq!(evidence.failed_tests, ["a::b", "a::later"]);
        assert!(evidence.stdout_tail.contains("failure detail"), "{evidence:?}");
    }

    /// The name reported is the one the run would have reported had it read the binary to the end.
    #[test]
    fn the_test_named_is_the_first_that_failed_and_not_a_later_one() {
        let (_directory, work) = scripted(&[
            "print:running 3 tests",
            "print:test a::first ... FAILED",
            "print:test a::second ... FAILED",
            "exit:101",
        ]);
        let binary = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &binary,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Failed(Some("a::first".to_owned()))
        );
    }

    /// A target built with `harness = false` prints whatever it likes, and a line of its own that
    /// happens to look like libtest's must not be read as a verdict it never gave.
    #[test]
    fn a_harness_that_announced_no_suite_is_judged_only_by_how_it_exits() {
        let (_directory, work) = scripted(&["print:test a::b ... FAILED", "exit:0"]);
        let binary = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &binary,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// With capture turned off, a test's own writing reaches the same stream as the harness's, and
    /// a test that prints something shaped like a failure would otherwise convict a mutant the
    /// suite had not caught — reporting it caught and inflating the score.
    #[test]
    fn a_run_with_capture_turned_off_takes_no_verdict_from_the_output() {
        let (_directory, work) = scripted(&["--nocapture", "print:running 1 test", "print:test a::b ... FAILED", "exit:0"]);
        let binary = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &binary,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// Insta is told not to write snapshots and not to pass on a mismatch, whatever the ambient
    /// environment asked for.
    ///
    /// Both are inherited, and a developer part-way through `cargo insta review` has them set. A
    /// snapshot accepted under a mutant lets the mutant pass and be reported missed, and the
    /// scratch tree serving every later mutant would carry the accepted snapshot with it.
    #[test]
    fn insta_is_told_not_to_accept_what_a_mutant_produced() {
        let (_directory, work) = scripted(&["write-env:seen.txt|INSTA_UPDATE|INSTA_FORCE_PASS", "exit:0"]);
        let binary = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &binary,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );

        let seen = std::fs::read_to_string(work.root.join("seen.txt").as_std_path()).expect("the child recorded what it saw");

        assert_eq!(seen, "no 0");
    }

    /// A binary that prints past the cap on what is kept is still drained and finishes cleanly,
    /// rather than the excess being buffered until the run itself runs out of memory.
    ///
    /// A runaway mutant that loops printing has to be readable to the point where the cap stops
    /// keeping it, but the pipe still has to be drained past that point or the child blocks in
    /// `write` forever — turning a mutant that should be judged on its own merits into a hang this
    /// run has to time out instead, and a machine slowly filling with buffered text nobody reads.
    #[test]
    fn a_binary_that_prints_past_the_pipe_cap_is_still_drained_to_completion() {
        let (_directory, work) = scripted(&["flood:5000000", "print:test a::b ... ok", "exit:0"]);
        let chatty = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &chatty,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// A binary that exits non-zero fails, named by libtest's own report.
    #[test]
    fn a_failing_binary_is_named_by_its_first_failing_test() {
        let (_directory, work) = scripted(&[
            "when-env:GAMMA_ACTIVE|print:test a::b ... FAILED",
            "when-env:GAMMA_ACTIVE|exit:101",
            "print:test a::b ... ok",
            "exit:0",
        ]);
        let bad = crate::testing::helper();

        // The name comes out of the harness's own output rather than the exit status, which is
        // what makes a survivor report actionable.
        assert_eq!(
            run_binary(
                &work,
                &bad,
                Attempt {
                    active: Some(7),
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Failed(Some("a::b".to_owned()))
        );
    }

    /// Asserts the memory verdict carries the figures the report shows the user.
    ///
    /// The variant alone is a weak oracle: `MemoryLimit { peak: None, limit: 0 }` satisfies it
    /// while telling the reader nothing about how far past the ceiling the mutant ran, which is
    /// the whole diagnostic. The peak is checked as a fraction of the ceiling rather than as
    /// something exceeding it, because both enforcement mechanisms cap usage *at* the ceiling —
    /// the kernel kills rather than letting the number climb past it — so a peak strictly greater
    /// than the limit is not a thing either platform can report.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn assert_memory_verdict_reports_its_figures(verdict: &Verdict, expected: u64) {
        let Verdict::MemoryLimit { peak, limit } = verdict else {
            panic!("expected a memory verdict, got {verdict:?}");
        };

        assert_eq!(*limit, expected, "the verdict must carry the ceiling that fired: {verdict:?}");

        let peak = peak.expect("the platform reports a peak when it enforces a ceiling");

        assert!(
            peak >= expected / 2,
            "a peak of {peak} against a ceiling of {expected} is not the figure that crossed it",
        );
    }

    /// A binary that allocates past its ceiling is a memory verdict, not a failing test.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_passes_its_ceiling_is_a_memory_verdict() {
        if crate::testing::without_memory_support("a mutant stopped for passing its ceiling") {
            return;
        }

        // Anonymous pages the helper touches itself, rather than a shell writing a file. Page cache
        // backed by a disk is reclaimable, so a file-writing workload stays under the ceiling
        // indefinitely instead of crossing it; and the helper runs on every platform this tool
        // builds for, which is where the other half of the enforcement code lives.
        let (_directory, work) = crate::testing::helper_workspace("verdict-memory", &["eat:512", "exit:0"]);
        let greedy = crate::testing::helper();
        let limit = 32 * 1024 * 1024;

        let verdict = run_binary(
            &work,
            &greedy,
            Attempt {
                active: Some(1),
                timeout: Some(Duration::from_mins(1)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
            },
        );

        // Reporting this as a plain failure would be defensible and wrong: the suite noticed
        // nothing, the kernel did, and only the kernel's report distinguishes it from the
        // ordinary case of a test that exits non-zero.
        assert_memory_verdict_reports_its_figures(&verdict, limit);
    }

    /// A binary that stalls silently while also allocating past its ceiling is convicted of the
    /// memory it used, not merely reported as a stall.
    ///
    /// A workload thrashing against its ceiling runs out of time as well as out of memory, and the
    /// memory is the cause: reporting the stall instead would send whoever reads it looking for a
    /// hang that genuinely is not there, and cost a whole confirmation run reproducing an
    /// allocation that was already caught the first time. A long stall budget is used deliberately:
    /// the ceiling has unambiguously already been crossed by the time silence is judged a stall, so
    /// this test's own outcome does not depend on how fast the kernel's OOM path happens to run on
    /// whatever machine executes it.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_stalls_while_exhausting_its_ceiling_is_convicted_of_the_memory() {
        if crate::testing::without_memory_support("a stalling mutant convicted of its memory") {
            return;
        }

        let (_directory, work) = crate::testing::helper_workspace("verdict-memory-stall", &["eat:1024", "sleep:30000"]);
        let greedy = crate::testing::helper();
        let limit = 32 * 1024 * 1024;

        let verdict = run_with(
            &work,
            &greedy,
            Attempt {
                active: Some(1),
                timeout: Some(Duration::from_mins(1)),
                stall: Stall {
                    budget: Some(Duration::from_secs(10)),
                },
                request: MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
            false,
        )
        .0;

        assert_memory_verdict_reports_its_figures(&verdict, limit);
    }

    /// A binary that stays under its ceiling is judged by its tests, not by its allocations.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_stays_under_its_ceiling_is_judged_normally() {
        if crate::testing::without_memory_support("a ceiling reported with the figure that crossed it") {
            return;
        }

        let (_directory, work) = scripted(&["print:test a::b ... ok", "exit:0"]);
        let modest = crate::testing::helper();

        // The expensive mistake in the other direction: a ceiling that convicts a healthy mutant
        // credits the suite with a kill it never made and inflates the score.
        assert_eq!(
            run_binary(
                &work,
                &modest,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest {
                        calibrating: false,
                        meter: true,
                        limit: Some(512 * 1024 * 1024)
                    },
                    only: Only::All,
                }
            ),
            Verdict::Passed
        );
    }

    /// A run that reached its ceiling and still passed is not convicted by the peak alone.
    #[test]
    fn a_successful_run_is_never_convicted_by_its_peak() {
        // A peak can sit exactly at the ceiling because reclaim did its job. The suite passed; the
        // mutant was not caught by anything, and saying otherwise would be a detection invented
        // out of an accounting figure.
        let usage = MemoryUsage {
            peak: Some(1024),
            exhausted: false,
        };

        assert!(!exhausted(&usage, true));
    }

    /// A workload that reached its ceiling, reclaimed, and then failed for a reason of its own is
    /// not convicted of running out of memory.
    ///
    /// Regression, issue-023. A peak at the ceiling only says the workload touched it, and touching
    /// it is survivable: `memory.max` reclaims first and kills only when reclaim cannot keep up.
    /// Reading the peak as a verdict turns any failure that happened to follow a busy moment into
    /// "killed by the ceiling", and sends the reader off to raise a limit that was never the
    /// problem while the real cause goes unreported.
    #[test]
    fn a_workload_that_reclaimed_and_then_failed_is_not_a_memory_verdict() {
        let reclaimed = MemoryUsage {
            peak: Some(1024),
            exhausted: false,
        };

        assert!(!exhausted(&reclaimed, false));
    }

    /// The platform's own report convicts even when the peak reads below the ceiling.
    #[test]
    fn a_kernel_reported_kill_is_believed_whatever_the_peak_says() {
        // `memory.peak` is a high-water mark sampled by the kernel and an OOM kill can free the
        // charge before it is read, so the event is the authority and the peak is the detail.
        let usage = MemoryUsage {
            peak: Some(1),
            exhausted: true,
        };

        assert!(exhausted(&usage, false));
        assert!(!exhausted(&usage, true));
    }

    /// A failure the harness named survives a ceiling the same run also crossed.
    ///
    /// Taking the memory verdict before the output is read at all would leave a run that both
    /// failed a test and crossed its ceiling reporting a number and no name — leaving whoever read
    /// it with nothing to open. Both facts are true; only one of them is somewhere a reader can go.
    #[test]
    fn a_named_failure_outranks_the_ceiling_the_same_run_crossed() {
        let named = prefer_named(
            Verdict::Failed(Some("module::case".to_owned())),
            Some(300 * 1024 * 1024),
            Some(256 * 1024 * 1024),
        );

        assert_eq!(named, Verdict::Failed(Some("module::case".to_owned())));

        // With nothing named there is only the ceiling to report, and reporting it is what keeps
        // the reader from hunting for an assertion that never existed.
        let anonymous = prefer_named(Verdict::Failed(None), Some(300), Some(256));

        assert_eq!(
            anonymous,
            Verdict::MemoryLimit {
                peak: Some(300),
                limit: 256
            }
        );

        // A run that could not be metered is not a verdict about the mutant, and a ceiling does
        // not turn it into one.
        let unmetered = prefer_named(Verdict::Unmetered("no cgroup".to_owned()), None, Some(256));

        assert!(matches!(unmetered, Verdict::Unmetered(_)), "{unmetered:?}");

        // With no ceiling crossed, whatever was settled stands untouched.
        let plain = prefer_named(Verdict::Failed(Some("module::case".to_owned())), Some(1), None);

        assert_eq!(plain, Verdict::Failed(Some("module::case".to_owned())));
    }

    /// A run cut short by a named failure keeps the name, exactly as the ordinary exit does.
    ///
    /// The two exits carry the same pair of facts, and which one a run takes is decided by nothing
    /// but whether the reader published the failure before `try_wait` reaped the child. An early
    /// cut that preferred the ceiling would drop the name a reader goes and opens, discard the
    /// killer hint the next mutant would have probed with.
    ///
    /// Asserted here rather than through a launched binary because the state cannot be arranged on
    /// Linux: the ceiling sets `memory.oom.group`, so an OOM takes the whole subtree with it and
    /// there is no announcing process left to reach the early cut. Windows, where a job object
    /// refuses allocations rather than killing, is where the run really survives its ceiling.
    #[test]
    fn a_run_cut_short_by_a_named_failure_keeps_the_name_beside_the_ceiling() {
        let named = cut_by_named_failure("module::case".to_owned());

        assert_eq!(named, Verdict::Failed(Some("module::case".to_owned())));

        // With no ceiling crossed there is only the failure, which is the ordinary shape of this.
        let plain = prefer_named(Verdict::Failed(Some("module::case".to_owned())), None, None);

        assert_eq!(plain, Verdict::Failed(Some("module::case".to_owned())));
    }

    /// A named failure that arrives before an allocation ceiling is exceeded prefers the named
    /// failure end-to-end on Windows.
    ///
    /// On Windows, the job object enforces memory limits without killing the process immediately
    /// with SIGKILL/cgroup kill, allowing the named failure streaming reader to report the killing
    /// test name alongside the memory ceiling.
    #[test]
    #[cfg(windows)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_named_failure_survives_crossing_a_memory_ceiling_end_to_end() {
        if crate::testing::without_memory_support("a named failure surviving a memory ceiling") {
            return;
        }

        let (_directory, work) = crate::testing::helper_workspace(
            "verdict-named-memory",
            &[
                "print:running 1 test",
                "print:test failing::case ... FAILED",
                "eat:512",
                "sleep:30000",
                "exit:101",
            ],
        );
        let binary = crate::testing::helper();
        let limit = 32 * 1024 * 1024;

        let verdict = run_binary(
            &work,
            &binary,
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(1)),
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
            },
        );

        assert_eq!(verdict, Verdict::Failed(Some("failing::case".to_owned())));
    }

    #[test]
    fn confirmation_resource_failures_retain_their_cause_instead_of_claiming_a_flaky_test() {
        let attempt = Attempt {
            active: Some(7),
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::One("selected"),
        };
        for (verdict, expected) in [
            (Verdict::TimedOut, "exceeded its time budget"),
            (Verdict::Stalled(Some("last".into())), "stopped reporting progress"),
            (Verdict::MemoryLimit { peak: Some(11), limit: 10 }, "exceeded its memory ceiling"),
        ] {
            let confirmed = confirm_enumeration_with(attempt, "diagnostic".into(), |actual| {
                assert_eq!(actual.active, None);
                assert!(matches!(actual.only, Only::One("selected")));
                verdict
            });
            assert_eq!(confirmed, Verdict::Unconfirmed(None, expected));
        }
        assert_eq!(
            confirm_enumeration_with(attempt, "diagnostic".into(), |_| Verdict::TimedOut),
            Verdict::Unconfirmed(None, "exceeded its time budget")
        );
    }

    #[test]
    fn unexpected_confirmation_verdicts_keep_the_generic_unconfirmed_reason() {
        let attempt = Attempt {
            active: Some(7),
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::One("selected"),
        };
        let reason = "did not establish a trustworthy passing result";
        let verdicts: [fn() -> Verdict; 3] = [
            || Verdict::Flaky(Some("other".into())),
            || Verdict::TestEnumerationFailed("enumeration refused".into()),
            || Verdict::Unconfirmed(None, "previous confirmation was incomplete"),
        ];
        for verdict in verdicts {
            assert_eq!(
                confirm_enumeration_with(attempt, "output".into(), |_| verdict()),
                Verdict::Unconfirmed(None, reason)
            );
        }
    }

    #[test]
    fn enumeration_confirmation_preserves_output_and_failed_exonerations() {
        let attempt = Attempt {
            active: Some(7),
            timeout: Some(Duration::from_secs(30)),
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            only: Only::All,
        };
        for test in [None, Some("observed".to_owned())] {
            let check = |actual: Attempt<'_>, expected: Only<'_>| {
                assert_eq!(actual.active, None);
                assert_eq!(actual.only, expected);
                assert_eq!(actual.timeout, attempt.timeout);
                assert_eq!(actual.stall.budget, attempt.stall.budget);
                assert_eq!(actual.request, attempt.request);
            };
            assert_eq!(
                confirm_enumeration_with(attempt, "enumeration diagnostic".to_owned(), |actual| {
                    check(actual, Only::All);
                    Verdict::Passed
                }),
                Verdict::TestEnumerationFailed("enumeration diagnostic".to_owned())
            );
            assert_eq!(
                confirm_enumeration_with(attempt, "enumeration diagnostic".to_owned(), |actual| {
                    check(actual, Only::All);
                    Verdict::Failed(test.clone())
                }),
                Verdict::Flaky(test)
            );
        }
        for infrastructure in [Verdict::Unjudged, Verdict::Unmetered] {
            assert_eq!(
                confirm_enumeration_with(attempt, "diagnostic".to_owned(), |_| infrastructure("injected failure".to_owned())),
                infrastructure("injected failure".to_owned())
            );
        }
    }

    #[test]
    fn nextest_infrastructure_diagnostic_retains_exact_tail_line_boundary() {
        let text = (0..DIAGNOSTIC_TAIL_LINES + 2)
            .map(|i| format!("diagnostic-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = (2..DIAGNOSTIC_TAIL_LINES + 2)
            .map(|i| format!("diagnostic-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            nextest_runner_failure(Some(94), &text),
            format!("`cargo nextest` exited with code 94, which does not describe a test run:\n{expected}")
        );
    }

    #[test]
    fn retained_output_tail_reports_exact_capacity_as_a_complete_replacement() {
        for size in [OUTPUT_TAIL_CAP - 1, OUTPUT_TAIL_CAP, OUTPUT_TAIL_CAP + 1] {
            let mut tail = VecDeque::new();
            let bytes: Vec<u8> = (0..size).map(|i| u8::try_from(i % 251).expect("byte modulus fits")).collect();
            assert_eq!(retain_output_tail(&mut tail, &bytes), size >= OUTPUT_TAIL_CAP);
            assert_eq!(tail.into_iter().collect::<Vec<_>>(), bytes[size.saturating_sub(OUTPUT_TAIL_CAP)..]);
        }
    }

    /// The exonerating run's own overrun is never handed back as the mutant's verdict.
    ///
    /// The exoneration runs with no mutant active, so a timeout, a stall or a ceiling it hits is a
    /// fact about that run. Returning it would score the mutant `Outcome::Timeout` or
    /// `Outcome::OutOfMemory` — a detection credited to a mutant that was not even switched on in
    /// the run that produced the verdict.
    #[test]
    fn an_exonerations_own_overrun_is_not_adopted_as_the_mutants_verdict() {
        let (_directory, work) = scripted(&["when-env:GAMMA_ACTIVE|exit:101", "sleep:30000"]);
        let binary = crate::testing::helper();

        let verdict = confirm_enumeration(
            &work,
            &binary,
            Attempt {
                active: Some(7),
                timeout: Some(Duration::from_millis(200)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
            "nextest could not list the tests".to_owned(),
        );

        assert_eq!(verdict, Verdict::Unconfirmed(None, "exceeded its time budget"), "{verdict:?}");
    }

    /// The confirmation is skipped when there was no mutant to exonerate.
    ///
    /// A run with nothing active has already answered the question the confirmation would ask, so
    /// asking it again would buy the same answer at the price of a second run of the whole binary.
    #[test]
    fn a_failure_with_no_mutant_active_is_reported_as_it_stands() {
        let (_directory, work) = scripted(&["print:test a::b ... FAILED", "exit:101"]);
        let failing = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &failing,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                }
            ),
            Verdict::Failed(Some("a::b".to_owned()))
        );
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a\nb", 10), "a\nb");
        assert_eq!(tail("a\r\nb\r\nc\r\n", 2), "b\nc");
        assert_eq!(tail("a\nb", 0), "");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn the_first_libtest_failure_name_is_extracted() {
        let output = "running 2 tests\n\
                      test passing ... ok\n\
                      test module::case ... FAILED\n\
                      test later ... FAILED\n";

        // Only the first failing test is reported with a killed mutant; later failures may be
        // consequences of the same defect and add noise.
        assert_eq!(first_failure(output), Some("module::case"));
        assert_eq!(first_failure("custom harness output"), None);
    }

    /// Only nextest's own code for "the tests ran and one failed" convicts a mutant.
    #[test]
    fn nextest_convicts_a_mutant_only_on_a_real_test_failure() {
        let output = b"        FAIL [   0.024s] (1/2) nxspike tests::fails_when_asked\n";
        let (verdict, _usage) = settle(
            true,
            Some(7),
            Some(nextest::TEST_RUN_FAILED),
            output,
            output,
            MemoryUsage::default(),
        );

        assert_eq!(verdict, Verdict::Failed(Some("tests::fails_when_asked".to_owned())));
    }

    #[test]
    fn nextest_test_enumeration_failure_is_suspect_only_with_a_mutant_active() {
        let output = b"error: test binary exited while listing tests";
        let (suspect, _usage) = settle(
            true,
            Some(7),
            Some(nextest::TEST_LIST_CREATION_FAILED),
            output,
            output,
            MemoryUsage::default(),
        );
        let (baseline, _usage) = settle(
            true,
            None,
            Some(nextest::TEST_LIST_CREATION_FAILED),
            output,
            output,
            MemoryUsage::default(),
        );

        assert_eq!(
            suspect,
            Verdict::TestEnumerationFailed("error: test binary exited while listing tests".to_owned())
        );
        assert!(
            matches!(baseline, Verdict::Unmetered(ref reason) if reason.contains("code 104") && reason.contains("exited while listing")),
            "{baseline:?}"
        );
    }

    /// Nextest matching no tests means the filterset and the built tree disagree about what exists.
    /// Scoring that as a kill would credit the suite with one it never made, so the run stops.
    #[test]
    fn nextest_matching_no_tests_abandons_the_run_rather_than_scoring_it() {
        let (verdict, _usage) = settle(true, Some(7), Some(nextest::NO_TESTS_RUN), b"", b"", MemoryUsage::default());

        assert!(matches!(verdict, Verdict::Unmetered(_)), "{verdict:?}");
    }

    /// Any other code is nextest reporting its own failure — a bad filterset, an unreadable tree, a
    /// signal. None of those is a statement about the mutant.
    #[test]
    fn a_code_nextest_uses_for_its_own_failures_is_not_a_verdict() {
        for code in [Some(94), Some(101), None] {
            let (verdict, _usage) = settle(true, Some(7), code, b"", b"", MemoryUsage::default());

            assert!(matches!(verdict, Verdict::Unmetered(_)), "{code:?} gave {verdict:?}");
        }
    }

    /// The same codes mean nothing when the binaries are run directly: a binary is free to exit
    /// with any of them, and all a non-zero exit says is that the suite failed.
    #[test]
    fn a_directly_run_binary_is_judged_by_its_output_and_not_its_code() {
        let output = b"test module::case ... FAILED\n";

        for code in [Some(nextest::NO_TESTS_RUN), Some(101), None] {
            let (verdict, _usage) = settle(false, Some(7), code, output, output, MemoryUsage::default());

            assert_eq!(verdict, Verdict::Failed(Some("module::case".to_owned())), "{code:?}");
        }
    }

    #[test]
    fn an_environment_acquisition_failure_never_convicts_a_mutant() {
        let mut output = b"test module::case ... FAILED\n".to_vec();
        output.extend_from_slice(gamma_rt::ENVIRONMENT_ERROR_MARKER);

        for under_nextest in [false, true] {
            let (verdict, _usage) = settle(
                under_nextest,
                Some(7),
                Some(nextest::TEST_RUN_FAILED),
                &output,
                &output,
                MemoryUsage::default(),
            );

            assert!(
                matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("startup environment")),
                "{verdict:?}"
            );
        }
    }

    #[test]
    fn a_guard_before_runtime_installation_never_convicts_a_mutant() {
        let mut output = b"test module::case ... FAILED\n".to_vec();
        output.extend_from_slice(gamma_rt::PRE_INSTALL_ERROR_MARKER);

        for under_nextest in [false, true] {
            let (verdict, _usage) = settle(
                under_nextest,
                Some(7),
                Some(nextest::TEST_RUN_FAILED),
                &output,
                &output,
                MemoryUsage::default(),
            );

            assert!(
                matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("startup selection")),
                "{verdict:?}"
            );
        }
    }

    /// Nextest announces `FAIL` before replaying the failed process's captured output. The runtime
    /// marker therefore arrives too late for a live-output shortcut but in time for `settle`.
    #[test]
    fn a_nextest_failure_waits_for_a_later_environment_error_marker() {
        let progress = Mutex::new(Progress::new(Watch::Nextest));
        progress
            .lock()
            .expect("the progress lock is not poisoned")
            .heard("        FAIL [   0.024s] (1/1) nxspike tests::case\n");

        assert_eq!(announced_failure(&progress).as_deref(), Some("tests::case"));
        assert_eq!(failure_to_cut_short(true, &progress), None);
        assert!(matches!(unfinished_nextest_failure(true, &progress), Some(Verdict::Unmetered(_))));

        let mut output = b"        FAIL [   0.024s] (1/1) nxspike tests::case\n".to_vec();
        output.extend_from_slice(gamma_rt::ENVIRONMENT_ERROR_MARKER);
        progress
            .lock()
            .expect("the progress lock is not poisoned")
            .heard(core::str::from_utf8(gamma_rt::ENVIRONMENT_ERROR_MARKER).expect("the marker is ASCII"));

        assert!(environment_failure(&progress));
        assert_eq!(failure_to_cut_short(true, &progress), None);

        let direct = Mutex::new(Progress::new(Watch::Libtest));
        {
            let mut state = direct.lock().expect("the progress lock is not poisoned");
            state.heard("running 1 test\n");
            state.heard("test tests::case ... FAILED\n");
        }
        assert_eq!(failure_to_cut_short(false, &direct).as_deref(), Some("tests::case"));

        let (verdict, _usage) = settle(
            true,
            Some(7),
            Some(nextest::TEST_RUN_FAILED),
            &output,
            &output,
            MemoryUsage::default(),
        );

        assert!(
            matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("startup environment")),
            "{verdict:?}"
        );
    }

    /// Nextest's output is its own format, so a run through it is watched for nextest's failure
    /// lines rather than libtest's — and the `--nocapture` question does not arise, because nextest
    /// captures per test regardless of what the tests are handed.
    #[test]
    fn a_run_through_nextest_is_watched_in_nextest_format() {
        let (_scratch, mut work) = crate::testing::helper_workspace("watch-nextest", &["exit:0"]);

        work.set_test_args(vec!["--nocapture".to_owned()]);
        work.set_runner(nextest::Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]));

        assert_eq!(watch(&work), Watch::Nextest);
    }

    /// A plain libtest run captures each test's output and replays it only after every test has
    /// run, so a failure line seen during the run is the harness's own and can be trusted.
    #[test]
    fn a_plain_libtest_run_is_watched_in_libtest_format() {
        let (_scratch, mut work) = crate::testing::helper_workspace("watch-libtest", &["exit:0"]);

        work.set_test_args(Vec::new());

        assert_eq!(watch(&work), Watch::Libtest);
    }

    /// Under `--nocapture` or `--show-output` a test's own writing lands among the harness's, so a
    /// test that printed something shaped like a failure would convict a mutant the suite never
    /// caught. The optimization is given up rather than risk inflating the score.
    #[test]
    fn interleaved_output_gives_up_watching_entirely() {
        for argument in ["--nocapture", "--show-output"] {
            let (_scratch, mut work) = crate::testing::helper_workspace("watch-off", &["exit:0"]);

            work.set_test_args(vec![argument.to_owned()]);

            assert_eq!(watch(&work), Watch::Off, "{argument}");
        }
    }

    /// A binary the runner does not know cannot be launched, and the reason has to survive as the
    /// unmetered verdict's text — a mutant recorded as caught here would be one no test ever ran
    /// against.
    #[test]
    fn a_binary_the_runner_does_not_know_yields_no_command() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-stranger", &["exit:0"]);

        work.set_runner(nextest::Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]));

        let reason = launcher(&work, &crate::testing::test_binary("/t/deps/stranger-def"), Only::All, false)
            .expect_err("an unknown binary cannot be launched");

        assert!(
            matches!(reason, Verdict::Unmetered(ref reason) if reason.contains("/t/deps/stranger-def")),
            "{reason:?}"
        );
    }

    /// A run that cannot even be launched is unmetered, not a kill. Recording it as a kill would
    /// credit the suite with catching a mutant that no test was ever run against.
    #[test]
    fn a_run_that_cannot_be_launched_is_unmetered_rather_than_a_kill() {
        let (_directory, mut work) = scripted(&["exit:0"]);

        work.set_runner(nextest::Harness::fake(&[("/t/deps/nxspike-abc", "nxspike")]));

        let verdict = run_with(
            &work,
            &crate::testing::test_binary("/t/deps/stranger-def"),
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(1)),
                stall: Stall { budget: None },
                request: MemoryRequest {
                    calibrating: false,
                    meter: false,
                    limit: None,
                },
                only: Only::All,
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Nextest))),
            false,
        )
        .0;

        assert!(
            matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("stranger-def")),
            "{verdict:?}"
        );
    }

    /// With no runner the binary is invoked directly, carrying the arguments the harness reads —
    /// the baseline included, or the baseline would measure a different suite from the one each
    /// mutant is judged against.
    #[test]
    fn a_run_without_a_runner_invokes_the_binary_itself() {
        let (_scratch, work) = crate::testing::helper_workspace("launch-direct", &["exit:0"]);
        let binary = crate::testing::helper();
        let command = launcher(&work, &binary, Only::All, false).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let cache_home = command
            .get_envs()
            .find(|(name, _value)| *name == crate::testing::CACHE_HOME_VAR)
            .and_then(|(_name, value)| value);

        assert_eq!(command.get_program(), binary.path.as_str());
        assert_eq!(args, vec!["--gamma-step=exit:0"]);
        assert!(cache_home.is_some(), "a nested Cargo Gamma process could escape the test cache");
    }

    #[test]
    fn a_direct_mutant_run_uses_no_unstable_harness_flags_or_forced_thread_count() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-direct-fail-fast", &["exit:0"]);
        let binary = crate::testing::helper();
        work.set_test_args(vec!["module::selected".to_owned(), "--skip".to_owned(), "module::slow".to_owned()]);

        let command = launcher(&work, &binary, Only::All, true).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(&args[args.len() - 3..], ["module::selected", "--skip", "module::slow"]);
        assert!(!args.iter().any(|arg| arg == "--fail-fast"), "{args:?}");
        assert!(!args.iter().any(|arg| arg.starts_with("--test-threads")), "{args:?}");
    }

    #[test]
    fn a_resource_bearing_direct_run_forces_one_harness_thread() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-direct-resource", &["exit:0"]);
        let binary = crate::testing::helper();
        work.set_test_args(vec!["--test-threads=8".to_owned()]);
        work.set_resources(super::super::resources::Resources::fake_binary(&binary));

        let command = launcher(&work, &binary, Only::All, true).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(
            args.iter().filter(|arg| arg.starts_with("--test-threads")).collect::<Vec<_>>(),
            ["--test-threads"]
        );
        assert!(args.windows(2).any(|pair| pair == ["--test-threads", "1"]), "{args:?}");
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_direct_mutant_launch_is_accepted_by_the_real_stable_libtest_harness() {
        let (_scratch, work) = crate::testing::helper_workspace("launch-direct-stable-libtest", &[]);
        let executable = std::env::current_exe().expect("the test harness has an executable path");
        let executable = executable.to_str().expect("the test harness path is UTF-8");
        let binary = crate::testing::test_binary(executable);
        let mut command = launcher(&work, &binary, Only::One("exec::verdict::tests::stable_libtest_child_passes"), true)
            .expect("a direct launch needs nothing from a runner");

        let output = command.output().expect("the current test harness can be launched");

        assert!(
            output.status.success(),
            "stable libtest rejected the mutant launch:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stable_libtest_child_passes() {}

    /// A run narrowed to one test asks libtest for that name and for an exact match.
    ///
    /// Both halves matter. Without `--exact` the name is a substring filter, so `parses` would drag
    /// in `parses_empty` and `parses_nested` and the probe would stop being one test. Only this
    /// run's own name is passed: the user gave no filter of their own here, so there is nothing to
    /// intersect with and nothing that could widen the selection back out.
    #[test]
    fn a_run_narrowed_to_one_test_asks_for_it_by_name_and_exactly() {
        let (_scratch, work) = crate::testing::helper_workspace("launch-filtered", &["exit:0"]);
        let binary = crate::testing::helper();
        let command = launcher(&work, &binary, Only::One("tests::parses"), false).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--gamma-step=exit:0", "tests::parses", "--exact"]);
    }

    /// A test this run chose that the user's own filter excludes is refused, not run.
    ///
    /// libtest matches a test that any *one* of its positional filters matches, so appending the
    /// chosen name to the user's would widen the selection rather than narrow it — and the mutant
    /// would then be convicted by a test the user deliberately took out of the run, inflating the
    /// score with a detection their suite as configured never made. The intersection is empty here,
    /// and an empty intersection has no launch that expresses it.
    #[test]
    fn a_chosen_test_the_users_filter_excludes_is_refused_rather_than_run() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-excluded", &["exit:0"]);
        let binary = crate::testing::helper();

        work.set_test_args(vec!["allowed".to_owned()]);

        let refusal = launcher(&work, &binary, Only::One("tests::excluded"), false).expect_err("the selection is empty");

        assert!(
            matches!(refusal, Verdict::Unjudged(ref reason) if reason.contains("harness filters allow")),
            "{refusal:?}"
        );
    }

    /// A test the user's filter admits is run alone, without their filter being repeated.
    ///
    /// Repeating it would restore the widening, and the `--exact` that pins this run's own name is
    /// global — so leaving the user's substring filter in place would silently redefine it as a
    /// whole-name match, which for `allowed` matches nothing at all. Their `--skip` stays, because
    /// it only ever removes tests.
    #[test]
    fn a_chosen_test_the_users_filter_admits_is_run_alone_under_their_flags() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-admitted", &["exit:0"]);
        let binary = crate::testing::helper();

        work.set_test_args(vec!["--skip".to_owned(), "slow".to_owned(), "allowed".to_owned()]);

        let command = launcher(&work, &binary, Only::One("tests::allowed_case"), false).expect("the selection is not empty");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--skip", "slow", "tests::allowed_case", "--exact"]);
    }

    /// A wakeup raised while the waiter was busy elsewhere is not slept through.
    ///
    /// This is the whole reason the pulse counts generations instead of setting a flag the waiter
    /// clears. The loop reads the generation, then checks the child, then waits; a signal landing in
    /// that gap must make the wait return at once, or the news it carried is delayed by the cap.
    #[test]
    fn a_wakeup_raised_before_the_wait_is_not_slept_through() {
        let pulse = Pulse::default();
        let seen = pulse.seen();

        pulse.signal();

        let started = Instant::now();
        pulse.wait(seen, Duration::from_secs(30));

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait slept through a signal it had already been sent"
        );
    }

    /// A wakeup raised during a wait ends it, rather than leaving it to time out.
    #[test]
    fn a_wakeup_raised_during_a_wait_ends_it() {
        let pulse = Arc::new(Pulse::default());
        let seen = pulse.seen();
        let started = Instant::now();

        thread::scope(|scope| {
            let _signaller = scope.spawn(|| {
                thread::sleep(Duration::from_millis(20));
                pulse.signal();
            });

            pulse.wait(seen, Duration::from_secs(30));
        });

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait ran to its timeout despite being signalled"
        );
    }

    /// The gauge has to count a reader for exactly as long as its thread is running.
    ///
    /// This is the measurement the backlog asks for before any bound is built, so what it reports has to
    /// mean what it says: `live` back at its starting value once the readers have finished, and a
    /// `peak` that recorded them while they had not. A gauge that decremented early would report
    /// an untroubled run over a suite stranding readers on every mutant, which is the exact
    /// conclusion it exists to prevent.
    #[test]
    fn a_readers_gauge_counts_a_reader_for_as_long_as_it_runs() {
        let gauge = Readers::new();

        assert_eq!(gauge.live(), 0);
        assert_eq!(gauge.peak(), 0);

        gauge.started();
        gauge.started();

        assert_eq!(gauge.live(), 2);
        assert_eq!(gauge.peak(), 2);

        gauge.finished();

        assert_eq!(gauge.live(), 1, "a finished reader is still counted as running");
        assert_eq!(gauge.peak(), 2, "the peak fell back to the live count");

        gauge.started();
        gauge.finished();
        gauge.finished();

        assert_eq!(gauge.live(), 0);
        assert_eq!(gauge.peak(), 2, "the peak forgot a reader that has since finished");
    }

    /// Reading a binary's output must leave nothing running behind it.
    ///
    /// The readers are abandoned rather than joined, so nothing in the ordinary path proves they
    /// ever stop. This runs a binary that exits at once and holds nothing open, which is the case
    /// that must return the gauge to where it started — if even that leaks a reader, the count
    /// reported under `--diag` would climb on every mutant and say nothing about stray descendants.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_ordinary_binary_leaves_no_reader_behind() {
        let before = READERS.live();
        let (_directory, work) = scripted(&["print:done", "exit:0"]);
        let quick = crate::testing::helper();

        let verdict = run_binary(
            &work,
            &quick,
            Attempt {
                active: None,
                timeout: Some(Duration::from_mins(1)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
            },
        );

        assert!(matches!(verdict, Verdict::Passed), "{verdict:?}");
        assert!(READERS.peak() > 0, "the run started no readers at all");

        // The reader publishes its text and then decrements, so the two are not simultaneous.
        //
        // The gauge is process-global and the suite runs in parallel, so the count is compared with
        // `<=` rather than `==`: another test's readers may start or finish at any moment, and only
        // a reader this call leaked could hold the figure permanently above where it began. That is
        // the leak the test is for, and it is the one thing polling cannot wait out.
        for _attempt in 0..200 {
            if READERS.live() <= before {
                return;
            }

            thread::sleep(Duration::from_millis(10));
        }

        assert!(READERS.live() <= before, "a reader outlived the binary it was reading");
    }
}
