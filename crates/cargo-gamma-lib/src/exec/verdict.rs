// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::borrow::Cow;
use std::io::{self, BufReader, Read};
use std::process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use camino::Utf8Path;
use cargo_gamma_process::{MemoryRequest, MemoryUsage, PreparedCommand, ProcessTree, SpawnedCommand, prepare};

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
use super::progress::{Progress, Watch};
use super::stall::Stall;
use super::test_binary::{TEST_THREADS_VAR, TestBinary};
use super::workspace::Workspace;

/// The variable Insta reads to decide whether to write snapshots it was asked to compare against.
const INSTA_UPDATE_VAR: &str = "INSTA_UPDATE";

/// The variable Insta reads to decide whether a mismatched snapshot still passes.
const INSTA_FORCE_PASS_VAR: &str = "INSTA_FORCE_PASS";

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

    /// Every test a census established can reach the mutant, and nothing else.
    ///
    /// Unlike [`Self::One`] this *is* a substitute for the binary, and a verdict may be read from
    /// its absence of failures: the census measured that no other test executes the mutant's site,
    /// so no other test can convict it. See `census` for why that is exact rather than a guess.
    These(&'name [&'name str]),
}

impl<'name> Only<'name> {
    /// The tests to run, empty when the whole binary runs.
    ///
    /// Borrowed rather than copied wherever a borrow reaches far enough: a census's [`Self::These`]
    /// selection can already name most of a large suite, and every reachable censused binary
    /// launches at least one attempt against it, so cloning that slice into a second vector on every
    /// one of those launches would be an allocation with no reason behind it — `launcher` only ever
    /// reads through the result, once, to build one command. [`Self::One`] still allocates: a name
    /// held by value here has nowhere with lifetime `'name` to be borrowed from, so there is no
    /// slice of that lifetime to hand back without first storing the name somewhere that outlives
    /// this call.
    fn names(self) -> Cow<'name, [&'name str]> {
        match self {
            Self::All => Cow::Owned(Vec::new()),
            Self::One(name) => Cow::Owned(vec![name]),
            Self::These(names) => Cow::Borrowed(names),
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

    /// Where the runtime is to write the sites this run reached, for a census.
    ///
    /// `None` for every ordinary run. Setting it also selects unmutated behavior in the runtime,
    /// which is why it is only ever paired with an `active` of `None`.
    pub(super) census: Option<&'name Utf8Path>,
}

impl Attempt<'_> {
    /// The same attempt with no mutant active, which is how a suspected kill is exonerated.
    const fn exonerating(self) -> Self {
        Self { active: None, ..self }
    }

    /// The same attempt with several times the budget, which is how a suspected timeout is checked.
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

    /// A test failed with the mutant active and failed again with no mutant active.
    ///
    /// Not a verdict about the mutant either way. The suite noticed something, but the same thing
    /// happens without the mutant, so nothing about this run is evidence about this mutant.
    /// Carries the test that failed, which is the only actionable thing here: the remedy is to fix
    /// that test, not to write a new one.
    Flaky(Option<String>),

    /// The run could not be measured as this run was configured, so nothing about the mutant was
    /// learned.
    ///
    /// Two shapes reach this: the accounting the run asked for could not be installed, so nothing
    /// was started at all, and a run that started but could not be followed to a conclusion — a
    /// wait on the child that failed, which leaves the one question that would have settled the
    /// mutant unanswerable.
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

/// Runs one test binary with an optional active mutant, under a wall-clock budget.
///
/// Every provisional abnormal verdict is confirmed by a second run before it is believed. A false
/// kill inflates the score, while a false resource-exhaustion verdict lowers it and can fail the
/// run; neither should depend on one noisy execution.
///
/// The confirmations differ because the suspicions do:
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
/// * A failing test is retried with **no** mutant active. Nothing about a red test says the mutant
///   made it red, and a test that is merely flaky would otherwise be scored as a kill every time it
///   happened to fail. If it fails again with the mutant out of the picture, the suite is what is
///   unreliable, and the run says so with [`Verdict::Flaky`] rather than crediting the mutant.
///
/// The kill confirmation costs one extra run of the cheapest verdict class — the one that finishes
/// as soon as a test fails rather than running the whole binary. `confirm` turns it off, at the
/// price of a score that counts flakes as kills and cannot show which ones they were.
pub(super) fn run_binary(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, confirm: bool) -> Verdict {
    let confirmed = settle_suspicions(attempt, |attempt| observe(work, binary, attempt).verdict);

    // Reached from the first run and from a confirmation alike: a suspected timeout that turns out
    // to be a failing test is exactly as much of a suspicion as one reported straight away.
    match confirmed {
        Verdict::Failed(test) if confirm => confirm_kill(work, binary, attempt, test),
        Verdict::TestEnumerationFailed(output) => confirm_enumeration(work, binary, attempt, output),
        other => other,
    }
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
    Containment(String),
    Spawn(io::Error),
}

fn spawn_patiently(command: Command, request: MemoryRequest) -> Result<SpawnedCommand, StartError> {
    let mut waited = SPAWN_BACKOFF;

    // Prepared once, outside the loop, and carried through every wait: a retry re-spawns the launch
    // it already has rather than building a second one, which is the only shape `prepare` supports.
    let mut prepared = prepare(command, request).map_err(|cause| StartError::Containment(cause.to_string()))?;

    for _attempt in 1..SPAWN_ATTEMPTS {
        match spawn_once(prepared) {
            Ok(spawned) => return Ok(spawned),
            Err(FailedSpawn { cause, prepared: returned }) if transient(&cause) => {
                prepared = (*returned)
                    .backoff(waited)
                    .map_err(|cause| StartError::Containment(cause.to_string()))?;
            }
            Err(FailedSpawn { cause, .. }) => return Err(StartError::Spawn(cause)),
        }

        waited = waited.saturating_mul(2);
    }

    spawn_once(prepared).map_err(|failure| StartError::Spawn(failure.cause))
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
            break;
        };

        if checked == Some(suspicion) {
            break;
        }

        checked = Some(suspicion);
        verdict = run(suspicion.checking(attempt));
    }

    verdict
}

