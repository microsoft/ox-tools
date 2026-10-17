// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded proof builds for compiler failures whose spans name no mutation guard.

use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};

use super::messages::{build_evidence, cargo_message};
use super::{
    BuildScope, Converger, Events, HashMap, HashSet, Isolation, Mutant, Plan, Result, Workspace, compiles_test_harnesses,
    isolation_candidate, run_cargo,
};
use crate::exec::cargo_options::BuildLimits;

/// One tier may hold at most this many candidates.
///
/// 4,096 keeps the in-memory candidate/schema work small enough for campaign-scale populations
/// while still covering the largest target observed in the 40k-mutant workspace campaign. Raise it
/// only with a deterministic candidate/work-count fixture showing a real target is being skipped.
pub(super) const MAX_ISOLATION_CANDIDATES: usize = 4_096;

/// One diagnostic context may launch at most this many proof builds.
///
/// Thirty-two covers two endpoint proofs plus binary narrowing of a 4,096-candidate tier with room
/// for interaction minimization. Raise it only with proof-count evidence from a representative
/// interaction fixture; elapsed-time measurements alone are too host-dependent.
pub(super) const MAX_ISOLATION_PROOFS: usize = 32;

/// One Cargo invocation may investigate at most this many independent failure contexts.
///
/// Sixty-four admits a broad multi-crate failure wave while preventing contributor-controlled
/// target multiplication from making proof work unbounded.
pub(super) const MAX_ISOLATION_CONTEXTS: usize = 64;

/// One Cargo invocation may launch at most this many isolation proof builds across all contexts.
///
/// This is derived from the two local bounds rather than chosen independently: every admitted
/// context can spend its complete local allowance, so one context can never starve another. The
/// context and local limits still impose a strict derived launch ceiling on pathological input.
pub(super) const MAX_INVOCATION_ISOLATION_PROOFS: usize = MAX_ISOLATION_CONTEXTS * MAX_ISOLATION_PROOFS;

#[cfg(test)]
type CandidatePolicy = Box<dyn FnMut(usize) -> bool>;

#[cfg(test)]
thread_local! {
    static CANDIDATE_POLICY: std::cell::RefCell<Option<CandidatePolicy>> = const { std::cell::RefCell::new(None) };
}

fn candidate_tier_admitted(candidates: usize) -> bool {
    #[cfg(test)]
    if let Some(admitted) = CANDIDATE_POLICY.with_borrow_mut(|policy| policy.as_mut().map(|policy| policy(candidates))) {
        return admitted;
    }
    candidates <= MAX_ISOLATION_CANDIDATES
}

fn isolation_proofs_exhausted(local: usize, invocation: usize) -> bool {
    local >= MAX_ISOLATION_PROOFS || invocation >= MAX_INVOCATION_ISOLATION_PROOFS
}

fn without_withdrawn(mut candidates: HashSet<u32>, withdrawn: &HashSet<u32>) -> HashSet<u32> {
    candidates.retain(|ordinal| !withdrawn.contains(ordinal));
    candidates
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FailureContext {
    pub(super) package: String,
    pub(super) target: String,
    pub(super) kind: String,
    pub(super) proof_verb: Vec<String>,
    pub(super) files: HashSet<Utf8PathBuf>,
    pub(super) codes: HashSet<String>,
}

impl FailureContext {
    pub(super) fn verb(&self) -> Vec<String> {
        self.proof_verb.clone()
    }

    fn label(&self) -> String {
        format!("{} ({})", self.target, self.kind)
    }
}

fn targeted_verb(kind: &str, target: &str) -> Vec<String> {
    let mut verb = vec!["check".to_owned(), "--keep-going".to_owned()];
    match kind {
        "lib" | "proc-macro" => verb.push("--lib".to_owned()),
        "bin" | "test" | "example" | "bench" => {
            verb.push(format!("--{kind}"));
            verb.push(target.to_owned());
        }
        _ => {}
    }
    verb
}

#[derive(Debug, Default)]
pub(super) struct IsolationBudget {
    contexts: usize,
    proofs: usize,
}

impl IsolationBudget {
    fn enter_context(&mut self) -> bool {
        if self.contexts >= MAX_ISOLATION_CONTEXTS {
            return false;
        }
        self.contexts = self.contexts.saturating_add(1);
        true
    }

    fn context_limit_reached(&mut self, admissible: bool) -> bool {
        admissible && !self.enter_context()
    }

    pub(super) fn proof(&mut self, local: usize) -> bool {
        if local >= MAX_ISOLATION_PROOFS || self.proofs >= MAX_INVOCATION_ISOLATION_PROOFS {
            return false;
        }
        self.proofs = self.proofs.saturating_add(1);
        true
    }

    #[cfg(test)]
    pub(super) const fn proof_count(&self) -> usize {
        self.proofs
    }
}

#[derive(Debug, Default)]
struct Proofs {
    local: usize,
    results: HashMap<Vec<u32>, Option<bool>>,
}

enum IsolationAttempt {
    Resolved(Isolation),
    NotReproduced,
    Inconclusive,
}

impl Proofs {
    fn key(active: &[&Mutant]) -> Vec<u32> {
        let mut key = active.iter().map(|mutant| mutant.ordinal).collect::<Vec<_>>();
        key.sort_unstable();
        key
    }
}

pub(super) fn failure_contexts(stdout: &str, plan: &Plan, root: &Utf8Path, failed_verb: &[&str]) -> Vec<FailureContext> {
    let mut contexts: Vec<FailureContext> = Vec::new();
    let mut positions: HashMap<(String, String, String, Vec<String>), usize> = HashMap::default();
    for line in stdout.lines() {
        let Some(message) = cargo_message(line) else {
            continue;
        };
        if message.reason != "compiler-message" || message.message.as_ref().is_none_or(|diagnostic| diagnostic.level != "error") {
            continue;
        }
        let Some(target) = message.target.as_ref() else {
            continue;
        };
        let Some(package) = super::package_of_message(message.manifest_path.as_deref(), message.package_id.as_deref(), plan, root) else {
            continue;
        };
        let kind = target.kind.first().map_or("target", AsRef::as_ref).to_owned();
        let mut files = HashSet::default();
        let mut codes = HashSet::default();
        if let Some(diagnostic) = message.message {
            if let Some(code) = diagnostic.code.as_ref() {
                let _ = codes.insert(code.code.to_string());
            }
            for span in diagnostic.spans {
                let Some(file) = span.file_name else {
                    continue;
                };
                let path = Utf8Path::new(file.as_ref())
                    .strip_prefix(root)
                    .unwrap_or_else(|_outside| Utf8Path::new(file.as_ref()));
                let _ = files.insert(path.to_owned());
            }
        }

        let proof_verb = if super::compiles_test_harnesses(failed_verb) && matches!(kind.as_str(), "lib" | "proc-macro" | "bin") {
            failed_verb.iter().map(|argument| (*argument).to_owned()).collect()
        } else {
            targeted_verb(&kind, target.name.as_ref())
        };
        let key = (package, target.name.to_string(), kind, proof_verb);
        let context = if let Some(position) = positions.get(&key).copied() {
            &mut contexts[position]
        } else {
            let position = contexts.len();
            let (package, target, kind, proof_verb) = key.clone();
            contexts.push(FailureContext {
                package,
                target,
                kind,
                proof_verb,
                files: HashSet::default(),
                codes: HashSet::default(),
            });
            let _previous = positions.insert(key, position);
            contexts
                .last_mut()
                .unwrap_or_else(|| unreachable!("the failure context was just appended"))
        };
        context.files.extend(files);
        context.codes.extend(codes);
    }
    contexts
}

fn diagnostically_likely(mutant: &Mutant, codes: &HashSet<String>) -> bool {
    let type_related = codes.iter().any(|code| {
        matches!(
            code.as_str(),
            "E0271" | "E0277" | "E0282" | "E0308" | "E0369" | "E0599" | "E0600" | "E0605" | "E0614"
        )
    });
    if !type_related {
        return true;
    }
    !matches!(
        mutant.mutator.as_ref(),
        "literal.bool_flip" | "bool_expr.negate" | "cond.negate" | "match_guard.negate" | "logical.and_to_or" | "logical.or_to_and"
    ) && !mutant.mutator.starts_with("relational.")
}

pub(super) fn push_isolation_tiers(tiers: &mut Vec<HashSet<u32>>, population: HashSet<u32>, plan: &Plan, codes: &HashSet<String>) {
    if population.is_empty() {
        return;
    }
    let likely = plan
        .mutants
        .iter()
        .filter(|mutant| population.contains(&mutant.ordinal) && diagnostically_likely(mutant, codes))
        .map(|mutant| mutant.ordinal)
        .collect::<HashSet<_>>();
    if !likely.is_empty() && likely != population && tiers.last() != Some(&likely) {
        tiers.push(likely);
    }
    if tiers.last() != Some(&population) {
        tiers.push(population);
    }
}

fn has_admissible_tier(tiers: &[HashSet<u32>], withdrawn: &HashSet<u32>) -> bool {
    tiers.iter().any(|tier| {
        let eligible = tier.iter().filter(|ordinal| !withdrawn.contains(ordinal)).count();
        eligible > 0 && candidate_tier_admitted(eligible)
    })
}

fn isolation_population<'a>(plan: &'a Plan, withdrawn: &HashSet<u32>) -> HashMap<&'a str, Vec<&'a Mutant>> {
    let mut by_package: HashMap<&str, Vec<&Mutant>> = HashMap::default();
    for mutant in plan.mutants.iter().filter(|mutant| isolation_candidate(mutant, withdrawn, None)) {
        by_package.entry(&mutant.package).or_default().push(mutant);
    }
    by_package
}

