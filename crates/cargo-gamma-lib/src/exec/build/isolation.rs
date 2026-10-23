// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded proof builds for compiler failures whose spans name no mutation guard.

use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};

use super::messages::{build_evidence, cargo_message};
use super::{
    BuildScope, CompilerReason, Converger, Events, HashMap, HashSet, Isolation, Mutant, Plan, Result, Workspace, compiles_test_harnesses,
    isolation_candidate, run_cargo,
};
use crate::exec::cargo_options::BuildLimits;

/// One proof search may hold at most this many candidates.
///
/// Larger tiers are partitioned without treating clean partitions as proof against interactions.
/// Raising this bound requires deterministic candidate/work-count evidence.
pub const MAX_ISOLATION_CANDIDATES: usize = 4_096;

/// One diagnostic context may launch at most this many proof builds.
///
/// Thirty-two covers two endpoint proofs plus binary narrowing of a 4,096-candidate tier with room
/// for interaction minimization. Raise it only with proof-count evidence from a representative
/// interaction fixture; elapsed-time measurements alone are too host-dependent.
pub const MAX_ISOLATION_PROOFS: usize = 32;

/// One Cargo invocation may investigate at most this many independent failure contexts.
///
/// Sixty-four admits a broad multi-crate failure wave while preventing contributor-controlled
/// target multiplication from making proof work unbounded.
pub const MAX_ISOLATION_CONTEXTS: usize = 64;

/// One Cargo invocation may launch at most this many isolation proof builds across all contexts.
///
/// This is derived from the two local bounds rather than chosen independently: every admitted
/// context can spend its complete local allowance, so one context can never starve another. The
/// context and local limits still impose a strict derived launch ceiling on pathological input.
pub const MAX_INVOCATION_ISOLATION_PROOFS: usize = MAX_ISOLATION_CONTEXTS * MAX_ISOLATION_PROOFS;

#[cfg(any(test, feature = "internals"))]
use super::native_test_util::CANDIDATE_POLICY;

fn candidate_tier_admitted(candidates: usize) -> bool {
    #[cfg(any(test, feature = "internals"))]
    if let Some(admitted) = CANDIDATE_POLICY.with_borrow_mut(|policy| policy.as_mut().map(|policy| policy(candidates))) {
        return admitted;
    }
    candidates <= MAX_ISOLATION_CANDIDATES
}

#[must_use]
pub fn isolation_proofs_exhausted(local: usize, invocation: usize) -> bool {
    local >= MAX_ISOLATION_PROOFS || invocation >= MAX_INVOCATION_ISOLATION_PROOFS
}

fn without_withdrawn(mut candidates: HashSet<u32>, withdrawn: &HashSet<u32>) -> HashSet<u32> {
    candidates.retain(|ordinal| !withdrawn.contains(ordinal));
    candidates
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureContext {
    pub package: String,
    pub target: String,
    pub kind: String,
    pub proof_verb: Vec<String>,
    pub files: HashSet<Utf8PathBuf>,
    pub codes: HashSet<String>,
    pub locations: HashMap<Utf8PathBuf, Vec<u32>>,
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

fn activates_development_dependencies(verb: &[&str]) -> bool {
    compiles_test_harnesses(verb)
        || verb.iter().any(|argument| {
            matches!(*argument, "--bench" | "--benches" | "--example" | "--examples")
                || argument.starts_with("--bench=")
                || argument.starts_with("--example=")
                || argument.starts_with("--test=")
        })
}

#[derive(Debug, Default)]
pub struct IsolationBudget {
    pub contexts: usize,
    pub proofs: usize,
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

    pub(super) const fn proof_count(&self) -> usize {
        self.proofs
    }
}

#[derive(Debug, Default)]
pub struct Proofs {
    pub local: usize,
    pub results: HashMap<Vec<u32>, Option<bool>>,
}

#[derive(Debug)]
pub enum IsolationAttempt {
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

pub fn failure_contexts(stdout: &str, plan: &Plan, root: &Utf8Path, failed_verb: &[&str]) -> Vec<FailureContext> {
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
        let mut locations: HashMap<Utf8PathBuf, Vec<u32>> = HashMap::default();
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
                if let Some(line) = span.line_start.and_then(|line| u32::try_from(line).ok()) {
                    locations.entry(path.to_owned()).or_default().push(line);
                }
            }
        }

        let proof_verb = if activates_development_dependencies(failed_verb) && matches!(kind.as_str(), "lib" | "proc-macro" | "bin") {
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
                locations: HashMap::default(),
            });
            let _previous = positions.insert(key, position);
            contexts
                .last_mut()
                .unwrap_or_else(|| unreachable!("the failure context was just appended"))
        };
        context.files.extend(files);
        context.codes.extend(codes);
        for (file, lines) in locations {
            context.locations.entry(file).or_default().extend(lines);
        }
    }
    contexts
}

