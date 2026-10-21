// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;

use camino::Utf8Path;

use super::dashboard::Dashboard;
use super::host::Host;
use super::verdict_log::VerdictLog;
use crate::model::Outcome;
use crate::report::{Progress, Styler};

/// Drives the console progress display from execution events.
pub(super) struct ConsoleEvents<'host, H: Host> {
    pub(super) host: &'host mut H,
    pub(super) progress: Progress,
    pub(super) dashboard: Dashboard,
    pub(super) styler: Styler,

    /// Whether `--show-build` asked for cargo's own narration of the build.
    pub(super) show_build: bool,

    pub(super) verdict_log: VerdictLog,
}

impl<H: Host> ConsoleEvents<'_, H> {
    /// Closes a phase line whose phase failed, so the error that follows starts on its own line.
    pub(super) fn abandon(&mut self) {
        self.dashboard.finish(self.host);
        self.progress.abandon(self.host);
    }

    // #[gamma::skip(fn_value.ok, reason = "the optional progress log is auxiliary and best-effort; command correctness is unchanged when no log was opened or its final flush is already complete")]
    pub(super) fn finish_verdict_log(&mut self) -> crate::Result<()> {
        self.verdict_log.finish()
    }

    fn sync_testing_status(&mut self) {
        self.progress.resize(self.host.terminal_width());
        if self.dashboard.is_active() {
            self.dashboard.testing_status(self.progress.render());
        }
    }
}

