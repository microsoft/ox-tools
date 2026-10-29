// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Global distinct-mutant kill counts for inferred named-test selection.

use core::cmp::Reverse;

use super::{Judgement, Killer, Killers, Outcome, Plan, TestBinary};
use crate::HashMap;

#[derive(Debug, Default)]
pub(super) struct NamedKills {
    counts: HashMap<Killer, usize>,
    credited: HashMap<u32, Killer>,
}

impl NamedKills {
    pub(super) fn new(plan: &Plan, killers: &Killers) -> Self {
        let mut result = Self::default();
        for (_, killer) in killers.iter() {
            *result.counts.entry(killer.clone()).or_default() += 1;
        }
        for mutant in plan.mutants.iter().filter(|mutant| mutant.ordinal > 0) {
            if let Some(killer) = killers.hint(&mutant.id) {
                result.credited.insert(mutant.ordinal, killer.clone());
            }
        }
        result
    }

    pub(super) fn record(&mut self, ordinal: u32, judged: &Judgement) {
        let Judgement::Reached(Outcome::Killed, Some(killer), _) = judged else {
            return;
        };
        if let Some(previous) = self.credited.insert(ordinal, killer.clone()) {
            if
            // #[gamma::skip(cond.always_false, reason = "On reconfirmation previous==new identity, forcing the early-return guard false decrements that identity's count then increments the very same entry. Credited map was already updated identically; count>=1 is guaranteed by its credited member. No callback/order read occurs between decrement/increment. Final counts and all exposed ranking remain identical.", tag = "equivalent")]
            previous == *killer {
                return;
            }
            let count = self
                .counts
                .get_mut(&previous)
                .expect("each credited mutant contributes to its named test's count");
            *count = count.checked_sub(1).expect("a credited mutant contributes at least one kill");
        }
        *self.counts.entry(killer.clone()).or_default() += 1;
    }

    pub(super) fn rank(&self, selections: &mut [(usize, &str)], reachable: &[&TestBinary]) {
        selections.sort_by_cached_key(|(index, test)| {
            let binary = reachable[*index];
            (
                Reverse(self.count(binary, test)),
                binary.package.as_str(),
                binary.killer_target(),
                *test,
            )
        });
    }

