// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The edits one build round writes into the scratch tree.

use std::fs;

use camino::Utf8PathBuf;

use super::super::workspace::Workspace;
use super::{Converger, Guards};
use crate::discover::{Plan, TargetFile};
use crate::error::error;
use crate::parse::{BOM, strip_bom};
use crate::schema::{self, Guard};
use crate::{HashMap, HashSet, Result};

#[derive(Debug)]
pub(super) struct Instrumented {
    pub(super) guards: Guards,
    pub(super) unavailable: Vec<u32>,
}

/// What each file of the copied tree was last instrumented with, so a round can skip the rest.
///
/// The rollback loop instruments the whole tree once per round, but between two rounds only the
/// files whose mutants were withdrawn can differ — every other file would be read from disk,
/// re-spliced, and found to be byte-identical to what is already there. With a rollback ceiling in
/// the hundreds and a large tree that product is the cost, so what a round produced is remembered
/// and the next round rewrites only what it changed.
///
/// Only the text of files that carry live mutants is kept. A file with none is written back to its
/// original once and then never touches this again, so caching it would hold the whole tree in
/// memory to serve a case that does not recur.
#[derive(Debug, Default)]
pub(super) struct Splices {
    /// The tree these splices describe, so a workspace swapped underneath is not trusted.
    pub(super) root: Utf8PathBuf,

    /// The original text of each instrumented file, read once rather than once per round.
    pub(super) sources: HashMap<Utf8PathBuf, Original>,

    /// The live ordinals last spliced into each file, and the guards that resulted.
    ///
    /// The ordinals are the identity of the splice: two rounds that put the same live mutants into
    /// a file produce the same text and therefore the same guards, so the recorded ones can be
    /// handed back instead of being recomputed.
    pub(super) placed: HashMap<Utf8PathBuf, (Vec<u32>, HashMap<u32, Guard>)>,

    /// Maps file paths to positions in the growing plan.
    file_index: HashMap<Utf8PathBuf, usize>,

    /// Mutant positions grouped once as stages extend the plan.
    mutants_by_file: HashMap<Utf8PathBuf, Vec<usize>>,

    /// The file containing each live ordinal, for withdrawal deltas.
    file_by_ordinal: HashMap<u32, Utf8PathBuf>,

    indexed_files: usize,
    indexed_mutants: usize,
    plan_identity: Option<usize>,
    withdrawn: HashSet<u32>,
}

#[derive(Debug)]
pub(super) struct Original {
    parsed: String,
    serialized: String,
    digest: String,
}

impl Original {
    fn instrumented(&self, parsed: String) -> String {
        if self.serialized.starts_with(BOM) {
            // #[gamma::skip(arith.add_to_mul, reason = "String capacity is only a reservation and cannot change the bytes appended to the returned string")]
            let mut serialized = String::with_capacity(BOM.len_utf8() + parsed.len());
            serialized.push(BOM);
            serialized.push_str(&parsed);
            serialized
        } else {
            parsed
        }
    }
}

impl Splices {
    /// Drops indexes whose positions were invalidated by reordering the plan's mutant vector.
    ///
    /// Instrumented text and guards remain valid because they are keyed by file and ordinal, not
    /// by vector position.
    pub(super) fn plan_reordered(&mut self) {
        self.file_index.clear();
        self.mutants_by_file.clear();
        self.file_by_ordinal.clear();
        // #[gamma::skip(all, reason = "the replacement is exactly the type default already written here, so it is semantically identical")]
        self.indexed_files = 0;
        // #[gamma::skip(all, reason = "the replacement is exactly the type default already written here, so it is semantically identical")]
        self.indexed_mutants = 0;
        // #[gamma::skip(all, reason = "the replacement is exactly the type default already written here, so it is semantically identical")]
        self.plan_identity = None;
        self.withdrawn.clear();
    }