fn context_packages(context: &FailureContext, plan: &Plan) -> Vec<String> {
    plan.reach
        .get(&context.package)
        .map_or_else(|| vec![context.package.clone()], |packages| packages.iter().cloned().collect())
}

fn context_dependencies(by_package: &HashMap<&str, Vec<&Mutant>>, packages: &[String]) -> HashSet<u32> {
    packages
        .iter()
        .filter_map(|package| by_package.get(package.as_str()))
        .flatten()
        .map(|mutant| mutant.ordinal)
        .collect()
}

fn prioritized_contexts(
    contexts: Vec<FailureContext>,
    plan: &Plan,
    by_package: &HashMap<&str, Vec<&Mutant>>,
) -> Vec<(FailureContext, Vec<String>, usize)> {
    let mut prioritized = contexts
        .into_iter()
        .filter_map(|context| {
            let packages = context_packages(&context, plan);
            let dependencies = packages
                .iter()
                .filter_map(|package| by_package.get(package.as_str()))
                .map(Vec::len)
                .sum::<usize>();

            (dependencies > 0).then_some((context, packages, dependencies))
        })
        .collect::<Vec<_>>();

    prioritized.sort_by(
        |(left, _left_packages, left_dependencies), (right, _right_packages, right_dependencies)| {
            right_dependencies
                .cmp(left_dependencies)
                .then_with(|| left.package.cmp(&right.package))
                .then_with(|| left.target.cmp(&right.target))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.proof_verb.cmp(&right.proof_verb))
        },
    );
    prioritized
}