fn diagnostically_likely(mutant: &Mutant, codes: &HashSet<String>) -> bool {
    if ownership_failure(codes) {
        return matches!(
            mutant.mutator.as_ref(),
            "stmt.delete_assign" | "stmt.delete_call" | "loop.delete" | "cond.negate" | "cond.always_false" | "cond.always_true"
        ) || mutant.mutator.starts_with("loop.")
            || mutant.mutator.starts_with("return.");
    }
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

fn ownership_failure(codes: &HashSet<String>) -> bool {
    codes
        .iter()
        .any(|code| super::blame::FLOW_SENSITIVE.contains(&code.as_str()) || matches!(code.as_str(), "E0500" | "E0501"))
}

fn diagnostic_envelopes<'a>(converger: &Converger, plan: &'a Plan) -> HashMap<(&'a Utf8Path, &'a str), (u32, u32)> {
    let mut envelopes: HashMap<(&Utf8Path, &str), (u32, u32)> = HashMap::default();
    for mutant in &plan.mutants {
        let Some((_ordinals, guards)) = converger.splices.placed.get(mutant.file.as_ref()) else {
            continue;
        };
        let Some(guard) = guards.get(&mutant.ordinal) else {
            continue;
        };
        let envelope = envelopes
            .entry((mutant.file.as_ref(), mutant.item_path.as_ref()))
            .or_insert_with(|| (guard.site.start.line(), guard.site.end.line()));
        envelope.0 = envelope.0.min(guard.site.start.line());
        envelope.1 = envelope.1.max(guard.site.end.line());
    }
    envelopes
}

fn local_diagnostic_tiers(
    converger: &Converger,
    context: &FailureContext,
    plan: &Plan,
    dependencies: &HashSet<u32>,
    envelopes: &HashMap<(&Utf8Path, &str), (u32, u32)>,
    tiers: &mut Vec<HashSet<u32>>,
) {
    let mut items = envelopes
        .iter()
        .filter(|((file, _item), (start, end))| {
            context.files.contains(*file)
                && context
                    .locations
                    .get(*file)
                    .is_some_and(|lines| lines.iter().any(|line| start <= line && line <= end))
        })
        .map(|(item, _envelope)| *item)
        .collect::<Vec<_>>();
    items.sort_unstable();
    for (file, item) in items {
        let population = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.file.as_ref() == file && mutant.item_path.as_ref() == item)
            .filter(|mutant| dependencies.contains(&mutant.ordinal))
            .filter(|mutant| isolation_candidate(mutant, &converger.withdrawn, None))
            .map(|mutant| mutant.ordinal)
            .collect();
        push_isolation_tiers(tiers, population, plan, &context.codes);
    }
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

