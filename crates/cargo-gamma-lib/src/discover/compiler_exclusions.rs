// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent, explicit exclusions of historically unviable mutant identities.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs;
use std::io::ErrorKind;

use camino::Utf8Path;

use super::{Hints, RunRecord};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::{HashSet, Result};

const FILE: &str = "compiler-exclusions.json";
const VERSION: u32 = 1;

/// Authoritative cumulative exclusion policy, not context-validated compiler-cache evidence.
///
/// Exact identities remain excluded across compiler-context changes until explicitly removed.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct CompilerExclusions {
    version: u32,
    mutants: BTreeMap<String, String>,
}

impl CompilerExclusions {
    /// Loads the authoritative inventory, importing historical evidence only on initialization.
    pub fn load(base: &Utf8Path, root: &Utf8Path) -> Result<Self> {
        let path = base.join(FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let saved: Self = serde_json::from_str(&text).map_err(|cause| {
                    error!("could not read compiler exclusion inventory `{path}`; repair or remove the file").caused_by(cause)
                })?;
                if saved.version != VERSION {
                    return Err(error!(
                        "compiler exclusion inventory `{path}` has unsupported version {}; expected {VERSION}",
                        saved.version
                    ));
                }
                Ok(saved)
            }
            Err(cause) if cause.kind() == ErrorKind::NotFound => {
                // Reimporting into an existing inventory would undo the user's explicit removals.
                let completed = RunRecord::load(base);
                let incomplete = RunRecord::load_incomplete(base);
                let hints = Hints::load(root);
                let mutants = completed
                    .ordering()
                    .into_iter()
                    .chain(incomplete.ordering())
                    .chain(hints.ordering())
                    .map(|id| {
                        (
                            id.to_owned(),
                            "historical compiler exclusion imported from earlier campaign evidence".into(),
                        )
                    })
                    .collect();
                Ok(Self { version: VERSION, mutants })
            }
            Err(cause) => Err(error!("could not read compiler exclusion inventory `{path}`").caused_by(cause)),
        }
    }

    /// Returns the first recorded reason for this exact mutant identity.
    pub fn reason(&self, mutant: &Mutant) -> Option<&str> {
        self.mutants.get(mutant.id.as_str()).map(String::as_str)
    }

    /// Adds newly proven exact exclusions while preserving each identity's first recorded reason.
    pub fn remember(&mut self, mutants: &[Mutant], ordinals: &HashSet<u32>, reason: impl Fn(u32) -> String) -> bool {
        let mut changed = false;
        for mutant in mutants.iter().filter(|mutant| ordinals.contains(&mutant.ordinal)) {
            if let Entry::Vacant(entry) = self.mutants.entry(mutant.id.as_str().to_owned()) {
                entry.insert(reason(mutant.ordinal));
                changed = true;
            }
        }
        changed
    }

    /// Publishes the complete inventory to the campaign output directory.
    pub fn store(&self, base: &Utf8Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|cause| error!("could not serialize compiler exclusion inventory").caused_by(cause))?;
        crate::elements::write(&base.join(FILE), &text)
    }

    /// Applies historical exclusions before executable numbering, without changing scorable results.
    pub(crate) fn exclude(&self, mutants: &mut [Mutant]) -> usize {
        let mut excluded = 0;
        for mutant in mutants.iter_mut().filter(|mutant| mutant.outcome == Outcome::Pending) {
            if let Some(reason) = self.reason(mutant) {
                mutant.outcome = Outcome::CompileError;
                mutant.ordinal = 0;
                mutant.note = Some(format!("historical compiler exclusion: {reason}"));
                excluded += 1;
            }
        }
        excluded
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::testing::ci_fixture::mutant;

    fn candidate(ordinal: u32) -> Mutant {
        Mutant {
            ordinal,
            ..mutant("lib.rs", ordinal as usize, "literal.bool_flip", Outcome::Pending)
        }
    }

    #[test]
    fn correcting_a_stated_replacement_admits_it_without_discarding_old_exclusions() {
        let stated = |expression: &str| {
            let file = crate::parse::SourceFile::parse(
                "src/lib.rs",
                format!("#[gamma::value({expression})]\npub fn answer() -> u32 {{ 7 }}\n"),
            )
            .unwrap();
            let selection = crate::ops::Selection::parse("fn_value.stated").unwrap();
            let candidates = crate::ops::collect_candidates(&file, &selection);
            let mut mutants = crate::ops::into_mutants(&file, "subject", candidates);
            assert_eq!(mutants.len(), 1);
            mutants[0].ordinal = 1;
            mutants.remove(0)
        };
        let rejected = stated("\"wrong type\"");
        let mut inventory = CompilerExclusions {
            version: VERSION,
            mutants: BTreeMap::new(),
        };
        inventory.remember(core::slice::from_ref(&rejected), &HashSet::from_iter([1]), |_| "E0308".into());
        let corrected = stated("8");
        assert_ne!(corrected.id, rejected.id);
        let mut current = [rejected, corrected];
        assert_eq!(inventory.exclude(&mut current), 1);
        assert_eq!(current[0].outcome, Outcome::CompileError);
        assert_eq!(current[1].outcome, Outcome::Pending);
        assert_eq!(current[1].ordinal, 1);
    }

    #[test]
    fn exclusion_is_exact_score_neutral_and_precedes_executable_ordinals() {
        let mut inventory = CompilerExclusions {
            version: VERSION,
            mutants: BTreeMap::new(),
        };
        let first = candidate(1);
        inventory.remember(core::slice::from_ref(&first), &HashSet::from_iter([1]), |_| "E0308".into());
        let mut population = vec![
            first.clone(),
            candidate(2),
            Mutant {
                outcome: Outcome::NotBuilt,
                ..first
            },
        ];
        assert_eq!(inventory.exclude(&mut population), 1);
        assert_eq!(population[0].outcome, Outcome::CompileError);
        assert_eq!(population[0].ordinal, 0);
        assert_eq!(population[0].note.as_deref(), Some("historical compiler exclusion: E0308"));
        assert_eq!(population[1].outcome, Outcome::Pending);
        assert_eq!(population[1].ordinal, 2);
        assert_eq!(population[2].outcome, Outcome::NotBuilt);
        assert_eq!(inventory.exclude(&mut population), 0);
    }

    #[test]
    fn a_rejected_plain_operand_does_not_suppress_a_proven_borrowed_boolean() {
        let file = crate::parse::SourceFile::parse(
            "src/lib.rs",
            "fn flags(values: &[(u8, bool)]) -> bool { values.iter().any(|(_, flag)| !flag) }".into(),
        )
        .unwrap();
        let selection = crate::ops::Selection::parse("unary.remove_not").unwrap();
        let mut candidates = crate::ops::collect_candidates(&file, &selection);
        assert_eq!(candidates.len(), 1);
        let borrowed = candidates.remove(0);
        assert_eq!(borrowed.replacement.as_str(), "*(flag)");
        let mut plain = borrowed.clone();
        plain.replacement = "flag".into();
        plain.replacement_index = 0;
        let mut population = crate::ops::into_mutants(&file, "subject", vec![plain, borrowed]);
        assert_ne!(population[0].id, population[1].id);
        population[0].ordinal = 1;
        population[1].ordinal = 2;
        let mut inventory = CompilerExclusions {
            version: VERSION,
            mutants: BTreeMap::new(),
        };
        inventory.remember(&population, &HashSet::from_iter([1]), |_| "rustc E0308".into());
        assert_eq!(inventory.exclude(&mut population), 1);
        assert_eq!(population[0].outcome, Outcome::CompileError);
        assert_eq!(population[1].outcome, Outcome::Pending);
        assert_eq!(population[1].ordinal, 2);
    }
}
