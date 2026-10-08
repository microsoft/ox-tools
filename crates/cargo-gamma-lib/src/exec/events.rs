// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;

use camino::Utf8Path;
use serde::Serialize;

use super::session::Session;
use crate::Result;
use crate::discover::Plan;
use crate::estimate::MutationWork;
use crate::model::Mutant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SelectionTier {
    Exact,
    Item,
    Reach,
    File,
    Census,
    Selected,
    Whole,
    HintedFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SelectionResult {
    Hit,
    Miss,
    Inconclusive,
}

/// One test-selection attempt, retained so a completed campaign can be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct SelectionAttempt {
    pub(crate) ordinal: u32,
    pub(crate) tier: SelectionTier,
    pub(crate) package: String,
    pub(crate) target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) test: Option<String>,
    pub(crate) rank: usize,
    pub(crate) result: SelectionResult,
    /// Whether the process ran the complete test binary rather than a selected test set.
    pub(crate) all_tests: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) fallback_ms: u64,
}

/// Progress notifications, so this module needs to know nothing about terminals.
pub trait Events {
    /// A measured run acquired its scratch directory and is ready to record verdicts there.
    fn testing_log(&mut self, _scratch: &Utf8Path) -> Result<()> {
        Ok(())
    }

    /// A new phase started.
    fn phase(&mut self, verb: &str, detail: &str);

    /// A phase started, and will report what it found on the same line once it knows.
    fn begin(&mut self, active: &str, _completed: &str, detail: &str) {
        self.phase(active, detail);
    }

    /// A phase that opened a line with [`begin`](Self::begin) is closing it.
    fn end(&mut self, detail: &str) {
        self.outcome(detail);
    }

    /// A phase that opened a line with [`begin`](Self::begin) is closing it with a result that
    /// replaces, rather than extends, the description of work in progress.
    fn complete(&mut self, detail: &str) {
        self.outcome(detail);
    }

    /// Reports progress within the phase opened by [`begin`](Self::begin).
    fn phase_progress(&mut self, _completed: usize, _total: usize, _unit: &str) {}

    /// A phase that has already announced itself is reporting what it found.
    ///
    /// Rendered under the phase it belongs to rather than repeating the verb, since a phase and
    /// its result are one event to a reader even though they are two to the code.
    fn outcome(&mut self, detail: &str) {
        self.phase("", detail);
    }

    /// The build reported how far along its current Cargo invocation is.
    ///
    /// Hidden by default because convergence launches multiple invocations with unrelated unit
    /// graphs. Reporters may expose it as part of an explicitly requested raw build transcript.
    fn build_progress(&mut self, _bar: &str) {}

    /// The build wrote a line, or the compiler rendered a diagnostic.
    ///
    /// Only shown when asked for, since cargo narrates every crate it compiles, and because a
    /// compiler error during an instrumented build is the mechanism rather than a fault: the tree
    /// was checked before any mutant was applied, so the rollback loop is already about to withdraw
    /// whatever failed. What withdrew a mutant is reported with the mutant.
    fn build_output(&mut self, _line: &str) {}

    /// Whether anything would be done with a line handed to `build_output`.
    ///
    /// Asked before the work of producing one. Cargo's JSON stream runs to megabytes on a cold
    /// build and a compiler diagnostic has to be decoded out of it, which is pure loss when the
    /// answer is going to be dropped — and it is dropped by default, since `--show-build` is off.
    fn wants_build_output(&self) -> bool {
        false
    }

    /// The build finished, so anything drawn in its place can be taken down.
    fn build_finished(&mut self) {}

    /// The number of mutants the compiler has proved unviable so far.
    fn convergence_progress(&mut self, _unviable: usize) {}

    /// Whether convergence work evidence has a consumer.
    ///
    /// The evidence requires another pass over Cargo's JSON transcript. Reporters that do not
    /// retain it should leave this false so ordinary runs pay only for verdict-bearing decoding.
    fn wants_convergence_evidence(&self) -> bool {
        false
    }

    /// Reports what changed and what Cargo rebuilt during one convergence round.
    fn convergence_evidence(
        &mut self,
        _round: u32,
        _written: &[&Path],
        _fresh: usize,
        _rebuilt: usize,
        _rebuilt_targets: &[String],
        _failed_targets: &[String],
    ) {
    }

    /// Whether isolation proof evidence has a consumer.
    fn wants_isolation_evidence(&self) -> bool {
        false
    }

