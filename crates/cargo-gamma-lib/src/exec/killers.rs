// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remembering which test caught each mutant, so the next run can try that one first.

use camino::Utf8Path;

use crate::discover::{Hints, Killer, RunRecord};
use crate::model::{Mutant, MutantId};
use crate::{HashMap, HashSet};

/// What caught each mutant last time, keyed by mutant id.
///
/// A run already stops at the first binary that convicts, and the output watcher ends a binary as
/// soon as the harness announces a failure. What neither does is remember: the next run rediscovers
/// the same killer by paying for everything ahead of it in the same order. This is the memory.
///
/// It comes from two places and is written back to one. The run record beside the verdicts is the
/// warm local copy; the checked-in artifact is the one that survives a clean checkout, which is the
/// case this is worth the most in — a fresh CI container has no record at all, so without it every
/// scheduled run pays full price for every re-kill. The record wins where both speak, because it is
/// this machine's last answer rather than whatever was committed.
///
/// Both sit on quite different terms from a verdict. A verdict is believed, so it is gated on the
/// build context and on the digest of the file it was found in. Every entry here is a guess that
/// the run immediately checks by running the named test, and a guess that does not convict costs
/// one filtered process and is discarded. Nothing here can move a verdict, so nothing here needs
/// invalidating.
#[derive(Debug, Default)]
pub(super) struct Killers {
    /// The killing test of each mutant that had one, keyed by [`crate::model::Mutant::id`].
    ///
    /// Keyed by id rather than by position or line because the id hashes the file, item path,
    /// mutator, normalized site text, occurrence and replacement index. An edit elsewhere in the
    /// file leaves the key alone, and a site that genuinely changed simply stops matching instead
    /// of pointing at a test that has nothing to do with it.
    entries: HashMap<MutantId, Killer>,
}

impl Killers {
    /// Reads the probes the record and the checked-in artifact hold, or returns an empty map.
    ///
    /// `base` is the scratch directory holding the run record; `root` is the workspace the artifact
    /// is checked in to.
    ///
    /// Every failure on either side is an empty map. Being unable to read them costs the run only
    /// the time it would have saved, which is what lets either file be deleted, truncated or
    /// written by another version without anyone having to care.
    pub(super) fn load(base: &Utf8Path, root: &Utf8Path) -> Self {
        let mut entries = Hints::load(root).probes();

        // The record last, so this machine's own last answer wins over the committed one wherever
        // both name a mutant: a probe is checked either way, but the fresher guess is likelier to
        // convict, and paying for the stale one first would be paying for a test twice.
        entries.extend(RunRecord::load(base).probes().clone());

        Self { entries }
    }

    /// What caught this mutant last time, if anything did.
    pub(super) fn hint(&self, id: &str) -> Option<&Killer> {
        self.entries.get(id)
    }

    /// Records what caught a mutant this run.
    pub(super) fn record(&mut self, id: MutantId, killer: Killer) {
        let _previous = self.entries.insert(id, killer);
    }

    /// Forgets what it thought caught this mutant.
    ///
    /// Called when a run's own verdict for a mutant names no test — the mutant survived, or was
    /// caught by something other than a failing assertion. Keeping the old entry would make the
    /// run after this one pay for a probe that has already been shown not to convict.
    pub(super) fn forget(&mut self, id: &str) {
        let _previous = self.entries.remove(id);
    }

    /// Writes the hints back into the record, and says nothing if it cannot.
    ///
    /// A run that could not write this has still produced every verdict it was asked for, and
    /// failing it over a scratch file would turn an optimization into a dependency.
    pub(super) fn store(&self, base: &Utf8Path, population: &[Mutant]) {
        let current: HashSet<&MutantId> = population.iter().map(|mutant| &mutant.id).collect();
        let probes: HashMap<MutantId, Killer> = self
            .entries
            .iter()
            .filter(|(id, _killer)| current.contains(id))
            .map(|(id, killer)| (id.clone(), killer.clone()))
            .collect();

        RunRecord::store_probes(base, &probes);
    }

