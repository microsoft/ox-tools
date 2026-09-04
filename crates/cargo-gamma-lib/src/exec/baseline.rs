// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use cargo_gamma_process::MemoryRequest;
use serde_json::json;

use super::stall::Stall;
use super::test_binary::TestBinary;
use super::verdict::{Attempt, FailureEvidence, Observation, Only, Verdict, baseline_environment, observe_baseline, working_directory};
use super::workspace::Workspace;
use crate::Result;
use crate::error::{Error, error};
use crate::report::encode_controls;

/// What the baseline measured.
#[derive(Debug, Clone, Copy)]
pub(super) struct Baseline {
    /// How long the suite took, as the sum of what each binary in it took.
    ///
    /// The sum rather than the wall clock, because the binaries are measured many at a time while
    /// a mutant runs its own binaries one after another: what a mutant's budget has to cover is
    /// every binary it may have to visit, so the figure that budget is derived from has to be the
    /// same total. Wall clock over a concurrent measurement would be that total divided by however
    /// many workers happened to overlap, and every budget derived from it short by the same factor.
    pub(super) elapsed: Duration,

    /// How long the concurrent baseline measurement took on the wall clock.
    pub(super) wall: Duration,

    /// The longest the suite legitimately went quiet, which calibrates the stall budget.
    pub(super) quiet: Duration,

    /// How many tests ran, or `None` if no harness said.
    ///
    /// This is what ran, not what exists: `--test-package`, `--test-workspace` and any filter
    /// passed through to the harness all narrow it. That is the useful figure, since it is exactly
    /// the set of tests that will pass judgement on every mutant.
    pub(super) tests: Option<usize>,

    /// The largest peak memory any one binary reached, when the run asked for a measurement.
    ///
    /// The whole suite's peaks are not added up, because each binary is metered inside its own
    /// accounting boundary and later judged against a ceiling of its own: what a ceiling has to
    /// admit is the most any single one of them needed, not the sum of what all of them needed at
    /// different moments.
    pub(super) peak: Option<u64>,
}

/// How long one binary gets to produce a baseline before the run gives up on it.
///
/// Generous rather than calibrated, because there is nothing to calibrate from yet: this is the
/// measurement every later budget is derived from. It only ever fires on a suite that has hung.
const BASELINE_BUDGET: Duration = Duration::from_mins(10);

/// Runs the suite with no mutant active and returns how long it took.
///
/// `jobs` is the sweep's own concurrency, and the measurement is taken at exactly that width. A
/// baseline measured one binary at a time on an otherwise idle machine describes a situation that
/// never occurs again in the run: the timeouts derived from it are then spent by `jobs` workers
/// contending for the same cores, so they are systematically too tight, and the mutants that lose
/// that race are recorded as timeouts — which count as kills, and inflate the score with detections
/// the suite never made. Calibrating and spending under the same load makes every derived quantity
/// correct by construction rather than by a fudge factor.
pub(super) fn measure_baseline(
    work: &Workspace,
    binaries: &mut [TestBinary],
    request: MemoryRequest,
    jobs: usize,
    completed: impl FnMut(),
) -> Result<Baseline> {
    measure_within_reporting(work, binaries, BASELINE_BUDGET, request, jobs, completed)
}

/// Measures the baseline under an explicit budget.
///
/// The budget is a parameter so that the paths a hung or failing suite takes can be exercised
/// without waiting out the real one.
#[cfg(all(test, unix))]
fn measure_within(
    work: &Workspace,
    binaries: &mut [TestBinary],
    budget: Duration,
    request: MemoryRequest,
    jobs: usize,
) -> Result<Baseline> {
    measure_within_reporting(work, binaries, budget, request, jobs, || {})
}

