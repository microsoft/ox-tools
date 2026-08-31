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
}

impl Original {
    fn instrumented(&self, parsed: String) -> String {
        if self.serialized.starts_with(BOM) {
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
        self.indexed_files = 0;
        self.indexed_mutants = 0;
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
    pub(super) fn instrument(&mut self, work: &Workspace, plan: &Plan, withdrawn: &HashSet<u32>) -> Result<Guards> {
        if self.root != work.root {
            self.root = work.root.clone();
            self.sources.clear();
            self.placed.clear();
            self.file_index.clear();
            self.mutants_by_file.clear();
            self.file_by_ordinal.clear();
            self.indexed_files = 0;
            self.indexed_mutants = 0;
            self.withdrawn.clear();
        }

        self.restore_removed_files(work, plan)?;
        let dirty = self.refresh_index(plan, withdrawn);
        let mut guards = Guards::default();

        for (path, (_ordinals, found)) in &self.placed {
            if !dirty.contains(path) {
                for (ordinal, guard) in found {
                    let _ = guards.insert(*ordinal, (path.clone(), guard.clone()));
                }
            }
        }

        let mut dirty: Vec<usize> = dirty.iter().filter_map(|path| self.file_index.get(path).copied()).collect();
        dirty.sort_unstable();

        for position in dirty {
            let Some(file) = plan.files.get(position) else {
                continue;
            };
            let live: Vec<_> = self
                .mutants_by_file
                .get(&file.path)
                .into_iter()
                .flatten()
                .filter_map(|position| plan.mutants.get(*position))
                .filter(|mutant| mutant.ordinal > 0 && !withdrawn.contains(&mutant.ordinal))
                .collect();
            let ordinals: Vec<u32> = live.iter().map(|mutant| mutant.ordinal).collect();

            if let Some((placed, found)) = self.placed.get(&file.path)
                && *placed == ordinals
            {
                for (ordinal, guard) in found {
                    let _ = guards.insert(*ordinal, (file.path.clone(), guard.clone()));
                }

                continue;
            }

            let original = self.original(file)?;

            // A file whose every mutant has been withdrawn still has to be rewritten, back to the
            // original, or the previous round's instrumented copy would survive its own withdrawal
            // and the rollback loop could never converge.
            let (instrumented, found) = if live.is_empty() {
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
            if let Some(missing) = live.iter().find(|mutant| !guards.contains_key(&mutant.ordinal)) {
                return Err(Converger::missing_guard_error(missing));
            }

            // Rewriting a file with the text it already holds would make cargo rebuild its crate, so
            // an unchanged file is left alone and its mtime with it.
            let destination = work.root.join(&file.path);
            let _written = Workspace::overwrite(&work.root, &destination, &instrumented)?;

            let _replaced = self.placed.insert(file.path.clone(), (ordinals, found));

            // A file back at its original text will not be spliced again unless its mutants come
            // back, which they cannot: withdrawal is permanent for the rest of the run.
            if live.is_empty() {
                let _dropped = self.sources.remove(&file.path);
            }
        }

        Ok(guards)
    }

    fn restore_removed_files(&mut self, work: &Workspace, plan: &Plan) -> Result<()> {
        let identity = core::ptr::from_ref(plan) as usize;
        if self.plan_identity.is_none_or(|previous| previous == identity) {
            return Ok(());
        }

        let current: HashSet<&camino::Utf8Path> = plan.files.iter().map(|file| file.path.as_path()).collect();
        let removed: Vec<Utf8PathBuf> = self
            .placed
            .keys()
            .filter(|path| !current.contains(path.as_path()))
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

        if self.plan_identity != Some(plan_identity) || self.indexed_files > plan.files.len() || self.indexed_mutants > plan.mutants.len() {
            dirty.extend(self.file_index.keys().cloned());
            self.file_index.clear();
            self.mutants_by_file.clear();
            self.file_by_ordinal.clear();
            self.indexed_files = 0;
            self.indexed_mutants = 0;
            dirty.extend(plan.files.iter().map(|file| file.path.clone()));
        }
        self.plan_identity = Some(plan_identity);

        for (position, file) in plan.files.iter().enumerate().skip(self.indexed_files) {
            let _previous = self.file_index.insert(file.path.clone(), position);
            let _new = dirty.insert(file.path.clone());
        }
        self.indexed_files = plan.files.len();

        for (position, mutant) in plan.mutants.iter().enumerate().skip(self.indexed_mutants) {
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

    /// The file's original text, read from disk the first time a round needs it and kept after.
    ///
    /// Read here rather than taken from the survey's `SourceFile`, so the byte-order mark has to be
    /// dropped here too: mutant spans index the text `syn` saw, which is the text after the mark.
    pub(super) fn original(&mut self, file: &TargetFile) -> Result<&Original> {
        if !self.sources.contains_key(&file.path) {
            let serialized = fs::read_to_string(file.absolute.as_std_path())
                .map_err(|cause| error!("could not read `{}`", file.absolute).caused_by(cause))?;
            let parsed = strip_bom(&serialized).to_owned();

            let _stored = self.sources.insert(file.path.clone(), Original { parsed, serialized });
        }

        Ok(self.sources.get(&file.path).unwrap_or_else(|| unreachable!("just inserted")))
    }
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
        };

        assert_eq!(
            original.instrumented("fn f() { gamma(); }\n".to_owned()),
            format!("{BOM}fn f() {{ gamma(); }}\n")
        );
    }
}