    /// Writes the instrumented form of every mutated file into the copied tree.
    ///
    /// Returns where each live mutant's guard landed, which is what attributes a compiler diagnostic
    /// back to the mutant responsible.
    ///
    /// The implementation visits only files whose live ordinals actually changed since the prior
    /// round ("dirty" files). Every other file's cached guards are returned from `self.placed`
    /// without re-reading, re-splicing or re-writing the file.
    pub(super) fn instrument(&mut self, work: &Workspace, plan: &Plan, withdrawn: &HashSet<u32>) -> Result<Instrumented> {
        if self.root != work.root {
            self.root = work.root.clone();
            self.sources.clear();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.placed.clear();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.file_index.clear();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.mutants_by_file.clear();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.file_by_ordinal.clear();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.indexed_files = 0;
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.indexed_mutants = 0;
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            self.withdrawn.clear();
        }

        self.restore_removed_files(work, plan)?;
        let dirty = self.refresh_index(plan, withdrawn);
        let mut guards = Guards::default();
        let mut unavailable = Vec::new();

        for (path, (_ordinals, found)) in &self.placed {
            if !dirty.contains(path) {
                for (ordinal, guard) in found {
                    let _ = guards.insert(*ordinal, (path.clone(), guard.clone()));
                }
            }
        }

        let mut dirty: Vec<usize> = dirty.iter().filter_map(|path| self.file_index.get(path).copied()).collect();
        // #[gamma::skip(iter.remove_sort, reason = "dirty files are independent and guards are keyed by ordinal, so visitation order cannot affect text, guards, or errors")]
        dirty.sort_unstable();

        'dirty_files: for position in dirty {
            let Some(file) = plan.files.get(position) else {
                continue 'dirty_files;
            };
            let live: Vec<_> = self
                .mutants_by_file
                .get(&file.path)
                .into_iter()
                .flatten()
                .filter_map(|position| plan.mutants.get(*position))
                // #[gamma::skip(relational.gt_to_ge, reason = "ordinal is u32 and zero is absent from mutants_by_file because refresh_index excludes the sentinel before indexing")]
                .filter(|mutant| mutant.ordinal > 0 && !withdrawn.contains(&mutant.ordinal))
                .collect();
            let ordinals: Vec<u32> = live.iter().map(|mutant| mutant.ordinal).collect();

            if let Some((placed, found)) = self.placed.get(&file.path)
                && *placed == ordinals
            {
                for (ordinal, guard) in found {
                    let _ = guards.insert(*ordinal, (file.path.clone(), guard.clone()));
                }

                // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
                continue 'dirty_files;
            }

            let original = self.original(work, file)?;
            let generation_matches = plan.digests.get(&file.path).is_none_or(|expected| original.digest == *expected);

            // A file whose every mutant has been withdrawn still has to be rewritten, back to the
            // original, or the previous round's instrumented copy would survive its own withdrawal
            // and the rollback loop could never converge.
            // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
            let (instrumented, found) = if live.as_slice().is_empty() || !generation_matches {
                if !generation_matches {
                    unavailable.extend(live.iter().map(|mutant| mutant.ordinal));
                }
                (original.serialized.clone(), HashMap::default())
            } else {
                let (parsed, found) = schema::instrument_with_guards(&original.parsed, &live)?;
                (original.instrumented(parsed), found)
            };

            for (ordinal, guard) in &found {
                let _ = guards.insert(*ordinal, (file.path.clone(), guard.clone()));
            }

            // A live mutant with no guard would still be run — with nothing in the tree to make it
            // behave differently — and its verdict recorded as a survivor. That is a wrong answer
            // rather than a missing one, and nothing downstream could tell the difference, so the
            // invariant is checked rather than assumed.
            if generation_matches && let Some(missing) = live.iter().find(|mutant| !guards.contains_key(&mutant.ordinal)) {
                return Err(Converger::missing_guard_error(missing));
            }

            // Rewriting a file with the text it already holds would make cargo rebuild its crate, so
            // an unchanged file is left alone and its mtime with it.
            let destination = work.root.join(&file.path);
            let _written = Workspace::overwrite(&work.root, &destination, &instrumented)?;

            let _replaced = self.placed.insert(file.path.clone(), (ordinals, found));

            // A file back at its original text will not be spliced again unless its mutants come
            // back, which they cannot: withdrawal is permanent for the rest of the run.
            // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
            if live.as_slice().is_empty() {
                let _dropped = self.sources.remove(&file.path);
            }
        }