// #[gamma::skip(all, reason = "this impl is a thin terminal/event adapter whose observable behavior is covered deterministically through the Host sink; mutating individual forwarding operations duplicates those end-to-end contracts without exposing additional production logic")]
impl<H: Host> crate::exec::Events for ConsoleEvents<'_, H> {
    fn testing_log(&mut self, scratch: &Utf8Path) -> crate::Result<()> {
        self.verdict_log.start(scratch)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn phase(&mut self, verb: &str, detail: &str) {
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, verb, detail);
            return;
        }
        self.progress.status(self.host, verb, detail);
    }

    fn begin(&mut self, active: &str, completed: &str, detail: &str) {
        let detail = if active == "Excluding" {
            self.progress.reset_unviable();
            "unviable mutants (0 found)"
        } else {
            detail
        };
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, active, detail);
            return;
        }
        self.progress.begin(self.host, active, completed, detail);
    }

    fn end(&mut self, detail: &str) {
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, "Planning", detail.trim_start_matches(',').trim());
            return;
        }
        self.progress.end(self.host, detail);
    }

    fn complete(&mut self, detail: &str) {
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, "Planning", detail);
            return;
        }
        self.progress.complete(self.host, detail);
    }

    fn phase_progress(&mut self, completed: usize, total: usize, unit: &str) {
        if self.dashboard.is_active() {
            self.dashboard.phase_progress(self.host, completed, total, unit);
            return;
        }
        self.progress.phase_progress(self.host, completed, total, unit);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn outcome(&mut self, detail: &str) {
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, "Planning", detail.trim_start_matches(',').trim());
            return;
        }
        self.progress.labelled(self.host, &crate::report::continuation(), detail);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn build_progress(&mut self, bar: &str) {
        if !self.show_build {
            return;
        }

        self.progress.borrowed(self.host, bar);
    }

    fn convergence_progress(&mut self, unviable: usize) {
        let unviable = self.progress.confirmed_unviable(unviable);
        let detail = format!("unviable mutants ({unviable} found)");
        if self.dashboard.is_active() {
            self.dashboard.status(self.host, "Excluding", &detail);
            return;
        }
        self.progress.update_phase(self.host, &detail);
    }

    fn convergence_activity(&mut self, _round: u32, unviable: usize, _proof: usize) {
        // Both displays report monotonic confirmed exclusions, never check rounds, proof subset
        // sizes, or interaction-only exclusions that do not establish individual unviability.
        self.convergence_progress(unviable);
    }

    fn build_output(&mut self, line: &str) {
        if !self.show_build {
            return;
        }

        // Written whether or not the display is on. The display goes quiet when output is piped,
        // and that is precisely where a user who asked to see the build needs to see it. Relayed
        // rather than insisted on because `--show-build` promises Cargo's color formatting; every
        // other terminal control is encoded.
        self.progress.relay(self.host, &crate::report::continuation(), line);
    }

    fn wants_build_output(&self) -> bool {
        self.show_build
    }

    fn build_finished(&mut self) {
        self.progress.restore(self.host);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn warn(&mut self, message: &str) {
        let label = self.styler.warning();

        for line in message.lines() {
            if self.dashboard.is_active() {
                self.dashboard.line(self.host, &label, line);
            } else {
                self.progress.insist(self.host, &label, line);
            }
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn mutant(&mut self, mutant: &crate::model::Mutant) {
        self.verdict_log.record(mutant);

        // A survivor is the entire point of the exercise; a timeout is the most expensive thing a
        // run can find; and a mutant stopped by its memory ceiling is usually a sign the ceiling is
        // wrong rather than a finding about the code. All three are printed as they happen rather
        // than held back for the summary. Everything else only moves the bar. The label is the one
        // the summary would use, so the same mutant is never named two different things.
        match mutant.outcome {
            Outcome::Survived | Outcome::Flaky => {
                let label = self.styler.outcome(mutant.outcome);
                let detail = mutant_detail(mutant);

                if self.dashboard.is_active() {
                    self.dashboard.line(self.host, &label, &detail);
                } else {
                    self.progress.labelled(self.host, &label, &detail);
                }
            }

            // Both carry a note that says something the label cannot: which test a timeout stalled
            // in, and what a memory kill peaked at against what ceiling. Neither is worth repeating
            // the label for, so only the note is appended.
            outcome @ (Outcome::Timeout | Outcome::OutOfMemory) => {
                let label = self.styler.outcome(outcome);
                let detail = mutant_detail(mutant);

                if self.dashboard.is_active() {
                    self.dashboard.line(self.host, &label, &detail);
                } else {
                    self.progress.labelled(self.host, &label, &detail);
                }
            }

            _ => {}
        }

        self.progress.record(mutant.outcome);
        self.dashboard.record(mutant);
        self.sync_testing_status();
        if self.dashboard.is_active() {
            self.dashboard.tick(self.host);
        } else {
            self.progress.tick(self.host);
        }
    }

    fn selection_attempt(&mut self, attempt: &crate::exec::SelectionAttempt) {
        self.verdict_log.record_selection(attempt);
        self.dashboard.record_selection(attempt);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn sweep_planned(&mut self, work: &[crate::estimate::MutationWork], jobs: usize) {
        if !self.dashboard.enabled() {
            self.progress.complete(self.host, &crate::report::quantity(work.len(), "mutant"));
        }
        self.progress.set_remaining_total(work.len());
        if self.dashboard.enabled() {
            self.progress.finish(self.host);
            self.progress.resize(self.host.terminal_width());
            self.dashboard.start_testing(self.host, jobs, self.progress.render());
        } else {
            self.progress.draw_now(self.host);
        }
    }

    fn mutant_started(&mut self) {
        self.dashboard.mutant_started();
        self.sync_testing_status();
        self.dashboard.tick(self.host);
    }

    fn mutant_activity(&mut self, active: bool) {
        self.dashboard.mutant_activity(active);
        self.sync_testing_status();
        self.dashboard.tick(self.host);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn heartbeat(&mut self) {
        if self.dashboard.is_active() {
            self.sync_testing_status();
            self.dashboard.tick(self.host);
        }
    }

    fn sweep_planning(&mut self, plan: &crate::discover::Plan, binaries: usize, jobs: usize) {
        if self.dashboard.enabled() {
            self.progress.finish(self.host);
            self.dashboard.start_planning(self.host, plan, binaries, jobs);
        } else {
            let pending = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Pending).count();
            let binaries = if binaries == 1 {
                "1 test binary".to_owned()
            } else {
                format!("{binaries} test binaries")
            };
            self.progress.begin(
                self.host,
                "Planning",
                "Planned",
                &format!("0/{pending} mutants across {binaries} using {jobs} jobs"),
            );
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn sweep_plan_progress(&mut self, completed: usize, total: usize) {
        if self.dashboard.is_active() {
            self.dashboard.phase_progress(self.host, completed, total, "mutants planned");
        } else {
            self.progress.phase_progress(self.host, completed, total, "mutants planned");
        }
    }

    fn measured(&mut self, plan: &crate::discover::Plan, session: &crate::exec::Session) {
        self.dashboard.measured(plan, session);
        self.progress.restore_outcomes(plan.mutants.iter().map(|mutant| mutant.outcome));
        for mutant in &plan.mutants {
            if mutant.outcome != Outcome::Pending {
                self.verdict_log.record_prepared(mutant);
            }
        }
    }
}

pub(super) fn mutant_detail(mutant: &crate::model::Mutant) -> String {
    let mut detail = match mutant.outcome {
        Outcome::Timeout | Outcome::OutOfMemory | Outcome::Flaky | Outcome::CompileError => mutant
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
#[cfg_attr(coverage_nightly, coverage(off))]
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

    fn empty_plan() -> crate::discover::Plan {
        crate::discover::Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: Utf8PathBuf::from("C:/workspace"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    #[test]
    fn dashboard_starts_while_the_sweep_is_still_being_planned() {
        let mut host = Sink::default().terminal(160);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(160)),
            dashboard: Dashboard::new(true, Some(160), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        let mut plan = empty_plan();
        plan.mutants.push(mutant(Outcome::Pending, None));
        events.sweep_planning(&plan, 12, 4);
        let planning_draw = events.host.err.len();
        let rendered = String::from_utf8_lossy(&events.host.err);
        assert!(rendered.contains("    Planning 0/1 mutants planned"), "{rendered}");

        events.begin("Testing", "Tested", "executing mutants");
        events.phase_progress(2, 10, "tests");
        events.end(", 4 killed");
        events.complete("reachability index ready");

        assert_eq!(
            events.host.err.len(),
            planning_draw,
            "planning events must update dashboard state without writing a competing progress line"
        );
    }

    #[test]
    fn exceptional_outcomes_are_announced_as_they_happen() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            dashboard: Dashboard::new(false, Some(80), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.mutant(&mutant(Outcome::Survived, None));
        events.mutant(&mutant(Outcome::Timeout, Some("stalled, last test named was `slow`")));
        events.mutant(&mutant(Outcome::Flaky, Some("test `unstable` also failed without a mutant")));

        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(err.contains("SURVIVED"), "{err}");
        assert!(err.contains("TIMEOUT"), "{err}");
        assert!(err.contains("FLAKY"), "{err}");
        assert!(err.contains("stalled, last test named was `slow`"), "{err}");
        assert!(err.contains("test `unstable` also failed without a mutant"), "{err}");
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
            dashboard: Dashboard::new(false, Some(200), Styler::new(false)),
            styler: Styler::new(false),
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
            dashboard: Dashboard::new(false, Some(200), Styler::new(false)),
            styler: Styler::new(false),
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

    #[test]
    #[cfg_attr(
        miri,
        ignore = "restores a 45,153-mutant campaign and allocates 40,008 work items; native population-scale regression"
    )]
    fn testing_status_is_painted_before_execution_with_the_announced_retained_count() {
        let mut host = Sink::default().terminal(200);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(200)),
            dashboard: Dashboard::new(false, Some(200), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };
        let outcomes = core::iter::repeat_n(Outcome::Killed, 1381)
            .chain([Outcome::Survived, Outcome::Timeout, Outcome::OutOfMemory])
            .chain(core::iter::repeat_n(Outcome::CompileError, 1000))
            .chain(core::iter::repeat_n(Outcome::Ignored, 2000))
            .chain(core::iter::repeat_n(Outcome::NotBuilt, 761))
            .chain(core::iter::repeat_n(Outcome::Pending, 40008));
        let retained = events.progress.restore_outcomes(outcomes);
        assert_eq!(retained, 1384);
        events.sweep_planning(&empty_plan(), 1, 1);
        let work = (0..40008)
            .map(|_| crate::estimate::MutationWork::new(crate::estimate::WorkKind::Whole, core::time::Duration::from_secs(1)))
            .collect::<Vec<_>>();
        events.sweep_planned(&work, 1);
        assert!(
            events.host.err().contains(&format!("{retained}/41392 mutants evaluated")),
            "{}",
            events.host.err()
        );
        events.mutant(&mutant(Outcome::Killed, None));
        assert!(events.progress.render().contains("1385/41392 mutants evaluated"));
        assert!(!events.progress.render().contains("45153"));
    }

    #[test]
    fn sweep_events_drive_testing_progress_without_an_eta() {
        let work = (1..=12)
            .map(|_| crate::estimate::MutationWork::new(crate::estimate::WorkKind::Whole, core::time::Duration::from_secs(10)))
            .collect::<Vec<_>>();
        let mut host = Sink::default().terminal(200);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(200)),
            dashboard: Dashboard::new(false, Some(200), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        let mut plan = empty_plan();
        plan.mutants.extend((1..=12).map(|ordinal| {
            let mut mutant = mutant(Outcome::Pending, None);
            mutant.ordinal = ordinal;
            mutant
        }));
        events.sweep_planning(&plan, 7, 4);
        events.sweep_plan_progress(6, 12);
        events.sweep_planned(&work, 4);

        let planning = String::from_utf8_lossy(&events.host.err);
        assert!(
            planning.contains("Planning 0/12 mutants across 7 test binaries using 4 jobs"),
            "{planning}"
        );
        assert!(planning.contains("6/12 mutants planned"), "{planning}");
        assert!(planning.contains("Planned 12 mutants"), "{planning}");

        for ordinal in 1..=8 {
            events.mutant_started();
            let mut completed = timed(Outcome::Killed, None, 6_000);
            completed.ordinal = ordinal;
            events.mutant(&completed);
        }

        let rendered = events.progress.render();
        assert!(rendered.contains("8/12 mutants evaluated"), "{rendered}");
        assert!(!rendered.contains("ETA"), "{rendered}");
    }

    /// The phase verbs all reach the display, and a caught mutant is left for the summary.
    #[test]
    fn phase_events_are_displayed_and_ordinary_verdicts_are_not_announced() {
        let mut host = Sink::default().terminal(80);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            dashboard: Dashboard::new(false, Some(80), Styler::new(false)),
            styler: Styler::new(false),
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
            dashboard: Dashboard::new(false, None, Styler::new(false)),
            styler: Styler::new(false),
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
                    dashboard: Dashboard::new(false, None, Styler::new(false)),
                    styler: Styler::new(false),
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
            dashboard: Dashboard::new(false, None, Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.build_output("warning: unused variable: `x`");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    #[test]
    fn cargo_unit_progress_is_hidden_unless_build_output_was_requested() {
        for show_build in [false, true] {
            let mut host = Sink::default().terminal(120);
            {
                let mut events = ConsoleEvents {
                    host: &mut host,
                    progress: Progress::new(true, Styler::new(false), Some(120)),
                    dashboard: Dashboard::new(false, Some(120), Styler::new(false)),
                    styler: Styler::new(false),
                    show_build,
                    verdict_log: VerdictLog::default(),
                };
                events.build_progress("Building [====>    ] 4/17: serde_core");
            }

            assert_eq!(host.err().contains("4/17"), show_build, "{}", host.err());
        }
    }

    #[test]
    fn convergence_progress_replaces_the_phase_with_the_monotonic_unviable_count() {
        let mut host = Sink::default().terminal(120);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(120)),
            dashboard: Dashboard::new(false, Some(120), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        events.begin("Excluding", "Excluded", "unviable mutants");
        assert!(events.host.err().contains("Excluding unviable mutants (0 found)"));
        events.convergence_progress(10);
        events.complete("10 unviable mutants, leaving 20 viable mutants");

        let rendered = host.err();
        assert!(rendered.contains("Excluding unviable mutants (10 found)"), "{rendered}");
        assert!(
            rendered.contains("Excluded 10 unviable mutants, leaving 20 viable mutants"),
            "{rendered}"
        );
        assert_eq!(
            rendered.matches('\n').count(),
            1,
            "the completed result must overwrite the active phase row: {rendered:?}"
        );
        assert!(!rendered.contains("(0 found)   Excluding"), "{rendered}");
        assert!(!rendered.contains("schema checks"), "{rendered}");
        assert!(!rendered.contains("round"), "{rendered}");
        assert!(!rendered.contains("Cargo reused"), "{rendered}");
    }

    #[test]
    fn excluding_retains_confirmed_counts_through_proofs_in_both_displays() {
        for dashboard in [false, true] {
            let mut host = Sink::default().terminal(160);
            let mut events = ConsoleEvents {
                host: &mut host,
                progress: Progress::new(true, Styler::new(false), Some(160)),
                dashboard: Dashboard::new(dashboard, Some(160), Styler::new(false)),
                styler: Styler::new(false),
                show_build: false,
                verdict_log: VerdictLog::default(),
            };
            if dashboard {
                events.sweep_planning(&empty_plan(), 1, 1);
            }
            events.begin("Excluding", "Excluded", "unviable mutants");
            if dashboard {
                assert_eq!(events.dashboard.testing_status_for_test(), "Excluding unviable mutants (0 found)");
            }
            for (reported, retained) in [(3, 3), (1, 3), (10, 10), (0, 10)] {
                events.convergence_progress(reported);
                events.isolation_evidence(123, 456, &[], 1, 2, &[]);
                events.build_progress("checking 99 active mutants");
                events.build_finished();
                let expected = format!("Excluding unviable mutants ({retained} found)");
                if dashboard {
                    assert_eq!(events.dashboard.testing_status_for_test(), expected);
                } else {
                    assert!(events.host.err().ends_with(&expected), "{}", events.host.err());
                }
            }
            assert!(!events.host.err().contains("123 found"));
            assert!(!events.host.err().contains("456 found"));
            events.begin("Excluding", "Excluded", "unviable mutants");
            if dashboard {
                assert_eq!(events.dashboard.testing_status_for_test(), "Excluding unviable mutants (0 found)");
            } else {
                assert!(events.host.err().ends_with("Excluding unviable mutants (0 found)"));
            }
        }
    }

    #[test]
    fn excluding_shows_only_confirmed_counts_during_check_activity_in_both_displays() {
        for dashboard in [false, true] {
            let mut host = Sink::default().terminal(160);
            let mut events = ConsoleEvents {
                host: &mut host,
                progress: Progress::new(true, Styler::new(false), Some(160)),
                dashboard: Dashboard::new(dashboard, Some(160), Styler::new(false)),
                styler: Styler::new(false),
                show_build: false,
                verdict_log: VerdictLog::default(),
            };
            if dashboard {
                events.sweep_planning(&empty_plan(), 1, 1);
            }
            events.begin("Excluding", "Excluded", "unviable mutants");
            for (confirmed, retained, round, proof) in [(0, 0, 2, 7), (3, 3, 3, 0), (10, 10, 4, 2), (0, 10, 5, 0)] {
                if confirmed > 0 {
                    events.convergence_progress(confirmed);
                }
                events.convergence_activity(round, confirmed, proof);
                events.build_finished();
                let expected = format!("Excluding unviable mutants ({retained} found)");
                if dashboard {
                    assert_eq!(events.dashboard.testing_status_for_test(), expected);
                } else {
                    assert!(events.host.err().ends_with(&expected), "{}", events.host.err());
                }
            }
            assert!(!events.host.err().contains("105 found"));
            assert!(!events.host.err().contains("13 found"));
            for hidden in ["provisional", "confirmed", "; check", "proof"] {
                assert!(!events.host.err().contains(hidden), "{}", events.host.err());
            }
        }
    }

    #[test]
    fn file_only_sweep_telemetry_never_changes_either_console_display() {
        use core::time::Duration;

        let mut session = crate::exec::Session {
            baseline: Duration::ZERO,
            baseline_wall: Duration::ZERO,
            tests: None,
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::ZERO,
            peak: None,
            metered: false,
            unbounded: None,
            withdrawn: 0,
            census: Vec::new(),
            rounds: 0,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            scratch: Utf8PathBuf::from("unused"),
            filtered: 0,
            widened: false,
            ordering: crate::exec::OrderingHints::default(),
            phases: crate::exec::Phases::default(),
        };
        let render = |dashboard, session: &crate::exec::Session| {
            let mut host = Sink::default().terminal(160);
            let mut events = ConsoleEvents {
                host: &mut host,
                progress: Progress::new(true, Styler::new(false), Some(160)),
                dashboard: Dashboard::new(dashboard, Some(160), Styler::new(false)),
                styler: Styler::new(false),
                show_build: false,
                verdict_log: VerdictLog::default(),
            };
            let plan = empty_plan();
            events.sweep_planning(&plan, 1, 1);
            events.measured(&plan, session);
            events.sweep_planned(&[], 1);
            let status = events.dashboard.testing_status_for_test().to_owned();
            let progress = events.progress.render();
            (host.err(), host.out, status, progress)
        };
        for dashboard in [false, true] {
            session.phases.sweep = None;
            let before = render(dashboard, &session);
            session.phases.sweep = Some(crate::exec::SweepCost {
                accounting: Some(crate::exec::SweepAccounting {
                    complete: true,
                    initial_population: Some(43_164),
                    initial_exact_hints: Some(34_603),
                    initial_killer_confirmed: Some(245),
                    exact: crate::exec::AttemptCost {
                        launches: 999,
                        execution_ms: 123_456,
                        ..crate::exec::AttemptCost::default()
                    },
                    duplicate_launches_avoided: 777,
                    ..crate::exec::SweepAccounting::default()
                }),
                ..crate::exec::SweepCost::default()
            });
            assert_eq!(render(dashboard, &session), before);
        }
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
            dashboard: Dashboard::new(false, None, Styler::new(false)),
            styler: Styler::new(false),
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
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_multi_line_warning_is_labelled_on_every_line() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            dashboard: Dashboard::new(false, None, Styler::new(false)),
            styler: Styler::new(false),
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
    #[cfg_attr(miri, ignore = "writes and reads the filesystem-backed testing progress log")]
    fn the_testing_progress_log_is_truncated_and_flushed_after_every_verdict() {
        let directory = crate::testing::workdir("testing-progress-log-");
        let scratch = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the test path is UTF-8");
        let path = scratch.join(TESTING_PROGRESS_LOG);

        std::fs::write(path.as_std_path(), "stale verdict\n").expect("seed the old log");

        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), None),
            dashboard: Dashboard::new(false, None, Styler::new(false)),
            styler: Styler::new(false),
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
    #[test]
    fn survivor_testing_status_resizes_progress_and_only_updates_an_active_dashboard() {
        for dashboard in [false, true] {
            let mut host = Sink::default().terminal(160);
            let styler = Styler::new(false);
            let mut events = ConsoleEvents {
                host: &mut host,
                progress: Progress::new(true, styler, Some(160)),
                dashboard: Dashboard::new(dashboard, Some(160), styler),
                styler,
                show_build: false,
                verdict_log: VerdictLog::default(),
            };
            events.progress.set_total(4);
            events.progress.record(Outcome::Killed);
            if dashboard {
                events.dashboard.start_testing(events.host, 2, "old status".into());
            }
            let previous = events.dashboard.testing_status_for_test().to_owned();
            events.host.resize_terminal(80);
            events.sync_testing_status();
            let rendered = events.progress.render();
            assert!(rendered.contains("1/4 mutants evaluated"), "{rendered}");
            if dashboard {
                assert_ne!(rendered, previous);
                assert_eq!(events.dashboard.testing_status_for_test(), rendered);
            } else {
                assert_eq!(events.dashboard.testing_status_for_test(), previous);
            }
            events.host.resize_terminal(20);
            events.sync_testing_status();
            assert!(crate::report::unstyled_width(&events.progress.render()) <= 20);
        }
    }

    #[test]
    fn survivor_abandon_clears_the_dashboard_and_closes_the_pending_progress_row() {
        let mut host = Sink::default().terminal(160);
        let styler = Styler::new(false);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, styler, Some(160)),
            dashboard: Dashboard::new(true, Some(160), styler),
            styler,
            show_build: false,
            verdict_log: VerdictLog::default(),
        };
        events.progress.begin(events.host, "Building", "Built", "subject");
        events.progress.borrowed(events.host, "compiler progress");
        events.dashboard.start_testing(events.host, 2, "testing".into());
        assert!(events.dashboard.is_active());
        events.abandon();
        assert!(!events.dashboard.is_active());
        assert!(events.host.err().ends_with("subject\n"), "{}", events.host.err());
        let length = events.host.err.len();
        events.abandon();
        assert_eq!(events.host.err.len(), length);
    }
}