    fn count(&self, binary: &TestBinary, test: &str) -> usize {
        let mut killer = super::staged::killer_for(binary, test);
        let qualified = self.counts.get(&killer).copied().unwrap_or(0);
        if killer.target == binary.target {
            return qualified;
        }
        killer.target.clone_from(&binary.target);
        qualified + self.counts.get(&killer).copied().unwrap_or(0)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::super::Mutant;
    use super::*;
    use crate::exec::sweep::staged::killer_for;
    use crate::exec::sweep::tests::{ranked_binaries, unreachable_harness};
    use crate::fixtures;

    #[test]
    fn recovered_missing_legacy_named_credit_is_exactly_zero_after_a_qualified_hit() {
        let binary = TestBinary {
            package: "package".into(),
            target: "same-name".into(),
            target_kind: "test".into(),
            ..crate::testing::test_binary("unused")
        };
        let mut killers = Killers::default();
        killers.record("historical".into(), killer_for(&binary, "known"));
        let counts = NamedKills::new(&fixtures::plan(Vec::new()), &killers);
        assert_ne!(killer_for(&binary, "known").target, binary.target);
        assert_eq!(counts.count(&binary, "known"), 1);
        assert_eq!(counts.count(&binary, "missing"), 0);
    }

    #[test]
    fn recovered_zero_ordinal_hints_do_not_become_named_executable_credits() {
        let mutant = Mutant {
            ordinal: 0,
            ..fixtures::mutant()
        };
        let binary = crate::testing::test_binary("unused-binary");
        let killer = killer_for(&binary, "known");
        let mut killers = Killers::default();
        killers.record(mutant.id.clone(), killer);
        let counts = NamedKills::new(&fixtures::plan(vec![mutant]), &killers);
        assert!(counts.credited.is_empty());
        assert_eq!(counts.count(&binary, "known"), 1);
    }

    fn killed(killer: Killer) -> Judgement {
        Judgement::Reached(Outcome::Killed, Some(killer), None)
    }

    #[test]
    fn historical_exact_kills_and_live_kills_count_distinct_mutants_not_observations() {
        let (_directory, _work, mut plan, binaries) = unreachable_harness(ranked_binaries().to_vec());
        plan.mutants[0].outcome = Outcome::Killed;
        let mutant = &plan.mutants[0];
        let first = killer_for(&binaries[0], "same");
        let second = killer_for(&binaries[1], "same");
        let mut killers = Killers::default();
        killers.record(mutant.id.clone(), first.clone());
        killers.record("historical-outside-plan".into(), first.clone());
        let mut counts = NamedKills::new(&plan, &killers);
        assert_eq!(counts.count(&binaries[0], "same"), 2);
        counts.record(mutant.ordinal, &killed(first));
        assert_eq!(counts.count(&binaries[0], "same"), 2, "resumed confirmations must not double count");
        counts.record(mutant.ordinal, &killed(second.clone()));
        assert_eq!(counts.count(&binaries[0], "same"), 1);
        assert_eq!(counts.count(&binaries[1], "same"), 1);
        counts.record(mutant.ordinal + 1, &killed(second.clone()));
        counts.record(mutant.ordinal + 1, &killed(second));
        assert_eq!(counts.count(&binaries[1], "same"), 2);
    }

    #[test]
    fn counts_are_global_but_test_binary_and_package_identity_remain_distinct() {
        let binaries = ranked_binaries();
        let mut other_package = binaries[0].clone();
        other_package.package = "other".into();
        let mut counts = NamedKills::default();
        counts.record(1, &killed(killer_for(&binaries[0], "same")));
        counts.record(2, &killed(killer_for(&binaries[1], "same")));
        counts.record(3, &killed(killer_for(&binaries[1], "same")));
        counts.record(4, &killed(killer_for(&other_package, "same")));
        assert_eq!(counts.count(&binaries[0], "same"), 1);
        assert_eq!(counts.count(&binaries[1], "same"), 2);
        assert_eq!(counts.count(&other_package, "same"), 1);
        assert_eq!(counts.count(&binaries[0], "different"), 0);
        let reachable = [&binaries[0], &other_package, &binaries[1]];
        let mut selections = [(0, "same"), (1, "same"), (2, "same")];
        counts.rank(&mut selections, &reachable);
        assert_eq!(selections, [(2, "same"), (1, "same"), (0, "same")]);
    }

    #[test]
    fn legacy_exact_credit_moves_to_the_qualified_target_without_double_counting() {
        let mut binaries = ranked_binaries();
        binaries[0].target_kind = "lib".into();
        let (_directory, _work, plan, binaries) = unreachable_harness(binaries.to_vec());
        let mut legacy = killer_for(&binaries[0], "known");
        legacy.target.clone_from(&binaries[0].target);
        let mut killers = Killers::default();
        killers.record(plan.mutants[0].id.clone(), legacy);
        let mut counts = NamedKills::new(&plan, &killers);
        assert_eq!(counts.count(&binaries[0], "known"), 1);
        counts.record(plan.mutants[0].ordinal, &killed(killer_for(&binaries[0], "known")));
        assert_eq!(counts.count(&binaries[0], "known"), 1);
        let mut other_kind = binaries[0].clone();
        other_kind.target_kind = "test".into();
        assert_eq!(counts.count(&other_kind, "known"), 0);
    }

    #[test]
    fn only_final_named_killed_judgements_earn_credit() {
        let binary = &ranked_binaries()[0];
        let killer = killer_for(binary, "test");
        let mut counts = NamedKills::default();
        for judged in [
            Judgement::Reached(Outcome::Killed, None, None),
            Judgement::Reached(Outcome::Survived, Some(killer.clone()), None),
            Judgement::Reached(Outcome::Timeout, None, None),
            Judgement::Abandoned("containment lost".into()),
        ] {
            counts.record(1, &judged);
        }
        assert_eq!(counts.count(binary, "test"), 0);
        counts.record(1, &killed(killer));
        assert_eq!(
            counts.count(binary, "test"),
            1,
            "a newly discovered killer immediately earns one kill"
        );
    }
}
