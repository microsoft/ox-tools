// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;
use std::io::Write as _;

use camino::Utf8Path;

use super::host::Host;
use super::verdict_log::VerdictLog;
use crate::model::Outcome;
use crate::report::{Progress, Styler};

/// Drives the console progress display from execution events.
pub(super) struct ConsoleEvents<'host, H: Host> {
    pub(super) host: &'host mut H,
    pub(super) progress: Progress,
    pub(super) styler: Styler,

    /// Whether `--estimate` asked for a projection of the wait still to come.
    pub(super) estimate: bool,

    /// Whether `--show-build` asked for cargo's own narration of the build.
    pub(super) show_build: bool,

    pub(super) verdict_log: VerdictLog,
}

impl<H: Host> ConsoleEvents<'_, H> {
    /// Closes a phase line whose phase failed, so the error that follows starts on its own line.
    pub(super) fn abandon(&mut self) {
        self.progress.abandon(self.host);
    }

    pub(super) fn finish_verdict_log(&mut self) -> crate::Result<()> {
        self.verdict_log.finish()
    }
}

impl<H: Host> crate::exec::Events for ConsoleEvents<'_, H> {
    fn testing_log(&mut self, scratch: &Utf8Path) -> crate::Result<()> {
        self.verdict_log.start(scratch)
    }

    fn phase(&mut self, verb: &str, detail: &str) {
        self.progress.status(self.host, verb, detail);
    }

    fn begin(&mut self, active: &str, completed: &str, detail: &str) {
        self.progress.begin(self.host, active, completed, detail);
    }

    fn end(&mut self, detail: &str) {
        self.progress.end(self.host, detail);
    }

    fn complete(&mut self, detail: &str) {
        self.progress.complete(self.host, detail);
    }

    fn phase_progress(&mut self, completed: usize, total: usize, unit: &str) {
        self.progress.phase_progress(self.host, completed, total, unit);
    }

    fn outcome(&mut self, detail: &str) {
        self.progress.labelled(self.host, &crate::report::continuation(), detail);
    }

    fn build_progress(&mut self, bar: &str) {
        // Suppressed while cargo's own output is coming through, because both redraw the same line.
        if self.show_build {
            return;
        }

        self.progress.borrowed(self.host, bar);
    }

    fn build_output(&mut self, line: &str) {
        if !self.show_build {
            return;
        }

        // Written whether or not the display is on. The display goes quiet when output is piped,
        // and that is precisely where a user who asked to see the build needs to see it. Relayed
        // rather than insisted on, because this is cargo's own text: its colour is the reason the
        // option exists, and everything else it could carry is not cargo's to say.
        self.progress.relay(self.host, &crate::report::continuation(), line);
    }

    fn wants_build_output(&self) -> bool {
        self.show_build
    }

    fn build_finished(&mut self) {
        self.progress.restore(self.host);
    }

    fn warn(&mut self, message: &str) {
        let label = self.styler.warning();

        for line in message.lines() {
            self.progress.insist(self.host, &label, line);
        }
    }

    fn mutant(&mut self, mutant: &crate::model::Mutant) {
        self.verdict_log.record(mutant);

        // A survivor is the entire point of the exercise; a timeout is the most expensive thing a
        // run can find; and a mutant stopped by its memory ceiling is usually a sign the ceiling is
        // wrong rather than a finding about the code. All three are printed as they happen rather
        // than held back for the summary. Everything else only moves the bar. The label is the one
        // the summary would use, so the same mutant is never named two different things.
        match mutant.outcome {
            Outcome::Survived => {
                let label = self.styler.outcome(Outcome::Survived);

                self.progress.labelled(self.host, &label, &mutant.describe());
            }

            // Both carry a note that says something the label cannot: which test a timeout stalled
            // in, and what a memory kill peaked at against what ceiling. Neither is worth repeating
            // the label for, so only the note is appended.
            outcome @ (Outcome::Timeout | Outcome::OutOfMemory) => {
                let label = self.styler.outcome(outcome);
                let detail = mutant_detail(mutant);

                self.progress.labelled(self.host, &label, &detail);
            }

            _ => {}
        }

        self.progress.record(mutant.outcome);
        self.progress.tick(self.host);
    }

    fn measured(&mut self, plan: &crate::discover::Plan, _session: &crate::exec::Session, estimate: &crate::estimate::Estimate) {
        for mutant in &plan.mutants {
            if mutant.outcome != Outcome::Pending {
                self.verdict_log.record(mutant);
            }
        }

        // The bar's scale is the population that is about to be tested, which is not known until
        // every package has been scanned — and this is the moment that becomes true.
        let live = plan.mutants.iter().filter(|mutant| mutant.ordinal > 0).count();

        self.progress.set_total(live);

        if !self.estimate {
            return;
        }

        // Written straight to the stream rather than through the progress display, because the
        // display goes quiet when output is piped and an explicitly requested estimate must not.
        self.progress.clear(self.host);

        let projection = format!("{} {}", self.styler.verb("Estimate"), crate::estimate::render(estimate));
        let mut stream = self.host.error();

        let _ = writeln!(stream, "{projection}");
        let _ = stream.flush();
    }
}

