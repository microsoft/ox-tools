// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::time::Instant;

use super::nextest;

/// How a failure is recognised in a harness's output, if it can be recognised safely at all.
///
/// Reading a failure out of the output as it arrives lets a mutant be convicted the moment the
/// first test catches it, instead of after the whole binary has run. That is only sound where a
/// line announcing a failure could not have been written by a test rather than by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Watch {
    /// Take no verdict from the output; wait for the process to exit and judge that.
    ///
    /// The safe reading, and the necessary one wherever a test's own writing reaches the same
    /// stream as the harness's.
    Off,

    /// libtest's `test <name> ... FAILED`.
    Libtest,

    /// Nextest's `FAIL [   0.024s] (1/2) <binary> <name>`.
    Nextest,
}

/// What the harness has said so far, published by the reader for the waiting thread to watch.
#[derive(Debug)]
pub(super) struct Progress {
    /// How to recognise a failure announcement, if this run's output can be trusted to carry one.
    watch: Watch,

    /// The first test the harness announced as failed, once one has been announced.
    ///
    /// The first is kept rather than the last because it is the one that convicted the mutant, and
    /// it is what the run would have reported had it read the output to exhaustion.
    pub(super) failed: Option<String>,

    /// Whether the guard runtime reported that process startup could not establish a selection.
    pub(super) environment_error: bool,

    /// When the harness last said anything at all.
    pub(super) heard: Instant,

    /// The longest silence so far.
    ///
    /// Calibrated from the baseline, this is how long the suite legitimately goes quiet while its
    /// slowest test runs, and it becomes the basis for the stall budget.
    pub(super) quiet: Duration,

    /// The last test the harness named.
    pub(super) test: Option<String>,

    /// How many tests the harness said it was about to run, summed over every suite in the binary.
    ///
    /// A binary holds one suite per `#[cfg(test)]` module tree plus the doc tests, and each
    /// announces itself separately, so the figure is a running total rather than the last one
    /// seen. `None` until something announces anything: a target built with `harness = false`
    /// prints whatever it likes and must contribute nothing rather than a confident zero.
    pub(super) tests: Option<usize>,
}

impl Progress {
    pub(super) fn new(watch: Watch) -> Self {
        Self {
            watch,
            failed: None,
            environment_error: false,
            heard: Instant::now(),
            quiet: Duration::ZERO,
            test: None,
            tests: None,
        }
    }

    /// Records that the harness produced a line, and how long it had been silent beforehand.
    pub(super) fn heard(&mut self, line: &str) {
        self.note_activity(line);

        if let Some(rest) = line.strip_prefix("test ")
            && let Some((name, verdict)) = rest.split_once(" ... ")
        {
            let name = name.trim();

            // Overwritten in place rather than reallocated: every result line a suite prints lands
            // here, and only the last one survives to name the test that was running when a budget
            // or a ceiling cut the process short.
            match &mut self.test {
                Some(previous) => {
                    previous.clear();
                    previous.push_str(name);
                }
                None => self.test = Some(name.to_owned()),
            }

            // Only once libtest has announced a suite. A target built with `harness = false`
            // prints whatever it likes, and its output must not be read as a verdict it never gave.
            if self.watch == Watch::Libtest && self.tests.is_some() && verdict.trim() == "FAILED" {
                self.note(name);
            }

            return;
        }

        if self.watch == Watch::Nextest
            && let Some(name) = nextest::first_failure(line)
        {
            self.note(name);
        }

        // libtest announces each suite with `running N tests`, which is the only place the size of
        // the run is stated. Counting the result lines instead would miss tests that were filtered
        // out and would double-count a harness that reports progress more than once.
        if let Some(count) = line
            .trim()
            .strip_prefix("running ")
            .and_then(|rest| rest.strip_suffix(" tests").or_else(|| rest.strip_suffix(" test")))
            .and_then(|count| count.trim().parse::<usize>().ok())
        {
            self.tests = Some(self.tests.unwrap_or(0).saturating_add(count));
        }
    }

    /// Records activity and runtime diagnostics from a non-authoritative output stream.
    pub(super) fn heard_diagnostic(&mut self, line: &str) {
        self.note_activity(line);
    }

    fn note_activity(&mut self, line: &str) {
        let now = Instant::now();

        self.quiet = self.quiet.max(now.saturating_duration_since(self.heard));

        self.heard = now;
        self.environment_error |= runtime_startup_failure(line.as_bytes());
    }