fn measure_within_reporting(
    work: &Workspace,
    binaries: &mut [TestBinary],
    budget: Duration,
    request: MemoryRequest,
    jobs: usize,
    completed: impl FnMut(),
) -> Result<Baseline> {
    let started = Instant::now();
    let measured = sweep_binaries(work, binaries, budget, request, jobs, completed);
    let wall = started.elapsed();
    let mut elapsed = Duration::ZERO;
    let mut quiet = Duration::ZERO;
    let mut tests: Option<usize> = None;
    let mut peak: Option<u64> = None;

    // Folded in the binaries' own order rather than in the order the workers happened to finish, so
    // that which failure a red suite reports does not depend on the scheduler.
    for (entry, taken) in binaries.iter_mut().zip(measured) {
        let Some((took, observed)) = taken else {
            continue;
        };

        entry.baseline = took;
        entry.peak = observed.peak;
        entry.tests = observed.tests;
        elapsed = elapsed.saturating_add(took);
        quiet = quiet.max(observed.quiet);

        if let Some(measured) = observed.peak {
            peak = Some(peak.unwrap_or(0).max(measured));
        }

        // A binary with no harness contributes nothing rather than turning the total into a
        // guess, but one binary reporting is enough for the total to be worth stating.
        if let Some(counted) = observed.tests {
            tests = Some(tests.unwrap_or(0).saturating_add(counted));
        }

        let failure = observed.failure.as_ref();

        match observed.verdict {
            Verdict::Passed => {}
            // Every non-passing observation is fatal here where some are not during the sweep:
            // there is no mutant to record it against, and a baseline binary that went unmeasured
            // leaves every mutant that binary covers without a budget to be judged against.
            verdict => {
                return Err(baseline_failure_error(work, entry, took, budget, &verdict, failure));
            }
        }
    }

    Ok(Baseline {
        elapsed,
        wall,
        quiet,
        tests,
        peak,
    })
}

/// Runs every binary once with no mutant active, `jobs` of them at a time.
///
/// Returns what each one produced, positionally, so the caller can fold the results in the order
/// the binaries were given rather than the order they finished.
fn sweep_binaries(
    work: &Workspace,
    binaries: &[TestBinary],
    budget: Duration,
    request: MemoryRequest,
    jobs: usize,
    completed: impl FnMut(),
) -> Vec<Option<(Duration, Observation)>> {
    sweep_binaries_with(work, binaries, budget, request, jobs, observe_baseline, completed)
}

fn sweep_binaries_with<O>(
    work: &Workspace,
    binaries: &[TestBinary],
    budget: Duration,
    request: MemoryRequest,
    jobs: usize,
    observer: O,
    mut completed: impl FnMut(),
) -> Vec<Option<(Duration, Observation)>>
where
    O: Fn(&Workspace, &TestBinary, Attempt<'_>) -> Observation + Sync,
{
    let mut measured: Vec<Option<(Duration, Observation)>> = (0..binaries.len()).map(|_index| None).collect();
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel::<(usize, Duration, Observation)>();
    let notes = crate::notes::current();

    thread::scope(|scope| {
        for _worker in 0..jobs.max(1) {
            let sender = sender.clone();
            let next = &next;
            let notes = notes.clone();
            let observer = &observer;

            let _handle = scope.spawn(move || {
                let _notes = crate::notes::enter(notes.as_ref());

                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);

                    let Some(binary) = binaries.get(index) else {
                        break;
                    };

                    let began = Instant::now();
                    let observation = observer(
                        work,
                        binary,
                        Attempt {
                            active: None,
                            timeout: Some(budget),
                            stall: Stall::NONE,
                            request,
                            only: Only::All,
                            census: None,
                        },
                    );

                    // A closed receiver means the calling thread is gone, which cannot happen while
                    // the scope is open.
                    let _sent = sender.send((index, began.elapsed(), observation));
                }
            });
        }

        // The workers hold the only remaining senders, so the drain ends when the last one finishes.
        drop(sender);

        for (index, took, observed) in receiver {
            if let Some(slot) = measured.get_mut(index) {
                *slot = Some((took, observed));
            }

            completed();
        }
    });

    measured
}