/// Decides whether a failing test says anything about the mutant that was active when it failed.
///
/// The question is settled the only way it can be: by running the same binary again with the
/// mutant switched off. A suite that fails either way was not detecting anything.
fn confirm_kill(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, test: Option<String>) -> Verdict {
    // Nothing to exonerate. A run with no mutant active has already answered the question the
    // confirmation would ask, and asking it again would only pay for the same answer.
    if attempt.active.is_none() {
        return Verdict::Failed(test);
    }

    let exonerating_attempt = test.as_deref().map_or_else(
        || attempt.exonerating(),
        |name| Attempt {
            active: None,
            only: Only::One(name),
            ..attempt
        },
    );

    match observe(work, binary, exonerating_attempt).verdict {
        // The suite is green without the mutant and red with it. That is a detection, and the only
        // observation in this match that is one.
        Verdict::Passed => Verdict::Failed(test),

        // Neither a detection nor a gap in the tests. Crediting the mutant would let one
        // unreliable test manufacture a kill for every mutant it happens to be run against;
        // recording a survivor would send the reader to write an assertion for code an assertion
        // already covers. The test that failed both ways travels with the verdict, because fixing
        // it is the only thing anybody can do about this.
        Verdict::Failed(also) => Verdict::Flaky(test.or(also)),

        // The confirmation could not be metered as this run was configured, so it decided nothing.
        // The reason travels rather than being replaced by a verdict nobody established.
        Verdict::Unmetered(reason) => Verdict::Unmetered(reason),

        // The same, one scope down: the machinery refused this one run, so the exoneration
        // established nothing and the mutant goes unjudged rather than taking the sweep with it.
        Verdict::Unjudged(reason) => Verdict::Unjudged(reason),

        // The exoneration exceeded a budget instead of finishing, so the suite was never observed
        // green without the mutant — and the one thing this must not do is read "not observed" as
        // "observed green". The exoneration inherits the mutant run's budgets while running the
        // whole binary, where the mutant's own run stopped at its first failing test, so an
        // overrun here is as much a fact about the machine as about anything.
        //
        // Recorded as a flake rather than as an unmetered run: nothing about the mutant was
        // established, but the machinery worked, and abandoning the whole sweep over a slow
        // confirmation would be a far worse answer than leaving one mutant out of the score. The
        // failing test travels for the same reason it does above.
        //
        // The remaining variants cannot arrive from an exoneration — `TestEnumerationFailed` is
        // only settled with a mutant active, and `Flaky` is reached from here rather than from
        // `observe` — and they are folded in rather than matched separately because the conclusion
        // would be the same either way: nothing was established.
        _unestablished => Verdict::Flaky(test),
    }
}

/// Confirms that nextest's inability to enumerate tests was caused by the active mutant.
fn confirm_enumeration(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, output: String) -> Verdict {
    match observe(work, binary, attempt.exonerating()).verdict {
        Verdict::Passed => Verdict::TestEnumerationFailed(output),
        Verdict::Failed(test) => Verdict::Flaky(test),
        Verdict::Unmetered(reason) => Verdict::Unmetered(reason),
        Verdict::Unjudged(reason) => Verdict::Unjudged(reason),

        // Whatever the exoneration ran into, it ran into it with no mutant active, so it is not a
        // verdict about the mutant and must not be handed back as one: a `TimedOut` or a
        // `MemoryLimit` is scored as an undetected mutant, so returning one here would record the
        // suite as having missed a mutant that was switched off for the entire run that produced
        // the evidence. Nothing was established, and that is what this says.
        _unestablished => Verdict::Flaky(None),
    }
}

