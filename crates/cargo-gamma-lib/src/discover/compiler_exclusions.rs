// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent, explicit exclusions of historically unviable mutant identities.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;

use camino::Utf8Path;

use super::{Hints, RunRecord};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::ops::collect::Shape;
use crate::{HashSet, Result};

const FILE: &str = "compiler-exclusions.json";
const VERSION: u32 = 1;
const EVIDENCE_REVISION: u32 = 2;
const HISTORICAL_REASON: &str = "historical compiler exclusion imported from earlier campaign evidence";

/// Authoritative cumulative exclusion policy, not context-validated compiler-cache evidence.
///
/// Exact identities remain excluded across compiler-context changes. Evidence invalidated by an
/// attribution/schema repair is retained, but requires a fresh proof before exclusion.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct CompilerExclusions {
    version: u32,
    mutants: BTreeMap<String, String>,
    #[serde(default)]
    evidence_revision: u32,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    revalidate: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    schema_revalidate: BTreeSet<String>,
}

impl CompilerExclusions {
    /// Loads the authoritative inventory, importing historical evidence only on initialization.
    pub fn load(base: &Utf8Path, root: &Utf8Path) -> Result<Self> {
        let path = base.join(FILE);
        match fs::read_to_string(&path) {
            Ok(text) => Self::from_saved(&text, &path, |saved| saved.store(base)),
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
                    .map(str::to_owned);
                Ok(Self::from_historical(mutants))
            }
            Err(cause) => Err(error!("could not read compiler exclusion inventory `{path}`").caused_by(cause)),
        }
    }

    fn decode(text: &str, path: &Utf8Path) -> Result<Self> {
        let saved: Self = serde_json::from_str(text)
            .map_err(|cause| error!("could not read compiler exclusion inventory `{path}`; repair or remove the file").caused_by(cause))?;
        if saved.version != VERSION {
            return Err(error!(
                "compiler exclusion inventory `{path}` has unsupported version {}; expected {VERSION}",
                saved.version
            ));
        }
        if saved.evidence_revision > EVIDENCE_REVISION {
            return Err(error!(
                "compiler exclusion inventory `{path}` has unsupported evidence revision {}; expected at most {EVIDENCE_REVISION}",
                saved.evidence_revision
            ));
        }
        Ok(saved)
    }

    fn from_saved(text: &str, path: &Utf8Path, store: impl FnOnce(&Self) -> Result<()>) -> Result<Self> {
        let mut saved = Self::decode(text, path)?;
        if saved.migrate_evidence() {
            store(&saved)?;
        }
        Ok(saved)
    }

    fn from_historical(ids: impl Iterator<Item = String>) -> Self {
        let mutants = ids.map(|id| (id, HISTORICAL_REASON.into())).collect::<BTreeMap<_, _>>();
        Self {
            version: VERSION,
            schema_revalidate: mutants.keys().cloned().collect(),
            mutants,
            evidence_revision: EVIDENCE_REVISION,
            revalidate: BTreeSet::new(),
        }
    }

    fn migrate_evidence(&mut self) -> bool {
        if self.evidence_revision == EVIDENCE_REVISION {
            return false;
        }
        if self.evidence_revision == 0 {
            self.revalidate.extend(
                self.mutants
                    .iter()
                    .filter(|(_id, reason)| legacy_positional(reason) || legacy_isolated(reason))
                    .map(|(id, _reason)| id.clone()),
            );
        }
        // The inventory has identities and reasons, not source/shape metadata. Classify the
        // repaired schema cohorts when their exact current mutants are available in reason().
        self.schema_revalidate.extend(self.mutants.keys().cloned());
        self.evidence_revision = EVIDENCE_REVISION;
        true
    }

    /// Returns the first recorded reason unless this exact identity still requires revalidation.
    #[must_use]
    pub fn reason(&self, mutant: &Mutant) -> Option<&str> {
        let id = mutant.id.as_str();
        let reason = self.mutants.get(id)?;
        let legacy = self.evidence_revision == 0 || self.revalidate.contains(id);
        // The old note also covered exact secondary replacement spans. Those remain valid:
        // only empty replacements used deletion containment instead of generated-text evidence.
        if legacy && (legacy_isolated(reason) || (legacy_positional(reason) && mutant.replacement.is_empty())) {
            return None;
        }
        if (self.evidence_revision < EVIDENCE_REVISION || self.schema_revalidate.contains(id)) && repaired_schema(mutant, reason) {
            return None;
        }
        Some(reason)
    }

    /// Adds newly proven exact exclusions while preserving each identity's first recorded reason.
    pub fn remember(&mut self, mutants: &[Mutant], ordinals: &HashSet<u32>, reason: impl Fn(u32) -> String) -> bool {
        let mut changed = self.migrate_evidence();
        for mutant in mutants.iter().filter(|mutant| ordinals.contains(&mutant.ordinal)) {
            // Reconfirmation updates evidence status, never the identity's original reason.
            changed |= self.revalidate.remove(mutant.id.as_str());
            changed |= self.schema_revalidate.remove(mutant.id.as_str());
            if let Entry::Vacant(entry) = self.mutants.entry(mutant.id.as_str().to_owned()) {
                entry.insert(reason(mutant.ordinal));
                changed = true;
            }
        }
        changed
    }

    /// Publishes the complete inventory to the campaign output directory.
    pub fn store(&self, base: &Utf8Path) -> Result<()> {
        let text =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "CompilerExclusions serializer derives solely over u32, BTreeMap<String,String>, and BTreeSet<String>. Every key/value is JSON representable, no user serializer or I/O writer is accepted, so to_string_pretty cannot return Err for panic_on_failure.", tag = "equivalent")]
        serde_json::to_string_pretty(self)
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