fn baseline_failure_error(
    work: &Workspace,
    binary: &TestBinary,
    elapsed: Duration,
    budget: Duration,
    verdict: &Verdict,
    evidence: Option<&FailureEvidence>,
) -> Error {
    let runner = if work.runner().is_some() { "nextest" } else { "libtest" };
    let directory = working_directory(work, binary);
    let (kind, test, last_test, reason, limit) = match verdict {
        Verdict::Failed(test) | Verdict::Flaky(test) => ("testFailure", test.as_deref(), None, None, None),
        Verdict::TestEnumerationFailed(reason) => ("enumerationFailure", None, None, Some(reason.as_str()), None),
        Verdict::TimedOut => (
            "timeout",
            None,
            None,
            Some("the test binary exceeded its baseline time budget"),
            None,
        ),
        Verdict::Stalled(test) => (
            "stall",
            None,
            test.as_deref(),
            Some("the test binary stopped reporting progress"),
            None,
        ),
        Verdict::MemoryLimit { limit, .. } => (
            "memoryLimit",
            None,
            None,
            Some("the test workload exceeded its configured baseline memory ceiling"),
            Some(*limit),
        ),
        Verdict::Unmetered(reason) | Verdict::Unjudged(reason) => ("infrastructureFailure", None, None, Some(reason.as_str()), None),
        Verdict::Passed => unreachable!("a passing baseline never constructs a failure"),
    };

    let peak = match verdict {
        Verdict::MemoryLimit { peak, .. } => *peak,
        _other => None,
    };
    let termination = evidence.and_then(|evidence| evidence.termination);
    let stdout = evidence.map_or("", |evidence| evidence.stdout_tail.as_str());
    let stderr = evidence.map_or("", |evidence| evidence.stderr_tail.as_str());
    let artifact_stdout = encode_controls(stdout);
    let artifact_stderr = encode_controls(stderr);
    let output_truncated = evidence.is_some_and(|evidence| evidence.output_truncated);
    let elapsed_ms = duration_ms(elapsed);
    let budget_ms = duration_ms(budget);
    let artifact = json!({
        "schemaVersion": 1,
        "kind": kind,
        "package": binary.package,
        "packageId": binary.package_id,
        "target": binary.target,
        "runner": runner,
        "executable": binary.path,
        "workingDirectory": directory,
        "environmentOverrides": baseline_environment(work),
        "test": test,
        "lastObservedTest": last_test,
        "termination": termination,
        "elapsedMs": elapsed_ms,
        "budgetMs": budget_ms,
        "peakBytes": peak,
        "memoryLimitBytes": limit,
        "reason": reason,
        "stdoutTail": artifact_stdout,
        "stderrTail": artifact_stderr,
        "outputTruncated": output_truncated,
    });

    let mut message = format!(
        "the baseline could not be measured\n\n\
         Every verdict in a run is a comparison against the baseline, so there is nothing to \
         measure until this failure is resolved.\n\n\
         Package:       {}\n\
         Target:        {}\n\
         Runner:        {runner}\n\
         Executable:    {}\n\
         Working dir:   {}\n\
         Failure:       {}",
        encode_controls(&binary.package),
        encode_controls(&binary.target),
        encode_controls(binary.path.as_str()),
        encode_controls(directory.as_str()),
        baseline_failure_description(verdict, budget),
    );

    if let Some(termination) = termination {
        let _ = write!(message, "\nTermination:   {termination}");
    }

    let _ = write!(message, "\nElapsed:       {elapsed:.2?}");

    match verdict {
        Verdict::MemoryLimit { .. } => {
            message.push_str("\nGuidance:      Raise `--baseline-memory-limit` if this workload is expected.");
        }
        _other => {}
    }

    error!("{message}").with_artifact("baseline-failure.json", artifact)
}