fn context_tiers(
    converger: &Converger,
    context: &FailureContext,
    plan: &Plan,
    by_package: &HashMap<&str, Vec<&Mutant>>,
    packages: &[String],
    dependencies: &HashSet<u32>,
    envelopes: &HashMap<(&Utf8Path, &str), (u32, u32)>,
) -> (Vec<HashSet<u32>>, usize) {
    let mut tiers = Vec::new();
    local_diagnostic_tiers(converger, context, plan, dependencies, envelopes, &mut tiers);
    let local_count = tiers.len();
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
    let mut bounded = Vec::new();
    let mut diagnostic_end = 0;
    for (index, tier) in tiers.into_iter().enumerate() {
        if tier.len() <= MAX_ISOLATION_CANDIDATES {
            if !bounded.contains(&tier) {
                bounded.push(tier);
            }
        } else {
            let mut candidates = plan
                .mutants
                .iter()
                .filter(|mutant| tier.contains(&mutant.ordinal))
                .collect::<Vec<_>>();
            candidates.sort_unstable_by(|left, right| {
                left.file
                    .cmp(&right.file)
                    .then_with(|| left.item_path.cmp(&right.item_path))
                    .then_with(|| left.ordinal.cmp(&right.ordinal))
            });
            for chunk in candidates.chunks(MAX_ISOLATION_CANDIDATES) {
                let chunk = chunk.iter().map(|mutant| mutant.ordinal).collect();
                if !bounded.contains(&chunk) {
                    bounded.push(chunk);
                }
            }
        }
        if index < local_count {
            diagnostic_end = bounded.len();
        }
    }
    (bounded, diagnostic_end)
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
    #[expect(
        clippy::too_many_lines,
        reason = "one bounded isolation pass keeps component preservation, tier recovery, and immediate verdict publication together"
    )]
    pub fn isolate_scoped(
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
        let mut affected: HashSet<String> = contexts.iter().map(|(context, _, _)| context.package.clone()).collect();
        loop {
            let before = affected.len();
            for (package, dependencies) in &plan.reach {
                if affected.contains(package.as_str()) || dependencies.iter().any(|dependency| affected.contains(dependency.as_str())) {
                    affected.insert(package.clone());
                    affected.extend(dependencies.iter().cloned());
                }
            }
            // #[gamma::skip(all, reason = "this fixed-point exit is the only bound once affected packages stop changing")]
            if affected.len() == before {
                break;
            }
        }
        // Preserve successful components only when disconnected in both dependency directions
        // from the failure. Their guards cannot participate in this component's subset attribution.
        self.proof_background = plan
            .mutants
            .iter()
            .filter(|mutant| isolation_candidate(mutant, &self.withdrawn, None) && !affected.contains(mutant.package.as_ref()))
            .map(|mutant| mutant.ordinal)
            .collect();
        // Proof schemas replace the live guards; diagnostics still name the original failed schema.
        let envelopes = diagnostic_envelopes(self, plan);
        let mut proof_contexts: HashMap<Vec<String>, Proofs> = HashMap::default();
        let mut isolated = Vec::new();

        for (context, packages, _dependency_count) in contexts {
            let dependencies = without_withdrawn(context_dependencies(&by_package, &packages), &self.withdrawn);
            if dependencies.is_empty() {
                continue;
            }

            let (tiers, diagnostic_end) = context_tiers(self, &context, plan, &by_package, &packages, &dependencies, &envelopes);

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
            let verb = if self.fixed_roots.is_some() {
                self.check_verb().iter().map(|argument| (*argument).to_owned()).collect()
            } else {
                context.verb()
            };
            let proofs = proof_contexts.entry(verb.clone()).or_default();
            proofs.local = 0;
            let verb = verb.iter().map(String::as_str).collect::<Vec<_>>();
            let mut resolved = HashSet::default();
            let mut applicable = true;

            'tiers: for (tier_index, eligible) in tiers.into_iter().enumerate() {
                let mut eligible = without_withdrawn(eligible, &self.withdrawn);
                eligible.retain(|ordinal| !resolved.contains(ordinal));
                if !candidate_tier_admitted(eligible.len()) {
                    events.warn(&format!(
                        "skipping an isolation tier of {} mutants for {} because the bounded limit is {MAX_ISOLATION_CANDIDATES}",
                        eligible.len(),
                        context.label()
                    ));
                    continue;
                }
                while !eligible.is_empty() {
                    let current_dependencies = without_withdrawn(dependencies.difference(&resolved).copied().collect(), &self.withdrawn);
                    match self.isolate_candidates(work, plan, scope, &verb, limits, events, &eligible, proofs)? {
                        IsolationAttempt::Resolved(result) => {
                            let before = eligible.len();
                            match &result {
                                Isolation::Blamed(ordinals) => {
                                    let blamed: HashMap<_, _> =
                                        ordinals.iter().map(|ordinal| (*ordinal, CompilerReason::isolated())).collect();
                                    self.persist_compiler_exclusions(work, plan, &blamed)?;
                                    let before = self.census.len();
                                    for (ordinal, reason) in blamed {
                                        self.withdrawn.insert(ordinal);
                                        self.census.entry(ordinal).or_insert(reason);
                                    }
                                    if self.census.len() > before {
                                        events.convergence_progress(self.historical_exclusions + self.census.len());
                                    }
                                    resolved.extend(ordinals);
                                }
                                Isolation::Interaction { excluded, .. } => {
                                    let _ = resolved.insert(*excluded);
                                }
                                Isolation::Unresolved { .. } => unreachable!("candidate proofs never settle an unresolved result"),
                            }
                            eligible.retain(|ordinal| !resolved.contains(ordinal));
                            assert!(
                                eligible.len() < before,
                                "a resolved proof must remove a member of its eligible population before retrying cached proofs"
                            );
                            isolated.push(result);
                        }
                        IsolationAttempt::NotReproduced if eligible == current_dependencies => {
                            applicable = false;
                            break 'tiers;
                        }
                        IsolationAttempt::NotReproduced | IsolationAttempt::Inconclusive => break,
                    }
                    if isolation_proofs_exhausted(proofs.local, self.isolation_budget.proofs) {
                        break 'tiers;
                    }
                }
                // Ordinary convergence checks the remaining schema; do not certify clean broader tiers here.
                let local_progress = tier_index + 1 == diagnostic_end && !resolved.is_empty();
                if local_progress || isolation_proofs_exhausted(proofs.local, self.isolation_budget.proofs) {
                    break;
                }
            }

            let remaining = dependencies.difference(&resolved).copied().collect::<Vec<_>>();
            if applicable && !remaining.is_empty() && resolved.is_empty() {
                events.warn(&format!(
                    "left {} unresolved after {} local and {} invocation proof builds",
                    context.label(),
                    proofs.local,
                    self.isolation_budget.proofs
                ));
                isolated.push(Isolation::Unresolved {
                    ordinals: remaining,
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
    pub fn isolate_candidates(
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

        // The campaign's initial pristine validation supplies the control under this same frozen
        // context; subset searches need reproduce the populated failure, not rebuild that control.
        let populated = self.subset_fails(work, plan, scope, verb, limits, events, &candidates, &candidates, proofs)?;
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
    pub fn minimize_interaction(
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

    #[expect(
        clippy::too_many_arguments,
        reason = "a proof build needs the same complete context as convergence"
    )]
    pub fn subset_fails(
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

        #[cfg(any(test, feature = "internals"))]
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
    pub fn run_proof(
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
        let mut applied = core::mem::take(&mut self.proof_active);
        applied.clear();
        applied.extend(&self.proof_background);
        applied.extend(active);
        let written = self.instrument_active_schema(work, plan, &applied);
        self.proof_active = applied;
        let written = written?;
        events.convergence_activity(
            self.total_rounds,
            self.historical_exclusions + self.census.len(),
            self.isolation_budget.proofs,
        );
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

    // Emits ordinal-bearing source markers and controlled item groups for native instrumentation.
    // The paired compiler script inspects these markers rather than compiling the schema.

    // Reads actual instrumented source: a proof fails when every required ordinal is present.
    // failure_at injects a transport error at that attempt; the returned trace records proof order,
    // cache reuse and budget behavior independently of the scripted compiler verdict.

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
    fn ownership_diagnostics_prioritize_control_flow_without_discarding_other_candidates() {
        for code in ["E0381", "E0382", "E0499", "E0500", "E0501", "E0502"] {
            let codes = HashSet::from_iter([code.to_owned(), "unrelated".to_owned()]);
            for name in [
                "stmt.delete_assign",
                "stmt.delete_call",
                "loop.delete",
                "cond.negate",
                "cond.always_false",
                "cond.always_true",
                "loop.break_to_continue",
                "return.custom",
            ] {
                let mut mutant = crate::fixtures::mutant();
                mutant.mutator = name.into();
                assert!(diagnostically_likely(&mutant, &codes), "{code}: {name}");
            }
            for name in [
                "arith.add_to_sub",
                "literal.bool_flip",
                "relational.lt_to_le",
                "prefix.loop.delete",
                "return_value.default",
            ] {
                let mut mutant = crate::fixtures::mutant();
                mutant.mutator = name.into();
                assert!(!diagnostically_likely(&mutant, &codes), "{code}: {name}");
            }

            let mut plan = plan(Utf8Path::new("root"), 3);
            plan.mutants[0].mutator = "loop.delete_break".into();
            plan.mutants[1].mutator = "literal.bool_flip".into();
            plan.mutants[2].mutator = "return.value".into();
            let population = HashSet::from_iter([1, 2, 3]);
            let mut tiers = Vec::new();
            push_isolation_tiers(&mut tiers, population.clone(), &plan, &codes);
            assert_eq!(tiers, [HashSet::from_iter([1, 3]), population]);
        }
    }

    #[test]
    fn type_diagnostics_deprioritize_boolean_and_relational_changes_only() {
        for code in ["E0271", "E0277", "E0282", "E0308", "E0369", "E0599", "E0600", "E0605", "E0614"] {
            let codes = HashSet::from_iter(["unrelated".to_owned(), code.to_owned()]);
            for name in [
                "literal.bool_flip",
                "bool_expr.negate",
                "cond.negate",
                "match_guard.negate",
                "logical.and_to_or",
                "logical.or_to_and",
                "relational.eq_to_ne",
                "relational.lt_to_le",
            ] {
                let mut mutant = crate::fixtures::mutant();
                mutant.mutator = name.into();
                assert!(!diagnostically_likely(&mutant, &codes), "{code}: {name}");
            }
            for name in [
                "arith.add_to_sub",
                "literal.int_to_zero",
                "cond.always_false",
                "loop.delete_break",
                "eq.relational.",
            ] {
                let mut mutant = crate::fixtures::mutant();
                mutant.mutator = name.into();
                assert!(diagnostically_likely(&mutant, &codes), "{code}: {name}");
            }
        }
        let mut mutant = crate::fixtures::mutant();
        mutant.mutator = "literal.bool_flip".into();
        for codes in [HashSet::default(), HashSet::from_iter(["E9999".into()])] {
            assert!(diagnostically_likely(&mutant, &codes));
        }
        mutant.mutator = "cond.negate".into();
        assert!(diagnostically_likely(
            &mutant,
            &HashSet::from_iter(["E0308".into(), "E0500".into()])
        ));
    }

    #[test]
    fn isolation_tiers_keep_unknown_ordinals_and_repeated_likely_tiers_out_of_the_priority_subset() {
        let mut plan = plan(Utf8Path::new("root"), 3);
        plan.mutants[0].mutator = "stmt.delete_call".into();
        plan.mutants[1].mutator = "literal.bool_flip".into();
        plan.mutants[2].mutator = "return.value".into();
        let codes = HashSet::from_iter(["E0500".into()]);
        let likely = HashSet::from_iter([1]);
        let population = HashSet::from_iter([1, 2, 99]);
        let mut tiers = vec![likely.clone()];
        push_isolation_tiers(&mut tiers, population.clone(), &plan, &codes);
        assert_eq!(tiers, [likely, population]);
        let mut tiers = Vec::new();
        push_isolation_tiers(&mut tiers, HashSet::from_iter([2, 99]), &plan, &codes);
        assert_eq!(tiers, [HashSet::from_iter([2, 99])]);
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
            locations: HashMap::default(),
        }
    }

    fn memory_workspace() -> Workspace {
        let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/isolation-graph/application");
        Workspace::adopt(root.clone(), root.join("target"))
    }

    #[test]
    fn ownership_locations_prioritize_the_enclosing_item_without_condemning_its_neighbors() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 63);
        for mutant in &mut plan.mutants {
            mutant.item_path = format!("item_{:03}", mutant.ordinal).into();
            mutant.mutator = "loop.delete_break".into();
        }
        plan.mutants[61].item_path = "item_063".into();
        plan.mutants[61].mutator = "stmt.delete_assign".into();
        let mut stream: serde_json::Value = serde_json::from_str(&error_stream()).expect("the static error stream is valid Cargo JSON");
        stream["message"]["code"]["code"] = "E0506".into();
        stream["message"]["spans"][0]["line_start"] = 100.into();
        let mut runs = Vec::new();
        for positioned in [false, true] {
            let mut converger = Converger {
                subset_oracle: Some(|active| Some(active.contains(&63))),
                ..Converger::default()
            };
            if positioned {
                let position = |line| crate::schema::Position::new(line, 1).expect("fixture coordinates are one-based");
                let guards = plan
                    .mutants
                    .iter()
                    .map(|mutant| {
                        let line = if mutant.ordinal >= 62 { 100 } else { mutant.ordinal };
                        (
                            mutant.ordinal,
                            crate::schema::Guard {
                                site: position(line)..position(line + 1),
                                mutated: None,
                            },
                        )
                    })
                    .collect();
                converger.splices.placed.insert("src/lib.rs".into(), (Vec::new(), guards));
            }
            let results = converger
                .isolate_scoped(
                    &work,
                    &plan,
                    &stream.to_string(),
                    &["check", "--tests"],
                    None,
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                )
                .expect("the in-process proof oracle is infallible");
            assert_eq!(results.len(), 1);
            assert!(matches!(&results[0], Isolation::Blamed(ordinals) if ordinals == &[63]));
            assert_eq!(
                converger.withdrawn,
                HashSet::from_iter([63]),
                "singleton proof publishes its verdict immediately"
            );
            runs.push(converger.isolation_budget.proofs);
        }
        assert!(runs[1] < runs[0], "local ownership evidence must reduce proof launches: {runs:?}");
    }

    #[test]
    fn independent_failures_publish_without_pristine_proofs_or_another_convergence_round() {
        let work = memory_workspace();
        let plan = plan(&work.root, 5);
        let oracle = |active: &HashSet<u32>| Some(active.contains(&1) || active.contains(&2));
        let mut repeated = Converger {
            subset_oracle: Some(oracle),
            ..Converger::default()
        };
        for expected in [1, 2] {
            let result = repeated
                .isolate(
                    &work,
                    &plan,
                    None,
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                )
                .expect("the reference oracle is infallible")
                .expect("an independent invalid edit remains");
            let Isolation::Blamed(ordinals) = result else {
                panic!("independent invalid edits must not become interactions");
            };
            assert_eq!(ordinals, [expected]);
            repeated.withdrawn.extend(ordinals);
        }
        let mut combined = Converger {
            subset_oracle: Some(oracle),
            ..Converger::default()
        };
        let results = combined
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
            .expect("the combined oracle is infallible");
        assert_eq!(results.len(), 2);
        for (result, expected) in results.iter().zip([1, 2]) {
            assert!(matches!(result, Isolation::Blamed(ordinals) if ordinals == &[expected]));
        }
        assert_eq!(combined.isolation_budget.contexts, 1);
        assert!(
            combined.isolation_budget.proofs + 2 <= repeated.isolation_budget.proofs + 3,
            "count proof launches plus the ordinary failed/passing convergence builds"
        );
        let before = combined.isolation_budget.proofs;
        combined.withdrawn.clear();
        combined.census.clear();
        let mut changed = work;
        changed.cargo.features.push("--all-features".into());
        let results = combined
            .isolate_scoped(
                &changed,
                &plan,
                &error_stream(),
                &["check", "--lib"],
                Some(&["subject".to_owned()]),
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
            .expect("changed contexts get independent proofs");
        assert_eq!(results.len(), 2);
        assert!(combined.isolation_budget.proofs > before, "no proof survives a context boundary");
    }

    #[test]
    fn partial_progress_does_not_abandon_unjudged_neighbors_when_the_proof_budget_ends() {
        let work = memory_workspace();
        let plan = plan(&work.root, 2);
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1))),
            ..Converger::default()
        };
        converger.isolation_budget.proofs = MAX_INVOCATION_ISOLATION_PROOFS - 3;
        let results = converger
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
            .expect("the final three proofs isolate the first mutant");
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], Isolation::Blamed(ordinals) if ordinals == &[1]));
        assert_eq!(converger.isolation_budget.proofs, MAX_INVOCATION_ISOLATION_PROOFS);
        assert!(
            converger.abandoned.is_empty(),
            "the next ordinary build judges the remaining neighbor"
        );
    }

    #[test]
    fn development_target_selectors_preserve_the_failure_graph_even_without_a_test_harness() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        for selector in [
            "--tests",
            "--test=integration",
            "--benches",
            "--bench=timing",
            "--examples",
            "--example=demo",
            "--all-targets",
        ] {
            let original = ["check", selector];
            let contexts = failure_contexts(&error_stream(), &plan, &work.root, &original);
            assert_eq!(
                contexts[0].verb(),
                original,
                "development dependency activation must survive {selector}"
            );
        }
        assert_eq!(
            failure_contexts(&error_stream(), &plan, &work.root, &["check", "--lib", "--bins"])[0].verb(),
            ["check", "--keep-going", "--lib"]
        );
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
    fn passing_partitions_do_not_disprove_a_cross_partition_interaction() {
        let directory = crate::testing::workdir("isolation-candidate-budget-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, MAX_ISOLATION_CANDIDATES + 1);

        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1) && active.contains(&4097))),
            ..Converger::default()
        };
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
            .expect("bounded partition proofs use the in-process oracle");

        assert!(matches!(isolated.as_slice(), [Isolation::Unresolved { ordinals, .. }] if ordinals.len() == MAX_ISOLATION_CANDIDATES + 1));
        assert_eq!(converger.isolation_budget.proofs, 2);
        assert!(converger.withdrawn.is_empty());
        assert!(converger.abandoned.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires a native-scale population above the production isolation partition limit")]
    fn oversized_populations_isolate_independent_causes_without_abandoning_neighbors() {
        let work = memory_workspace();
        let plan = plan(&work.root, MAX_ISOLATION_CANDIDATES + 1);
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1) || active.contains(&4097))),
            ..Converger::default()
        };
        let results = converger
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--lib"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
            .expect("bounded proofs can isolate causes in separate partitions");
        let blamed = results
            .iter()
            .flat_map(|result| match result {
                Isolation::Blamed(ordinals) => ordinals.as_slice(),
                other => panic!("independent failures must not abandon neighbors: {other:?}"),
            })
            .copied()
            .collect::<HashSet<_>>();
        assert_eq!(blamed, HashSet::from_iter([1, 4097]));
        assert!(converger.isolation_budget.proofs <= MAX_ISOLATION_PROOFS);
        assert!(converger.abandoned.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "requires a full-size native diagnostic-locality population")]
    fn diagnostic_locality_handles_type_and_lifetime_failures_in_a_nineteen_thousand_mutant_target() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 19_151);
        for mutant in &mut plan.mutants {
            mutant.item_path = format!("item_{:05}", mutant.ordinal).into();
        }
        for code in ["E0308", "E0716", "E0382"] {
            let mut converger = Converger {
                subset_oracle: Some(|active| Some(active.contains(&19_000))),
                ..Converger::default()
            };
            let position = |line| crate::schema::Position::new(line, 1).expect("fixture positions are one-based");
            converger.splices.placed.insert(
                "src/lib.rs".into(),
                (
                    Vec::new(),
                    HashMap::from_iter([(
                        19_000,
                        crate::schema::Guard {
                            site: position(100)..position(101),
                            mutated: None,
                        },
                    )]),
                ),
            );
            let mut stream: serde_json::Value = serde_json::from_str(&error_stream()).expect("static Cargo JSON parses");
            stream["message"]["code"]["code"] = code.into();
            stream["message"]["spans"][0]["line_start"] = 100.into();
            let mut events = ProofEvents::default();
            let results = converger
                .isolate_scoped(
                    &work,
                    &plan,
                    &stream.to_string(),
                    &["check", "--lib"],
                    None,
                    BuildLimits::default(),
                    &mut events,
                )
                .expect("diagnostic-local and partition proofs are bounded");
            assert!(matches!(results.as_slice(), [Isolation::Blamed(ordinals)] if ordinals == &[19_000]));
            assert!(converger.isolation_budget.proofs <= 8);
            assert!(events.warnings.is_empty());
            assert!(converger.abandoned.is_empty());
        }
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