    /// How many mutants this map has a killer for.
    ///
    /// Only the tests ask. A run has nothing to say about the size of the map: an entry that missed
    /// cost one filtered process, and an entry whose mutant is no longer there cost nothing at all.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it knows of no killers at all.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn killer(test: &str) -> Killer {
        Killer {
            package: "alpha".to_owned(),
            target: "lib".to_owned(),
            test: test.to_owned(),
        }
    }

    fn mutant(id: &str) -> Mutant {
        Mutant {
            id: id.into(),
            ..crate::fixtures::mutant()
        }
    }

    #[test]
    fn a_map_round_trips_through_the_record() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a utf-8 path");

        let mut killers = Killers::default();
        killers.record("abc".into(), killer("tests::round_trip"));

        killers.store(base, &[mutant("abc")]);

        let read = Killers::load(base, base);

        assert_eq!(read.len(), 1);
        assert_eq!(read.hint("abc"), Some(&killer("tests::round_trip")));
    }

    #[test]
    fn a_missing_record_is_an_empty_map() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a utf-8 path");

        assert!(Killers::load(base, base).is_empty());
    }

    #[test]
    fn a_corrupt_record_is_an_empty_map() {
        // A record that cannot be parsed must cost the run nothing but the speed-up it would have
        // given. Failing here would make a scratch file something a run depends on.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a utf-8 path");

        std::fs::write(base.join("last-gamma-run.json").as_std_path(), "{ not json").expect("the file to be written");

        assert!(Killers::load(base, base).is_empty());
    }

    #[test]
    fn hints_survive_a_build_context_this_run_does_not_share() {
        // The whole reason hints sit outside the digested part of the record: a feature change
        // discards every verdict, and discarding the hints with them would make the map cold on
        // exactly the runs it is worth the most on.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a utf-8 path");

        let mut killers = Killers::default();
        killers.record("abc".into(), killer("tests::round_trip"));
        killers.store(base, &[mutant("abc")]);

        let elsewhere = crate::discover::record_context(&crate::discover::RecordContext {
            toolchain: Some("some other toolchain"),
            ..crate::discover::RecordContext::default()
        })
        .expect("a named toolchain gives a context");

        RunRecord::from_run(base, &[], &elsewhere, &[]).store(base, base);

        assert_eq!(Killers::load(base, base).hint("abc"), Some(&killer("tests::round_trip")));
    }

    #[test]
    fn forgetting_a_mutant_drops_its_hint() {
        // A mutant whose verdict named no test must not leave the previous run's hint behind, or
        // every run after this one pays for a probe already shown not to convict.
        let mut killers = Killers::default();
        killers.record("abc".into(), killer("tests::round_trip"));

        killers.forget("abc");

        assert!(killers.hint("abc").is_none());
    }

    #[test]
    fn a_hint_names_only_the_binary_it_was_recorded_against() {
        let hint = killer("tests::round_trip");

        assert!(hint.names("alpha", "lib"));
        assert!(!hint.names("alpha", "integration"));
        assert!(!hint.names("beta", "lib"));
    }

    #[test]
    fn recording_a_mutant_twice_keeps_the_later_killer() {
        // The sweep records as it goes, and a mutant is judged once per run; a second record for
        // the same id is a re-run, whose answer is the current one.
        let mut killers = Killers::default();

        killers.record("abc".into(), killer("tests::first"));
        killers.record("abc".into(), killer("tests::second"));

        assert_eq!(killers.len(), 1);
        assert_eq!(killers.hint("abc").map(|found| found.test.as_str()), Some("tests::second"));
    }

    #[test]
    fn storing_prunes_hints_outside_the_current_population() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a UTF-8 path");
        let mut killers = Killers::default();

        killers.record("old".into(), killer("tests::old"));
        killers.record("new".into(), killer("tests::new"));
        killers.store(base, &[mutant("new")]);

        let read = Killers::load(base, base);

        assert!(read.hint("old").is_none());
        assert_eq!(read.hint("new"), Some(&killer("tests::new")));
    }
}