    /// Reports one proof build used to isolate an unattributed compiler failure.
    fn isolation_evidence(
        &mut self,
        _active: usize,
        _population: usize,
        _written: &[&Path],
        _fresh: usize,
        _rebuilt: usize,
        _failed_targets: &[String],
    ) {
    }

    /// Something about this run is likely to cost far more than it is worth, and the user can fix it.
    ///
    /// Distinct from a phase because a phase describes what is happening and this describes what
    /// should perhaps not be. It is also the one kind of progress that has to survive the display
    /// being off: the display resolves to whether a terminal is attached, and a CI job is exactly
    /// where a run that quietly takes six hours is least affordable and least visible.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn warn(&mut self, _message: &str) {}

    /// A mutant finished.
    fn mutant(&mut self, mutant: &Mutant);

    /// One candidate or canonical test selection completed.
    fn selection_attempt(&mut self, _attempt: &SelectionAttempt) {}

    /// Baseline measurement is complete and the sweep is being prepared.
    ///
    /// This boundary is deliberately earlier than [`Self::sweep_planned`]: reachability,
    /// optional census work, hint loading, scheduling-cost construction, and queue construction
    /// can take long enough that leaving the terminal unchanged makes a healthy run look stalled.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn sweep_planning(&mut self, _plan: &Plan, _binaries: usize, _jobs: usize) {}

    /// One pending mutant's scheduling work has been constructed.
    fn sweep_plan_progress(&mut self, _completed: usize, _total: usize) {}

    /// The sweep has fixed its queue and measured the work represented by every pending mutant.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn sweep_planned(&mut self, _work: &[MutationWork], _jobs: usize) {}

    /// A worker began evaluating one mutant.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn mutant_started(&mut self) {}

    /// The sweep is still waiting for workers, allowing time-dependent displays to refresh.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn heartbeat(&mut self) {}

    /// The fixed cost is paid, the tree compiles, and the first mutant is about to be tested.
    fn measured(&mut self, _plan: &Plan, _session: &Session) {}
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::time::Duration;

    use camino::Utf8PathBuf;

    use super::*;
    use crate::fixtures;
    use crate::testing::Recorder;

    #[test]
    fn default_event_methods_are_expressed_in_terms_of_phase_and_outcome() {
        let mut events = Recorder::default();
        let plan = Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: Utf8PathBuf::from("/workspace"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let session = Session {
            census: Vec::new(),
            baseline: Duration::ZERO,
            baseline_wall: Duration::ZERO,
            tests: None,
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::ZERO,
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 0,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            peak: None,
            scratch: Utf8PathBuf::new(),
            filtered: 0,
            widened: false,
            ordering: crate::exec::OrderingHints::default(),
            phases: crate::exec::Phases::default(),
        };
        let work = [MutationWork::new(crate::estimate::WorkKind::Whole, Duration::from_secs(1))];

        events.begin("Doing", "Done", "the thing");
        events.end(", done");
        events.complete("the result");
        events.outcome(", noted");
        events.convergence_progress(3);
        events.convergence_progress(5);
        events.isolation_evidence(4, 10, &[Path::new("src/lib.rs")], 6, 2, &["subject (test)".to_owned()]);
        events.measured(&plan, &session);
        events.sweep_planned(&work, 3);
        events.mutant_started();
        events.heartbeat();
        events.mutant(&mutant());

        // Implementors only have to provide the primitive rendering hooks; the default helpers
        // keep their routing stable for plain reporters.
        assert_eq!(
            events.phases,
            vec![
                ("Doing".to_owned(), "the thing".to_owned()),
                (String::new(), ", done".to_owned()),
                (String::new(), "the result".to_owned()),
                (String::new(), ", noted".to_owned()),
            ]
        );
        assert_eq!(events.mutants, 1);
        assert_eq!(events.sweep_plan, Some((1, 3)));
        assert_eq!(events.mutant_starts, 1);
        assert_eq!(events.heartbeats, 1);
        assert_eq!(events.convergence, [3, 5]);
        assert!(events.convergence.windows(2).all(|counts| counts[0] < counts[1]));
    }

    /// The one hook with no default has to be routed by the implementor, not by the trait.
    fn mutant() -> Mutant {
        Mutant {
            item_path: ("subject::less".to_owned()).into(),
            original: "a < b".to_owned().into(),
            replacement: "a <= b".to_owned().into(),
            ..fixtures::mutant()
        }
    }
}
