// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{REGISTRY, resolve};
use crate::{HashSet, Result};

/// The mutator that consumes caller-supplied error values.
const ERR_WITH: &str = "fn_value.err_with";

/// Returns whether optimistic sites for a mutator require an explicit selector.
#[must_use]
pub fn optimistic_requires_explicit(name: &str) -> bool {
    matches!(
        name,
        "literal.int_decrement" | "expr.increment" | "expr.decrement" | "iter.remove_filter"
    )
}

/// A resolved set of mutator names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    names: HashSet<&'static str>,
    errors: Vec<String>,
    optimistic: HashSet<&'static str>,
}

impl Selection {
    /// The set enabled when the user names nothing.
    #[must_use]
    pub fn default_preset() -> Self {
        Self {
            names: REGISTRY.iter().filter(|m| m.default_on).map(|m| m.name).collect(),
            errors: Vec::new(),
            optimistic: HashSet::default(),
        }
    }

    /// Every registered mutator.
    #[must_use]
    pub fn everything() -> Self {
        Self {
            names: REGISTRY.iter().map(|m| m.name).collect(),
            errors: Vec::new(),
            optimistic: REGISTRY.iter().map(|m| m.name).collect(),
        }
    }

    /// An empty set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Sets the caller-supplied `Err(...)` payloads that `fn_value.err_with` will use.
    ///
    /// `Err(Default::default())` only reaches error types that implement `Default`, which most
    /// hand-rolled error enums do not. Naming values here is how those functions get an error
    /// mutant at all, so supplying any also turns the mutator on.
    pub fn set_errors(&mut self, errors: Vec<String>) {
        if !errors.is_empty() {
            let _ = self.names.insert(ERR_WITH);
        }

        self.errors = errors;
    }

    /// Removes the error mutator, for a selection the user spelled out without it.
    pub fn drop_errors(&mut self) {
        let _ = self.names.remove(ERR_WITH);
        self.errors.clear();
    }

    /// Returns the caller-supplied `Err(...)` payloads.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Returns whether a mutator is in the set.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Returns whether unresolved semantic guesses were explicitly requested.
    #[must_use]
    pub fn includes_optimistic(&self, name: &str) -> bool {
        self.optimistic.contains(name)
    }

    /// Returns whether any mutator of a family is selected.
    ///
    /// A family is the part of a name before the dot. Asked by the collector, which builds some of
    /// its per-file indexes only for the family that consults them and would otherwise pay for an
    /// answer nothing was going to read.
    #[must_use]
    pub fn any_in_family(&self, family: &str) -> bool {
        self.names
            .iter()
            .any(|name| name.strip_prefix(family).is_some_and(|rest| rest.starts_with('.')))
    }

    /// Returns the names in sorted order.
    #[must_use]
    pub fn sorted(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.names.iter().copied().collect();

        names.sort_unstable();
        names
    }

    /// Adds every mutator matched by one selector.
    fn add(&mut self, selector: &str, admit_optimistic: bool) -> Result<()> {
        for name in resolve(selector)? {
            let _ = self.names.insert(name);
            if admit_optimistic {
                let _ = self.optimistic.insert(name);
            }
        }

        Ok(())
    }

    /// Removes every mutator matched by one selector.
    fn remove(&mut self, selector: &str) -> Result<()> {
        for name in resolve(selector)? {
            let _ = self.names.remove(name);
            let _ = self.optimistic.remove(name);
        }

        Ok(())
    }

    /// Applies a comma-separated selector list to this set.
    ///
    /// Selectors are applied left to right, so a later `!family` can carve out of an earlier
    /// preset. A selector that matches nothing is an error, never a silent no-op: a suppression
    /// that quietly does nothing is the single most damaging failure mode a mutation tool can
    /// have, because the score stays high and nobody learns why.
    pub fn apply(&mut self, selectors: &str) -> Result<()> {
        for raw in selectors.split(',') {
            let selector = raw.trim();

            if selector.is_empty() {
                continue;
            }

            if let Some(rest) = selector.strip_prefix('!') {
                self.remove(rest.trim())?;
            } else {
                self.add(selector, selector != "@default" && selector != "default")?;
            }
        }

        Ok(())
    }

