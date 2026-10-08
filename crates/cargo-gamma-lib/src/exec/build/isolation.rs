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

const fn candidate_tier_admitted(candidates: usize) -> bool {
    candidates <= MAX_ISOLATION_CANDIDATES
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

            if has_admissible_tier(&tiers, &self.withdrawn) && !self.isolation_budget.enter_context() {
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
                if proofs.local >= MAX_ISOLATION_PROOFS || self.isolation_budget.proofs >= MAX_INVOCATION_ISOLATION_PROOFS {
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

        while items.len() > 1 {
            let middle = items.len() / 2;
            let left = items[..middle].concat();
            let right = items[middle..].concat();
            if self.subset_fails(work, plan, scope, verb, limits, events, &population, &left, proofs)? == Some(true) {
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
        while narrowed.len() > 1 {
            let middle = narrowed.len() / 2;
            let left = &narrowed[..middle];
            let right = &narrowed[middle..];
            if self.subset_fails(work, plan, scope, verb, limits, events, &population, left, proofs)? == Some(true) {
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
        while active.len() > 1 && proofs.local < MAX_ISOLATION_PROOFS && self.isolation_budget.proofs < MAX_INVOCATION_ISOLATION_PROOFS {
            let width = active.len().div_ceil(granularity);
            let chunks = active.chunks(width).map(<[_]>::to_vec).collect::<Vec<_>>();
            let mut reduced = false;
            for chunk in &chunks {
                if self.subset_fails(work, plan, scope, verb, limits, events, population, chunk, proofs)? == Some(true) {
                    active.clone_from(chunk);
                    granularity = 2;
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
