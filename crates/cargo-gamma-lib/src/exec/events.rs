// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::Utf8Path;

use super::session::Session;
use crate::Result;
use crate::discover::Plan;
use crate::estimate::Estimate;
use crate::model::Mutant;

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

    /// The build reported how far along it is, in a line of its own rendering.
    ///
    /// Passed through rather than reconstructed: cargo holds the unit graph, so it is the only
    /// party that knows the denominator.
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

    /// Something about this run is likely to cost far more than it is worth, and the user can fix it.
    ///
    /// Distinct from a phase because a phase describes what is happening and this describes what
    /// should perhaps not be. It is also the one kind of progress that has to survive the display
    /// being off: the display resolves to whether a terminal is attached, and a CI job is exactly
    /// where a run that quietly takes six hours is least affordable and least visible.
    fn warn(&mut self, _message: &str) {}

    /// A mutant finished.
    fn mutant(&mut self, mutant: &Mutant);

    /// The fixed cost is paid, the tree compiles, and the first mutant is about to be tested.
    ///
    /// The only moment at which a projection of the run is both possible and useful: everything
    /// before it is measured, everything after it is the wait the user is deciding whether to sit
    /// through, so the projection is handed over here rather than recomputed by whoever wants it.
    fn measured(&mut self, _plan: &Plan, _session: &Session, _estimate: &Estimate) {}
}

#[cfg(test)]
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
        let estimate = Estimate {
            live: 0,
            withdrawn: 0,
            build: Duration::ZERO,
            baseline: Duration::ZERO,
            mutants: Duration::ZERO,
            settled: Duration::ZERO,
            stalling: Duration::ZERO,
            jobs: 1,
            worst: Duration::ZERO,
        };

        events.begin("Doing", "Done", "the thing");
        events.end(", done");
        events.complete("the result");
        events.outcome(", noted");
        events.measured(&plan, &session, &estimate);
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