pub(super) fn mutant_detail(mutant: &crate::model::Mutant) -> String {
    let mut detail = match mutant.outcome {
        Outcome::Timeout | Outcome::OutOfMemory | Outcome::Flaky => mutant
            .note
            .as_deref()
            .map_or_else(|| mutant.describe(), |note| format!("{}: {note}", mutant.describe())),
        _ => mutant.describe(),
    };

    // A timeout that does not say how long it was given cannot be acted on: a genuine hang and a
    // budget calibrated too tightly print identically, and they call for opposite responses.
    if mutant.outcome == Outcome::Timeout {
        let _ = write!(detail, " (after {})", crate::advise::human(mutant.elapsed()));
    }

    detail
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use camino::Utf8PathBuf;

    use super::super::verdict_log::TESTING_PROGRESS_LOG;
    use super::*;
    use crate::exec::Events as _;
    use crate::fixtures;
    use crate::model::Mutant;
    use crate::testing::Sink;

    fn mutant(outcome: Outcome, note: Option<&str>) -> Mutant {
        timed(outcome, note, 0)
    }

    fn timed(outcome: Outcome, note: Option<&str>, elapsed_ms: u64) -> Mutant {
        Mutant {
            line: 3,
            column: 4,
            item_path: ("subject::less".to_owned()).into(),
            original: "a < b".to_owned().into(),
            replacement: "a <= b".to_owned().into(),
            outcome,
            elapsed_ms,
            note: note.map(str::to_owned),
            ..fixtures::mutant()
        }
    }

    #[test]
    fn survivors_and_timeouts_are_announced_as_they_happen() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.mutant(&mutant(Outcome::Survived, None));
        events.mutant(&mutant(Outcome::Timeout, Some("stalled, last test named was `slow`")));

        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(err.contains("SURVIVED"), "{err}");
        assert!(err.contains("TIMEOUT"), "{err}");
        assert!(err.contains("stalled, last test named was `slow`"), "{err}");
    }

    /// A timeout says how long the mutant was given, alongside whatever the note already said.
    ///
    /// Without the figure a genuine hang and a budget calibrated too tightly print the same line,
    /// and they call for opposite responses — lengthen the budget, or go and look at the code.
    #[test]
    fn a_timeout_says_how_long_the_mutant_ran_before_it_was_stopped() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(200)),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.mutant(&timed(Outcome::Timeout, Some("stalled, last test named was `slow`"), 22_400));

        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(err.contains("(after 22.4s)"), "{err}");

        // The note names the likely culprit where the figure only sizes the problem, so the figure
        // has to compose with it rather than displace it.
        assert!(err.contains("stalled, last test named was `slow`"), "{err}");
    }

    /// Only a timeout gains the figure. Every other outcome's line is unchanged.
    ///
    /// A memory kill's note already carries the two numbers that matter — the peak and the ceiling
    /// — and how long it took to get there says nothing about either.
    #[test]
    fn no_other_outcome_gains_an_elapsed_figure() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(200)),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.mutant(&timed(Outcome::Survived, None, 9_000));
        events.mutant(&timed(
            Outcome::OutOfMemory,
            Some("peaked at 2.1 GiB against a 512 MiB ceiling"),
            9_000,
        ));

        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(err.contains("SURVIVED"), "{err}");
        assert!(err.contains("peaked at 2.1 GiB"), "{err}");
        assert!(!err.contains("after"), "{err}");
    }

    /// The phase verbs all reach the display, and a caught mutant is left for the summary.
    #[test]
    fn phase_events_are_displayed_and_ordinary_verdicts_are_not_announced() {
        let mut host = Sink::default().terminal(80);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.phase("Baseline", "measuring the suite");
        events.begin("Building", "Built", "the test binaries");
        events.end(", done");
        events.outcome("withdrew 2 mutants");
        events.mutant(&mutant(Outcome::Killed, None));

        let err = host.err();

        assert!(err.contains("Baseline"), "{err}");
        assert!(err.contains("Building"), "{err}");
        assert!(err.contains("withdrew 2 mutants"), "{err}");
        assert!(!err.contains("SURVIVED"), "{err}");
    }

    /// The display is off wherever output is piped, which is where a CI log is written — so a build
    /// the user asked to see by name has to be printed anyway, or the flag does nothing in the one
    /// place it was asked for.
    #[test]
    fn requested_build_output_survives_a_display_that_is_off() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            styler: Styler::new(false),
            estimate: false,
            show_build: true,
            verdict_log: VerdictLog::default(),
        };

        events.build_output("warning: unused variable: `x`");

        assert!(host.err().contains("warning: unused variable: `x`"), "{}", host.err());
    }

    /// The question asked before the work is answered by the same flag that gates the work's result.
    ///
    /// `wants_build_output` exists so that the megabytes of cargo's JSON stream are not decoded to
    /// produce a line that `build_output` is about to drop. That is only sound while the two agree:
    /// a `wants` that said yes when `build_output` discards would cost the parse it is there to
    /// avoid, and one that said no when `build_output` would have shown the line would silently
    /// empty `--show-build`.
    #[test]
    fn asking_whether_build_output_is_wanted_agrees_with_what_is_done_with_it() {
        for show_build in [false, true] {
            let mut host = Sink::default();

            {
                let mut events = ConsoleEvents {
                    host: &mut host,
                    progress: Progress::new(false, Styler::new(false), None),
                    styler: Styler::new(false),
                    estimate: false,
                    show_build,
                    verdict_log: VerdictLog::default(),
                };

                assert_eq!(events.wants_build_output(), show_build);
                events.build_output("warning: unused variable: `x`");
            }

            assert_eq!(!host.err().is_empty(), show_build, "{}", host.err());
        }
    }

    /// And an unrequested build stays unrequested: the flag is what turns cargo's narration on, not
    /// the state of the display.
    #[test]
    fn build_output_stays_hidden_when_it_was_not_asked_for() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.build_output("warning: unused variable: `x`");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// A warning about what a run is about to cost is worth nothing if it is only shown when
    /// someone is watching. The display turns itself off wherever output is piped, and a CI job is
    /// exactly where a run that quietly takes six hours is least affordable.
    #[test]
    fn a_warning_survives_a_display_that_is_off() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.warn("router_compile_fail runs the compiler\nexclude it with --exclude-test router_compile_fail");

        assert!(host.err().contains("router_compile_fail runs the compiler"), "{}", host.err());
        assert!(host.err().contains("--exclude-test router_compile_fail"), "{}", host.err());
    }

    /// A multi-line warning is written line by line, because the status column is per line and a
    /// message written as one string would put the second line under the label rather than beside
    /// it.
    #[test]
    fn a_multi_line_warning_is_labelled_on_every_line() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.warn("first\nsecond");

        assert_eq!(
            host.err().lines().filter(|line| line.contains("warning")).count(),
            2,
            "{}",
            host.err()
        );
    }

    #[test]
    fn the_testing_progress_log_is_truncated_and_flushed_after_every_verdict() {
        let directory = crate::testing::workdir("testing-progress-log-");
        let scratch = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the test path is UTF-8");
        let path = scratch.join(TESTING_PROGRESS_LOG);

        std::fs::write(path.as_std_path(), "stale verdict\n").expect("seed the old log");

        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            styler: Styler::new(false),
            estimate: false,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.testing_log(&scratch).expect("create the verdict log");
        events.mutant(&mutant(Outcome::Killed, None));

        let first = std::fs::read_to_string(path.as_std_path()).expect("read the still-open log");

        assert!(first.contains("killed"), "{first}");
        assert!(!first.contains("stale verdict"), "{first}");

        for outcome in [
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::OutOfMemory,
            Outcome::Flaky,
            Outcome::NoCoverage,
            Outcome::CompileError,
            Outcome::Ignored,
            Outcome::NotBuilt,
        ] {
            events.mutant(&mutant(outcome, Some("verdict detail")));
        }

        events.finish_verdict_log().expect("finish the verdict log");

        let text = std::fs::read_to_string(path.as_std_path()).expect("read the verdict log");

        for label in [
            "killed",
            "SURVIVED",
            "TIMEOUT",
            "OUTOFMEM",
            "FLAKY",
            "uncovered",
            "unviable",
            "skipped",
            "notbuilt",
        ] {
            assert!(text.contains(label), "{label} was absent from:\n{text}");
        }
        assert!(!text.contains("\u{1b}["), "the durable log contains terminal styling: {text:?}");
    }
}