    /// Remembers the first failure announced, leaving any later one alone.
    fn note(&mut self, name: &str) {
        if self.failed.is_none() && !name.is_empty() {
            self.failed = Some(name.to_owned());
        }
    }
}

pub(super) fn runtime_startup_failure(text: &[u8]) -> bool {
    [gamma_rt::ENVIRONMENT_ERROR_MARKER, gamma_rt::PRE_INSTALL_ERROR_MARKER]
        .into_iter()
        .any(|marker| text.windows(marker.len()).any(|window| window == marker))
}

#[cfg(all(test, not(miri)))]
mod fuzz {
    use super::{Progress, Watch};
    use crate::testing::{spliced, token};

    /// No sequence of lines panics the reader, whichever harness it believes it is watching.
    ///
    /// This one is fed a live stream rather than a buffer, so it carries state across lines and is
    /// the only parser here where the order of inputs can matter. A panic would land on the thread
    /// draining a child's pipe, which is the thread that must keep draining for the child not to
    /// block forever in `write`.
    #[test]
    fn arbitrary_output_never_panics_the_reader() {
        bolero::check!().with_type::<Vec<String>>().for_each(|lines| {
            for watch in [Watch::Off, Watch::Libtest, Watch::Nextest] {
                let mut progress = Progress::new(watch);

                for line in lines {
                    progress.heard(line);
                }
            }
        });
    }

    /// A libtest failure is noticed whatever the surrounding output, once a suite has announced.
    #[test]
    fn a_libtest_failure_is_never_lost_among_arbitrary_output() {
        bolero::check!()
            .with_type::<(Vec<String>, String, usize)>()
            .for_each(|(noise, name, at)| {
                let output = spliced(noise, &format!("test {} ... FAILED", token(name)), *at);
                let mut progress = Progress::new(Watch::Libtest);

                progress.heard("running 1 test");

                for line in output.lines() {
                    progress.heard(line);
                }

                assert!(progress.failed.is_some(), "the failure was lost in {output:?}");
            });
    }

