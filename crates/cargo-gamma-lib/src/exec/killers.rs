// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remembering which test caught each mutant, so the next run can try that one first.

use camino::Utf8Path;

#[cfg(test)]
use crate::discover::SiteIdentity;
use crate::discover::{GeneralizedHints, Hints, Killer, RunRecord};
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
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Killers {
    /// The killing test of each mutant that had one, keyed by [`crate::model::Mutant::id`].
    ///
    /// Keyed by id rather than by position or line because the id hashes the file, item path,
    /// mutator, normalized site text, occurrence and replacement index. An edit elsewhere in the
    /// file leaves the key alone, and a site that genuinely changed simply stops matching instead
    /// of pointing at a test that has nothing to do with it.
    entries: HashMap<MutantId, Killer>,

    /// Score-neutral item, file-binary and reach-cluster knowledge.
    generalized: GeneralizedHints,
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
        let promoted = Hints::load(root);
        let record = RunRecord::load(base);
        let incomplete = RunRecord::load_incomplete(base);
        Self::from_records(&promoted, &record, &incomplete)
    }

    fn from_records(promoted: &Hints, record: &RunRecord, incomplete: &RunRecord) -> Self {
        let mut entries = promoted.probes();
        let mut generalized = promoted.generalized();

        // The record last, so this machine's own last answer wins over the committed one wherever
        // both name a mutant: a probe is checked either way, but the fresher guess is likelier to
        // convict, and paying for the stale one first would be paying for a test twice.
        entries.extend(record.probes().iter().map(|(id, killer)| (id.clone(), killer.clone())));
        Self::merge_generalized(&mut generalized, record.generalized());
        entries.extend(incomplete.probes().iter().map(|(id, killer)| (id.clone(), killer.clone())));
        Self::merge_generalized(&mut generalized, incomplete.generalized());

        Self { entries, generalized }
    }

    /// What caught this mutant last time, if anything did.
    pub(super) fn hint(&self, id: &str) -> Option<&Killer> {
        self.entries.get(id)
    }

    /// Records what caught a mutant this run.
    pub(crate) fn record(&mut self, id: MutantId, killer: Killer) {
        let _previous = self.entries.insert(id, killer);
    }

    /// Forgets what it thought caught this mutant.
    ///
    /// Called when a run's own verdict for a mutant names no test — the mutant survived, or was
    /// caught by something other than a failing assertion. Keeping the old entry would make the
    /// run after this one pay for a probe that has already been shown not to convict.
    pub(crate) fn forget(&mut self, id: &str) {
        let _previous = self.entries.remove(id);
    }

    pub(super) const fn generalized(&self) -> &GeneralizedHints {
        &self.generalized
    }

    pub(crate) fn replace_generalized(&mut self, generalized: GeneralizedHints) {
        self.generalized = generalized;
    }

    pub(super) fn absorb_generalized(&mut self, record: &RunRecord) {
        Self::merge_generalized(&mut self.generalized, record.generalized());
    }

    #[cfg(test)]
    pub(super) fn reach_hints(&self, site: &SiteIdentity) -> Vec<Killer> {
        self.generalized
            .reach
            .iter()
            .find(|cluster| &cluster.site == site)
            .and_then(|cluster| usize::try_from(cluster.test_set).ok())
            .and_then(|index| self.generalized.test_sets.get(index))
            .cloned()
            .unwrap_or_default()
    }

    /// Writes the hints back into the record, and says nothing if it cannot.
    ///
    /// A run that could not write this has still produced every verdict it was asked for, and
    /// failing it over a scratch file would turn an optimization into a dependency.
    pub(super) fn store(&self, base: &Utf8Path, population: &[Mutant]) {
        let probes = self.current_probes(population);

        RunRecord::store_incomplete_knowledge(base, &probes, Some(&self.generalized));
    }

    pub(crate) fn stage(&self, record: &mut RunRecord, population: &[Mutant]) {
        record.replace_knowledge(self.current_probes(population), self.generalized.clone());
    }

    fn current_probes(&self, population: &[Mutant]) -> HashMap<MutantId, Killer> {
        let current: HashSet<&MutantId> = population.iter().map(|mutant| &mutant.id).collect();
        self.entries
            .iter()
            .filter(|(id, _killer)| current.contains(id))
            .map(|(id, killer)| (id.clone(), killer.clone()))
            .collect()
    }

    fn merge_generalized(target: &mut GeneralizedHints, newer: GeneralizedHints) {
        if newer.supported().is_none() {
            return;
        }
        if target.supported().is_none() {
            *target = GeneralizedHints::empty_supported();
        }
        let items: HashMap<_, _> = newer
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| ((item.file.as_path(), item.item.as_str()), index))
            .collect();
        target
            .items
            .retain(|item| !items.contains_key(&(item.file.as_path(), item.item.as_str())));
        let keep = newer
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| items.get(&(item.file.as_path(), item.item.as_str())) == Some(&index))
            .collect::<Vec<_>>();
        drop(items);
        target
            .items
            .extend(newer.items.into_iter().zip(keep).filter(|(_, keep)| *keep).map(|(item, _)| item));
        let binaries: HashMap<_, _> = newer
            .binaries
            .iter()
            .enumerate()
            .map(|(index, binary)| (binary.file.as_path(), index))
            .collect();
        target.binaries.retain(|binary| !binaries.contains_key(binary.file.as_path()));
        let keep = newer
            .binaries
            .iter()
            .enumerate()
            .map(|(index, binary)| binaries.get(binary.file.as_path()) == Some(&index))
            .collect::<Vec<_>>();
        drop(binaries);
        target.binaries.extend(
            newer
                .binaries
                .into_iter()
                .zip(keep)
                .filter(|(_, keep)| *keep)
                .map(|(binary, _)| binary),
        );
        // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
        if !newer.reach.is_empty() {
            target.test_sets = newer.test_sets;
            target.reach = newer.reach;
        }
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::discover::{BinaryHint, FileBinaryHints, GENERALIZED_HINTS_VERSION, ItemHints, RankedHint, ReachCluster};

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

    fn completed_record() -> RunRecord {
        let population = [mutant("fedcba987654")];
        let mut learned = Killers::default();
        learned.record("fedcba987654".into(), killer("tests::completed"));
        learned.replace_generalized(GeneralizedHints {
            binaries: vec![FileBinaryHints {
                file: "src/completed.rs".into(),
                candidates: vec![RankedHint {
                    candidate: BinaryHint {
                        package: "alpha".into(),
                        target: "lib".into(),
                    },
                    seeds: 2,
                    hits: 3,
                    misses: 1,
                    measured_ms: 37,
                    samples: 2,
                    order: 11,
                }],
            }],
            ..GeneralizedHints::empty_supported()
        });
        let mut record = RunRecord::default();
        learned.stage(&mut record, &population);
        record
    }

    #[test]
    fn completed_records_round_trip_without_incomplete_fallback() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let base = root.join("cache");
        std::fs::create_dir(&base).unwrap();
        let record = completed_record();
        record.store_completed(&base).unwrap();
        assert_eq!(
            RunRecord::load(&base).probes().get("fedcba987654"),
            Some(&killer("tests::completed"))
        );
        assert!(RunRecord::load_incomplete(&base).probes().is_empty());
        let loaded = Killers::load(&base, root);
        assert_completed_knowledge(&loaded);
    }

    #[test]
    fn completed_records_supply_exact_probes_and_generalized_file_knowledge() {
        let record = completed_record();
        let loaded = Killers::from_records(&Hints::default(), &record, &RunRecord::default());
        assert_completed_knowledge(&loaded);
    }

    fn assert_completed_knowledge(loaded: &Killers) {
        assert_eq!(loaded.hint("fedcba987654"), Some(&killer("tests::completed")), "{loaded:?}");
        let hints = loaded.generalized().supported().unwrap();
        assert_eq!(hints.binaries.len(), 1);
        assert_eq!(hints.binaries[0].file, Utf8Path::new("src/completed.rs"));
        let candidate = &hints.binaries[0].candidates[0];
        assert_eq!(
            (
                candidate.seeds,
                candidate.hits,
                candidate.misses,
                candidate.measured_ms,
                candidate.samples,
                candidate.order
            ),
            (2, 3, 1, 37, 2, 11)
        );
        assert_eq!(
            candidate.candidate,
            BinaryHint {
                package: "alpha".into(),
                target: "lib".into()
            }
        );
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

    fn ranked<T>(candidate: T) -> RankedHint<T> {
        RankedHint {
            candidate,
            seeds: 1,
            hits: 1,
            misses: 2,
            measured_ms: 3,
            samples: 4,
            order: 5,
        }
    }

    fn site(item: &str) -> SiteIdentity {
        SiteIdentity {
            file: "src/lib.rs".into(),
            item: item.to_owned(),
            mutator: "cond.negate".to_owned(),
            normalized_text: "ready".to_owned(),
            occurrence: 0,
        }
    }

    #[test]
    fn generalized_knowledge_replaces_matching_keys_and_preserves_others() {
        let mut target = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            items: vec![
                ItemHints {
                    file: "src/lib.rs".into(),
                    item: "old".to_owned(),
                    candidates: vec![ranked(killer("tests::old"))],
                },
                ItemHints {
                    file: "src/keep.rs".into(),
                    item: "keep".to_owned(),
                    candidates: vec![ranked(killer("tests::keep"))],
                },
            ],
            binaries: vec![FileBinaryHints {
                file: "src/lib.rs".into(),
                candidates: vec![ranked(BinaryHint {
                    package: "old".to_owned(),
                    target: "old".to_owned(),
                })],
            }],
            test_sets: vec![vec![killer("tests::old")]],
            reach: vec![ReachCluster {
                site: site("old"),
                test_set: 0,
            }],
        };
        let newer = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            items: vec![ItemHints {
                file: "src/lib.rs".into(),
                item: "old".to_owned(),
                candidates: vec![ranked(killer("tests::new"))],
            }],
            binaries: vec![FileBinaryHints {
                file: "src/lib.rs".into(),
                candidates: vec![ranked(BinaryHint {
                    package: "new".to_owned(),
                    target: "new".to_owned(),
                })],
            }],
            test_sets: vec![vec![killer("tests::reach")]],
            reach: vec![ReachCluster {
                site: site("new"),
                test_set: 0,
            }],
        };

        Killers::merge_generalized(&mut target, newer.clone());

        assert_eq!(target.items.len(), 2);
        assert_eq!(target.items[0].file, Utf8Path::new("src/keep.rs"));
        assert_eq!(target.items[1], newer.items[0]);
        assert_eq!(target.binaries, newer.binaries);
        assert_eq!(target.test_sets, newer.test_sets);
        assert_eq!(target.reach, newer.reach);
    }

    #[test]
    fn indexed_merge_preserves_last_duplicate_precedence_and_order() {
        let item = |name: &str, test: &str| ItemHints {
            file: "src/lib.rs".into(),
            item: name.into(),
            candidates: vec![ranked(killer(test))],
        };
        let binary = |file: &str, target: &str| FileBinaryHints {
            file: file.into(),
            candidates: vec![ranked(BinaryHint {
                package: "subject".into(),
                target: target.into(),
            })],
        };
        let mut target = GeneralizedHints::empty_supported();
        target.items = vec![item("replace", "old"), item("keep", "keep"), item("replace", "older")];
        target.binaries = vec![binary("src/a.rs", "old"), binary("src/keep.rs", "keep")];
        let mut newer = GeneralizedHints::empty_supported();
        newer.items = vec![item("replace", "first"), item("other", "other"), item("replace", "last")];
        newer.binaries = vec![binary("src/a.rs", "first"), binary("src/b.rs", "other"), binary("src/a.rs", "last")];
        Killers::merge_generalized(&mut target, newer);
        assert_eq!(
            target.items,
            vec![item("keep", "keep"), item("other", "other"), item("replace", "last")]
        );
        assert_eq!(
            target.binaries,
            vec![
                binary("src/keep.rs", "keep"),
                binary("src/b.rs", "other"),
                binary("src/a.rs", "last")
            ]
        );
    }

    #[test]
    fn unsupported_generalized_knowledge_is_ignored() {
        let mut target = GeneralizedHints::empty_supported();
        target.items.push(ItemHints {
            file: "src/keep.rs".into(),
            item: "keep".to_owned(),
            candidates: vec![ranked(killer("tests::keep"))],
        });
        let original = target.clone();

        Killers::merge_generalized(
            &mut target,
            GeneralizedHints {
                version: GENERALIZED_HINTS_VERSION + 1,
                items: vec![ItemHints {
                    file: "src/new.rs".into(),
                    item: "new".to_owned(),
                    candidates: vec![ranked(killer("tests::new"))],
                }],
                ..GeneralizedHints::default()
            },
        );

        assert_eq!(target, original);
    }

    #[test]
    fn replacement_and_reach_lookup_use_the_new_generalized_knowledge() {
        let expected = killer("tests::reach");
        let known_site = site("known");
        let generalized = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            test_sets: vec![vec![expected.clone()]],
            reach: vec![ReachCluster {
                site: known_site.clone(),
                test_set: 0,
            }],
            ..GeneralizedHints::default()
        };
        let mut killers = Killers::default();

        killers.replace_generalized(generalized.clone());

        assert_eq!(killers.generalized(), &generalized);
        assert_eq!(killers.reach_hints(&known_site), vec![expected]);
        assert!(killers.reach_hints(&site("unknown")).is_empty());
    }

    #[test]
    fn generalized_knowledge_round_trips_with_exact_probes() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let base = Utf8Path::from_path(dir.path()).expect("a UTF-8 path");
        let known_site = site("stored");
        let generalized = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            test_sets: vec![vec![killer("tests::reach")]],
            reach: vec![ReachCluster {
                site: known_site.clone(),
                test_set: 0,
            }],
            ..GeneralizedHints::default()
        };
        let mut killers = Killers::default();
        killers.record("abc".into(), killer("tests::exact"));
        killers.replace_generalized(generalized.clone());

        killers.store(base, &[mutant("abc")]);
        let read = Killers::load(base, base);

        assert_eq!(read.hint("abc"), Some(&killer("tests::exact")));
        assert_eq!(read.generalized(), &generalized);
        assert_eq!(read.reach_hints(&known_site), vec![killer("tests::reach")]);
    }
}