        Ok(Instrumented { guards, unavailable })
    }

    fn restore_removed_files(&mut self, work: &Workspace, plan: &Plan) -> Result<()> {
        let identity = core::ptr::from_ref(plan) as usize;
        // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
        if same_plan(self.plan_identity, identity) {
            return Ok(());
        }

        let current: HashSet<&camino::Utf8Path> = plan.files.iter().map(|file| file.path.as_path()).collect();
        let removed: Vec<Utf8PathBuf> = self
            .placed
            .keys()
            .filter(|path| absent_from_plan(path, &current))
            .cloned()
            .collect();

        for path in removed {
            if let Some(original) = self.sources.get(&path) {
                let destination = work.root.join(&path);
                let _written = Workspace::overwrite(&work.root, &destination, &original.serialized)?;
            }
            let _placed = self.placed.remove(&path);
            let _source = self.sources.remove(&path);
        }

        Ok(())
    }

    fn refresh_index(&mut self, plan: &Plan, withdrawn: &HashSet<u32>) -> HashSet<Utf8PathBuf> {
        let mut dirty = HashSet::default();
        let plan_identity = core::ptr::from_ref(plan) as usize;

        // #[gamma::skip(all, reason = "perturbing the cached pointer identity can only force a conservative rebuild of indexes from the same plan")]
        if self.plan_identity != Some(plan_identity) || self.indexed_files > plan.files.len() || self.indexed_mutants > plan.mutants.len() {
            dirty.extend(self.file_index.keys().cloned());
            self.file_index.clear();
            self.mutants_by_file.clear();
            self.file_by_ordinal.clear();
            self.indexed_files = 0;
            self.indexed_mutants = 0;
            dirty.extend(plan.files.iter().map(|file| file.path.clone()));
        }
        // #[gamma::skip(all, reason = "perturbing the stored pointer identity only makes the next call conservatively rebuild equivalent indexes")]
        self.plan_identity = Some(plan_identity);

        for (position, file) in plan.files.iter().enumerate().skip(self.indexed_files) {
            let _previous = self.file_index.insert(file.path.clone(), position);
            let _new = dirty.insert(file.path.clone());
        }
        self.indexed_files = plan.files.len();

        for (position, mutant) in plan.mutants.iter().enumerate().skip(self.indexed_mutants) {
            // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
            if mutant.ordinal > 0 {
                self.mutants_by_file.entry(mutant.file.to_path_buf()).or_default().push(position);
                let _previous = self.file_by_ordinal.insert(mutant.ordinal, mutant.file.to_path_buf());
                let _new = dirty.insert(mutant.file.to_path_buf());
            }
        }
        self.indexed_mutants = plan.mutants.len();

        if self.withdrawn.is_subset(withdrawn) {
            for ordinal in withdrawn.difference(&self.withdrawn) {
                if let Some(path) = self.file_by_ordinal.get(ordinal) {
                    let _new = dirty.insert(path.clone());
                }
            }
        } else {
            dirty.extend(self.file_index.keys().cloned());
        }
        self.withdrawn.clone_from(withdrawn);

        dirty
    }

    /// The copied file's original text, read the first time a round needs it and kept after.
    ///
    /// The scratch tree is the immutable snapshot this run builds. Reading `file.absolute` would
    /// consult the live checkout again after discovery and synchronization; if an editor,
    /// generator, or another process changed that file meanwhile, the discovered spans would be
    /// applied to a different generation. A span that moved beyond the new text then lost its
    /// guard, while spans that remained in bounds could mutate the wrong construct.
    ///
    /// Read here rather than taken from the survey's `SourceFile`, so the byte-order mark has to be
    /// dropped here too: mutant spans index the text `syn` saw, which is the text after the mark.
    pub(super) fn original(&mut self, work: &Workspace, file: &TargetFile) -> Result<&Original> {
        if !self.sources.contains_key(&file.path) {
            let source = work.root.join(&file.path);
            let serialized =
                fs::read_to_string(source.as_std_path()).map_err(|cause| error!("could not read `{source}`").caused_by(cause))?;
            let parsed = strip_bom(&serialized).to_owned();
            let digest = crate::discover::digest(parsed.as_bytes());

            let _stored = self.sources.insert(
                file.path.clone(),
                Original {
                    parsed,
                    serialized,
                    digest,
                },
            );
        }

        Ok(self.sources.get(&file.path).unwrap_or_else(|| unreachable!("just inserted")))
    }
}