    /// A target that never announced a suite is never convicted by its own output.
    ///
    /// A `harness = false` target prints whatever its author chose, which can include text shaped
    /// exactly like libtest's. Reading that as a verdict would convict a mutant on the strength of
    /// a `println!`, so the absence of a `running N tests` line has to hold whatever follows it.
    #[test]
    fn output_from_a_target_that_announced_no_suite_is_never_a_verdict() {
        bolero::check!()
            .with_type::<(Vec<String>, String, usize)>()
            .for_each(|(noise, name, at)| {
                let output = spliced(noise, &format!("test {} ... FAILED", token(name)), *at);
                let mut progress = Progress::new(Watch::Libtest);

                for line in output.lines() {
                    // Anything that announces a suite makes the rest legitimate, which is a different
                    // case than the one under test here.
                    if line.trim().starts_with("running ") {
                        return;
                    }

                    progress.heard(line);
                }

                assert!(progress.failed.is_none(), "an unannounced target was convicted by {output:?}");
            });
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn the_harness_naming_a_test_records_it() {
        let mut progress = Progress::new(Watch::Off);

        progress.heard("test tests::the_boundary_is_pinned ... ok\n");

        assert_eq!(progress.test.as_deref(), Some("tests::the_boundary_is_pinned"));
    }

    #[test]
    fn a_line_that_is_not_a_test_result_does_not_rename_the_test() {
        let mut progress = Progress::new(Watch::Off);

        progress.heard("test tests::first ... ok\n");
        progress.heard("running 3 tests\n");
        progress.heard("some output from the test itself\n");

        assert_eq!(progress.test.as_deref(), Some("tests::first"));
    }

    #[test]
    fn the_size_of_each_suite_is_summed() {
        // One binary announces its unit tests and its doc tests separately.
        let mut progress = Progress::new(Watch::Off);

        progress.heard("running 12 tests\n");
        progress.heard("test a ... ok\n");
        progress.heard("running 3 tests\n");

        assert_eq!(progress.tests, Some(15));
    }

    #[test]
    fn a_single_test_is_announced_in_the_singular() {
        let mut progress = Progress::new(Watch::Off);

        progress.heard("running 1 test\n");

        assert_eq!(progress.tests, Some(1));
    }

    #[test]
    fn an_empty_suite_still_counts_as_having_reported() {
        // `running 0 tests` is what every target without tests prints, and is not the same as a
        // harness that said nothing at all.
        let mut progress = Progress::new(Watch::Off);

        progress.heard("running 0 tests\n");

        assert_eq!(progress.tests, Some(0));
    }

    #[test]
    fn a_harness_that_announces_nothing_reports_no_count() {
        // A target with `harness = false` prints whatever it likes. Guessing zero would understate
        // a suite that really did run.
        let mut progress = Progress::new(Watch::Off);

        progress.heard("Running my own tests\n");
        progress.heard("all good\n");

        assert_eq!(progress.tests, None);
    }

    /// libtest announcing a failure is recorded, so the run can stop at the test that caught it.
    #[test]
    fn a_libtest_failure_is_recorded_as_soon_as_it_is_announced() {
        let mut progress = Progress::new(Watch::Libtest);

        progress.heard("running 2 tests\n");
        progress.heard("test tests::first ... ok\n");
        progress.heard("test tests::second ... FAILED\n");

        assert_eq!(progress.failed.as_deref(), Some("tests::second"));
    }

    /// The first failure is the one that convicted the mutant, and is what reading the output to
    /// exhaustion would have reported.
    #[test]
    fn a_later_failure_does_not_replace_the_first() {
        let mut progress = Progress::new(Watch::Libtest);

        progress.heard("running 2 tests\n");
        progress.heard("test tests::first ... FAILED\n");
        progress.heard("test tests::second ... FAILED\n");

        assert_eq!(progress.failed.as_deref(), Some("tests::first"));
    }

    /// A target built with `harness = false` announces no suite and prints whatever it likes, so
    /// nothing it says can be read as a verdict.
    #[test]
    fn a_failure_before_any_suite_is_announced_is_not_recorded() {
        let mut progress = Progress::new(Watch::Libtest);

        progress.heard("test tests::second ... FAILED\n");

        assert_eq!(progress.failed, None);
    }

    /// With the watch off nothing is read out of the output at all, however it is shaped.
    #[test]
    fn an_unwatched_run_records_no_failure() {
        let mut progress = Progress::new(Watch::Off);

        progress.heard("running 1 test\n");
        progress.heard("test tests::second ... FAILED\n");
        progress.heard("        FAIL [   0.024s] (1/2) spike tests::second\n");

        assert_eq!(progress.failed, None);
    }

    /// The two runners announce failures differently, and each is read only in its own format.
    #[test]
    fn each_runner_reads_only_its_own_announcement() {
        let mut nextest = Progress::new(Watch::Nextest);

        nextest.heard("        FAIL [   0.024s] (1/2) spike tests::fails\n");

        assert_eq!(nextest.failed.as_deref(), Some("tests::fails"));

        // libtest's shape means nothing to nextest, which never prints it.
        let mut confused = Progress::new(Watch::Nextest);

        confused.heard("running 1 test\n");
        confused.heard("test tests::second ... FAILED\n");

        assert_eq!(confused.failed, None);
    }

    /// A passing result is not a failure however closely it is shaped like one.
    #[test]
    fn a_passing_result_is_not_recorded_as_a_failure() {
        let mut progress = Progress::new(Watch::Libtest);

        progress.heard("running 2 tests\n");
        progress.heard("test tests::first ... ok\n");
        progress.heard("test tests::ignored ... ignored\n");

        assert_eq!(progress.failed, None);
    }

    #[test]
    fn either_runtime_startup_marker_disqualifies_live_output() {
        for marker in [gamma_rt::ENVIRONMENT_ERROR_MARKER, gamma_rt::PRE_INSTALL_ERROR_MARKER] {
            let mut progress = Progress::new(Watch::Off);
            let line = core::str::from_utf8(marker).expect("runtime markers are ASCII");

            progress.heard_diagnostic(line);

            assert!(progress.environment_error, "{line:?}");
        }
    }

    #[test]
    fn the_longest_silence_is_the_one_remembered() {
        let mut progress = Progress::new(Watch::Off);

        progress.heard("started\n");
        thread::sleep(Duration::from_millis(30));
        progress.heard("a\n");
        let long = progress.quiet;

        progress.heard("b\n");

        assert_eq!(progress.quiet, long, "a short gap must not replace a long one");
        assert!(long >= Duration::from_millis(25), "{long:?}");
    }

    #[test]
    fn process_startup_calibrates_test_silence() {
        let mut progress = Progress::new(Watch::Off);

        thread::sleep(Duration::from_millis(30));
        progress.heard("running 1 test\n");

        assert!(progress.quiet >= Duration::from_millis(25), "{:?}", progress.quiet);
    }
}
