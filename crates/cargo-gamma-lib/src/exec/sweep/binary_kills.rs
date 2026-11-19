// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Live per-binary kill counts for fallback execution order.

use core::cmp::Reverse;
use core::fmt;
use std::sync::Arc;

use super::{BinaryIdentity, Killers, Plan, TestBinary};
use crate::HashMap;

#[derive(Debug, Default)]
pub(super) struct BinaryKills {
    counts: TargetCounts<usize>,
    credited: HashMap<u32, BinaryIdentity>,
    packages: HashMap<u32, Arc<str>>,
}

/// Separates persisted target qualifiers so live lookups can borrow all three string components.
pub(super) struct TargetCounts<V> {
    packages: HashMap<String, HashMap<String, HashMap<String, V>>>,
}

impl<V> fmt::Debug for TargetCounts<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(core::any::type_name::<Self>())
            .field("packages", &self.packages.len())
            .finish()
    }
}

impl<V> Default for TargetCounts<V> {
    fn default() -> Self {
        Self {
            packages: HashMap::default(),
        }
    }
}

impl<V: Default> TargetCounts<V> {
    pub(super) fn entry(&mut self, package: &str, target: &str) -> &mut V {
        let (kind, name) = split_target(target);
        self.packages
            .entry(package.to_owned())
            .or_default()
            .entry(kind.to_owned())
            .or_default()
            .entry(name.to_owned())
            .or_default()
    }

    pub(super) fn get_mut(&mut self, package: &str, target: &str) -> Option<&mut V> {
        let (kind, name) = split_target(target);
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Production callers move previously credited killers whose package and kind entries were already inserted and never removed.")]
        self.packages.get_mut(package)?.get_mut(kind)?.get_mut(name)
    }

    pub(super) fn for_binary(&self, binary: &TestBinary) -> impl Iterator<Item = &V> {
        let package = self.packages.get(&binary.package);
        let qualified = package
            .and_then(|kinds| kinds.get(binary.target_kind.as_str()))
            .and_then(|names| names.get(binary.target.as_str()));
        let legacy = (!binary.target_kind.is_empty())
            .then(|| {
                package
                    .and_then(|kinds| kinds.get(""))
                    .and_then(|names| names.get(binary.target.as_str()))
            })
            .flatten();
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Both production consumers sum these bounded counts; lookup order changes neither membership nor the sum.")]
        [qualified, legacy].into_iter().flatten()
    }
}

fn split_target(target: &str) -> (&str, &str) {
    target
        .split_once(':')
        .filter(|(kind, _name)| !kind.is_empty())
        .unwrap_or(("", target))
}

impl BinaryKills {
    pub(super) fn new(plan: &Plan, killers: &Killers) -> Self {
        let mut result = Self::default();
        for (_, killer) in killers.iter() {
            *result.counts.entry(&killer.package, &killer.target) += 1;
        }
        for mutant in plan.mutants.iter().filter(|mutant| mutant.ordinal > 0) {
            result.packages.insert(mutant.ordinal, Arc::clone(&mutant.package));
            if let Some(killer) = killers.hint(&mutant.id) {
                result.credited.insert(mutant.ordinal, BinaryIdentity::from_killer(killer));
            }
        }
        result
    }

    pub(super) fn record(&mut self, ordinal: u32, binary: &TestBinary) {
        let identity = BinaryIdentity::from_binary(binary);
        if let Some(previous) = self.credited.insert(ordinal, identity.clone()) {
            if
            // #[gamma::skip(cond.always_false, reason = "On reconfirmation previous==new identity, forcing the early-return guard false decrements that identity's count then increments the very same entry. Credited map was already updated identically; count>=1 is guaranteed by its credited member. No callback/order read occurs between decrement/increment. Final counts and all exposed ranking remain identical.", tag = "equivalent")]
            previous == identity {
                return;
            }
            let count = self
                .counts
                .get_mut(&previous.package, &previous.target)
                .expect("each credited mutant contributes to its binary's count");
            *count = count.checked_sub(1).expect("a credited mutant contributes at least one kill");
        }
        *self.counts.entry(&identity.package, &identity.target) += 1;
    }

    pub(super) fn order(&self, ordinal: u32, reachable: &[&TestBinary]) -> Vec<usize> {
        let package = self.packages.get(&ordinal);
        let mut order = (0..reachable.len()).collect::<Vec<_>>();
        order.sort_by_cached_key(|index| {
            let binary = reachable[*index];
            (
                package.is_some_and(|package| package.as_ref() != binary.package),
                Reverse(self.count(binary)),
                *index,
            )
        });
        order
    }