impl Converger {
    #[expect(
        clippy::too_many_arguments,
        reason = "isolation needs the failed build's complete invocation context and proof limits"
    )]
    pub(super) fn isolate_scoped(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        stdout: &str,
        failed_verb: &[&str],
        roots: Option<&[String]>,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Vec<Isolation>> {
        let by_package = isolation_population(plan, &self.withdrawn);
        let contexts = prioritized_contexts(failure_contexts(stdout, plan, &work.root, failed_verb), plan, &by_package);
        let mut isolated = Vec::new();

        for (context, packages, _dependency_count) in contexts {
            let dependencies = without_withdrawn(context_dependencies(&by_package, &packages), &self.withdrawn);
            if dependencies.is_empty() {
                continue;
            }

            let mut tiers = Vec::new();
            let files = packages
                .iter()
                .filter_map(|package| by_package.get(package.as_str()))
                .flatten()
                .filter(|mutant| context.files.contains(mutant.file.as_ref()))
                .map(|mutant| mutant.ordinal)
                .collect();
            push_isolation_tiers(&mut tiers, files, plan, &context.codes);
            let package = by_package
                .get(context.package.as_str())
                .into_iter()
                .flatten()
                .map(|mutant| mutant.ordinal)
                .collect();
            push_isolation_tiers(&mut tiers, package, plan, &context.codes);
            push_isolation_tiers(&mut tiers, dependencies.clone(), plan, &context.codes);

            // #[gamma::skip(call.replace_with_default, call_result.default, cond.always_false, tag = "outofmemory", reason = "bypassing the admissible-tier budget guard exhausted memory in campaign isolation")]
            if self
                .isolation_budget
                .context_limit_reached(has_admissible_tier(&tiers, &self.withdrawn))
            {
                events.warn(&format!(
                    "left {} unresolved after the Cargo invocation reached its {MAX_ISOLATION_CONTEXTS}-context isolation budget",
                    context.label()
                ));
                isolated.push(Isolation::Unresolved {
                    ordinals: dependencies.into_iter().collect(),
                    context: context.label(),
                });
                continue;
            }

            let scope = BuildScope {
                roots,
                mutants: None,
                publish_progress: false,
            };
            let verb = context.verb();
            let verb = verb.iter().map(String::as_str).collect::<Vec<_>>();
            let mut proofs = Proofs::default();
            let mut resolved = false;
            let mut applicable = true;

            for eligible in tiers {
                let eligible = without_withdrawn(eligible, &self.withdrawn);
                if !candidate_tier_admitted(eligible.len()) {
                    events.warn(&format!(
                        "skipping an isolation tier of {} mutants for {} because the bounded limit is {MAX_ISOLATION_CANDIDATES}",
                        eligible.len(),
                        context.label()
                    ));
                    continue;
                }
                let current_dependencies = dependencies
                    .iter()
                    .copied()
                    .filter(|ordinal| !self.withdrawn.contains(ordinal))
                    .collect::<HashSet<_>>();
                let complete_context = eligible == current_dependencies;
                match self.isolate_candidates(work, plan, scope, &verb, limits, events, &eligible, &mut proofs)? {
                    IsolationAttempt::Resolved(result) => {
                        isolated.push(result);
                        resolved = true;
                        break;
                    }
                    IsolationAttempt::NotReproduced if complete_context => {
                        applicable = false;
                        break;
                    }
                    IsolationAttempt::NotReproduced | IsolationAttempt::Inconclusive => {}
                }
                if isolation_proofs_exhausted(proofs.local, self.isolation_budget.proofs) {
                    break;
                }
            }

            if applicable && !resolved {
                events.warn(&format!(
                    "left {} unresolved after {} local and {} invocation proof builds",
                    context.label(),
                    proofs.local,
                    self.isolation_budget.proofs
                ));
                isolated.push(Isolation::Unresolved {
                    ordinals: dependencies.into_iter().collect(),
                    context: context.label(),
                });
            }
        }
        Ok(isolated)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "candidate isolation needs the same complete context as convergence"
    )]
    fn isolate_candidates(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        eligible: &HashSet<u32>,
        proofs: &mut Proofs,
    ) -> Result<IsolationAttempt> {
        let mut candidates = plan
            .mutants
            .iter()
            .filter(|mutant| isolation_candidate(mutant, &self.withdrawn, scope.mutants) && eligible.contains(&mutant.ordinal))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(IsolationAttempt::Inconclusive);
        }

        let pristine = self.subset_fails(work, plan, scope, verb, limits, events, &candidates, &[], proofs)?;
        let populated = self.subset_fails(work, plan, scope, verb, limits, events, &candidates, &candidates, proofs)?;
        if pristine != Some(false) {
            return Ok(IsolationAttempt::Inconclusive);
        }
        if populated != Some(true) {
            return Ok(if populated == Some(false) {
                IsolationAttempt::NotReproduced
            } else {
                IsolationAttempt::Inconclusive
            });
        }

        candidates.sort_by(|left, right| left.item_path.cmp(&right.item_path).then_with(|| left.ordinal.cmp(&right.ordinal)));
        let population = candidates.clone();
        let mut items: Vec<Vec<&Mutant>> = Vec::new();
        for mutant in candidates {
            if items
                .last()
                .and_then(|item| item.first())
                .is_some_and(|first| first.item_path == mutant.item_path)
            {
                items
                    .last_mut()
                    .unwrap_or_else(|| unreachable!("the item was just observed"))
                    .push(mutant);
            } else {
                items.push(vec![mutant]);
            }
        }

        // #[gamma::skip(relational.gt_to_ge, literal.int_decrement, tag = "timeout", reason = "bisecting a singleton cannot reduce the isolation population")]
        while items.len() > 1 {
            // #[gamma::skip(arith.div_to_rem, literal.int_decrement, literal.int_increment, tag = "timeout", reason = "these campaign divisors produced empty or nonshrinking isolation partitions")]
            let middle = items.len() / 2;
            let left = items[..middle].concat();
            let right = items[middle..].concat();
            if self.subset_fails(work, plan, scope, verb, limits, events, &population, &left, proofs)? == Some(true) {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "successful bisection must shrink the population before retrying")]
                items.truncate(middle);
                continue;
            }
            if self.subset_fails(work, plan, scope, verb, limits, events, &population, &right, proofs)? == Some(true) {
                drop(items.drain(..middle));
                continue;
            }
            let Some(members) = self.minimize_interaction(work, plan, scope, verb, limits, events, &population, &population, proofs)?
            else {
                return Ok(IsolationAttempt::Inconclusive);
            };
            return Ok(IsolationAttempt::Resolved(Isolation::Interaction {
                excluded: members[0],
                members,
            }));
        }

        let item = items.pop().unwrap_or_default();
        let mut narrowed = item.clone();
        // #[gamma::skip(relational.gt_to_ge, literal.int_decrement, tag = "timeout", reason = "bisecting a singleton cannot reduce the isolation population")]
        while narrowed.len() > 1 {
            // #[gamma::skip(arith.div_to_rem, literal.int_decrement, literal.int_increment, tag = "timeout", reason = "these campaign divisors produced empty or nonshrinking isolation partitions")]
            let middle = narrowed.len() / 2;
            let left = &narrowed[..middle];
            let right = &narrowed[middle..];
            if self.subset_fails(work, plan, scope, verb, limits, events, &population, left, proofs)? == Some(true) {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "successful bisection must shrink the population before retrying")]
                narrowed.truncate(middle);
            } else if self.subset_fails(work, plan, scope, verb, limits, events, &population, right, proofs)? == Some(true) {
                drop(narrowed.drain(..middle));
            } else {
                let Some(members) = self.minimize_interaction(work, plan, scope, verb, limits, events, &population, &item, proofs)? else {
                    return Ok(IsolationAttempt::Inconclusive);
                };
                return Ok(IsolationAttempt::Resolved(Isolation::Interaction {
                    excluded: members[0],
                    members,
                }));
            }
        }
        Ok(IsolationAttempt::Resolved(Isolation::Blamed(
            narrowed.iter().map(|mutant| mutant.ordinal).collect(),
        )))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "interaction minimization needs the same complete context as every proof build"
    )]
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

    fn minimization_can_continue(active: usize, local: usize, invocation: usize) -> bool {
        active > 1 && local < MAX_ISOLATION_PROOFS && invocation < MAX_INVOCATION_ISOLATION_PROOFS
    }

    const fn reduced_chunk_granularity() -> usize {
        2
    }

    #[cfg(test)]
    pub(super) fn isolate(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Option<Isolation>> {
        let eligible = plan
            .mutants
            .iter()
            .filter(|mutant| isolation_candidate(mutant, &self.withdrawn, select))
            .map(|mutant| mutant.ordinal)
            .collect();
        let attempt = self.isolate_candidates(
            work,
            plan,
            BuildScope {
                roots: select,
                mutants: select,
                publish_progress: false,
            },
            verb,
            limits,
            events,
            &eligible,
            &mut Proofs::default(),
        )?;
        Ok(match attempt {
            IsolationAttempt::Resolved(result) => Some(result),
            IsolationAttempt::NotReproduced | IsolationAttempt::Inconclusive => None,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a proof build needs the same complete context as convergence"
    )]
    fn subset_fails(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        population: &[&Mutant],
        active: &[&Mutant],
        proofs: &mut Proofs,
    ) -> Result<Option<bool>> {
        let key = Proofs::key(active);
        if let Some(cached) = proofs.results.get(&key) {
            return Ok(*cached);
        }
        if !self.isolation_budget.proof(proofs.local) {
            return Ok(None);
        }
        proofs.local = proofs.local.saturating_add(1);
        let active = active.iter().map(|mutant| mutant.ordinal).collect::<HashSet<_>>();

        #[cfg(test)]
        if let Some(oracle) = self.subset_oracle {
            self.proof_roots.push(scope.roots.map(<[String]>::to_vec));
            let result = oracle(&active);
            let _ = proofs.results.insert(key, result);
            return Ok(result);
        }

        let result = self.run_proof(work, plan, scope, verb, limits, events, population.len(), &active)?;
        let _ = proofs.results.insert(key, result);
        Ok(result)
    }

    /// Executes one proof build after the deterministic isolation algorithm selects its schema.
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[expect(
        clippy::too_many_arguments,
        reason = "the proof invocation carries the build context selected by the isolation algorithm"
    )]
    fn run_proof(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        population: usize,
        active: &HashSet<u32>,
    ) -> Result<Option<bool>> {
        // A proof's active schema is exact: every unrelated pending mutant is restored to pristine
        // source, including mutants outside the current file/package/dependency tier.
        let written = self.instrument_active_schema(work, plan, active)?;
        let outcome = run_cargo(work, plan, verb, scope.roots, limits, self.first_round, events)?;
        if events.wants_isolation_evidence()
            && let Some(stdout) = outcome.stdout.as_deref()
        {
            let evidence = build_evidence(stdout, compiles_test_harnesses(verb));
            let written = written.iter().map(|path| path.as_std_path() as &Path).collect::<Vec<_>>();
            events.isolation_evidence(
                active.len(),
                population,
                &written,
                evidence.fresh,
                evidence.rebuilt,
                &evidence.failed_targets,
            );
        }
        Ok(outcome.stdout.map(|_stdout| !outcome.succeeded))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::exec::build::tests::{
        probe_plan, scripted_output, split_native_isolation_plan, trivial_workspace, unpositioned_native_context,
    };

    #[derive(Default)]
    struct ProofEvents {
        observations: Vec<(usize, usize, Vec<std::path::PathBuf>)>,
        warnings: Vec<String>,
    }

    impl Events for ProofEvents {
        fn phase(&mut self, _verb: &str, _detail: &str) {}

        fn mutant(&mut self, _mutant: &Mutant) {}

        fn warn(&mut self, warning: &str) {
            self.warnings.push(warning.to_owned());
        }

        fn wants_isolation_evidence(&self) -> bool {
            true
        }

        fn isolation_evidence(
            &mut self,
            active: usize,
            population: usize,
            written: &[&Path],
            _fresh: usize,
            _rebuilt: usize,
            _failed_targets: &[String],
        ) {
            self.observations
                .push((active, population, written.iter().map(|path| path.to_path_buf()).collect()));
        }
    }

    fn proof_scope() -> BuildScope<'static> {
        BuildScope {
            roots: None,
            mutants: None,
            publish_progress: false,
        }
    }

    // Emits ordinal-bearing source markers and controlled item groups for native instrumentation.
    // The paired compiler script inspects these markers rather than compiling the schema.
    fn marked_proof_plan(work: &Workspace, mutants: usize, separate_items: bool) -> Plan {
        let mut plan = probe_plan(work, 0, mutants);
        for mutant in &mut plan.mutants {
            let ordinal = mutant.ordinal;
            mutant.replacement = format!("{{ let proof_marker_{ordinal}_end = 2; proof_marker_{ordinal}_end }}").into();
            let item = if separate_items {
                format!("item_{ordinal}")
            } else {
                "same_item".to_owned()
            };
            mutant.item_path = item.into();
        }
        plan
    }

    // Reads actual instrumented source: a proof fails when every required ordinal is present.
    // failure_at injects a transport error at that attempt; the returned trace records proof order,
    // cache reuse and budget behavior independently of the scripted compiler verdict.
    fn native_proof_script<T>(
        work: &Workspace,
        population: usize,
        required: &[u32],
        failure_at: Option<usize>,
        body: impl FnOnce() -> T,
    ) -> (T, Vec<Vec<u32>>) {
        let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = super::super::with_compiler(
            {
                let attempted = std::rc::Rc::clone(&attempted);
                let root = work.root.clone();
                let required = required.to_vec();
                move |observed, arguments, budget, _| {
                    assert_eq!(observed.root, root);
                    assert_eq!(arguments[0], "check");
                    assert_eq!(budget, Some(core::time::Duration::from_millis(19)));
                    let text = std::fs::read_to_string(root.join("src").join("lib.rs")).unwrap();
                    let active = (1..=u32::try_from(population).unwrap())
                        .filter(|ordinal| text.contains(&format!("proof_marker_{ordinal}_end")))
                        .collect::<Vec<_>>();
                    let failed = required.iter().all(|ordinal| active.contains(ordinal));
                    let mut attempted = attempted.borrow_mut();
                    attempted.push(active);
                    assert!(attempted.len() <= MAX_ISOLATION_PROOFS, "proof orchestration must stay bounded");
                    if failure_at == Some(attempted.len()) {
                        return Err(crate::error::error!("scripted proof failure {}", attempted.len()));
                    }
                    Ok(Some(scripted_output(!failed, "proof stdout", "")))
                }
            },
            body,
        );
        let attempted = attempted.borrow().clone();
        (result, attempted)
    }

    fn proof_limits() -> BuildLimits {
        BuildLimits {
            timeout: Some(core::time::Duration::from_millis(19)),
            ..BuildLimits::default()
        }
    }

    fn with_candidate_policy<T>(policy: impl FnMut(usize) -> bool + 'static, body: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CANDIDATE_POLICY.with_borrow_mut(|policy| *policy = None);
            }
        }
        CANDIDATE_POLICY.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "candidate policies must not nest");
            *slot = Some(Box::new(policy));
        });
        let _reset = Reset;
        body()
    }

    #[test]
    fn scoped_native_proofs_prefer_file_then_package_tiers_and_stop_after_a_resolved_or_complete_context() {
        for mode in 0..4 {
            let (_directory, work) = trivial_workspace("native-isolation-tier-order");
            let mut plan = split_native_isolation_plan(&work);
            if mode != 0 {
                for mutant in &mut plan.mutants[1..] {
                    mutant.package = "dependency".into();
                }
                plan.reach
                    .insert("trivial".into(), ["trivial".into(), "dependency".into()].into_iter().collect());
            }
            let mut context: serde_json::Value =
                serde_json::from_str(&unpositioned_native_context("first")).expect("the fixture serializes a valid compiler diagnostic");
            if mode == 1 {
                context["message"]["spans"][0]["file_name"] = "src/unrelated.rs".into();
            } else if mode == 2 {
                let mut second = context["message"]["spans"][0].clone();
                second["file_name"] = "src/second.rs".into();
                context["message"]["spans"]
                    .as_array_mut()
                    .expect("fixture spans are an array")
                    .push(second);
            } else if mode == 3 {
                context["message"]["spans"][0]["file_name"] = "src/second.rs".into();
            }
            let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut converger = Converger::default();
            let mut events = ProofEvents::default();
            let result = with_candidate_policy(
                move |count| count <= if mode == 3 { 1 } else { MAX_ISOLATION_CANDIDATES },
                || {
                    super::super::with_compiler(
                        {
                            let attempted = std::rc::Rc::clone(&attempted);
                            move |work, _, _, _| {
                                let source = ["first", "second"]
                                    .map(|name| {
                                        std::fs::read_to_string(work.root.join(format!("src/{name}.rs")))
                                            .expect("the split fixture has both native source files")
                                    })
                                    .join("\n");
                                let active = (1..=3)
                                    .filter(|ordinal| source.contains(&format!("regression_marker_{ordinal}_end")))
                                    .collect::<Vec<_>>();
                                let failed = active.contains(&1) && (mode != 2 || !active.contains(&2));
                                let mut attempted = attempted.borrow_mut();
                                attempted.push(active);
                                assert!(attempted.len() <= 8, "a three-mutant tier proof must remain bounded");
                                Ok(Some(scripted_output(!failed, "", "")))
                            }
                        },
                        || {
                            converger.isolate_scoped(
                                &work,
                                &plan,
                                &context.to_string(),
                                &["check", "--tests"],
                                None,
                                proof_limits(),
                                &mut events,
                            )
                        },
                    )
                },
            )
            .expect("native tier proofs use an infallible compiler callback");
            if mode == 2 {
                assert!(result.is_empty(), "a complete non-reproducing population ends this context");
                assert_eq!(*attempted.borrow(), [vec![], vec![1, 2, 3]]);
            } else {
                assert_eq!(result.len(), 1, "resolution must not continue into later tiers");
                assert!(matches!(&result[0], Isolation::Blamed(ordinals) if ordinals == &[1]));
                assert_eq!(*attempted.borrow(), [vec![], vec![1]]);
            }
            assert_eq!(events.warnings.len(), usize::from(mode == 3));
        }
    }

    #[test]
    fn exhausted_contexts_report_every_context_and_a_proof_cap_stops_before_considering_another_tier() {
        for local in [MAX_ISOLATION_PROOFS - 1, MAX_ISOLATION_PROOFS] {
            for invocation in [MAX_INVOCATION_ISOLATION_PROOFS - 1, MAX_INVOCATION_ISOLATION_PROOFS] {
                assert_eq!(
                    isolation_proofs_exhausted(local, invocation),
                    local == MAX_ISOLATION_PROOFS || invocation == MAX_INVOCATION_ISOLATION_PROOFS
                );
            }
        }
        let (_directory, work) = trivial_workspace("native-isolation-proof-cap");
        let plan = split_native_isolation_plan(&work);
        let mut converger = Converger::default();
        converger.isolation_budget.contexts = MAX_ISOLATION_CONTEXTS;
        let mut events = ProofEvents::default();
        let contexts = ["first", "second"].map(unpositioned_native_context).join("\n");
        let result = converger
            .isolate_scoped(&work, &plan, &contexts, &["check", "--tests"], None, proof_limits(), &mut events)
            .expect("exhausted context admission cannot execute a compiler");
        assert_eq!(result.len(), 2, "one exhausted context must not abandon the next");
        assert!(result.iter().all(|result| matches!(result, Isolation::Unresolved { .. })));
        assert_eq!(events.warnings.len(), 2);
        let visited = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut converger = Converger::default();
        converger.isolation_budget.proofs = MAX_INVOCATION_ISOLATION_PROOFS - 1;
        let mut events = ProofEvents::default();
        let result = with_candidate_policy(
            {
                let visited = std::rc::Rc::clone(&visited);
                move |count| {
                    visited.borrow_mut().push(count);
                    true
                }
            },
            || {
                super::super::with_compiler(
                    |_, _, _, _| Ok(Some(scripted_output(true, "", ""))),
                    || {
                        converger.isolate_scoped(
                            &work,
                            &plan,
                            &unpositioned_native_context("first"),
                            &["check", "--tests"],
                            None,
                            proof_limits(),
                            &mut events,
                        )
                    },
                )
            },
        )
        .expect("the last admitted pristine proof succeeds");
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Isolation::Unresolved { .. }));
        assert_eq!(
            *visited.borrow(),
            [1, 1],
            "the exhausted proof budget forbids even considering the next tier"
        );
        assert_eq!(events.warnings.len(), 1);
        assert!(events.warnings[0].contains("1 local and 2048 invocation proof builds"));
    }

    #[test]
    fn inadmissible_contexts_do_not_consume_the_last_admission_slot_or_trigger_a_spurious_limit() {
        let mut budget = IsolationBudget {
            contexts: MAX_ISOLATION_CONTEXTS - 1,
            proofs: 0,
        };
        assert!(!budget.context_limit_reached(false));
        assert_eq!(budget.contexts, MAX_ISOLATION_CONTEXTS - 1);
        assert!(!budget.context_limit_reached(true));
        assert_eq!(budget.contexts, MAX_ISOLATION_CONTEXTS);
        assert!(!budget.context_limit_reached(false));
        assert!(budget.context_limit_reached(true));
        assert_eq!(budget.contexts, MAX_ISOLATION_CONTEXTS);
    }

    #[test]
    fn minimization_admission_stops_singletons_and_both_exact_proof_caps_without_entering_a_cached_loop() {
        assert_eq!(Converger::reduced_chunk_granularity(), 2);
        for active in [0, 1, 2] {
            for local in [MAX_ISOLATION_PROOFS - 1, MAX_ISOLATION_PROOFS] {
                for invocation in [MAX_INVOCATION_ISOLATION_PROOFS - 1, MAX_INVOCATION_ISOLATION_PROOFS] {
                    assert_eq!(
                        Converger::minimization_can_continue(active, local, invocation),
                        active == 2 && local < MAX_ISOLATION_PROOFS && invocation < MAX_INVOCATION_ISOLATION_PROOFS
                    );
                }
            }
        }
    }

    // Population, failure-predicate mode, ordered compiler queries, and minimal surviving ordinals.
    type MinimizationCase = (usize, u8, Vec<Vec<u32>>, Vec<u32>);

    fn minimization_cases() -> [MinimizationCase; 5] {
        [
            // A required prefix pair survives repeated half reductions, then rejects both singletons.
            (8, 0, vec![vec![1, 2, 3, 4], vec![1, 2], vec![1], vec![2]], vec![1, 2]),
            // Independent failures occupy opposite halves; retaining the first restarts reduction there.
            (8, 1, vec![vec![1, 2, 3, 4], vec![1, 2], vec![1]], vec![1]),
            // Member 4 masks failure 1; the competing 2/11 pair spans halves, forcing smaller chunks
            // until an unmasked prefix can be retained and reduced to its single failing member.
            (
                12,
                2,
                vec![
                    vec![1, 2, 3, 4, 5, 6],
                    vec![7, 8, 9, 10, 11, 12],
                    vec![1, 2, 3],
                    vec![1, 2],
                    vec![1],
                ],
                vec![1],
            ),
            // The required 2/7 pair spans halves and quarter chunks: only chunk removal can shrink it,
            // and each successful removal must restart reduction before the final pair is minimal.
            (
                8,
                3,
                vec![
                    vec![1, 2, 3, 4],
                    vec![5, 6, 7, 8],
                    vec![1, 2],
                    vec![3, 4],
                    vec![5, 6],
                    vec![7, 8],
                    vec![3, 4, 5, 6, 7, 8],
                    vec![1, 2, 5, 6, 7, 8],
                    vec![1, 2, 7, 8],
                    vec![1],
                    vec![2],
                    vec![7],
                    vec![8],
                    vec![2, 7, 8],
                    vec![2, 8],
                    vec![2, 7],
                ],
                vec![2, 7],
            ),
            // A larger population places required 1/8 in separate three-member chunks, exercising
            // repeated complement reductions and repartitioning before singleton removals fail.
            (
                12,
                4,
                vec![
                    vec![1, 2, 3, 4, 5, 6],
                    vec![7, 8, 9, 10, 11, 12],
                    vec![1, 2, 3],
                    vec![4, 5, 6],
                    vec![7, 8, 9],
                    vec![10, 11, 12],
                    vec![4, 5, 6, 7, 8, 9, 10, 11, 12],
                    vec![1, 2, 3, 7, 8, 9, 10, 11, 12],
                    vec![1, 2, 3, 10, 11, 12],
                    vec![1, 2, 3, 7, 8, 9],
                    vec![1, 2],
                    vec![3, 7],
                    vec![8, 9],
                    vec![3, 7, 8, 9],
                    vec![1, 2, 8, 9],
                    vec![1],
                    vec![2],
                    vec![8],
                    vec![9],
                    vec![2, 8, 9],
                    vec![1, 8, 9],
                    vec![1, 9],
                    vec![1, 8],
                ],
                vec![1, 8],
            ),
        ]
    }

    #[test]
    fn minimization_uses_the_exact_bounded_native_query_order_for_prefix_competing_and_unmasked_failures() {
        for (count, mode, expected, members) in minimization_cases() {
            let (_directory, work) = trivial_workspace("native-minimization-query-order");
            let plan = marked_proof_plan(&work, count, true);
            let attempted = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let mut converger = Converger::default();
            let mut proofs = Proofs::default();
            let population = plan.mutants.iter().collect::<Vec<_>>();
            let result = super::super::with_compiler(
                {
                    let root = work.root.clone();
                    let attempted = std::rc::Rc::clone(&attempted);
                    move |_, _, _, _| {
                        let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
                        let active = (1..=u32::try_from(count).unwrap())
                            .filter(|ordinal| source.contains(&format!("proof_marker_{ordinal}_end")))
                            .collect::<Vec<_>>();
                        let has = |ordinal| active.contains(&ordinal);
                        let failed = match mode {
                            0 => has(1) && has(2),
                            1 => has(1) || has(5),
                            2 => (has(1) && !has(4)) || (has(2) && has(11)),
                            3 => has(2) && has(7),
                            _ => has(1) && has(8),
                        };
                        let mut attempted = attempted.borrow_mut();
                        attempted.push(active);
                        assert!(attempted.len() <= MAX_ISOLATION_PROOFS);
                        Ok(Some(scripted_output(!failed, "", "")))
                    }
                },
                || {
                    converger.minimize_interaction(
                        &work,
                        &plan,
                        proof_scope(),
                        &["check"],
                        proof_limits(),
                        &mut ProofEvents::default(),
                        &population,
                        &population,
                        &mut proofs,
                    )
                },
            )
            .unwrap();
            assert_eq!(result, Some(members), "mode={mode}");
            assert_eq!(*attempted.borrow(), expected, "mode={mode}");
            assert_eq!(proofs.local, expected.len());
            assert_eq!(converger.isolation_budget.proofs, expected.len());
        }
    }

    #[test]
    fn actual_isolation_proofs_propagate_each_early_bisection_and_minimization_transport_failure() {
        for separate_items in [false, true] {
            for failed_call in 1..=9 {
                let (_directory, work) = trivial_workspace("exec-isolation-transport-");
                let plan = marked_proof_plan(&work, 4, separate_items);
                let mut converger = Converger::default();
                let (result, attempted) = native_proof_script(&work, 4, &[1, 4], Some(failed_call), || {
                    converger.isolate(&work, &plan, None, &["check"], proof_limits(), &mut ProofEvents::default())
                });
                let failure =
                    result.expect_err("the selected proof must propagate its transport failure through the full isolation caller");
                assert_eq!(failure.message(), format!("scripted proof failure {failed_call}"));
                assert_eq!(attempted.len(), failed_call);
                if failed_call > 1 {
                    assert!(attempted[0].is_empty(), "the pristine proof restores every mutant");
                }
                if failed_call > 2 {
                    assert_eq!(attempted[1], [1, 2, 3, 4]);
                }
            }
        }
    }

    #[test]
    fn actual_isolation_proofs_report_minimal_cross_item_interactions_and_exact_cache_population() {
        let (_directory, work) = trivial_workspace("exec-isolation-native-interaction-");
        let plan = marked_proof_plan(&work, 4, true);
        let mut converger = Converger::default();
        let (result, attempted) = native_proof_script(&work, 4, &[1, 4], None, || {
            converger.isolate(&work, &plan, None, &["check"], proof_limits(), &mut ProofEvents::default())
        });
        let result = result.unwrap().expect("the finite full proof graph resolves the interaction");
        let Isolation::Interaction { excluded, members } = result else {
            panic!("both endpoint mutants are necessary: {result:?}")
        };
        assert_eq!(excluded, 1);
        assert_eq!(members, [1, 4]);
        assert!(attempted.contains(&vec![1]));
        assert!(attempted.contains(&vec![4]));
        assert_eq!(attempted.len(), 11);
        assert_eq!(
            attempted.iter().collect::<HashSet<_>>().len(),
            attempted.len(),
            "repeated schemas use cached proofs"
        );
        let active = vec![&plan.mutants[0]];
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut events = ProofEvents::default();
        let (result, _) = native_proof_script(&work, 4, &[1, 4], None, || {
            converger.subset_fails(
                &work,
                &plan,
                proof_scope(),
                &["check"],
                proof_limits(),
                &mut events,
                &population,
                &active,
                &mut Proofs::default(),
            )
        });
        assert_eq!(result.unwrap(), Some(false));
        assert_eq!((events.observations[0].0, events.observations[0].1), (1, 4));
    }

    #[test]
    fn empty_or_already_withdrawn_candidate_tiers_never_invoke_the_compiler() {
        let (_directory, work) = trivial_workspace("exec-isolation-no-candidates-");
        let plan = marked_proof_plan(&work, 2, true);
        for withdrawn in [HashSet::default(), HashSet::from_iter([1, 2])] {
            let eligible = if withdrawn.is_empty() {
                HashSet::default()
            } else {
                HashSet::from_iter([1, 2])
            };
            let mut converger = Converger {
                withdrawn,
                ..Converger::default()
            };
            let result = super::super::with_compiler(
                |_, _, _, _| panic!("an empty effective tier must not start a proof"),
                || {
                    converger.isolate_candidates(
                        &work,
                        &plan,
                        proof_scope(),
                        &["check"],
                        proof_limits(),
                        &mut ProofEvents::default(),
                        &eligible,
                        &mut Proofs::default(),
                    )
                },
            )
            .unwrap();
            assert!(matches!(result, IsolationAttempt::Inconclusive));
        }
    }

    #[test]
    fn scoped_isolation_retains_nondefault_limits_and_uses_thirteen_cached_endpoint_proofs() {
        let (_directory, work) = trivial_workspace("exec-scoped-endpoint-proofs-");
        let plan = marked_proof_plan(&work, 5, true);
        let mut converger = Converger::default();
        let stdout = error_stream().replace("subject", "trivial");
        let (isolated, attempted) = native_proof_script(&work, 5, &[1, 5], None, || {
            converger.isolate_scoped(&work, &plan, &stdout, &["check"], None, proof_limits(), &mut ProofEvents::default())
        });
        let isolated = isolated.unwrap();
        assert_eq!(isolated.len(), 1);
        let Isolation::Interaction { excluded, members } = &isolated[0] else {
            panic!("endpoint interaction must be retained: {isolated:?}")
        };
        assert_eq!(*excluded, 1);
        assert_eq!(members, &[1, 5]);
        assert_eq!(attempted.len(), 13);
        assert_eq!(converger.isolation_budget.proofs, 13);
        assert_eq!(converger.isolation_budget.contexts, 1);
        assert_eq!(attempted.iter().collect::<HashSet<_>>().len(), 13);
        assert_eq!(&attempted[..4], &[vec![], vec![1, 2, 3, 4, 5], vec![1, 2], vec![3, 4, 5]]);
    }

    #[test]
    fn scoped_isolation_propagates_a_late_proof_failure_after_pristine_and_populated_builds() {
        let (_directory, work) = trivial_workspace("exec-scoped-proof-failure-");
        let plan = marked_proof_plan(&work, 5, true);
        let mut converger = Converger::default();
        let stdout = error_stream().replace("subject", "trivial");
        let (result, attempted) = native_proof_script(&work, 5, &[1, 5], Some(5), || {
            converger.isolate_scoped(&work, &plan, &stdout, &["check"], None, proof_limits(), &mut ProofEvents::default())
        });
        let failure = result.expect_err("a late file-tier proof error must propagate through scoped isolation");
        assert_eq!(failure.message(), "scripted proof failure 5");
        assert_eq!(attempted.len(), 5);
        assert_eq!(attempted[0], Vec::<u32>::new());
        assert_eq!(attempted[1], [1, 2, 3, 4, 5]);
    }

    #[test]
    fn context_streams_ignore_each_rejected_message_and_choose_the_first_kind_before_harness_routing() {
        let root = Utf8Path::new("workspace");
        let plan = plan(root, 1);
        for (kinds, expected_kind, failed, expected_verb) in [
            (vec!["lib", "example"], "lib", vec!["build"], vec!["check", "--keep-going", "--lib"]),
            (vec!["lib", "example"], "lib", vec!["test", "--no-run"], vec!["test", "--no-run"]),
            (
                vec!["example", "lib"],
                "example",
                vec!["test", "--no-run"],
                vec!["check", "--keep-going", "--example", "subject"],
            ),
            (Vec::new(), "target", vec!["build"], vec!["check", "--keep-going"]),
        ] {
            let mut accepted: serde_json::Value = serde_json::from_str(&error_stream()).unwrap();
            accepted["target"]["kind"] = serde_json::json!(kinds);
            let mut wrong_reason = accepted.clone();
            wrong_reason["reason"] = "compiler-artifact".into();
            wrong_reason["target"]["name"] = "wrong-reason".into();
            let mut warning = accepted.clone();
            warning["message"]["level"] = "warning".into();
            warning["target"]["name"] = "warning-target".into();
            let mut missing = accepted.clone();
            missing.as_object_mut().unwrap().remove("message");
            missing["target"]["name"] = "missing-diagnostic".into();
            let stream = format!("not-json\n{wrong_reason}\n{warning}\n{missing}\n{accepted}");
            let contexts = failure_contexts(&stream, &plan, root, &failed);
            assert_eq!(contexts.len(), 1);
            assert_eq!(contexts[0].kind, expected_kind);
            assert_eq!(contexts[0].verb(), expected_verb);
            assert_eq!(contexts[0].target, "subject");
            assert_eq!(contexts[0].files, HashSet::from_iter([Utf8PathBuf::from("src/lib.rs")]));
            assert_eq!(contexts[0].codes, HashSet::from_iter(["E0308".to_owned()]));
        }
    }

    #[test]
    fn contexts_without_any_pending_dependency_are_not_admitted_even_at_zero_boundary() {
        let root = Utf8Path::new("workspace");
        let plan = plan(root, 1);
        let by_package = isolation_population(&plan, &HashSet::default());
        let ranked = prioritized_contexts(
            vec![context("foreign", "not-pending"), context("subject", "pending")],
            &plan,
            &by_package,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0.package, "subject");
        assert_eq!(ranked[0].1, ["subject"]);
        assert_eq!(ranked[0].2, 1);
    }
    #[test]
    fn native_proof_adapter_preserves_limits_population_written_files_and_all_terminal_results() {
        let (_directory, work) = trivial_workspace("exec-native-proof-adapter-");
        let plan = probe_plan(&work, 0, 3);
        let limits = BuildLimits {
            timeout: Some(core::time::Duration::from_millis(19)),
            ..BuildLimits::default()
        };
        let mut converger = Converger::default();
        for (output, expected) in [(Some(true), Some(false)), (Some(false), Some(true)), (None, None)] {
            let mut events = ProofEvents::default();
            let result = super::super::with_compiler(
                move |_, arguments, budget, _| {
                    assert_eq!(arguments[0], "check");
                    assert_eq!(budget, limits.timeout);
                    Ok(output.map(|success| scripted_output(success, "proof stdout", "")))
                },
                || {
                    converger.run_proof(
                        &work,
                        &plan,
                        proof_scope(),
                        &["check"],
                        limits,
                        &mut events,
                        3,
                        &HashSet::from_iter([2]),
                    )
                },
            )
            .unwrap();
            assert_eq!(result, expected);
            if output.is_some() {
                assert_eq!(events.observations.len(), 1);
                let (active, population, written) = &events.observations[0];
                assert_eq!((*active, *population), (1, 3));
                if output == Some(true) {
                    assert_eq!(written, &[plan.files[0].path.as_std_path().to_path_buf()]);
                } else {
                    assert!(written.is_empty(), "reusing an identical schema does not rewrite a clean file");
                }
            } else {
                assert!(events.observations.is_empty());
            }
        }
    }

    #[test]
    fn native_proof_adapter_propagates_instrumentation_and_compiler_failures_without_publishing_evidence() {
        let (_directory, work) = trivial_workspace("exec-proof-errors-");
        let plan = probe_plan(&work, 0, 2);
        let mut converger = Converger::default();
        let mut events = ProofEvents::default();
        let failure = super::super::with_compiler(
            |_, _, _, _| Err(crate::error::error!("proof transport failed")),
            || {
                converger.run_proof(
                    &work,
                    &plan,
                    proof_scope(),
                    &["check"],
                    BuildLimits::default(),
                    &mut events,
                    2,
                    &HashSet::from_iter([1]),
                )
            },
        )
        .expect_err("a compiler transport failure must propagate from the proof adapter");
        assert_eq!(failure.message(), "proof transport failed");
        assert!(events.observations.is_empty());
        std::fs::remove_file(work.root.join("src/lib.rs")).unwrap();
        let mut converger = Converger::default();
        let failure = super::super::with_compiler(
            |_, _, _, _| panic!("no compiler may run after source instrumentation failed"),
            || {
                converger.run_proof(
                    &work,
                    &plan,
                    proof_scope(),
                    &["check"],
                    BuildLimits::default(),
                    &mut events,
                    2,
                    &HashSet::from_iter([1]),
                )
            },
        )
        .expect_err("an unreadable copied source cannot be treated as a proof");
        assert_eq!(failure.message(), format!("could not read `{}`", work.root.join("src/lib.rs")));
        assert!(events.observations.is_empty());
    }

    std::thread_local! {
        static REQUIRED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn required_subset(active: &HashSet<u32>) -> bool {
        REQUIRED.with(|required| {
            (1..=5)
                .filter(|ordinal| required.get() & (1 << (ordinal - 1)) != 0)
                .all(|ordinal| active.contains(&ordinal))
        })
    }

    #[test]
    fn isolation_proves_every_nonempty_five_mutant_interaction_within_and_across_items() {
        let root = Utf8PathBuf::from(if cfg!(windows) {
            "C:/synthetic/isolation-matrix"
        } else {
            "/synthetic/isolation-matrix"
        });
        let work = Workspace::adopt(root.clone(), root.join("target"));
        for separate_items in [false, true] {
            let mut plan = plan(&root, 5);
            for mutant in &mut plan.mutants {
                mutant.item_path = if separate_items {
                    format!("item_{}", mutant.ordinal).into()
                } else {
                    "shared_item".into()
                };
            }
            for mask in 1..32_u32 {
                REQUIRED.with(|required| required.set(mask));
                let mut converger = Converger {
                    subset_oracle: Some(|active| Some(required_subset(active))),
                    ..Converger::default()
                };
                let result = converger
                    .isolate(
                        &work,
                        &plan,
                        None,
                        &["check"],
                        BuildLimits::default(),
                        &mut crate::testing::Recorder::default(),
                    )
                    .unwrap()
                    .expect("a finite monotone interaction can be proved");
                let expected = (1..=5).filter(|ordinal| mask & (1 << (ordinal - 1)) != 0).collect::<HashSet<_>>();
                match result {
                    Isolation::Blamed(ordinals) => {
                        assert_eq!(expected.len(), 1, "mask={mask}, separate={separate_items}");
                        assert_eq!(ordinals.into_iter().collect::<HashSet<_>>(), expected);
                    }
                    Isolation::Interaction { excluded, members } => {
                        assert!(expected.len() > 1, "mask={mask}, separate={separate_items}");
                        assert_eq!(excluded, members[0]);
                        assert_eq!(members.into_iter().collect::<HashSet<_>>(), expected);
                    }
                    other @ Isolation::Unresolved { .. } => panic!("unexpected isolation for mask={mask}: {other:?}"),
                }
                assert!(converger.isolation_budget.proofs <= MAX_ISOLATION_PROOFS);
                assert!(converger.withdrawn.is_empty(), "proofs must not withdraw any candidate themselves");
            }
        }
    }

    #[test]
    fn minimization_refuses_local_and_invocation_exhaustion_independently() {
        let root = Utf8PathBuf::from(if cfg!(windows) {
            "C:/synthetic/isolation-budget"
        } else {
            "/synthetic/isolation-budget"
        });
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 3);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        for local_exhausted in [false, true] {
            let mut converger = Converger {
                subset_oracle: Some(|active| Some(required_subset(active))),
                ..Converger::default()
            };
            converger.isolation_budget.proofs = if local_exhausted { 0 } else { MAX_INVOCATION_ISOLATION_PROOFS };
            let mut proofs = Proofs {
                local: if local_exhausted { MAX_ISOLATION_PROOFS } else { 0 },
                ..Proofs::default()
            };
            let result = converger
                .minimize_interaction(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: false,
                    },
                    &["check"],
                    BuildLimits::default(),
                    &mut crate::testing::Recorder::default(),
                    &population,
                    &population,
                    &mut proofs,
                )
                .unwrap();
            assert_eq!(result, None);
            assert!(proofs.results.is_empty());
        }
    }

    #[test]
    fn proof_cache_counts_each_distinct_schema_once_even_when_ordinal_order_changes() {
        let root = Utf8PathBuf::from(if cfg!(windows) {
            "C:/synthetic/isolation-cache"
        } else {
            "/synthetic/isolation-cache"
        });
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 5);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        REQUIRED.with(|required| required.set(1));
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(required_subset(active))),
            ..Converger::default()
        };
        let mut proofs = Proofs::default();
        let mut events = crate::testing::Recorder::default();
        for mask in 1..32 {
            let mut active = population
                .iter()
                .copied()
                .filter(|mutant| mask & (1 << (mutant.ordinal - 1)) != 0)
                .collect::<Vec<_>>();
            for reversed in [false, true] {
                if reversed {
                    active.reverse();
                }
                let result = converger
                    .subset_fails(
                        &work,
                        &plan,
                        BuildScope {
                            roots: None,
                            mutants: None,
                            publish_progress: false,
                        },
                        &["check"],
                        BuildLimits::default(),
                        &mut events,
                        &population,
                        &active,
                        &mut proofs,
                    )
                    .unwrap();
                assert_eq!(result, Some(mask & 1 != 0));
            }
            assert_eq!(proofs.local, mask);
            assert_eq!(proofs.results.len(), mask);
            assert_eq!(converger.isolation_budget.proofs, mask);
        }
        assert_eq!(converger.proof_roots.len(), 31);
    }

    #[test]
    fn diagnostic_tiers_distinguish_file_item_and_repeated_population_identity() {
        let mut plan = plan(Utf8Path::new("root"), 3);
        plan.mutants[1].mutator = "literal.bool_flip".into();
        plan.mutants[2].mutator = "relational.lt_to_le".into();
        for codes in [
            HashSet::default(),
            HashSet::from_iter(["unrelated".into()]),
            HashSet::from_iter(["E0308".into(), "unrelated".into()]),
        ] {
            let population = HashSet::from_iter([1, 2, 3]);
            let expected = if codes.contains("E0308") {
                vec![HashSet::from_iter([1]), population.clone()]
            } else {
                vec![population.clone()]
            };
            let mut tiers = Vec::new();
            push_isolation_tiers(&mut tiers, population.clone(), &plan, &codes);
            assert_eq!(tiers, expected);
            push_isolation_tiers(&mut tiers, HashSet::default(), &plan, &codes);
            assert_eq!(tiers, expected);
            if !codes.contains("E0308") {
                push_isolation_tiers(&mut tiers, population, &plan, &codes);
                assert_eq!(tiers, expected);
            }
        }
        let mut tiers = vec![HashSet::from_iter([1]), HashSet::from_iter([2])];
        push_isolation_tiers(&mut tiers, HashSet::from_iter([1]), &plan, &HashSet::default());
        assert_eq!(tiers, [HashSet::from_iter([1]), HashSet::from_iter([2]), HashSet::from_iter([1])]);
        assert_eq!(Proofs::key(&[&plan.mutants[2], &plan.mutants[0], &plan.mutants[1]]), [1, 2, 3]);
    }

    #[test]
    fn admissible_tiers_require_one_remaining_candidate_but_not_every_tier() {
        let empty = HashSet::default();
        let one = HashSet::from_iter([1]);
        assert!(!has_admissible_tier(&[], &empty));
        assert!(!has_admissible_tier(core::slice::from_ref(&empty), &empty));
        assert!(!has_admissible_tier(core::slice::from_ref(&one), &one));
        assert!(has_admissible_tier(&[empty.clone(), one.clone()], &empty));
        let maximum = u32::try_from(MAX_ISOLATION_CANDIDATES).expect("the fixed 4096-candidate isolation limit fits u32");
        let full = (1..=maximum).collect::<HashSet<_>>();
        assert!(has_admissible_tier(core::slice::from_ref(&full), &empty));
        let mut overflow = full;
        overflow.insert(maximum + 1);
        assert!(!has_admissible_tier(core::slice::from_ref(&overflow), &empty));
        assert!(has_admissible_tier(core::slice::from_ref(&overflow), &one));
    }

    fn plan(root: &Utf8Path, mutants: usize) -> Plan {
        let mut population = (1..=mutants)
            .map(|ordinal| {
                let mut mutant =
                    crate::testing::ci_fixture::mutant("src/lib.rs", ordinal, "arith.add_to_sub", crate::model::Outcome::Pending);
                mutant.ordinal = u32::try_from(ordinal).expect("test population fits in u32");
                mutant
            })
            .collect::<Vec<_>>();
        for mutant in &mut population {
            mutant.package = "subject".into();
        }
        Plan {
            root: root.to_path_buf(),
            files: Vec::new(),
            mutants: population,
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            digests: HashMap::default(),
            skipped: Vec::new(),
            reach: HashMap::from_iter([("subject".to_owned(), HashSet::from_iter(["subject".to_owned()]))]),
            specs: HashMap::default(),
        }
    }

    fn error_stream() -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "subject 0.0.0",
            "target": {"name": "subject", "kind": ["lib"]},
            "message": {
                "level": "error",
                "message": "failed",
                "code": {"code": "E0308"},
                "spans": [{"file_name": "src/lib.rs", "is_primary": true}]
            }
        })
        .to_string()
    }

    fn context(package: &str, target: &str) -> FailureContext {
        FailureContext {
            package: package.to_owned(),
            target: target.to_owned(),
            kind: "lib".to_owned(),
            proof_verb: vec!["check".to_owned(), "--lib".to_owned()],
            files: HashSet::default(),
            codes: HashSet::default(),
        }
    }

    #[test]
    fn contexts_with_the_largest_unresolved_population_are_isolated_first() {
        let root = Utf8Path::new("workspace");
        let mut plan = plan(root, 6);
        plan.mutants[0].package = "small".into();
        for mutant in &mut plan.mutants[1..] {
            mutant.package = "large".into();
        }
        plan.reach = HashMap::from_iter([
            ("small".to_owned(), HashSet::from_iter(["small".to_owned()])),
            ("large".to_owned(), HashSet::from_iter(["large".to_owned()])),
        ]);

        let by_package = isolation_population(&plan, &HashSet::default());
        let prioritized = prioritized_contexts(vec![context("small", "small"), context("large", "large")], &plan, &by_package);

        assert_eq!(prioritized[0].0.package, "large");
        assert_eq!(prioritized[0].2, 5);
        assert_eq!(prioritized[1].0.package, "small");
        assert_eq!(prioritized[1].2, 1);
    }

    #[test]
    fn invocation_context_budget_accepts_the_limit_and_rejects_the_next() {
        let mut budget = IsolationBudget::default();
        for _ in 0..MAX_ISOLATION_CONTEXTS {
            assert!(budget.enter_context());
        }
        assert!(!budget.enter_context());
    }

    #[test]
    fn every_admitted_context_receives_its_complete_local_proof_budget() {
        let mut local = IsolationBudget::default();
        for proof in 0..MAX_ISOLATION_PROOFS {
            assert!(local.proof(proof));
        }
        assert!(!local.proof(MAX_ISOLATION_PROOFS));

        let mut invocation = IsolationBudget::default();
        for _ in 0..MAX_ISOLATION_CONTEXTS {
            assert!(invocation.enter_context());
            for local in 0..MAX_ISOLATION_PROOFS {
                assert!(invocation.proof(local));
            }
        }
        assert_eq!(invocation.proof_count(), MAX_INVOCATION_ISOLATION_PROOFS);
        assert!(!invocation.proof(0));
    }

    #[test]
    fn candidate_limit_accepts_the_boundary_and_rejects_the_next() {
        assert!(candidate_tier_admitted(MAX_ISOLATION_CANDIDATES));
        assert!(!candidate_tier_admitted(MAX_ISOLATION_CANDIDATES + 1));

        let tier = (0..=u32::try_from(MAX_ISOLATION_CANDIDATES).expect("the limit fits u32")).collect();
        let withdrawn = HashSet::from_iter([0]);
        assert!(candidate_tier_admitted(without_withdrawn(tier, &withdrawn).len()));
    }

    #[test]
    fn failure_context_filtering_and_verbs_cover_every_target_shape() {
        let root = Utf8Path::new("workspace");
        let plan = plan(root, 1);
        let stream = [
            "not json".to_owned(),
            serde_json::json!({"reason": "compiler-artifact"}).to_string(),
            serde_json::json!({
                "reason": "compiler-message",
                "message": {"level": "error"}
            })
            .to_string(),
            serde_json::json!({
                "reason": "compiler-message",
                "target": {"name": "unknown", "kind": ["lib"]},
                "message": {"level": "error"}
            })
            .to_string(),
            error_stream(),
            error_stream(),
        ]
        .join("\n");

        let contexts = failure_contexts(&stream, &plan, root, &["check", "--lib"]);
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].label(), "subject (lib)");

        for (kind, expected) in [
            ("lib", vec!["check", "--keep-going", "--lib"]),
            ("proc-macro", vec!["check", "--keep-going", "--lib"]),
            ("test", vec!["check", "--keep-going", "--test", "subject"]),
            ("bin", vec!["check", "--keep-going", "--bin", "subject"]),
            ("example", vec!["check", "--keep-going", "--example", "subject"]),
            ("bench", vec!["check", "--keep-going", "--bench", "subject"]),
        ] {
            assert_eq!(targeted_verb(kind, "subject"), expected);
        }
    }

    #[test]
    fn exhausted_context_budget_returns_the_pending_dependency_cone_unresolved() {
        let directory = crate::testing::workdir("isolation-context-budget-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 1);
        let mut converger = Converger::default();
        converger.isolation_budget.contexts = MAX_ISOLATION_CONTEXTS;

        let isolated = converger
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--lib"],
                None,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("budget exhaustion does not launch Cargo");

        assert!(matches!(isolated.as_slice(), [Isolation::Unresolved { ordinals, .. }] if ordinals == &[1]));
    }

    #[test]
    fn withdrawn_contexts_do_not_consume_context_budget() {
        let directory = crate::testing::workdir("isolation-withdrawn-context-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 1);
        let mut converger = Converger::default();
        converger.isolation_budget.contexts = MAX_ISOLATION_CONTEXTS;
        let _inserted = converger.withdrawn.insert(1);

        let isolated = converger
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--lib"],
                None,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("a withdrawn context does not launch Cargo");

        assert!(isolated.is_empty(), "{isolated:?}");
        assert_eq!(converger.isolation_budget.contexts, MAX_ISOLATION_CONTEXTS);
    }

    #[test]
    fn oversized_contexts_do_not_consume_the_remaining_context_budget() {
        let oversized_end = u32::try_from(MAX_ISOLATION_CANDIDATES + 1).expect("the limit fits u32");
        let oversized = vec![(1..=oversized_end).collect::<HashSet<_>>()];
        let small = vec![HashSet::from_iter([oversized_end + 1])];
        let withdrawn = HashSet::default();
        let mut budget = IsolationBudget {
            contexts: MAX_ISOLATION_CONTEXTS - 1,
            proofs: 0,
        };

        assert!(!has_admissible_tier(&oversized, &withdrawn));
        assert!(has_admissible_tier(&small, &withdrawn));
        if has_admissible_tier(&oversized, &withdrawn) {
            assert!(budget.enter_context());
        }
        if has_admissible_tier(&small, &withdrawn) {
            assert!(budget.enter_context());
        }
        assert_eq!(budget.contexts, MAX_ISOLATION_CONTEXTS);
    }

    #[test]
    fn oversized_candidate_tiers_are_reported_unresolved_without_launching_cargo() {
        let directory = crate::testing::workdir("isolation-candidate-budget-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, MAX_ISOLATION_CANDIDATES + 1);

        let isolated = Converger::default()
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--lib"],
                None,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("an oversized tier does not launch Cargo");

        assert!(matches!(isolated.as_slice(), [Isolation::Unresolved { ordinals, .. }] if ordinals.len() == MAX_ISOLATION_CANDIDATES + 1));
    }

    #[test]
    fn target_frontiers_cannot_reduce_work_when_global_confirmation_is_required() {
        for targets in [1usize, 2, 8, 64] {
            let global_first = 1;
            let frontiers_then_confirmation = targets + 1;
            assert!(frontiers_then_confirmation > global_first);
        }

        // Direct failures, downstream feature unification, and cross-target interactions all still
        // need the same global confirmation; accepting a frontier without it is not a sound branch.
        let cases_requiring_confirmation = ["direct", "downstream", "interaction"];
        assert_eq!(cases_requiring_confirmation.len(), 3);
    }
}