fn baseline_failure_description(verdict: &Verdict, budget: Duration) -> String {
    match verdict {
        Verdict::Failed(Some(test)) | Verdict::Flaky(Some(test)) => {
            format!("test `{}` failed", encode_controls(test))
        }
        Verdict::Failed(None) | Verdict::Flaky(None) => "a test failed without naming itself".to_owned(),
        Verdict::TestEnumerationFailed(_) => "nextest could not enumerate the selected tests".to_owned(),
        Verdict::TimedOut => format!("the binary did not finish within {budget:.0?}"),
        Verdict::Stalled(Some(test)) => {
            format!("the binary stopped reporting progress after `{}`", encode_controls(test))
        }
        Verdict::Stalled(None) => "the binary stopped reporting progress".to_owned(),
        Verdict::MemoryLimit { peak, limit } => {
            let reached = peak.map_or_else(String::new, |peak| format!(", reaching {}", crate::report::bytes(peak)));

            format!("the workload exceeded its {} memory ceiling{reached}", crate::report::bytes(*limit))
        }
        Verdict::Unmetered(reason) => {
            format!(
                "the baseline could not be measured as this run was configured: {}",
                encode_controls(reason)
            )
        }
        Verdict::Unjudged(reason) => {
            format!("the machine would not run the baseline: {}", encode_controls(reason))
        }
        Verdict::Passed => unreachable!("a passing baseline has no failure description"),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn a_partial_stream_note_from_a_baseline_worker_reaches_the_parent() {
        crate::notes::alone(|| {
            let (_directory, work) = crate::testing::helper_workspace("baseline-worker-note", &[]);
            let binary = crate::testing::helper();

            let measured = sweep_binaries_with(
                &work,
                &[binary],
                Duration::from_secs(30),
                MemoryRequest::default(),
                1,
                |_work, _binary, _attempt| {
                    crate::notes::note("baseline binary produced a partial stream");

                    Observation {
                        verdict: Verdict::Passed,
                        failure: None,
                        quiet: Duration::ZERO,
                        tests: Some(1),
                        peak: None,
                    }
                },
                || {},
            );

            assert_eq!(measured.len(), 1);
            assert!(
                crate::notes::drain().iter().any(|note| note.contains("partial")),
                "the worker's partial-stream diagnostic was lost"
            );
        });
    }

    /// Wraps an existing directory as a workspace whose "test binary" is `/bin/sh`.
    ///
    /// The script is passed as an argument rather than written to disk: a file made executable
    /// while other threads are forking can be refused with `ETXTBSY`, which would make these
    /// tests intermittently fail for a reason that has nothing to do with what they assert.
    #[cfg(unix)]
    fn harness(body: &str) -> (tempfile::TempDir, Workspace, Vec<TestBinary>) {
        let (directory, work) = crate::testing::shell_workspace("baseline", body);
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::test_binary("/bin/sh")
        }];

        (directory, work, binaries)
    }

    fn diagnostic_harness() -> (tempfile::TempDir, Workspace, Vec<TestBinary>) {
        let (directory, work) = crate::testing::helper_workspace("baseline-diagnostic", &["exit:0"]);
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            target: "unit".to_owned(),
            ..crate::testing::helper()
        }];

        (directory, work, binaries)
    }

    /// A suite that hangs before any mutant is applied stops the run.
    #[test]
    #[cfg(unix)]
    fn a_baseline_that_never_finishes_stops_the_run() {
        let (_directory, work, mut binaries) = harness("sleep 30");
        let failure = measure_within(&work, &mut binaries, Duration::from_millis(50), MemoryRequest::default(), 1)
            .expect_err("the baseline must fail");

        // Continuing would time out every mutant against a suite that never finishes and report a
        // perfect score built entirely out of false detections.
        assert!(failure.to_string().contains("did not finish"), "{failure}");
    }

    /// A suite that is already failing stops the run, naming the test.
    #[test]
    #[cfg(unix)]
    fn a_red_baseline_stops_the_run_and_names_the_failing_test() {
        let (_directory, work, mut binaries) = harness(
            "echo 'test a::b ... FAILED'\n\
             echo 'assertion failed: left != right' >&2\n\
             exit 101",
        );
        let failure =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 1).expect_err("the baseline must fail");

        // Every verdict is a comparison against the baseline, so a red one makes every mutant
        // look killed by a failure that was there before mutation started.
        let message = failure.to_string();

        assert!(message.contains("test `a::b`"), "{message}");
        assert!(message.contains("exit code 101"), "{message}");
        assert!(!message.contains("test a::b ... FAILED"), "{message}");
        assert!(!message.contains("assertion failed: left != right"), "{message}");

        let artifact = failure.artifact().expect("a baseline failure carries a durable record");

        assert_eq!(artifact.file_name, "baseline-failure.json");
        assert_eq!(artifact.value["package"], "subject");
        assert_eq!(artifact.value["test"], "a::b");
        assert_eq!(artifact.value["termination"]["kind"], "exitCode");
        assert_eq!(artifact.value["termination"]["value"], 101);
        assert!(
            artifact.value["environmentOverrides"]["set"].get("RUST_MIN_STACK").is_none(),
            "an inherited stack value must not be serialized"
        );
        assert_eq!(artifact.value["environmentOverrides"]["valuesOmitted"][0], "RUST_MIN_STACK");
        assert!(artifact.value["stdoutTail"].as_str().is_some_and(|text| text.contains("FAILED")));
        assert!(
            artifact.value["stderrTail"]
                .as_str()
                .is_some_and(|text| text.contains("assertion failed"))
        );
    }

    /// A green suite yields the elapsed time and the harness's own test count.
    #[test]
    #[cfg(unix)]
    fn a_green_baseline_reports_the_elapsed_time_and_the_test_count() {
        let (_directory, work, mut binaries) = harness("echo 'running 3 tests'\necho 'test a::b ... ok'\nexit 0");
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 1).expect("the baseline must pass");

        // The per-binary baseline is what apportions each mutant's budget, so it has to be
        // written back rather than merely totalled.
        assert_eq!(baseline.tests, Some(3));
        assert_eq!(binaries[0].tests, Some(3));
        assert!(binaries[0].baseline > Duration::ZERO);
    }

    /// A suite of several binaries is measured with as many running at once as the sweep will use,
    /// and the total it reports is the sum of the parts rather than the wall clock over them.
    ///
    /// Regression, issue-016. Measured one at a time on an idle machine, every budget derived from
    /// the baseline describes a machine the run never sees again — the sweep spends those budgets
    /// with `jobs` binaries contending for the same cores, and the mutants that lose the race are
    /// recorded as timeouts, which count as kills and inflate the score. Wall clock over a
    /// concurrent measurement would be wrong in the same direction for a different reason: a mutant
    /// runs its own binaries one after another, so the budget has to cover their sum.
    #[test]
    #[cfg(unix)]
    fn a_baseline_is_measured_at_the_concurrency_the_sweep_will_use() {
        let (_directory, work) = crate::testing::shell_workspace("baseline-jobs", "sleep 0.4\nexit 0");
        let mut binaries: Vec<TestBinary> = (0..4)
            .map(|_index| TestBinary {
                package: "subject".to_owned(),
                ..crate::testing::test_binary("/bin/sh")
            })
            .collect();

        let began = Instant::now();
        let mut completed = 0;
        let baseline = measure_within_reporting(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 4, || {
            completed += 1;
        })
        .expect("the baseline passes");
        let wall = began.elapsed();

        let summed: Duration = binaries.iter().map(|binary| binary.baseline).sum();

        assert_eq!(baseline.elapsed, summed);
        assert_eq!(completed, binaries.len(), "each completed binary advances progress exactly once");
        assert!(binaries.iter().all(|binary| binary.baseline > Duration::ZERO));

        // Four four-hundred-millisecond sleeps run four at a time cannot take the 1.6 seconds a
        // serial measurement would; the reported total nevertheless has to be that sum.
        assert!(wall < summed, "wall {wall:?} against summed {summed:?}");
        assert!(
            baseline.wall < summed,
            "recorded wall {:?} against summed {summed:?}",
            baseline.wall
        );
        assert!(
            baseline.wall <= wall,
            "recorded wall {:?} against caller wall {wall:?}",
            baseline.wall
        );
    }

    #[test]
    fn baseline_progress_advances_once_for_every_finished_binary() {
        let (_directory, work) = crate::testing::helper_workspace("baseline-progress", &["exit:0"]);
        let mut binaries: Vec<TestBinary> = (0..3)
            .map(|_index| TestBinary {
                package: "subject".to_owned(),
                ..crate::testing::helper()
            })
            .collect();
        let mut completed = 0;

        let _baseline = measure_within_reporting(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 2, || {
            completed += 1;
        })
        .expect("the baseline passes");

        assert_eq!(completed, binaries.len());
    }

    /// A red binary is reported whichever worker happened to reach it first.
    ///
    /// With more than one binary in flight the order results arrive in is the scheduler's business,
    /// so the failure a run reports is folded in the binaries' own order instead — otherwise the
    /// same red suite would name a different test from run to run.
    #[test]
    #[cfg(unix)]
    fn a_red_binary_is_reported_whatever_order_the_workers_finished_in() {
        let (_directory, work) = crate::testing::shell_workspace("baseline-order", "echo 'test a::b ... FAILED'\nexit 101");
        let mut binaries: Vec<TestBinary> = (0..4)
            .map(|_index| TestBinary {
                package: "subject".to_owned(),
                ..crate::testing::test_binary("/bin/sh")
            })
            .collect();

        let failure =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 4).expect_err("the baseline must fail");

        assert!(failure.to_string().contains("test `a::b`"), "{failure}");
    }

    /// A binary whose harness announced no tests records that, so nothing later mistakes it for a
    /// binary that could have convicted something.
    ///
    /// Regression, issue-011. Cargo emits a unit-test binary for every lib target whether or not it
    /// holds a test, so the existence of a binary says nothing; the announced count is the only
    /// evidence the run has that a package has no tests at all.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_announced_no_tests_records_the_zero_rather_than_nothing() {
        let (_directory, work, mut binaries) = harness("echo 'running 0 tests'\nexit 0");
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 1).expect("the baseline passes");

        assert_eq!(binaries[0].tests, Some(0));
        assert_eq!(baseline.tests, Some(0));
    }

    /// A binary whose harness announces nothing still contributes its time.
    #[test]
    #[cfg(unix)]
    fn a_baseline_with_no_harness_count_still_measures_the_time() {
        let (_directory, work, mut binaries) = harness("echo 'custom harness'\nexit 0");
        let baseline =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 1).expect("the baseline must pass");

        // A custom harness is not a broken one; inventing a count would be worse than omitting it.
        assert_eq!(baseline.tests, None);
    }

    /// A suite that fails without ever printing a recognisable `test ... FAILED` line still stops
    /// the run, just without naming a specific test.
    ///
    /// A custom harness that exits non-zero has failed just as surely as one that names a test,
    /// and inventing a name for it would be worse than admitting the run only knows "a test" broke;
    /// a baseline stage that instead pressed on would judge every mutant against a suite that was
    /// already red for a reason nobody could see.
    #[test]
    #[cfg(unix)]
    fn a_baseline_that_fails_without_naming_a_test_still_stops_the_run() {
        let (_directory, work, mut binaries) = harness("exit 1");
        let failure =
            measure_within(&work, &mut binaries, Duration::from_secs(30), MemoryRequest::default(), 1).expect_err("the baseline must fail");

        assert!(failure.to_string().contains("a test"), "{failure}");
    }

    /// A baseline binary that outgrows the ceiling placed around the calibration itself is
    /// reported as such, not merely timed out or silently truncated.
    ///
    /// The baseline has no mutant to blame, so if this path were not wired up a runaway baseline
    /// would either hang the whole run past the timeout or, worse, get judged as if it were an
    /// ordinary passing suite while quietly starving the machine of memory.
    #[test]
    #[cfg(unix)]
    fn a_baseline_binary_that_outgrows_its_ceiling_is_reported_as_such() {
        if crate::testing::without_memory_support("a baseline measuring the suite's memory") {
            return;
        }

        let fill = format!("/dev/shm/gamma-baseline.{}", std::process::id());
        let (_directory, work, mut binaries) = harness(&format!("dd if=/dev/zero of={fill} bs=1M count=512 2>/dev/null"));
        let request = MemoryRequest {
            meter: true,
            limit: Some(32 * 1024 * 1024),
        };

        let failure = measure_within(&work, &mut binaries, Duration::from_mins(1), request, 1)
            .expect_err("a baseline that outgrows its ceiling must be reported, not merely timed out");

        let _removed = std::fs::remove_file(&fill);

        assert!(failure.to_string().contains("--baseline-memory-limit"), "{failure}");
    }

    /// A host that cannot install the memory accounting a metered baseline asked for stops the
    /// run rather than measure the suite unprotected.
    ///
    /// Reporting a "green" baseline that was never actually metered would mean every mutant
    /// afterwards is silently compared against an unprotected run, hiding the exact failure mode
    /// the ceiling was meant to catch.
    #[test]
    #[cfg(unix)]
    fn an_undelegated_host_stops_a_metered_baseline_rather_than_measure_it_unprotected() {
        if crate::exec::memory::support().is_ok() {
            return;
        }

        let (_directory, work, mut binaries) = harness("exit 0");
        let request = MemoryRequest { meter: true, limit: None };

        let failure = measure_within(&work, &mut binaries, Duration::from_secs(30), request, 1)
            .expect_err("a host that cannot meter memory must not measure a baseline unprotected");

        // The wrapper has to say the baseline could not be measured as configured, and it has to
        // carry the underlying cause through rather than replacing it.
        assert!(failure.to_string().contains("as this run was configured"), "{failure}");
        assert!(failure.to_string().contains("cgroup"), "{failure}");
    }

    /// A metered baseline writes each binary's peak back, which is what a ceiling is derived from.
    #[test]
    #[cfg(unix)]
    fn a_metered_baseline_records_what_each_binary_used() {
        if crate::testing::without_memory_support("a baseline measuring the suite's memory") {
            return;
        }

        let (_directory, work, mut binaries) = harness("dd if=/dev/zero of=/dev/null bs=1M count=32 2>/dev/null\nexit 0");
        let request = MemoryRequest { meter: true, limit: None };
        let baseline = measure_within(&work, &mut binaries, Duration::from_secs(30), request, 1).expect("the baseline must pass");

        // A ceiling is derived per binary, so the per-binary figure has to be written back and not
        // merely totalled; a run that only kept the total would bound every binary by the largest.
        assert!(binaries[0].peak.is_some(), "{:?}", binaries[0].peak);
        assert_eq!(baseline.peak, binaries[0].peak);
    }

    /// A baseline that outgrows its own explicit ceiling stops the run and says which number to move.
    #[test]
    fn a_baseline_that_outgrows_its_ceiling_names_the_ceiling() {
        let (_directory, work, binaries) = diagnostic_harness();
        let cause = baseline_failure_error(
            &work,
            &binaries[0],
            Duration::from_secs(1),
            Duration::from_secs(30),
            &Verdict::MemoryLimit {
                peak: Some(300 * 1024 * 1024),
                limit: 256 * 1024 * 1024,
            },
            None,
        )
        .to_string();

        // The user set this ceiling themselves, and no mutant is involved, so the message has to
        // point at the flag rather than read like a mutant was caught.
        assert!(cause.contains("unit"), "{cause}");
        assert!(cause.contains("--baseline-memory-limit"), "{cause}");
        assert!(cause.contains("256.0 MB"), "{cause}");
        assert!(cause.contains("300.0 MB"), "{cause}");
    }

    #[test]
    fn a_baseline_timeout_names_the_binary_that_stopped_progress() {
        let (_directory, work, binaries) = diagnostic_harness();
        let evidence = FailureEvidence {
            termination: None,
            stdout_tail: "\u{1b}[31mred\u{1b}[0m".to_owned(),
            stderr_tail: String::new(),
            output_truncated: false,
        };
        let failure = baseline_failure_error(
            &work,
            &binaries[0],
            Duration::from_secs(10),
            Duration::from_mins(10),
            &Verdict::TimedOut,
            Some(&evidence),
        );
        let cause = failure.to_string();

        // A timeout before mutants run is a property of the fixed suite, so the message must point
        // at the binary the user can run directly.
        assert!(cause.contains("unit"), "{cause}");
        assert!(cause.contains("600s"), "{cause}");
        assert_eq!(
            failure.artifact().expect("a baseline timeout carries a durable record").value["stdoutTail"],
            "\\e[31mred\\e[0m"
        );
    }
}