/// Says on the diagnostic stream that a verdict was reached on less output than the binary produced.
///
/// Worth saying because the shortfall is invisible in the verdict itself: a truncated stream that
/// lost the failure announcement is indistinguishable from a suite that failed without naming a
/// test, and both come out as a kill with no killer. A reader chasing an unnamed kill would
/// otherwise spend the search on the suite rather than on the pipe.
fn announce_partial(binary: &TestBinary) {
    announce(&format!(
        "the output of `{}` could not be read to the end, so the failing test may be named in text \
         this run never saw. The exit status still decided the verdict.",
        binary.path
    ));
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
fn launcher(work: &Workspace, binary: &TestBinary, only: Only<'_>) -> Result<Command, String> {
    let Some(harness) = work.runner() else {
        let mut command = Command::new(binary.path.as_std_path());
        let names = only.names();

        if names.is_empty() {
            // The whole binary runs, so the user's arguments are the whole selection and go through
            // exactly as written.
            let _ = command.args(work.test_arguments());
        } else {
            // libtest matches a test that any *one* positional filter matches, so appending the
            // name this run chose to the user's own filters would widen the set rather than narrow
            // it — and a mutant could be convicted by a test the user deliberately excluded. The
            // intersection libtest cannot express is computed here instead: the user's filters
            // decide which of the chosen names may run, and only the survivors are passed.
            let user = HarnessFilters::parse(work.test_arguments());
            let allowed: Vec<&str> = names.iter().copied().filter(|name| user.admits(name)).collect();

            if allowed.is_empty() {
                return Err(format!(
                    "the test selection this run made for `{}` names nothing the harness filters allow, \
                     so it was refused rather than run as a wider selection",
                    binary.path
                ));
            }

            // The user's positional filters are not repeated here: `admits` has already applied
            // them, and leaving them out is what lets `--exact` pin this run's own names without
            // also silently converting the user's substring filter into a whole-name match.
            let _ = command.args(user.flags());
            let _ = command.args(allowed).arg("--exact");
        }

        let _ = command.current_dir(working_directory(work, binary).as_std_path());

        return Ok(command);
    };

    harness.command(work, binary, &only.names()).map_err(|cause| cause.to_string())
}

/// Turns a runner's non-zero exit into a verdict about the mutant.
///
/// A binary run directly says only that it failed, and libtest names the first failing test in its
/// output. Nextest distinguishes far more, and the distinction matters: a code saying the tests ran
/// and one failed convicts the mutant, whereas a code saying nextest matched no tests or could not
/// start is a fact about this run. Crediting the suite with a kill it never made would inflate the
/// score, so anything nextest does not describe as a test failure stops the run instead.
fn settle(under_nextest: bool, active: Option<u32>, code: Option<i32>, text: &[u8], usage: MemoryUsage) -> (Verdict, MemoryUsage) {
    if text
        .windows(gamma_rt::ENVIRONMENT_ERROR_MARKER.len())
        .any(|window| window == gamma_rt::ENVIRONMENT_ERROR_MARKER)
    {
        return (
            Verdict::Unmetered("the guard runtime could not acquire the process startup environment".to_owned()),
            usage,
        );
    }

    let output = String::from_utf8_lossy(text);

    if !under_nextest {
        return (Verdict::Failed(first_failure(&output).map(str::to_owned)), usage);
    }

    match code {
        Some(nextest::TEST_RUN_FAILED) => (Verdict::Failed(nextest::first_failure(&output).map(str::to_owned)), usage),

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

/// Describes a nextest infrastructure failure without discarding its diagnostic output.
fn nextest_runner_failure(code: Option<i32>, output: &str) -> String {
    let mut reason = format!(
        "`cargo nextest` exited with {}, which does not describe a test run",
        code.map_or_else(|| "a signal".to_owned(), |code| format!("code {code}"))
    );
    let useful = output.trim();

    if !useful.is_empty() {
        reason.push_str(":\n");
        reason.push_str(&tail(useful, 20));
    }

    reason
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
    census: Option<&Utf8Path>,
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

    // Removed rather than left alone on an ordinary run, because it is inherited: a census that
    // crashed leaving the variable set in a developer's shell would otherwise turn every later run
    // into one that activates no mutant and reports the whole population as surviving.
    match census {
        Some(path) => {
            let _ = command.env(gamma_rt::CENSUS_VAR, path.as_std_path());
        }
        None => {
            let _ = command.env_remove(gamma_rt::CENSUS_VAR);
        }
    }
}

/// Runs one binary, publishing the harness's progress into `progress` as it goes.
fn run_with(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>, progress: &Arc<Mutex<Progress>>) -> (Verdict, MemoryUsage) {
    let (active, timeout, stall, request) = (attempt.active, attempt.timeout, attempt.stall, attempt.request);

    let mut command = match launcher(work, binary, attempt.only) {
        Ok(command) => command,
        Err(reason) => return (Verdict::Unmetered(reason), MemoryUsage::default()),
    };

    let launch = work.launch();

    configure_loader(&mut command, launch);

    // Nextest reports on stderr: the per-test `FAIL` lines that name what convicted a mutant, and
    // the progress that tells the stall detector the child is alive. Left discarded, every nextest
    // failure would be anonymous and a healthy run would look silent. A binary run directly says
    // all of that on stdout, and its stderr carries only panic noise that no verdict is read from.
    let under_nextest = work.runner().is_some();

    configure(&mut command, binary, launch, work.harness_threads(), active, attempt.census);

    let spawned = match spawn_patiently(command, request) {
        Ok(started) => started,

        // A spawn fails for reasons of the machine — descriptors, processes, address space, a
        // binary being written as it is read — and none of them is a test failing. Reporting one as
        // a failing suite would credit the tests with a kill they never made, which is the one
        // direction this tool must not be wrong in.
        //
        // Nor is it a fact about the run. `spawn_patiently` has already waited out the transient
        // shapes, so what is left is this one mutant's run being impossible right now, and the
        // sweep records that against the mutant and carries on.
        //
        // The accounting the run asked for decides only how the refusal reads, not what it means.
        // With a boundary installed the spawn is the step that puts the child inside it, so saying
        // so is the more useful sentence; with none installed — every host without a delegated
        // cgroup, and every run given `--memory off` — there is no boundary to blame and the
        // kernel's own reason is the whole of what is known.
        Err(StartError::Containment(reason)) => return (Verdict::Unmetered(reason), MemoryUsage::default()),
        Err(StartError::Spawn(cause)) => {
            let reason = if request.wanted() {
                format!(
                    "`{}` could not be started inside its memory accounting boundary: {cause}",
                    binary.path
                )
            } else {
                format!("`{}` could not be started: {cause}", binary.path)
            };

            return (Verdict::Unjudged(reason), MemoryUsage::default());
        }
    };

    // Adopted before anything is read from the child, so the typestate bundle cannot sit with its
    // interrupt window open while a grandchild starts outside the containment.
    let mut subtree = match ProcessTree::adopt(spawned) {
        Ok(subtree) => subtree,

        // The child is already live, so this is one run that cannot be accounted for rather than a
        // boundary the host will never provide — `prepare` above answers that question, and it
        // succeeded.
        Err(reason) => {
            return (Verdict::Unjudged(reason.to_string()), MemoryUsage::default());
        }
    };

    let pulse = Arc::new(Pulse::default());
    let drained = match readers(&mut subtree, progress, &pulse, under_nextest) {
        Ok(drained) => drained,
        Err(cause) => {
            let (usage, _ceiling) = cut_short(&mut subtree, request);

            return (
                Verdict::Unjudged(format!("`{}` output could not be supervised: {cause}", binary.path)),
                usage,
            );
        }
    };

    let deadline = timeout.map(deadline_after);

    loop {
        // Read before the child is checked, so a signal raised while this iteration is working is
        // not slept through.
        let seen = pulse.seen();

        match ask_after(&mut subtree) {
            Ok(Some(status)) => {
                // `ProcessTree::observe` used a non-reaping observation, swept descendants, released
                // the interrupt slot, and only then reaped the leader.
                debug_assert!(subtree.released(), "the containment is released before the output is drained");

                let (text, whole) = collected(&drained, DRAIN_GRACE);
                let usage = subtree.usage();
                let ceiling = request.limit.filter(|_limit| exhausted(&usage, status.success()));

                if let Some(verdict) = environment_verdict(progress) {
                    return (verdict, usage);
                }

                if status.success() {
                    return (Verdict::Passed, usage);
                }

                // Said before the text is read rather than after, because what follows draws a
                // conclusion from an absence — no announced failure means the harness named no
                // test — and that reading is only sound if the text is all of it. The run is not
                // abandoned over it: the exit status still says the suite failed, which is the
                // verdict, and only the name is at risk.
                if !whole {
                    announce_partial(binary);
                }

                // Asked before the ceiling is turned into a verdict, because the harness's own
                // report is the more useful of the two: a named test is somewhere a reader can go,
                // and a memory verdict that pre-empted it would leave them with a number and no
                // idea which of the suite's tests to open.
                let (verdict, usage) = settle(under_nextest, active, status.code(), &text, usage);

                return (prefer_named(verdict, usage.peak, ceiling), usage);
            }

            Ok(None) => {
                if let Some(verdict) = environment_verdict(progress) {
                    let (usage, _ceiling) = cut_short(&mut subtree, request);

                    return (verdict, usage);
                }

                // A direct libtest failure settles the verdict, so every test after it would be
                // paid for to learn nothing. Nextest is different: it prints its FAIL line before
                // replaying the failed child's captured output, which may hold the guard runtime's
                // environment-error marker. Cutting there would convict a mutant before the
                // evidence that the test never started was available.
                if let Some(name) = failure_to_cut_short(under_nextest, progress) {
                    let (usage, ceiling) = cut_short(&mut subtree, request);

                    return (cut_by_named_failure(name, usage.peak, ceiling), usage);
                }

                let stalled = stall.exceeded(progress);

                if stalled || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    let (usage, ceiling) = cut_short(&mut subtree, request);

                    if let Some(verdict) = unfinished_nextest_failure(under_nextest, progress) {
                        return (verdict, usage);
                    }

                    // No name was announced on this path, so there is nothing for the ceiling to
                    // outrank: a workload thrashing against its limit runs out of time as well, and
                    // the memory is the cause of both. See [`cut_short`].
                    if let Some(limit) = ceiling {
                        return (Verdict::MemoryLimit { peak: usage.peak, limit }, usage);
                    }

                    // The text is not read on this path, so there is nothing to wait for.
                    return if stalled {
                        (Verdict::Stalled(last_test(progress)), usage)
                    } else {
                        (Verdict::TimedOut, usage)
                    };
                }

                // Bounded by whichever comes first: the deadline, the earliest moment silence
                // could be called a stall, and the backstop. Sleeping past any of the three would
                // let the run overshoot a limit it promised to keep.
                pulse.wait(seen, WAIT_CAP.min(deadline.map_or(WAIT_CAP, remaining)).min(stall.slack(progress)));
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
                let (usage, _ceiling) = cut_short(&mut subtree, request);

                debug_assert!(subtree.released(), "the containment is released before the output is drained");

                // Discarded, since there is no verdict to read out of it: the drain is here so the
                // reader threads reach end of file and end, rather than being left holding pipes
                // for the rest of the run. Whether it was complete is discarded with it — nothing
                // was going to be concluded from the text either way.
                let (_text, _whole) = collected(&drained, DRAIN_GRACE);

                return (
                    Verdict::Unjudged(format!("`{}` could not be asked whether it had finished: {cause}", binary.path)),
                    usage,
                );
            }
        }
    }
}

fn deadline_after(timeout: Duration) -> Instant {
    let now = Instant::now();
    let timeout = timeout.min(Duration::from_secs(crate::bounds::MOST_SECONDS));

    now.checked_add(timeout).unwrap_or(now)
}

/// Gathers whatever the readers have handed back, and whether it is all of it.
///
/// Both streams are wanted and either may arrive first: the failure that names a test can be on one
/// while the other is still open, so this waits out the grace period rather than taking the first
/// chunk. A stream that ended in a read failure makes the whole collection partial, because there
/// is no way to tell which stream the missing text would have been on — the announcement that names
/// the killing test could have been on either.
///
/// A reader that never returns is the same loss and is reported the same way. Only a disconnect
/// says every reader has sent and dropped its sender; running out of grace means one of them is
/// still holding a pipe open — a descendant that escaped the containment inherited the write end —
/// and its stream is missing entirely. Reading that as a complete collection would let a run whose
/// `FAIL` line never arrived convict a mutant with no killer named and warn about nothing.
fn collected(drained: &Receiver<(Vec<u8>, bool)>, grace: Duration) -> (Vec<u8>, bool) {
    let mut text = Vec::new();
    let mut whole = true;

    loop {
        match drained.recv_timeout(grace) {
            Ok((chunk, complete)) => {
                text.extend_from_slice(&chunk);
                whole &= complete;
            }
            Err(RecvTimeoutError::Disconnected) => return (text, whole),
            Err(RecvTimeoutError::Timeout) => return (text, false),
        }
    }
}

/// Asks whether the child has finished, without giving cleanup a reused group id.
///
/// Named rather than called inline only so that the error arm below it can be reached from a test.
/// Outside test builds, [`ProcessTree::observe`] performs Unix's non-reaping observation and owns the
/// sweep, release, and final reap as one lifecycle.
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
        Verdict::Failed(Some(name)) => Verdict::Failed(Some(name)),
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
/// machine where the reader wins and replaced by a byte count where the reaper does. Worse than
/// losing the name: only [`Verdict::Failed`] is routed to the flake check, so a flaky test failing
/// beside a ceiling would be credited as an unconfirmed detection on one machine and confirmed on
/// the other.
///
/// [`cut_short`] has already ended the run by the time this is asked, so both facts are final.
fn cut_by_named_failure(name: String, peak: Option<u64>, ceiling: Option<u64>) -> Verdict {
    prefer_named(Verdict::Failed(Some(name)), peak, ceiling)
}

/// Ends a run whose verdict is already settled, and reports the ceiling when one fired.
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
/// there.
fn cut_short(subtree: &mut ProcessTree, request: MemoryRequest) -> (MemoryUsage, Option<u64>) {
    // Only Unix has a numeric watch slot that can be released too early. On Windows `released`
    // necessarily returns true because the job handle itself remains the authority over the child.
    #[cfg(unix)]
    debug_assert!(
        !subtree.released(),
        "the subtree is signalled while it still holds its leader and its watch slot"
    );

    let _reaped = subtree.terminate();

    let usage = subtree.usage();
    let ceiling = request.limit.filter(|_limit| exhausted(&usage, false));

    (usage, ceiling)
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
fn working_directory<'work>(work: &'work Workspace, binary: &'work TestBinary) -> &'work Utf8Path {
    if binary.manifest_dir.as_str().is_empty() {
        &work.root
    } else {
        &binary.manifest_dir
    }
}

/// Runs one binary and reports what the harness said as well as how it ended.
pub(super) fn observe(work: &Workspace, binary: &TestBinary, attempt: Attempt<'_>) -> Observation {
    let progress = Arc::new(Mutex::new(Progress::new(watch(work))));
    let (verdict, usage) = run_with(work, binary, attempt, &progress);
    let quiet = quiet_of(&progress);

    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    let tests = progress.lock().unwrap().tests;

    Observation {
        verdict,
        quiet,
        tests,
        peak: usage.peak,
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
    deadline.saturating_duration_since(Instant::now())
}

/// The longest the wait loop will sleep without being woken.
///
/// A backstop, not the mechanism. Everything the loop reacts to signals it — a child that exits
/// closes its pipes, and a harness that announces a failure is read from them — so this bounds only
/// the case where a signal was never sent at all, and the case the loop must poll for because
/// nothing can announce it: silence.
const WAIT_CAP: Duration = Duration::from_millis(50);

/// Returns a live failure only when its announcement cannot precede a startup-error marker.
fn failure_to_cut_short(under_nextest: bool, progress: &Mutex<Progress>) -> Option<String> {
    (!under_nextest).then(|| announced_failure(progress)).flatten()
}

/// The first failure announcement, without treating it as a completed verdict.
fn announced_failure(progress: &Mutex<Progress>) -> Option<String> {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().failed.clone()
}

/// Whether the runtime has independently disqualified this test process as evidence.
fn environment_failure(progress: &Mutex<Progress>) -> bool {
    #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
    progress.lock().unwrap().environment_error
}

fn environment_verdict(progress: &Mutex<Progress>) -> Option<Verdict> {
    environment_failure(progress)
        .then(|| Verdict::Unmetered("the guard runtime could not acquire the process startup environment".to_owned()))
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
) -> io::Result<Receiver<(Vec<u8>, bool)>> {
    let (sink, drained) = mpsc::channel::<(Vec<u8>, bool)>();

    // Only the harness's authoritative stream may settle a verdict. The other stream still keeps
    // the stall clock alive and can carry the runtime's startup-error marker.
    let pipes = [
        subtree.take_stdout().map(|pipe| (Either::Out(pipe), !under_nextest)),
        subtree.take_stderr().map(|pipe| (Either::Err(pipe), under_nextest)),
    ];

    for (pipe, authoritative) in pipes.into_iter().flatten() {
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
            thread::Builder::new().name("cargo-gamma-output".to_owned()).spawn(move || {
                READERS.started();

                let collected = match pipe {
                    Either::Out(pipe) => drain(pipe, &published, &pulse, authoritative),
                    Either::Err(pipe) => drain(pipe, &published, &pulse, authoritative),
                };

                let _sent = sink.send(collected);

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

        let _handle = spawned?;
    }

    // Dropped so the receiver sees the readers' senders as the only ones left, and disconnects as
    // soon as they finish rather than waiting out the grace period on a sender nobody is using.
    drop(sink);

    Ok(drained)
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
/// The second half of the pair says whether the stream really ended. A failed read leaves a prefix
/// of what the binary said, and a prefix reads exactly like the whole of a quiet suite: the
/// first-failure scan finds no announcement and the mutant is recorded as killed by a test nobody
/// can name. This loop is the only place that can still tell the two apart, so it says so, and the
/// caller reports the gap rather than presenting the shortfall as the binary having stayed silent.
///
/// An interrupted read is not truncation and does not reach the failing arm: `read_until` retries
/// `EINTR` itself, and nothing was taken out of the pipe when it fired.
fn drain<R: Read>(pipe: R, progress: &Mutex<Progress>, pulse: &Pulse, authoritative: bool) -> (Vec<u8>, bool) {
    use std::io::BufRead as _;

    let mut reader = BufReader::new(pipe);
    let mut kept = Vec::new();
    let mut line = Vec::new();
    let mut whole = true;

    loop {
        line.clear();

        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return (kept, whole),

            // The partial line this read was building goes with the bytes it lost: half a line is
            // not something the watcher should be shown, and it could as easily be half an
            // announcement as half a progress bar.
            Err(_truncated) => return (kept, false),

            Ok(_read) => {
                // Published before the text is kept, so a binary past the cap still counts as
                // making progress. Silence is the signal, not volume.
                #[expect(clippy::unwrap_used, reason = "the watcher only panics if the process is unwinding")]
                let decisive = {
                    let mut progress = progress.lock().unwrap();
                    let before = progress.failed.is_some();
                    let environment_error = progress.environment_error;

                    let line = String::from_utf8_lossy(&line);
                    if authoritative {
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

                if kept_now < line.len() {
                    whole = false;
                }
            }
        }
    }
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

/// Returns the last `count` lines of some text.
pub(super) fn tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);

    lines.get(start..).unwrap_or_default().join("\n")
}

#[cfg(all(test, not(loom), not(miri)))]
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
mod tests {
    use super::*;

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

        let (kept, whole) = drain(Faltering(false), &progress, &pulse, true);

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

        let (kept, whole) = drain(io::Cursor::new(output), &progress, &pulse, true);

        assert_eq!(kept.len(), OUTPUT_CAP);
        assert!(!whole, "discarding bytes past the cap was reported as a whole stream");
        assert!(environment_failure(&progress), "the marker remains visible outside the output cap");
    }

    #[test]
    fn diagnostic_output_cannot_forge_a_libtest_failure() {
        let output = b"running 1 test\ntest forged::failure ... FAILED\n";
        let progress = Mutex::new(Progress::new(Watch::Libtest));
        let pulse = Pulse::default();

        let (_kept, whole) = drain(io::Cursor::new(output), &progress, &pulse, false);

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

    /// A verdict that is already evidence costs no confirmation here; `confirm_kill` owns that one.
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
            census: None,
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
        let (sink, drained) = mpsc::channel::<(Vec<u8>, bool)>();

        // One reader finished; the other is still holding the pipe open, so its sender never sends
        // and never drops.
        let _still_reading = sink.clone();
        let _sent = sink.send((b"running 1 test\n".to_vec(), true));

        drop(sink);

        let (text, whole) = collected(&drained, Duration::from_millis(50));

        assert!(!whole, "an absent reader was read as the end of the output");
        assert_eq!(String::from_utf8_lossy(&text), "running 1 test\n");
    }

    /// Every reader having sent and dropped its sender is the whole of the output, and the ordinary
    /// case must not be reported as partial or every run would warn about nothing.
    #[test]
    fn readers_that_all_returned_leave_the_collection_whole() {
        let (sink, drained) = mpsc::channel::<(Vec<u8>, bool)>();

        let _sent = sink.send((b"running 1 test\n".to_vec(), true));
        let _also = sink.send((b"test a::b ... FAILED\n".to_vec(), true));

        drop(sink);

        let (text, whole) = collected(&drained, Duration::from_secs(30));

        assert!(whole, "a complete collection was reported as partial");
        assert!(
            String::from_utf8_lossy(&text).contains("FAILED"),
            "{:?}",
            String::from_utf8_lossy(&text)
        );
    }

    /// A read interrupted by a signal is retried rather than treated as truncation.
    ///
    /// `EINTR` has taken nothing out of the pipe, so the next read gets it, and the retry comes
    /// from `read_until` rather than from this module. Pinned anyway because the truncation rule
    /// above it is what makes the distinction matter: were the interruption to start counting as a
    /// lost stream, every run the user so much as resizes a terminal during would report its
    /// verdicts as reached on partial text.
    #[test]
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

        let (kept, whole) = drain(Interrupted(0), &progress, &pulse, true);

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
                    census: None,
                },
                &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
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
                request: MemoryRequest { meter: false, limit: None },
                only: Only::All,
                census: None,
            },
            true,
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
                request: MemoryRequest { meter: true, limit: None },
                only: Only::All,
                census: None,
            },
            true,
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

        let verdict = run_binary(&work, &binary, plain_attempt(), true);

        assert_eq!(verdict, Verdict::Passed, "the retry must let the run go ahead");
    }

    /// A machine that refuses every attempt leaves the one mutant unjudged, not the run abandoned.
    ///
    /// The distinction is the whole point of the separate verdict: an hours-long sweep that meets a
    /// persistent shortage on one mutant keeps every verdict it has already reached and records why
    /// that one has none, rather than discarding the lot.
    #[test]
    fn a_spawn_refused_every_time_leaves_the_mutant_unjudged() {
        let (_directory, work) = scripted(&["exit:0"]);
        let binary = crate::testing::helper();
        let _refusals: Vec<_> = (0..SPAWN_ATTEMPTS).map(|_round| faults::arm(Fault::Spawn)).collect();

        let verdict = run_binary(&work, &binary, plain_attempt(), true);

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

        let verdict = run_binary(&work, &binary, plain_attempt(), true);

        assert!(matches!(verdict, Verdict::Unmetered(_)), "{verdict:?}");
        assert!(started.elapsed() < SPAWN_BACKOFF.saturating_mul(2), "{:?}", started.elapsed());
    }

    #[test]
    fn reader_thread_creation_failure_leaves_the_attempt_unjudged() {
        let (_directory, work) = scripted(&["flood:5000000", "exit:1"]);
        let binary = crate::testing::helper();
        let _refused = faults::arm(Fault::Thread);

        let verdict = run_binary(&work, &binary, plain_attempt(), true);

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
            request: MemoryRequest { meter: false, limit: None },
            only: Only::All,
            census: None,
        }
    }

    /// A host that cannot install the memory accounting a metered run asked for says so before
    /// anything is spawned, rather than running the binary unprotected.
    ///
    /// Spawning anyway would mean every verdict that follows is compared against a run the ceiling
    /// never actually watched, and a mutant that genuinely exhausted memory would simply be scored
    /// as an ordinary pass or failure with nothing left to explain why.
    #[test]
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
                request: MemoryRequest { meter: true, limit: None },
                only: Only::All,
                census: None,
            },
            true,
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
                census: None,
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
                    census: None,
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
                    census: None,
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
                    census: None,
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
                    census: None,
                },
                true
            ),
            Verdict::TimedOut
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
                census: None,
            },
            true,
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
                census: None,
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
                census: None,
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
                    census: None,
                },
                true
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
                    census: None,
                },
                true
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
                census: None,
            },
            true,
        );
        let took = started.elapsed();

        assert_eq!(verdict, Verdict::Failed(Some("a::b".to_owned())));
        assert!(took < Duration::from_secs(15), "the run waited for the whole binary: {took:?}");
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
                    census: None,
                },
                true
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
                    census: None,
                },
                true
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
                    census: None,
                },
                true
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
                    census: None,
                },
                true
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
                    census: None,
                },
                true
            ),
            Verdict::Passed
        );
    }

    /// A binary that exits non-zero fails, named by libtest's own report.
    #[test]
    fn a_failing_binary_is_named_by_its_first_failing_test() {
        // Failing only while a mutant is active is what a detection looks like, and it is what the
        // kill confirmation is there to establish: a binary that failed either way would be flaky.
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
                    census: None,
                },
                true
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
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
                census: None,
            },
            true,
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
                    budget: Some(Duration::from_secs(2)),
                },
                request: MemoryRequest {
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
                census: None,
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Libtest))),
        )
        .0;

        assert_memory_verdict_reports_its_figures(&verdict, limit);
    }

    /// A binary that stays under its ceiling is judged by its tests, not by its allocations.
    #[test]
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
                        meter: true,
                        limit: Some(512 * 1024 * 1024)
                    },
                    only: Only::All,
                    census: None,
                },
                true
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
    /// killer hint the next mutant would have probed with, and — because only `Failed` is routed to
    /// `confirm_kill` — skip the flake check for exactly the runs that raced the other way.
    ///
    /// Asserted here rather than through a launched binary because the state cannot be arranged on
    /// Linux: the ceiling sets `memory.oom.group`, so an OOM takes the whole subtree with it and
    /// there is no announcing process left to reach the early cut. Windows, where a job object
    /// refuses allocations rather than killing, is where the run really survives its ceiling.
    #[test]
    fn a_run_cut_short_by_a_named_failure_keeps_the_name_beside_the_ceiling() {
        let named = cut_by_named_failure("module::case".to_owned(), Some(300), Some(256));

        assert_eq!(named, Verdict::Failed(Some("module::case".to_owned())));

        // With no ceiling crossed there is only the failure, which is the ordinary shape of this.
        let plain = cut_by_named_failure("module::case".to_owned(), None, None);

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
                    meter: true,
                    limit: Some(limit),
                },
                only: Only::All,
                census: None,
            },
            false,
        );

        assert_eq!(verdict, Verdict::Failed(Some("failing::case".to_owned())));
    }

    /// An exoneration isolates the failing test rather than running the whole binary to completion,
    /// so a slow tail in the rest of the suite does not cause the confirmation to time out and
    /// misclassify the detection as flaky.
    #[test]
    fn an_exoneration_runs_only_the_failing_test_avoiding_suite_tail_timeouts() {
        let (_directory, work) = scripted(&[
            "when-env:GAMMA_ACTIVE|print:test first ... FAILED",
            "when-env:GAMMA_ACTIVE|exit:101",
            "when-arg:first|exit:0",
            "sleep:30000",
        ]);
        let binary = crate::testing::helper();

        let verdict = run_binary(
            &work,
            &binary,
            Attempt {
                active: Some(7),
                timeout: Some(Duration::from_millis(500)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
                census: None,
            },
            true,
        );

        assert_eq!(verdict, Verdict::Failed(Some("first".to_owned())));
    }

    /// An exoneration that ran out of time did not establish that the suite is green without the
    /// mutant, so it does not confirm the kill.
    ///
    /// The confirmation asks one question — does the suite still fail with the mutant switched off
    /// — and a run that exceeded its budget answered neither way. Reading it as "green without,
    /// red with" credits the suite with a detection on the strength of a scheduling hiccup, and it
    /// does so in the score-inflating direction, which is the one this tool must not be wrong in.
    #[test]
    fn an_exoneration_that_ran_out_of_time_does_not_confirm_a_kill() {
        // Fails only while a mutant is active; with none it runs long enough to outlive any budget
        // a test would give it, which is the shape the exoneration has to cope with.
        let (_directory, work) = scripted(&["when-env:GAMMA_ACTIVE|exit:101", "sleep:30000"]);
        let binary = crate::testing::helper();

        let verdict = confirm_kill(
            &work,
            &binary,
            Attempt {
                active: Some(7),
                timeout: Some(Duration::from_millis(200)),
                stall: Stall::NONE,
                request: MemoryRequest::default(),
                only: Only::All,
                census: None,
            },
            Some("a::b".to_owned()),
        );

        // Neither a kill nor a survivor, and the test that failed travels with it: that name is the
        // only thing anybody can act on here.
        assert_eq!(verdict, Verdict::Flaky(Some("a::b".to_owned())), "{verdict:?}");
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
                census: None,
            },
            "nextest could not list the tests".to_owned(),
        );

        assert_eq!(verdict, Verdict::Flaky(None), "{verdict:?}");
    }

    /// A test that fails with the mutant and fails again without it is flaky, not a detection.
    ///
    /// Scoring a failing suite as a kill with nothing at all having established that the mutant
    /// caused it would let one unreliable test manufacture a kill for every mutant it was run
    /// against, and inflate the score by however many that was.
    #[test]
    fn a_test_that_fails_without_the_mutant_too_is_not_a_kill() {
        let (_directory, work) = scripted(&["print:running 1 test", "print:test a::b ... FAILED", "exit:101"]);
        let flaky = crate::testing::helper();

        // Neither a kill nor a survivor: something failed, but not because of the mutant, and the
        // mutant is what was being judged. The test that failed both ways travels with the verdict
        // because fixing it is the only remedy anybody has here.
        assert_eq!(
            run_binary(
                &work,
                &flaky,
                Attempt {
                    active: Some(7),
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                    census: None,
                },
                true
            ),
            Verdict::Flaky(Some("a::b".to_owned()))
        );
    }

    /// Turning the confirmation off buys back the second run and gives up telling the two apart.
    ///
    /// The same binary that is flaky above is scored as a detection here, which is exactly the cost
    /// of the flag: nothing established that the mutant caused the failure, and without the
    /// confirming run nothing could have.
    #[test]
    fn a_run_that_declines_to_confirm_believes_the_failing_test() {
        let (_directory, work) = scripted(&["print:running 1 test", "print:test a::b ... FAILED", "exit:101"]);
        let flaky = crate::testing::helper();

        assert_eq!(
            run_binary(
                &work,
                &flaky,
                Attempt {
                    active: Some(7),
                    timeout: Some(Duration::from_secs(30)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                    census: None,
                },
                false
            ),
            Verdict::Failed(Some("a::b".to_owned()))
        );
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
                    census: None,
                },
                true
            ),
            Verdict::Failed(Some("a::b".to_owned()))
        );
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a\nb", 10), "a\nb");
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
        let (verdict, _usage) = settle(true, Some(7), Some(nextest::TEST_RUN_FAILED), output, MemoryUsage::default());

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
            MemoryUsage::default(),
        );
        let (baseline, _usage) = settle(true, None, Some(nextest::TEST_LIST_CREATION_FAILED), output, MemoryUsage::default());

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
        let (verdict, _usage) = settle(true, Some(7), Some(nextest::NO_TESTS_RUN), b"", MemoryUsage::default());

        assert!(matches!(verdict, Verdict::Unmetered(_)), "{verdict:?}");
    }

    /// Any other code is nextest reporting its own failure — a bad filterset, an unreadable tree, a
    /// signal. None of those is a statement about the mutant.
    #[test]
    fn a_code_nextest_uses_for_its_own_failures_is_not_a_verdict() {
        for code in [Some(94), Some(101), None] {
            let (verdict, _usage) = settle(true, Some(7), code, b"", MemoryUsage::default());

            assert!(matches!(verdict, Verdict::Unmetered(_)), "{code:?} gave {verdict:?}");
        }
    }

    /// The same codes mean nothing when the binaries are run directly: a binary is free to exit
    /// with any of them, and all a non-zero exit says is that the suite failed.
    #[test]
    fn a_directly_run_binary_is_judged_by_its_output_and_not_its_code() {
        let output = b"test module::case ... FAILED\n";

        for code in [Some(nextest::NO_TESTS_RUN), Some(101), None] {
            let (verdict, _usage) = settle(false, Some(7), code, output, MemoryUsage::default());

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
                MemoryUsage::default(),
            );

            assert!(
                matches!(verdict, Verdict::Unmetered(ref reason) if reason.contains("startup environment")),
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

        let (verdict, _usage) = settle(true, Some(7), Some(nextest::TEST_RUN_FAILED), &output, MemoryUsage::default());

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

        let reason = launcher(&work, &crate::testing::test_binary("/t/deps/stranger-def"), Only::All)
            .expect_err("an unknown binary cannot be launched");

        assert!(reason.contains("/t/deps/stranger-def"), "{reason}");
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
                request: MemoryRequest { meter: false, limit: None },
                only: Only::All,
                census: None,
            },
            &Arc::new(Mutex::new(Progress::new(Watch::Nextest))),
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
        let command = launcher(&work, &binary, Only::All).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(command.get_program(), binary.path.as_str());
        assert_eq!(args, vec!["--gamma-step=exit:0"]);
    }

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
        let command = launcher(&work, &binary, Only::One("tests::parses")).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--gamma-step=exit:0", "tests::parses", "--exact"]);
    }

    /// A run narrowed by a census asks for every test the census named, in one launch.
    ///
    /// libtest matches a test that any one of its positional filters matches, so several names mean
    /// "any of these" — and one `--exact` covers them all, which is why it comes last rather than
    /// once per name.
    #[test]
    fn a_run_narrowed_by_a_census_asks_for_all_of_its_tests_at_once() {
        let (_scratch, work) = crate::testing::helper_workspace("launch-censused", &["exit:0"]);
        let binary = crate::testing::helper();
        let names = ["tests::parses", "tests::rejects"];
        let command = launcher(&work, &binary, Only::These(&names)).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--gamma-step=exit:0", "tests::parses", "tests::rejects", "--exact"]);
    }

    /// An empty census selection is not a filter that matches nothing.
    ///
    /// It reaches here only from a caller that had no names to give, and a launch carrying just
    /// `--exact` would run the whole binary anyway — so the shape that says "everything" is the one
    /// with no filter at all, which is what the whole-binary path already produces.
    #[test]
    fn a_census_selection_with_no_names_runs_the_binary_whole() {
        let (_scratch, work) = crate::testing::helper_workspace("launch-censused-empty", &["exit:0"]);
        let binary = crate::testing::helper();
        let command = launcher(&work, &binary, Only::These(&[])).expect("a direct launch needs nothing from a runner");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--gamma-step=exit:0"]);
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

        let refusal = launcher(&work, &binary, Only::One("tests::excluded")).expect_err("the selection is empty");

        assert!(refusal.contains("harness filters allow"), "{refusal}");
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

        let command = launcher(&work, &binary, Only::One("tests::allowed_case")).expect("the selection is not empty");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["--skip", "slow", "tests::allowed_case", "--exact"]);
    }

    /// A census selection keeps the part the user's filter admits rather than being refused whole.
    #[test]
    fn a_census_selection_keeps_only_the_tests_the_users_filter_admits() {
        let (_scratch, mut work) = crate::testing::helper_workspace("launch-intersected", &["exit:0"]);
        let binary = crate::testing::helper();

        work.set_test_args(vec!["allowed".to_owned()]);

        let names = ["tests::allowed_one", "tests::excluded", "tests::allowed_two"];
        let command = launcher(&work, &binary, Only::These(&names)).expect("the selection is not empty");
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args, vec!["tests::allowed_one", "tests::allowed_two", "--exact"]);
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

    /// A binary that exits at once is judged at once, not one wait later.
    ///
    /// Measured over a run of launches, because a single one is dominated by process creation and
    /// would only be measuring the platform. If the reader threads ever stop signalling, each launch
    /// falls back to the cap and the total blows past this bound by an order of magnitude — which is
    /// exactly the regression worth catching, since it costs a wait per reachable binary per mutant.
    #[cfg_attr(
        any(coverage_nightly, windows),
        ignore = "coverage instrumentation or Windows process startup makes this wall-clock assertion meaningless"
    )]
    #[test]
    fn a_binary_that_exits_at_once_is_not_waited_on() {
        let (_directory, work) = scripted(&["exit:0"]);
        let quick = crate::testing::helper();
        let launches = 20;
        let started = Instant::now();

        for _ in 0..launches {
            let verdict = run_binary(
                &work,
                &quick,
                Attempt {
                    active: None,
                    timeout: Some(Duration::from_mins(1)),
                    stall: Stall::NONE,
                    request: MemoryRequest::default(),
                    only: Only::All,
                    census: None,
                },
                false,
            );

            assert!(matches!(verdict, Verdict::Passed), "{verdict:?}");
        }

        assert!(
            started.elapsed() < WAIT_CAP * launches,
            "{launches} instant launches took {:?}, which is at least a full wait each",
            started.elapsed()
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
                census: None,
            },
            false,
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