    /// Builds a selection from a selector list, starting from nothing.
    pub fn parse(selectors: &str) -> Result<Self> {
        let mut selection = Self::empty();

        selection.apply(selectors)?;
        Ok(selection)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn unknown_selectors_are_errors_not_silent_no_ops() {
        let error = resolve("nonsense_family").unwrap_err();

        assert!(error.to_string().contains("unknown mutator selector"));
    }

    #[test]
    fn negation_carves_out_of_a_preset() {
        let mut selection = Selection::parse("@arithmetic").unwrap();
        let before = selection.names.len();

        selection.apply("!bitwise").unwrap();

        assert!(selection.contains("arith.add_to_sub"));
        assert!(!selection.contains("bitwise.and_to_or"));
        assert!(selection.names.len() < before);
    }

    #[test]
    fn selectors_apply_left_to_right() {
        let selection = Selection::parse("relational, !relational.eq_to_ne").unwrap();

        assert!(selection.contains("relational.lt_to_le"));
        assert!(!selection.contains("relational.eq_to_ne"));
    }

    #[test]
    fn default_and_pedantic_presets_partition_everything() {
        let default = Selection::default_preset();
        let pedantic = Selection::parse("@pedantic").unwrap();
        let all = Selection::everything();

        assert!(!default.names.is_empty());
        assert_eq!(pedantic.sorted(), ["fn_value.some"]);
        assert!(!default.contains("fn_value.some"));
        assert_eq!(default.names.len() + pedantic.names.len(), all.names.len());

        let combined = Selection::parse("@default,@pedantic").unwrap();

        for name in all.sorted() {
            assert!(combined.contains(name), "{name} is not reachable through a shipped preset");
        }
    }

    #[test]
    fn only_explicit_selection_admits_optimistic_candidates() {
        assert!(optimistic_requires_explicit("literal.int_decrement"));
        assert!(optimistic_requires_explicit("expr.increment"));
        assert!(optimistic_requires_explicit("expr.decrement"));
        assert!(optimistic_requires_explicit("iter.remove_filter"));
        assert!(!optimistic_requires_explicit("fn_value.default"));
        assert!(!Selection::default_preset().includes_optimistic("literal.int_decrement"));
        assert!(!Selection::parse("@default").unwrap().includes_optimistic("literal.int_decrement"));
        assert!(
            Selection::parse("literal.int_decrement")
                .unwrap()
                .includes_optimistic("literal.int_decrement")
        );
        assert!(
            !Selection::parse("literal.int_decrement")
                .unwrap()
                .includes_optimistic("fn_value.default")
        );
        assert!(Selection::parse("@all").unwrap().includes_optimistic("literal.int_decrement"));
        assert!(Selection::everything().includes_optimistic("literal.int_decrement"));
    }

    #[test]
    fn changed_presets_have_exact_membership() {
        assert_eq!(
            Selection::parse("@numeric").unwrap().sorted(),
            [
                "expr.decrement",
                "expr.increment",
                "literal.float_negate",
                "literal.float_to_one",
                "literal.float_to_zero",
                "literal.int_decrement",
                "literal.int_increment",
                "literal.int_to_one",
                "literal.int_to_zero",
            ]
        );
        assert_eq!(
            Selection::parse("@removal").unwrap().sorted(),
            [
                "call.replace_with_default",
                "collection.omit_element",
                "match_arm.never_matches",
                "stmt.delete_assign",
                "stmt.delete_call",
                "struct_field.omit",
                "unary.remove_neg",
                "unary.remove_not",
            ]
        );
    }

    #[test]
    fn the_noisier_families_are_still_on() {
        let default = Selection::default_preset();

        // Statement deletion has a high equivalent-mutant rate. It stays on anyway; this test
        // exists so that turning it off again is a deliberate edit rather than a quiet drift.
        assert!(default.contains("stmt.delete_call"));
    }

    #[test]
    fn empty_selectors_are_ignored() {
        let selection = Selection::parse("relational, , ").unwrap();

        assert_eq!(selection.names.len(), 10);
    }

    /// `any_in_family` is the gate that builds the per-file imports index only when a `fn_value`
    /// mutator is selected. It must answer on the family prefix: `true` when the family is present,
    /// `false` when it is not, or the index is skipped and undefaultable-type mutants slip through.
    #[test]
    fn any_in_family_gates_on_the_selected_family() {
        assert!(Selection::parse("fn_value.default").unwrap().any_in_family("fn_value"));
        assert!(!Selection::parse("relational").unwrap().any_in_family("fn_value"));
    }

    #[test]
    fn caller_supplied_error_values_toggle_the_error_mutator() {
        let mut selection = Selection::empty();

        selection.set_errors(vec!["Error::Broken".to_owned()]);

        assert!(selection.contains(ERR_WITH));
        assert_eq!(selection.errors(), ["Error::Broken"]);

        selection.drop_errors();

        assert!(!selection.contains(ERR_WITH));
        assert!(selection.errors().is_empty());
    }

    #[test]
    fn clearing_error_payloads_does_not_disable_an_enabled_error_mutator() {
        let mut selection = Selection::empty();

        selection.set_errors(vec!["Error::Old".to_owned()]);
        selection.set_errors(Vec::new());

        assert!(selection.contains(ERR_WITH));
        assert!(selection.errors().is_empty());
    }
}