    fn count(&self, binary: &TestBinary) -> usize {
        self.counts.for_binary(binary).copied().sum()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::time::Duration;

    use super::super::Mutant;
    use super::*;
    use crate::exec::sweep::staged::killer_for;
    use crate::exec::sweep::tests::{ranked_binaries, unreachable_harness};
    use crate::fixtures;

    #[test]
    fn target_count_debug_does_not_require_debuggable_values() {
        struct Opaque;

        let _summary = format!("{:?}", TargetCounts::<Opaque>::default());
    }

    #[test]
    fn recovered_missing_legacy_binary_credit_is_exactly_zero_after_a_qualified_hit() {
        let binary = TestBinary {
            package: "package".into(),
            target: "same-name".into(),
            target_kind: "test".into(),
            ..crate::testing::test_binary("unused")
        };
        let mut killers = Killers::default();
        killers.record("historical".into(), killer_for(&binary, "known"));
        let counts = BinaryKills::new(&fixtures::plan(Vec::new()), &killers);
        assert_ne!(BinaryIdentity::from_binary(&binary).target, binary.target);
        assert_eq!(counts.count(&binary), 1);
        let uncredited = TestBinary {
            target_kind: "bench".into(),
            ..binary
        };
        assert_eq!(counts.count(&uncredited), 0);
    }

    #[test]
    fn recovered_zero_ordinal_hints_do_not_become_executable_credits_or_packages() {
        let mutant = Mutant {
            ordinal: 0,
            ..fixtures::mutant()
        };
        let binary = crate::testing::test_binary("unused-binary");
        let mut killers = Killers::default();
        killers.record(mutant.id.clone(), killer_for(&binary, "known"));
        let counts = BinaryKills::new(&fixtures::plan(vec![mutant]), &killers);
        assert!(counts.credited.is_empty());
        assert!(counts.packages.is_empty());
        assert_eq!(counts.count(&binary), 1);
    }

    #[test]
    fn known_mutants_are_counted_once_and_credit_moves_when_the_killer_changes() {
        let (_directory, _work, plan, binaries) = unreachable_harness(ranked_binaries().to_vec());
        let mutant = &plan.mutants[0];
        let mut killers = Killers::default();
        killers.record(mutant.id.clone(), killer_for(&binaries[1], "known"));
        let mut counts = BinaryKills::new(&plan, &killers);
        assert_eq!(counts.count(&binaries[1]), 1);
        counts.record(mutant.ordinal, &binaries[1]);
        assert_eq!(counts.count(&binaries[1]), 1, "reconfirmation must not count the same mutant twice");
        counts.record(mutant.ordinal, &binaries[0]);
        assert_eq!(counts.count(&binaries[1]), 0);
        assert_eq!(counts.count(&binaries[0]), 1);
        counts.record(mutant.ordinal + 1, &binaries[0]);
        assert_eq!(counts.count(&binaries[0]), 2);
    }

    #[test]
    fn kill_counts_preserve_package_tiers_and_canonical_ties_and_update_live() {
        let mut binaries = ranked_binaries().to_vec();
        binaries[0].baseline = Duration::from_millis(1);
        binaries[1].baseline = Duration::from_secs(10);
        let other = binaries
            .iter()
            .cloned()
            .map(|mut binary| {
                binary.package = "other".into();
                binary
            })
            .collect::<Vec<_>>();
        binaries.extend(other);
        let (_directory, _work, plan, binaries) = unreachable_harness(binaries);
        let mut killers = Killers::default();
        for ordinal in 0..3 {
            killers.record(format!("own-{ordinal}").into(), killer_for(&binaries[1], "known"));
        }
        for ordinal in 0..5 {
            killers.record(format!("other-{ordinal}").into(), killer_for(&binaries[3], "known"));
        }
        let mut counts = BinaryKills::new(&plan, &killers);
        let reachable = binaries.iter().collect::<Vec<_>>();
        let ordinal = plan.mutants[0].ordinal;
        assert_eq!(counts.order(ordinal, &reachable), [1, 0, 3, 2]);
        for mutant in 100..104 {
            counts.record(mutant, &binaries[0]);
        }
        assert_eq!(counts.order(ordinal, &reachable), [0, 1, 3, 2]);
        let empty = BinaryKills::new(&plan, &Killers::default());
        assert_eq!(
            empty.order(ordinal, &reachable),
            [0, 1, 2, 3],
            "ties retain baseline and stable identity ordering"
        );
    }

    #[test]
    fn legacy_hint_credit_is_not_duplicated_when_a_qualified_killer_is_confirmed() {
        let mut binaries = ranked_binaries();
        binaries[0].target_kind = "lib".into();
        let (_directory, _work, plan, binaries) = unreachable_harness(binaries.to_vec());
        let mut known = killer_for(&binaries[0], "known");
        known.target = binaries[0].target.clone();
        let mut killers = Killers::default();
        killers.record(plan.mutants[0].id.clone(), known);
        let mut counts = BinaryKills::new(&plan, &killers);
        assert_eq!(counts.count(&binaries[0]), 1);
        counts.record(plan.mutants[0].ordinal, &binaries[0]);
        assert_eq!(counts.count(&binaries[0]), 1);
        let mut different_kind = binaries[0].clone();
        different_kind.target_kind = "test".into();
        assert_eq!(counts.count(&different_kind), 0);
    }
}