fn same_plan(previous: Option<usize>, identity: usize) -> bool {
    previous.is_none_or(|previous| previous == identity)
}

fn absent_from_plan(path: &Utf8PathBuf, current: &HashSet<&camino::Utf8Path>) -> bool {
    !current.contains(path.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrumented_text_retains_the_original_byte_order_mark() {
        let parsed = "fn f() {}\n";
        let original = Original {
            parsed: parsed.to_owned(),
            serialized: format!("{BOM}{parsed}"),
            digest: crate::discover::digest(parsed.as_bytes()),
        };

        assert_eq!(
            original.instrumented("fn f() { gamma(); }\n".to_owned()),
            format!("{BOM}fn f() {{ gamma(); }}\n")
        );
    }

    #[test]
    fn instrumented_text_without_a_byte_order_mark_is_returned_verbatim() {
        let original = Original {
            parsed: "original".to_owned(),
            serialized: "original".to_owned(),
            digest: crate::discover::digest(b"original"),
        };

        assert_eq!(original.instrumented("replacement".to_owned()), "replacement");
    }

    #[test]
    fn plan_reordering_clears_every_positional_cache_and_withdrawal_delta() {
        let path = Utf8PathBuf::from("src/lib.rs");
        let mut splices = Splices {
            file_index: HashMap::from_iter([(path.clone(), 2)]),
            mutants_by_file: HashMap::from_iter([(path.clone(), vec![3])]),
            file_by_ordinal: HashMap::from_iter([(5, path)]),
            indexed_files: 7,
            indexed_mutants: 11,
            plan_identity: Some(13),
            withdrawn: HashSet::from_iter([17]),
            ..Splices::default()
        };

        splices.plan_reordered();

        assert!(splices.file_index.is_empty());
        assert!(splices.mutants_by_file.is_empty());
        assert!(splices.file_by_ordinal.is_empty());
        assert_eq!(splices.indexed_files, 0);
        assert_eq!(splices.indexed_mutants, 0);
        assert_eq!(splices.plan_identity, None);
        assert!(splices.withdrawn.is_empty());
    }

    fn empty_plan(root: &camino::Utf8Path) -> Plan {
        Plan {
            root: root.to_owned(),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: HashMap::default(),
            skipped: Vec::new(),
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    #[test]
    fn changing_workspace_roots_discards_every_cached_value() {
        let directory = crate::testing::workdir("splices-root-change-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test path");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut splices = Splices {
            root: root.join("old"),
            sources: HashMap::from_iter([(
                Utf8PathBuf::from("src/lib.rs"),
                Original {
                    parsed: "old".to_owned(),
                    serialized: "old".to_owned(),
                    digest: crate::discover::digest(b"old"),
                },
            )]),
            placed: HashMap::from_iter([(Utf8PathBuf::from("src/lib.rs"), (vec![1], HashMap::default()))]),
            file_index: HashMap::from_iter([(Utf8PathBuf::from("src/lib.rs"), 1)]),
            mutants_by_file: HashMap::from_iter([(Utf8PathBuf::from("src/lib.rs"), vec![1])]),
            file_by_ordinal: HashMap::from_iter([(1, Utf8PathBuf::from("src/lib.rs"))]),
            indexed_files: 1,
            indexed_mutants: 1,
            plan_identity: Some(1),
            withdrawn: HashSet::from_iter([1]),
        };

        let guards = splices
            .instrument(&work, &empty_plan(&root), &HashSet::default())
            .expect("an empty plan resets stale caches");

        assert!(guards.guards.is_empty());
        assert!(guards.unavailable.is_empty());
        assert_eq!(splices.root, root);
        assert!(splices.sources.is_empty());
        assert!(splices.placed.is_empty());
        assert!(splices.file_index.is_empty());
        assert!(splices.mutants_by_file.is_empty());
        assert!(splices.file_by_ordinal.is_empty());
        assert_eq!(splices.indexed_files, 0);
        assert_eq!(splices.indexed_mutants, 0);
        assert!(splices.withdrawn.is_empty());
    }

    #[test]
    fn refresh_index_tracks_growth_withdrawal_reversal_and_shrinkage() {
        let root = Utf8PathBuf::from("workspace");
        let target = |path: &str| TargetFile {
            path: Utf8PathBuf::from(path),
            absolute: root.join(path),
            package: "subject".to_owned(),
        };
        let mut first = crate::fixtures::mutant();
        first.ordinal = 1;
        first.file = Utf8PathBuf::from("src/a.rs").into();
        let mut second = crate::fixtures::mutant();
        second.ordinal = 2;
        second.file = Utf8PathBuf::from("src/b.rs").into();
        let mut plan = empty_plan(&root);
        plan.files.push(target("src/a.rs"));
        plan.mutants.push(first);
        let mut splices = Splices::default();

        assert_eq!(
            splices.refresh_index(&plan, &HashSet::default()),
            HashSet::from_iter([Utf8PathBuf::from("src/a.rs")])
        );
        assert!(splices.refresh_index(&plan, &HashSet::default()).is_empty());

        plan.files.push(target("src/b.rs"));
        plan.mutants.push(second);
        assert_eq!(
            splices.refresh_index(&plan, &HashSet::default()),
            HashSet::from_iter([Utf8PathBuf::from("src/b.rs")])
        );
        assert_eq!(
            splices.refresh_index(&plan, &HashSet::from_iter([2])),
            HashSet::from_iter([Utf8PathBuf::from("src/b.rs")])
        );
        assert_eq!(
            splices.refresh_index(&plan, &HashSet::default()),
            HashSet::from_iter([Utf8PathBuf::from("src/a.rs"), Utf8PathBuf::from("src/b.rs")])
        );

        plan.files.truncate(1);
        plan.mutants.truncate(1);
        let dirty = splices.refresh_index(&plan, &HashSet::default());
        assert!(dirty.contains(camino::Utf8Path::new("src/a.rs")));
        assert!(dirty.contains(camino::Utf8Path::new("src/b.rs")));
        assert_eq!(splices.indexed_files, 1);
        assert_eq!(splices.indexed_mutants, 1);
        assert_eq!(splices.file_by_ordinal.len(), 1);
        assert!(splices.file_by_ordinal.contains_key(&1));
    }

    #[test]
    fn plan_identity_and_removed_path_predicates_cover_both_sides() {
        assert!(same_plan(None, 7));
        assert!(same_plan(Some(7), 7));
        assert!(!same_plan(Some(8), 7));

        let present = Utf8PathBuf::from("src/lib.rs");
        let absent = Utf8PathBuf::from("src/other.rs");
        let current = HashSet::from_iter([present.as_path()]);
        assert!(!absent_from_plan(&present, &current));
        assert!(absent_from_plan(&absent, &current));
    }

    #[test]
    fn removed_files_are_restored_and_original_reads_are_cached() {
        let directory = crate::testing::workdir("splices-restore-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test path");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let path = Utf8PathBuf::from("src/lib.rs");
        fs::create_dir_all(root.join("src")).expect("source directory");
        fs::write(root.join(&path), "first").expect("source");
        let file = TargetFile {
            path: path.clone(),
            absolute: root.join(&path),
            package: "subject".to_owned(),
        };
        let mut splices = Splices::default();
        assert_eq!(splices.original(&work, &file).unwrap().parsed, "first");
        fs::write(root.join(&path), "second").expect("changed source");
        assert_eq!(splices.original(&work, &file).unwrap().parsed, "first");

        splices.root = root.clone();
        splices.plan_identity = Some(usize::MAX);
        let _ = splices.placed.insert(path.clone(), (vec![1], HashMap::default()));
        fs::write(root.join(&path), "instrumented").expect("instrumented source");
        splices
            .restore_removed_files(&work, &empty_plan(&root))
            .expect("removed file restoration succeeds");

        assert_eq!(fs::read_to_string(root.join(&path)).unwrap(), "first");
        assert!(!splices.sources.contains_key(&path));
        assert!(!splices.placed.contains_key(&path));
    }
}