fn legacy_positional(reason: &str) -> bool {
    reason.ends_with("; primary span did not identify the replacement site")
}

fn legacy_isolated(reason: &str) -> bool {
    reason.ends_with("; isolated by proof builds without diagnostic-span attribution")
}

fn repaired_schema(mutant: &Mutant, reason: &str) -> bool {
    match mutant.mutator.as_ref() {
        "parameter.default_shadow" if mutant.replacement.contains("::gamma_rt::default_like") => return true,
        "fn_value.one_element" if mutant.replacement.contains("core::iter::once::<") => return true,
        "string.lower_to_upper" | "string.upper_to_lower" if mutant.shape == Shape::IterExpr => return true,
        "try.propagate_to_unwrap" if reason == HISTORICAL_REASON || reason.starts_with("rustc E0277:") => return true,
        _ => {}
    }
    mutant.shape == Shape::Expr && block_leading_binary_or_cast(&mutant.original)
}

fn block_leading_binary_or_cast(original: &str) -> bool {
    let Ok(expression) = syn::parse_str::<syn::Expr>(original) else {
        return false;
    };
    if !matches!(expression, syn::Expr::Binary(_) | syn::Expr::Cast(_)) {
        return false;
    }
    let mut leading = &expression;
    // #[gamma::skip(bool_expr.negate, tag = "equivalent", reason = "every loop exit returns directly from this function, so a negation outside the loop never receives a value")]
    loop {
        // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "the leading operand must advance or a binary or cast expression is inspected forever")]
        leading = match leading {
            syn::Expr::Binary(binary) => &binary.left,
            syn::Expr::Cast(cast) => &cast.expr,
            syn::Expr::Block(_)
            | syn::Expr::Unsafe(_)
            | syn::Expr::Const(_)
            | syn::Expr::If(_)
            | syn::Expr::Match(_)
            | syn::Expr::Loop(_)
            | syn::Expr::While(_)
            | syn::Expr::ForLoop(_)
            | syn::Expr::TryBlock(_) => return true,
            _ => return false,
        };
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::testing::ci_fixture::mutant;

    #[test]
    fn recovered_imported_exclusions_preserve_the_exact_historical_reason_for_each_identity() {
        let inventory = CompilerExclusions::from_historical(["first", "second", "first"].map(str::to_owned).into_iter());
        assert_eq!(inventory.version, 1);
        assert_eq!(inventory.mutants.len(), 2);
        for id in ["first", "second"] {
            assert_eq!(
                inventory.mutants.get(id).map(String::as_str),
                Some("historical compiler exclusion imported from earlier campaign evidence")
            );
        }
    }

    #[test]
    fn loading_saved_inventory_publishes_only_actual_migrations_and_preserves_store_failures() {
        let population = legacy_population();
        for revision in [0, 1, EVIDENCE_REVISION] {
            let mut original = legacy_inventory(&population);
            original.evidence_revision = revision;
            let text = serde_json::to_string(&original).unwrap();
            let publications = std::cell::Cell::new(0);
            let actual = CompilerExclusions::from_saved(&text, Utf8Path::new(FILE), |saved| {
                assert_ne!(revision, EVIDENCE_REVISION, "current evidence must not be rewritten");
                assert_eq!(saved.evidence_revision, EVIDENCE_REVISION);
                assert_eq!(saved.mutants, original.mutants);
                assert_eq!(saved.schema_revalidate.len(), population.len());
                publications.set(publications.get() + 1);
                Ok(())
            })
            .unwrap();
            assert_eq!(actual.evidence_revision, EVIDENCE_REVISION);
            assert_eq!(actual.mutants, original.mutants);
            assert_eq!(publications.get(), usize::from(revision != EVIDENCE_REVISION));
        }
        let text = serde_json::to_string(&legacy_inventory(&population)).unwrap();
        let failure = CompilerExclusions::from_saved(&text, Utf8Path::new(FILE), |_| {
            Err(error!("scripted inventory publication failure"))
        })
        .unwrap_err();
        assert_eq!(failure.message(), "scripted inventory publication failure");
    }

    fn candidate(ordinal: u32) -> Mutant {
        Mutant {
            ordinal,
            ..mutant("lib.rs", ordinal as usize, "literal.bool_flip", Outcome::Pending)
        }
    }

    fn legacy_inventory(population: &[Mutant]) -> CompilerExclusions {
        let positional = "rustc E0070: invalid left-hand side of assignment; primary span did not identify the replacement site";
        let isolated = "rustc: isolated compiler failure; isolated by proof builds without diagnostic-span attribution";
        let primary = "rustc E0308: mismatched types; primary span identified the replacement site";
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "mutants": {
                population[0].id.to_string(): positional,
                population[1].id.to_string(): isolated,
                population[2].id.to_string(): primary,
                population[3].id.to_string(): positional,
                population[4].id.to_string(): "historical compiler exclusion imported from earlier campaign evidence"
            }
        }))
        .unwrap()
    }

    fn legacy_population() -> Vec<Mutant> {
        let mut population = (1..=5).map(candidate).collect::<Vec<_>>();
        population[0].replacement = "".into();
        population[3].replacement = "replacement".into();
        population
    }

    #[test]
    fn cor2_legacy_migration_revalidates_only_affected_evidence_and_preserves_every_original_reason() {
        let mut population = legacy_population();
        let mut inventory = legacy_inventory(&population);
        let original = inventory.mutants.clone();
        assert!(inventory.migrate_evidence());
        assert!(!inventory.migrate_evidence());
        assert_eq!(inventory.mutants, original);
        assert_eq!(inventory.revalidate.len(), 3);
        assert_eq!(inventory.exclude(&mut population), 3);
        for (index, mutant) in population.iter().enumerate() {
            assert_eq!(mutant.outcome, if index < 2 { Outcome::Pending } else { Outcome::CompileError });
        }
        let changed = inventory.remember(&population[..2], &HashSet::from_iter([1]), |_| {
            panic!("reconfirmation must retain the exact historical reason")
        });
        assert!(changed);
        assert_eq!(
            inventory.reason(&population[0]),
            original.get(population[0].id.as_str()).map(String::as_str)
        );
        assert_eq!(inventory.reason(&population[1]), None);
        let round_trip: CompilerExclusions = serde_json::from_str(&serde_json::to_string(&inventory).unwrap()).unwrap();
        assert_eq!(round_trip.mutants, original);
        assert_eq!(round_trip.reason(&population[0]), inventory.reason(&population[0]));
        assert_eq!(round_trip.reason(&population[1]), None);
    }

    #[test]
    fn cor2_legacy_inventory_is_safe_before_migration_and_reconfirmation_publishes_the_revision() {
        let population = legacy_population();
        let mut inventory = legacy_inventory(&population);
        assert_eq!(inventory.reason(&population[0]), None);
        assert_eq!(inventory.reason(&population[1]), None);
        for mutant in &population[2..] {
            assert!(inventory.reason(mutant).is_some());
        }
        assert!(inventory.remember(&population, &HashSet::from_iter([2]), |_| panic!(
            "the original reason already exists"
        )));
        assert_eq!(inventory.evidence_revision, EVIDENCE_REVISION);
        assert!(inventory.reason(&population[1]).is_some());
        assert!(inventory.reason(&population[0]).is_none());
        assert!(!inventory.remember(&population, &HashSet::from_iter([2]), |_| panic!("no new identity")));
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one differential table pairs every repaired schema cohort with unaffected controls"
    )]
    fn schema_repair_population() -> Vec<(Mutant, String, bool)> {
        [
            (
                "parameter.default_shadow",
                "value",
                "::gamma_rt::default_like(&value)",
                Shape::Stmt,
                "E0308",
                true,
            ),
            (
                "parameter.default_shadow",
                "value",
                "Default::default()",
                Shape::Stmt,
                "E0308",
                false,
            ),
            (
                "literal.bool_flip",
                "value",
                "::gamma_rt::default_like(&value)",
                Shape::Expr,
                "E0308",
                false,
            ),
            (
                "fn_value.one_element",
                "{ values }",
                "core::iter::once::<& 'static [u8]>(&[])",
                Shape::IterBlock,
                "E0308",
                true,
            ),
            (
                "fn_value.one_element",
                "{ values }",
                "core::iter::once(0)",
                Shape::IterBlock,
                "E0308",
                false,
            ),
            (
                "fn_value.empty_collection",
                "{ values }",
                "core::iter::once::<&[u8]>(&[])",
                Shape::IterBlock,
                "E0308",
                false,
            ),
            (
                "string.lower_to_upper",
                "value.to_lowercase()",
                "value.to_uppercase()",
                Shape::IterExpr,
                "E0308",
                true,
            ),
            (
                "string.upper_to_lower",
                "value.to_uppercase()",
                "value.to_lowercase()",
                Shape::IterExpr,
                "E0308",
                true,
            ),
            (
                "string.lower_to_upper",
                "value.to_lowercase()",
                "value.to_uppercase()",
                Shape::Expr,
                "E0308",
                false,
            ),
            (
                "string.upper_to_lower",
                "value.to_uppercase()",
                "value.to_lowercase()",
                Shape::Expr,
                "E0308",
                false,
            ),
            ("literal.bool_flip", "value", "replacement", Shape::IterExpr, "E0308", false),
            ("try.propagate_to_unwrap", "value?", "unwrap(value)", Shape::Expr, "E0277", true),
            ("try.propagate_to_unwrap", "value?", "unwrap(value)", Shape::Expr, "E0308", false),
            ("try.propagate_to_unwrap", "value?", "unwrap(value)", Shape::Expr, "E02770", false),
            ("literal.bool_flip", "value", "replacement", Shape::Expr, "E0277", false),
            ("arith.add_to_sub", "unsafe { 1 } + 2", "replacement", Shape::Expr, "E0308", true),
            ("arith.add_to_sub", "{ 1 } + 2", "replacement", Shape::Expr, "E0308", true),
            (
                "arith.add_to_sub",
                "if ready { 1 } else { 2 } + 3",
                "replacement",
                Shape::Expr,
                "E0308",
                true,
            ),
            (
                "cast.remove",
                "match value { _ => 1 } as u32",
                "replacement",
                Shape::Expr,
                "E0308",
                true,
            ),
            (
                "arith.add_to_sub",
                "loop { break 1 } as u32 + 2",
                "replacement",
                Shape::Expr,
                "E0308",
                true,
            ),
            (
                "arith.add_to_sub",
                "unsafe { 1 } as u32 as u64 + 2",
                "replacement",
                Shape::Expr,
                "E0308",
                true,
            ),
            ("arith.add_to_sub", "const { 1 } + 2", "replacement", Shape::Expr, "E0308", true),
            ("cast.remove", "while false {} as u8", "replacement", Shape::Expr, "E0308", true),
            ("cast.remove", "for _ in [] {} as u8", "replacement", Shape::Expr, "E0308", true),
            ("arith.add_to_sub", "try { 1 } + 2", "replacement", Shape::Expr, "E0308", true),
            (
                "arith.add_to_sub",
                "/* source comment */ unsafe { 1 } + 2",
                "replacement",
                Shape::Expr,
                "E0308",
                true,
            ),
            ("arith.add_to_sub", "(unsafe { 1 }) + 2", "replacement", Shape::Expr, "E0308", false),
            ("literal.bool_flip", "unsafe { 1 }", "replacement", Shape::Expr, "E0308", false),
            ("arith.add_to_sub", "1 + unsafe { 2 }", "replacement", Shape::Expr, "E0308", false),
            ("arith.add_to_sub", "value as u32 + 2", "replacement", Shape::Expr, "E0308", false),
            ("unary.remove_not", "!(unsafe { true })", "replacement", Shape::Expr, "E0308", false),
            (
                "cast.remove",
                "(unsafe { 1 } + 2) as u32",
                "replacement",
                Shape::Expr,
                "E0308",
                false,
            ),
            ("stmt.delete_assign", "{ 1 } + 2", "", Shape::Stmt, "E0308", false),
            ("literal.bool_flip", "not an expression", "replacement", Shape::Expr, "E0308", false),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (mutator, original, replacement, shape, code, affected))| {
            let mut mutant = candidate(u32::try_from(index + 1).unwrap());
            mutant.mutator = mutator.into();
            mutant.original = original.into();
            mutant.replacement = replacement.into();
            mutant.shape = shape;
            (
                mutant,
                format!("rustc {code}: direct diagnostic; primary span identified the replacement site"),
                affected,
            )
        })
        .collect()
    }

    fn schema_inventory(population: &[(Mutant, String, bool)], revision: u32) -> CompilerExclusions {
        let mutants = population
            .iter()
            .map(|(mutant, reason, _affected)| (mutant.id.to_string(), reason.clone()))
            .collect::<BTreeMap<_, _>>();
        serde_json::from_value(serde_json::json!({"version": VERSION, "evidence_revision": revision, "mutants": mutants})).unwrap()
    }

    #[test]
    fn cor2_schema_repair_migration_precisely_revalidates_legacy_direct_diagnostics_before_and_after_serialization() {
        let population = schema_repair_population();
        for revision in [0, 1] {
            let mut inventory = schema_inventory(&population, revision);
            for (mutant, reason, affected) in &population {
                assert_eq!(
                    inventory.reason(mutant),
                    (!affected).then_some(reason.as_str()),
                    "unmigrated revision {revision}: {} / {}",
                    mutant.mutator,
                    mutant.original
                );
            }
            assert!(inventory.migrate_evidence());
            assert!(!inventory.migrate_evidence());
            let restored: CompilerExclusions = serde_json::from_str(&serde_json::to_string(&inventory).unwrap()).unwrap();
            assert_eq!(restored.schema_revalidate.len(), population.len());
            for (mutant, reason, affected) in &population {
                assert_eq!(
                    restored.reason(mutant),
                    (!affected).then_some(reason.as_str()),
                    "migrated revision {revision}: {} / {}",
                    mutant.mutator,
                    mutant.original
                );
            }
        }
    }

    #[test]
    fn cor2_schema_revision_does_not_repeat_completed_attribution_revalidation_or_affect_new_proofs() {
        let population = legacy_population();
        let mut inventory = legacy_inventory(&population);
        inventory.evidence_revision = 1;
        // Revision one already confirmed these exact identities under the repaired attribution.
        assert!(inventory.migrate_evidence());
        assert!(inventory.revalidate.is_empty());
        assert!(population.iter().all(|mutant| inventory.reason(mutant).is_some()));
        let population = schema_repair_population();
        let mut inventory = CompilerExclusions::from_historical(core::iter::empty());
        for (mutant, reason, _affected) in population {
            assert!(
                inventory.remember(core::slice::from_ref(&mutant), &HashSet::from_iter([mutant.ordinal]), |_| reason
                    .clone())
            );
            let restored: CompilerExclusions = serde_json::from_str(&serde_json::to_string(&inventory).unwrap()).unwrap();
            assert_eq!(restored.reason(&mutant), Some(reason.as_str()));
        }
    }

    #[test]
    fn cor2_first_historical_import_applies_the_same_selective_schema_revalidation() {
        let population = schema_repair_population();
        let inventory = CompilerExclusions::from_historical(population.iter().map(|(mutant, _, _)| mutant.id.to_string()));
        for (mutant, _reason, affected) in population {
            let affected = affected || mutant.mutator.as_ref() == "try.propagate_to_unwrap";
            assert_eq!(
                inventory.reason(&mutant).is_none(),
                affected,
                "{} / {}",
                mutant.mutator,
                mutant.original
            );
        }
    }

    #[test]
    fn historical_try_exclusions_require_a_fresh_proof_before_removing_candidates() {
        let population = [
            Mutant {
                mutator: "try.propagate_to_unwrap".into(),
                original: "value?".into(),
                ..candidate(1)
            },
            candidate(2),
        ];
        for revision in [0, 1, EVIDENCE_REVISION] {
            let mut inventory = CompilerExclusions::from_historical(population.iter().map(|mutant| mutant.id.to_string()));
            inventory.evidence_revision = revision;
            if revision != EVIDENCE_REVISION {
                inventory.schema_revalidate.clear();
                assert!(inventory.migrate_evidence());
            }
            let mut inventory = CompilerExclusions::decode(&serde_json::to_string(&inventory).unwrap(), Utf8Path::new(FILE)).unwrap();
            let mut discovered = population.clone();
            assert_eq!(inventory.exclude(&mut discovered), 1);
            assert_eq!(discovered[0].outcome, Outcome::Pending);
            assert_eq!(discovered[0].ordinal, 1);
            assert_eq!(discovered[1].outcome, Outcome::CompileError);
            assert_eq!(discovered[1].ordinal, 0);
            assert!(inventory.schema_revalidate.contains(population[0].id.as_str()));
            assert!(inventory.remember(&population, &HashSet::from_iter([1]), |_| {
                panic!("reconfirmation preserves the imported identity's original reason")
            }));
            let restored = CompilerExclusions::decode(&serde_json::to_string(&inventory).unwrap(), Utf8Path::new(FILE)).unwrap();
            assert_eq!(
                restored.reason(&population[0]),
                Some("historical compiler exclusion imported from earlier campaign evidence")
            );
            assert!(!restored.schema_revalidate.contains(population[0].id.as_str()));
        }
    }

    #[test]
    fn confirming_only_pending_attribution_evidence_reports_a_persistable_change() {
        let population = legacy_population();
        let mut inventory = legacy_inventory(&population);
        assert!(inventory.migrate_evidence());
        inventory.schema_revalidate.clear();
        assert!(inventory.revalidate.contains(population[1].id.as_str()));
        assert!(inventory.remember(&population, &HashSet::from_iter([2]), |_| {
            panic!("the original reason already exists")
        }));
        assert!(!inventory.revalidate.contains(population[1].id.as_str()));
        assert!(!inventory.remember(&population, &HashSet::from_iter([2]), |_| {
            panic!("the original reason already exists")
        }));
    }

    #[test]
    fn cor2_published_migration_is_idempotent_without_reimporting_removed_identities() {
        let population = legacy_population();
        let mut legacy = legacy_inventory(&population);
        legacy.mutants.remove(population[4].id.as_str());
        let path = Utf8Path::new(FILE);
        let mut migrated = CompilerExclusions::decode(&serde_json::to_string(&legacy).unwrap(), path).unwrap();
        assert!(migrated.migrate_evidence());
        let serialized = serde_json::to_string(&migrated).unwrap();
        let mut saved = CompilerExclusions::decode(&serialized, path).unwrap();
        assert_eq!(saved.evidence_revision, EVIDENCE_REVISION);
        assert_eq!(saved.revalidate, migrated.revalidate);
        assert_eq!(saved.mutants, legacy.mutants);
        assert_eq!(saved.reason(&population[0]), None);
        assert_eq!(saved.reason(&population[1]), None);
        assert_eq!(saved.reason(&population[4]), None);
        assert!(!saved.migrate_evidence());
        assert_eq!(serde_json::to_string(&saved).unwrap(), serialized);
    }

    #[test]
    fn cor2_future_inventory_metadata_is_rejected_without_overwriting_the_inventory() {
        for (version, revision) in [(VERSION + 1, 0), (VERSION, EVIDENCE_REVISION + 1)] {
            let text = serde_json::json!({"version": version, "mutants": {}, "evidence_revision": revision}).to_string();
            let failure = CompilerExclusions::decode(&text, Utf8Path::new(FILE)).unwrap_err();
            assert!(failure.message().contains("unsupported"));
        }
    }

    #[test]
    fn cor2_schema_revalidation_and_authoritative_removals_survive_inventory_reload() {
        let population = schema_repair_population();
        let (confirmed, first_reason, _) = &population[0];
        let (pending, _, _) = &population[3];
        let (removed, _, _) = &population[6];
        for revision in [0, 1] {
            let mut inventory = schema_inventory(&population, revision);
            inventory.mutants.remove(removed.id.as_str());
            let path = Utf8Path::new(FILE);
            let mut migrated = CompilerExclusions::decode(&serde_json::to_string(&inventory).unwrap(), path).unwrap();
            assert!(migrated.migrate_evidence());
            assert_eq!(migrated.reason(confirmed), None);
            assert_eq!(migrated.reason(pending), None);
            assert_eq!(migrated.reason(removed), None);
            let stored = CompilerExclusions::decode(&serde_json::to_string(&migrated).unwrap(), path).unwrap();
            assert_eq!(stored.evidence_revision, EVIDENCE_REVISION);
            assert!(stored.schema_revalidate.contains(confirmed.id.as_str()));
            assert!(!stored.schema_revalidate.contains(removed.id.as_str()));
            assert!(
                migrated.remember(core::slice::from_ref(confirmed), &HashSet::from_iter([confirmed.ordinal]), |_| {
                    panic!("reconfirmation must not replace the exact original reason")
                })
            );
            let restored = CompilerExclusions::decode(&serde_json::to_string(&migrated).unwrap(), path).unwrap();
            assert_eq!(restored.reason(confirmed), Some(first_reason.as_str()));
            assert_eq!(restored.reason(pending), None);
            assert_eq!(restored.reason(removed), None);
            assert!(!restored.schema_revalidate.contains(confirmed.id.as_str()));
            assert!(restored.schema_revalidate.contains(pending.id.as_str()));
            assert_eq!(restored.mutants.get(confirmed.id.as_str()), Some(first_reason));
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
            evidence_revision: EVIDENCE_REVISION,
            revalidate: BTreeSet::new(),
            schema_revalidate: BTreeSet::new(),
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
            evidence_revision: EVIDENCE_REVISION,
            revalidate: BTreeSet::new(),
            schema_revalidate: BTreeSet::new(),
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
            evidence_revision: EVIDENCE_REVISION,
            revalidate: BTreeSet::new(),
            schema_revalidate: BTreeSet::new(),
        };
        inventory.remember(&population, &HashSet::from_iter([1]), |_| "rustc E0308".into());
        assert_eq!(inventory.exclude(&mut population), 1);
        assert_eq!(population[0].outcome, Outcome::CompileError);
        assert_eq!(population[1].outcome, Outcome::Pending);
        assert_eq!(population[1].ordinal, 2);
    }
}
