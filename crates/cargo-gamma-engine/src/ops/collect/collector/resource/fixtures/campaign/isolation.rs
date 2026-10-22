// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

struct Converger;

impl Converger {
    fn minimize_interaction(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        population: &[&Mutant],
        failing: &[&Mutant],
        proofs: &mut Proofs,
    ) -> Result<Option<Vec<u32>>> {
        let mut active = failing.to_vec();
        let mut granularity = 2;
        while Self::minimization_can_continue(active.len(), proofs.local, self.isolation_budget.proofs) {
            let width = active.len().div_ceil(granularity);
            let chunks = active.chunks(width).map(<[_]>::to_vec).collect::<Vec<_>>();
            // #[gamma::skip(literal.bool_flip, tag = "timeout", reason = "claiming reduction without a successful proof keeps retrying the same active population")]
            let mut reduced = false;
            for chunk in &chunks {
                if self.subset_fails(work, plan, scope, verb, limits, events, population, chunk, proofs)? == Some(true) {
                    active.clone_from(chunk);
                    granularity = Self::reduced_chunk_granularity();
                    reduced = true;
                    break;
                }
            }
            if reduced {
                continue;
            }
            for chunk in &chunks {
                let removed = chunk.iter().map(|mutant| mutant.ordinal).collect::<HashSet<_>>();
                let complement = active
                    .iter()
                    .copied()
                    .filter(|mutant| !removed.contains(&mutant.ordinal))
                    .collect::<Vec<_>>();
                if complement.is_empty() {
                    continue;
                }
                if self.subset_fails(work, plan, scope, verb, limits, events, population, &complement, proofs)? == Some(true) {
                    active = complement;
                    granularity = granularity.saturating_sub(1).max(2);
                    reduced = true;
                    break;
                }
            }
            if reduced {
                continue;
            }
            if granularity >= active.len() {
                break;
            }
            granularity = granularity.saturating_mul(2).min(active.len());
        }
        Ok(
            (proofs.local < MAX_ISOLATION_PROOFS && self.isolation_budget.proofs < MAX_INVOCATION_ISOLATION_PROOFS)
                .then(|| active.iter().map(|mutant| mutant.ordinal).collect()),
        )
    }
}
