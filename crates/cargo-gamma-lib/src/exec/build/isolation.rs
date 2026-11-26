// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded proof builds for compiler failures whose spans name no mutation guard.

use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};

use super::messages::{build_evidence, cargo_message};
use super::{
    BuildScope, CompilerReason, Converger, Events, HashMap, HashSet, Isolation, Mutant, Plan, Result, Workspace, blame,
    compiles_test_harnesses, isolation_candidate, retain_blamed,
};
use crate::error::error;
use crate::exec::cargo_options::BuildLimits;

pub(super) mod proof_cache;

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
    pub attributed: HashMap<u32, CompilerReason>,
    pub pending: Vec<Vec<u32>>,
    pub reuse_generation: Option<[u8; 32]>,
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

    fn defer(&mut self, active: &[&Mutant]) {
        let key = Self::key(active);
        if !key.is_empty() && !self.pending.contains(&key) {
            self.pending.push(key);
        }
    }

    fn next_partition(
        &mut self,
        eligible: &HashSet<u32>,
        withdrawn: &HashSet<u32>,
        mut cost: impl FnMut(&[u32]) -> (usize, usize, bool),
    ) -> Option<Vec<u32>> {
        for partition in &mut self.pending {
            partition.retain(|ordinal| !withdrawn.contains(ordinal));
        }
        self.pending.retain(|partition| !partition.is_empty());
        let selected = self
            .pending
            .iter()
            .enumerate()
            .filter(|(_, partition)| partition.iter().all(|ordinal| eligible.contains(ordinal)))
            .min_by_key(|(index, partition)| (cost(partition), *index))
            .map(|(index, _)| index)?;
        Some(self.pending.remove(selected))
    }

    fn attributed_attempt(&self) -> Option<IsolationAttempt> {
        if self.attributed.is_empty() {
            return None;
        }
        let mut ordinals = self.attributed.keys().copied().collect::<Vec<_>>();
        ordinals.sort_unstable();
        Some(IsolationAttempt::Resolved(Isolation::Blamed(ordinals)))
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

fn may_have_nonlocal_effects(mutant: &Mutant) -> bool {
    let mutator = mutant.mutator.as_ref();
    mutator.starts_with("fn_value.")
        || mutator.starts_with("stmt.")
        || mutator.starts_with("loop.")
        || matches!(
            mutator,
            "parameter.default_shadow"
                | "cond.always_true"
                | "cond.always_false"
                | "match_guard.always_true"
                | "match_guard.always_false"
                | "match_arm.never_matches"
                | "return_value.default"
                | "try.propagate_to_unwrap"
        )
}

struct ProofMetadata<'a> {
    file: &'a Utf8Path,
    package: &'a str,
    nonlocal: bool,
}

struct ProofCostIndex<'a> {
    mutants: HashMap<u32, Vec<ProofMetadata<'a>>>,
    dependents: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> ProofCostIndex<'a> {
    fn new(plan: &'a Plan) -> Self {
        let mut mutants: HashMap<_, Vec<_>> = HashMap::default();
        for mutant in &plan.mutants {
            #[cfg(test)]
            tests::cost_work(|work| work.index_mutants += 1);
            // Historical exclusions can share ordinal zero; live ordinals need only one slot.
            mutants
                .entry(mutant.ordinal)
                // #[gamma::skip(literal.int_increment, literal.int_decrement, tag = "equivalent", reason = "Vec capacity changes allocation only, not the stored proof metadata.")]
                .or_insert_with(|| Vec::with_capacity(1))
                .push(ProofMetadata {
                    file: mutant.file.as_ref(),
                    package: mutant.package.as_ref(),
                    nonlocal: may_have_nonlocal_effects(mutant),
                });
        }
        let mut dependents: HashMap<_, Vec<_>> = HashMap::default();
        for (package, dependencies) in &plan.reach {
            #[cfg(test)]
            tests::cost_work(|work| work.index_packages += 1);
            for dependency in dependencies {
                #[cfg(test)]
                tests::cost_work(|work| work.index_dependencies += 1);
                dependents.entry(dependency.as_str()).or_default().push(package.as_str());
            }
        }
        Self { mutants, dependents }
    }

    fn cost(&self, previous: &HashSet<u32>, background: &HashSet<u32>, active: &[u32]) -> (usize, usize, bool) {
        let active = active.iter().copied().collect::<HashSet<_>>();
        let mut nonlocal = false;
        for ordinal in &active {
            #[cfg(test)]
            tests::cost_work(|work| work.ordinal_lookups += 1);
            if let Some(metadata) = self.mutants.get(ordinal) {
                for mutant in metadata {
                    #[cfg(test)]
                    tests::cost_work(|work| work.metadata += 1);
                    nonlocal |= mutant.nonlocal;
                }
            }
        }
        let mut files = HashSet::default();
        let mut packages = HashSet::default();
        for ordinal in previous
            .symmetric_difference(&active)
            .filter(|ordinal| !background.contains(ordinal))
        {
            #[cfg(test)]
            tests::cost_work(|work| work.ordinal_lookups += 1);
            if let Some(metadata) = self.mutants.get(ordinal) {
                for mutant in metadata {
                    #[cfg(test)]
                    tests::cost_work(|work| work.metadata += 1);
                    let _ = files.insert(mutant.file);
                    let _ = packages.insert(mutant.package);
                }
            }
        }
        let mut rebuilt = packages.clone();
        for package in packages {
            if let Some(dependents) = self.dependents.get(package) {
                for dependent in dependents {
                    #[cfg(test)]
                    tests::cost_work(|work| work.dependent_entries += 1);
                    let _ = rebuilt.insert(*dependent);
                }
            }
        }
        // Prefer nonlocal partitions when rebuild and changed-file costs tie.
        (rebuilt.len(), files.len(), !nonlocal)
    }
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
    if !likely.is_empty() && tiers.last() != Some(&likely) {
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
        if
        // #[gamma::skip(cond.always_false, reason = "All tier members originate from eligible plan mutants. For a nonempty tier<=4096, forcing the large-tier branch simply sorts these same members and creates one chunk containing exactly the same HashSet. Deduplication and diagnostic_end update are identical; tiers>4096 already take that branch.", tag = "equivalent")]
        // #[gamma::skip(relational.le_to_lt, reason = "Only boundary size==MAX_ISOLATION_CANDIDATES changes branches. The else branch sorts those same plan-owned candidates and chunks into exactly one HashSet containing the identical population; deduplication and diagnostic_end stay identical.", tag = "equivalent")]
        tier.len() <= MAX_ISOLATION_CANDIDATES {
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

fn extend_affected_packages(affected: &mut HashSet<String>, reach: &HashMap<String, HashSet<String>>) {
    loop {
        let before = affected.len();
        for (package, dependencies) in reach {
            if affected.contains(package.as_str()) || dependencies.iter().any(|dependency| affected.contains(dependency.as_str())) {
                affected.insert(package.clone());
                affected.extend(dependencies.iter().cloned());
            }
        }
        // #[gamma::skip(all, tag = "timeout", reason = "Without the fixed-point exit, isolation repeatedly scans the unchanged affected-package closure forever.")]
        if affected.len() == before {
            break;
        }
    }
}

impl Converger {
    #[expect(clippy::too_many_arguments, reason = "proofs retain the failed invocation and its limits")]
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
        self.isolate_with_candidates(work, plan, stdout, failed_verb, roots, limits, events, None)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "isolation needs the failed build's complete invocation context and proof limits"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one bounded isolation pass keeps component preservation, tier recovery, and immediate verdict publication together"
    )]
    pub(super) fn isolate_with_candidates(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        stdout: &str,
        failed_verb: &[&str],
        roots: Option<&[String]>,
        limits: BuildLimits,
        events: &mut dyn Events,
        positional: Option<&HashSet<u32>>,
    ) -> Result<Vec<Isolation>> {
        let costs = core::cell::LazyCell::new(|| ProofCostIndex::new(plan));
        let by_package = isolation_population(plan, &self.withdrawn);
        let contexts = prioritized_contexts(failure_contexts(stdout, plan, &work.root, failed_verb), plan, &by_package);
        let mut affected: HashSet<String> = contexts.iter().map(|(context, _, _)| context.package.clone()).collect();
        extend_affected_packages(&mut affected, &plan.reach);
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
        let reuse_generation = proof_cache::identity(work, plan).and_then(|_| proof_cache::generation(plan));
        let mut isolated = Vec::new();

        for (context, packages, _dependency_count) in contexts {
            let dependencies = without_withdrawn(context_dependencies(&by_package, &packages), &self.withdrawn);
            if
            // #[gamma::skip(cond.always_false, reason = "The empty-dependencies context contains no unwithdrawn ordinal in any package of its closed dependency scope. All generated file/package/local tiers are subsets of that scope, so has_admissible_tier is false and every eligible tier becomes empty after without_withdrawn. Processing instead of skipping performs no proof, admission, warning, publication or result insertion; proof-local reset is repeated before any later nonempty context. Only discarded local containers are built.", tag = "equivalent")]
            dependencies.is_empty() {
                continue;
            }

            let (mut tiers, mut diagnostic_end) = context_tiers(self, &context, plan, &by_package, &packages, &dependencies, &envelopes);
            if let Some(positional) = positional {
                let candidates = positional.intersection(&dependencies).copied().collect::<HashSet<_>>();
                // Containment narrows the first proof, but a passing deletion tier must still
                // widen to the diagnostic item/file to find an invalid child or an interaction.
                if
                // #[gamma::skip(logical.and_remove_left, tag = "equivalent", reason = "an empty prepended tier performs no proof or publication; shifting both subsequent tier indexes and diagnostic_end by one preserves every progress boundary")]
                !candidates.is_empty() && candidates.len() <= MAX_ISOLATION_CANDIDATES {
                    tiers.insert(0, candidates);
                    diagnostic_end += 1;
                }
            }

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
                publish_progress:
                // #[gamma::skip(literal.bool_flip, reason = "This literal initializes only BuildScope.publish_progress in isolate_scoped's local proof scope. That scope is passed to isolate_candidates/subset_fails/run_proof, where only roots and mutants are read. Proof activity and immediate singleton progress are unconditional with respect to this field; no call to converge_scoped consumes this scope. Changing the unread proof-scope field from false to true has no behavior.", tag = "equivalent")]
                false,
            };
            let verb = if positional.is_some() {
                // A positional suspicion is not evidence in a different target/feature graph.
                failed_verb.iter().map(|argument| (*argument).to_owned()).collect()
            } else if self.fixed_roots.is_some() {
                self.check_verb().iter().map(|argument| (*argument).to_owned()).collect()
            } else {
                context.verb()
            };
            let proofs = proof_contexts.entry(verb.clone()).or_default();
            proofs.reuse_generation = reuse_generation;
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
                let mut previous_population = None;
                while !eligible.is_empty() {
                    assert!(
                        previous_population.is_none_or(|before| eligible.len() < before),
                        "retrying an isolation tier requires a strictly smaller eligible population"
                    );

                    // #[gamma::skip(stmt.delete_assign, reason = "This assignment only records the assertion's previous eligible population. Valid isolation removes proved candidates before retrying; disabling this guard changes no proof or result without a second fault.", tag = "equivalent")]
                    previous_population =
                    // #[gamma::skip(option.some_to_none, reason = "This assignment only records the assertion's previous eligible population. Valid isolation removes proved candidates before retrying; disabling this guard changes no proof or result without a second fault.", tag = "equivalent")]
                    Some(eligible.len());
                    #[cfg(test)]
                    tests::visit_isolation_iteration();
                    let current_dependencies = without_withdrawn(dependencies.difference(&resolved).copied().collect(), &self.withdrawn);
                    match self.isolate_candidates_indexed(work, plan, scope, &verb, limits, events, &eligible, proofs, &costs)? {
                        IsolationAttempt::Resolved(result) => {
                            let before = eligible.len();
                            match &result {
                                Isolation::Blamed(ordinals) => {
                                    let blamed: HashMap<_, _> = ordinals
                                        .iter()
                                        .map(|ordinal| {
                                            (*ordinal, proofs.attributed.remove(ordinal).unwrap_or_else(CompilerReason::isolated))
                                        })
                                        .collect();
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
                            applicable =
                            // #[gamma::skip(assign_value.default, reason = "The assigned false is exactly the inferred primitive type's Default value.", tag = "equivalent")]
                            false;
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
            if applicable && resolved.is_empty() {
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
        self.isolate_candidates_indexed(
            work,
            plan,
            scope,
            verb,
            limits,
            events,
            eligible,
            proofs,
            &ProofCostIndex::new(plan),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "candidate isolation shares immutable cost metadata across tiers"
    )]
    fn isolate_candidates_indexed(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        eligible: &HashSet<u32>,
        proofs: &mut Proofs,
        costs: &ProofCostIndex<'_>,
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
        if let Some(attempt) = proofs.attributed_attempt() {
            return Ok(attempt);
        }
        if populated != Some(true) {
            return Ok(if populated == Some(false) {
                IsolationAttempt::NotReproduced
            } else {
                IsolationAttempt::Inconclusive
            });
        }

        let complete = candidates.clone();
        while let Some(pending) = proofs.next_partition(eligible, &self.withdrawn, |active| self.proof_cost(costs, active)) {
            let pending = pending.into_iter().collect::<HashSet<_>>();
            candidates = complete
                .iter()
                .copied()
                .filter(|mutant| pending.contains(&mutant.ordinal))
                .collect();
            let failed = self.subset_fails(work, plan, scope, verb, limits, events, &complete, &candidates, proofs)?;
            if let Some(attempt) = proofs.attributed_attempt() {
                return Ok(attempt);
            }
            match failed {
                Some(true) => break,
                Some(false) => candidates.clone_from(&complete),
                None => return Ok(IsolationAttempt::Inconclusive),
            }
        }

        self.narrow_candidates(work, plan, scope, verb, limits, events, candidates, proofs, costs)
    }

    #[expect(clippy::too_many_arguments, reason = "narrowing preserves the complete proof invocation context")]
    fn narrow_candidates(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        scope: BuildScope<'_>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        mut candidates: Vec<&Mutant>,
        proofs: &mut Proofs,
        costs: &ProofCostIndex<'_>,
    ) -> Result<IsolationAttempt> {
        candidates.sort_by(|left, right| {
            (
                &left.package,
                &left.file,
                &left.item_path,
                !may_have_nonlocal_effects(left),
                left.ordinal,
            )
                .cmp(&(
                    &right.package,
                    &right.file,
                    &right.item_path,
                    !may_have_nonlocal_effects(right),
                    right.ordinal,
                ))
        });
        let population = candidates.clone();
        let mut items: Vec<Vec<&Mutant>> = Vec::new();
        for mutant in candidates {
            if items
                .last()
                .and_then(|item|
                // #[gamma::skip(iter.first_to_last, reason = "Every item group is formed by equality of package, file, and item_path to its representative, so first() and last() have identical grouping fields.", tag = "equivalent")]
                item.first())
                .is_some_and(|first| first.package == mutant.package && first.file == mutant.file && first.item_path == mutant.item_path)
            {
                items
                    .last_mut()
                    .unwrap_or_else(|| unreachable!("the item was just observed"))
                    .push(mutant);
            } else {
                items.push(vec![mutant]);
            }
        }

        let mut failing = population.clone();
        while
        // #[gamma::skip(relational.gt_to_ge, literal.int_decrement, tag = "timeout", reason = "admitting a singleton repeats a cached proof of the same population without shrinking the partition")]
        // #[gamma::skip(iter.first_to_last, tag = "equivalent", reason = "the right operand is reached only with at most one item, whose first and last elements are identical")]
        items.len() > 1 || items.first().is_some_and(|item| item.len() > 1) {
            if
            // #[gamma::skip(cond.always_false, literal.int_decrement, tag = "timeout", reason = "the sole multi-mutant item must split into singleton items before bisection; otherwise its cached proof repeats forever")]
            items.len() == 1 {
                failing = items.pop().unwrap_or_default();
                items = failing.iter().copied().map(|mutant| vec![mutant]).collect();
            }
            // #[gamma::skip(arith.div_to_rem, literal.int_decrement, literal.int_increment, tag = "timeout", reason = "these campaign divisors produced empty or nonshrinking isolation partitions")]
            let middle = items.len() / 2;
            let left = items[..middle].concat();
            let right = items[middle..].concat();
            let right_first = self.proof_cost(costs, &Proofs::key(&right)) < self.proof_cost(costs, &Proofs::key(&left));
            let (first, second) = if right_first { (&right, &left) } else { (&left, &right) };
            let failed = self.subset_fails(work, plan, scope, verb, limits, events, &population, first, proofs)?;
            if let Some(attempt) = proofs.attributed_attempt() {
                return Ok(attempt);
            }
            if failed == Some(true) {
                proofs.defer(second);
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "successful bisection must shrink the population before retrying")]
                if right_first {
                    drop(items.drain(..middle));
                } else {
                    items.truncate(middle);
                }
                continue;
            }
            let failed = self.subset_fails(work, plan, scope, verb, limits, events, &population, second, proofs)?;
            if let Some(attempt) = proofs.attributed_attempt() {
                return Ok(attempt);
            }
            if failed == Some(true) {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "a failing second partition must replace the current items or the same cached split repeats without progress")]
                if right_first {
                    items.truncate(middle);
                } else {
                    drop(items.drain(..middle));
                }
                continue;
            }
            let Some(members) = self.minimize_interaction(work, plan, scope, verb, limits, events, &population, &failing, proofs)? else {
                return Ok(proofs.attributed_attempt().unwrap_or(IsolationAttempt::Inconclusive));
            };
            return Ok(IsolationAttempt::Resolved(Isolation::Interaction {
                excluded: members[0],
                members,
            }));
        }

        Ok(IsolationAttempt::Resolved(Isolation::Blamed(
            items.into_iter().flatten().map(|mutant| mutant.ordinal).collect(),
        )))
    }

    fn proof_cost(&self, costs: &ProofCostIndex<'_>, active: &[u32]) -> (usize, usize, bool) {
        costs.cost(&self.proof_active, &self.proof_background, active)
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
        let mut previous = None;
        while
        // #[gamma::skip(logical.and_remove_left, tag = "equivalent", reason = "after attribution subset_fails returns None before cache, budget, or compiler access; omitting this guard only performs finite chunk refinement, and the final attribution check still returns None")]
        proofs.attributed.is_empty() && Self::minimization_can_continue(active.len(), proofs.local, self.isolation_budget.proofs) {
            assert!(
                previous.is_none_or(|(length, partitions)| active.len() < length || (active.len() == length && granularity > partitions)),
                "interaction minimization must remove candidates or refine its partition before retrying cached proofs"
            );

            // #[gamma::skip(stmt.delete_assign, reason = "This assignment only records the minimizer's progress assertion. Valid chunk/complement steps change population or granularity; disabling the guard has no single-fault algorithmic effect.", tag = "equivalent")]
            previous =
            // #[gamma::skip(option.some_to_none, reason = "This assignment only records the minimizer's progress assertion. Valid chunk/complement steps change population or granularity; disabling the guard has no single-fault algorithmic effect.", tag = "equivalent")]
            Some((active.len(), granularity));
            let width = active.len().div_ceil(granularity);
            let chunks = active.chunks(width).map(<[_]>::to_vec).collect::<Vec<_>>();
            let mut reduced = false;
            for chunk in &chunks {
                if self.subset_fails(work, plan, scope, verb, limits, events, population, chunk, proofs)? == Some(true) {
                    active.clone_from(chunk);
                    granularity = Self::reduced_chunk_granularity();

                    // #[gamma::skip(stmt.delete_assign, reason = "Only bypasses the first successful-chunk reduced/continue shortcut. active has already become a nonempty disjoint chunk and granularity reset2. Complement iteration skips its own empty complement; any other chunk's complement is exactly active, whose successful proof is cached. That branch restores reduced=true and granularity=max(2-1,2)=2 with no new proof/events/IO. At least two chunks exist because active starts>1 and granularity>=2. Same next iteration/result/cache.", tag = "equivalent")]
                    reduced =
                    // #[gamma::skip(assign_value.default, literal.bool_flip, reason = "Only bypasses the first successful-chunk reduced/continue shortcut. active has already become a nonempty disjoint chunk and granularity reset2. Complement iteration skips its own empty complement; any other chunk's complement is exactly active, whose successful proof is cached. That branch restores reduced=true and granularity=max(2-1,2)=2 with no new proof/events/IO. At least two chunks exist because active starts>1 and granularity>=2. Same next iteration/result/cache.", tag = "equivalent")]
                    true;
                    break;
                }
            }
            if
            // #[gamma::skip(cond.always_false, reason = "Only bypasses the first successful-chunk reduced/continue shortcut. active has already become a nonempty disjoint chunk and granularity reset2. Complement iteration skips its own empty complement; any other chunk's complement is exactly active, whose successful proof is cached. That branch restores reduced=true and granularity=max(2-1,2)=2 with no new proof/events/IO. At least two chunks exist because active starts>1 and granularity>=2. Same next iteration/result/cache.", tag = "equivalent")]
            reduced {
                // #[gamma::skip(loop.delete_continue, reason = "Only bypasses the first successful-chunk reduced/continue shortcut. active has already become a nonempty disjoint chunk and granularity reset2. Complement iteration skips its own empty complement; any other chunk's complement is exactly active, whose successful proof is cached. That branch restores reduced=true and granularity=max(2-1,2)=2 with no new proof/events/IO. At least two chunks exist because active starts>1 and granularity>=2. Same next iteration/result/cache.", tag = "equivalent")]
                continue;
            }
            for chunk in &chunks {
                let removed = chunk.iter().map(|mutant| mutant.ordinal).collect::<HashSet<_>>();
                let complement = active
                    .iter()
                    .copied()
                    .filter(|mutant| !removed.contains(&mutant.ordinal))
                    .collect::<Vec<_>>();
                if
                // #[gamma::skip(cond.always_false, reason = "Without another mutation, complements are considered only when no chunk reduced active. active.len()>1 and granularity>=2 imply ceil(len/granularity)<len, so each chunk is strictly smaller than active. Its complement cannot be empty; mutating the empty-complement guard/continue has no reachable effect.", tag = "equivalent")]
                complement.is_empty() {
                    // #[gamma::skip(loop.continue_to_break, loop.delete_continue, reason = "Without another mutation, complements are considered only when no chunk reduced active. active.len()>1 and granularity>=2 imply ceil(len/granularity)<len, so each chunk is strictly smaller than active. Its complement cannot be empty; mutating the empty-complement guard/continue has no reachable effect.", tag = "equivalent")]
                    continue;
                }
                if self.subset_fails(work, plan, scope, verb, limits, events, population, &complement, proofs)? == Some(true) {
                    active = complement;
                    granularity = granularity.saturating_sub(1).max(
                        // #[gamma::skip(literal.int_decrement, literal.int_to_zero, reason = "A successful complement reduction cannot first occur at granularity2: the two chunks are each other's complements, already queried in the immediately preceding chunk loop. Thus every reachable successful-complement branch has granularity>=3 and saturating_sub1>=2. max2/max1/max0 select exactly the same next granularity.", tag = "equivalent")]
                        2,
                    );
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
        Ok((proofs.attributed.is_empty()
            && proofs.local < MAX_ISOLATION_PROOFS
            && self.isolation_budget.proofs < MAX_INVOCATION_ISOLATION_PROOFS)
            .then(|| active.iter().map(|mutant| mutant.ordinal).collect()))
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
        if !proofs.attributed.is_empty() {
            return Ok(None);
        }
        let key = Proofs::key(active);
        if let Some(cached) = proofs.results.get(&key) {
            return Ok(*cached);
        }
        let compilation = proofs
            .reuse_generation
            .filter(|_| self.unavailable.is_empty())
            .and_then(|_| proof_cache::identity(work, plan));
        let reuse_key = proof_cache::key(proofs, compilation, scope, verb, &self.proof_background, &key);
        if let Some(cached) = self.proof_cache.get(reuse_key) {
            let _previous = proofs.results.insert(key, Some(cached));
            return Ok(Some(cached));
        }
        if !self.isolation_budget.proof(proofs.local) {
            return Ok(None);
        }
        proofs.local = proofs.local.saturating_add(1);
        let active = active.iter().map(|mutant| mutant.ordinal).collect::<HashSet<_>>();

        #[cfg(any(test, feature = "internals"))]
        if let Some(oracle) = self.subset_oracle {
            self.proof_roots.push(scope.roots.map(<[String]>::to_vec));
            self.proof_active.clone_from(&self.proof_background);
            self.proof_active.extend(&active);
            let result = oracle(&active);
            if compilation == proofs.reuse_generation.and_then(|_| proof_cache::identity(work, plan)) {
                self.proof_cache.insert(reuse_key, result);
            }
            let _ = proofs.results.insert(key, result);
            return Ok(result);
        }

        let result = self.run_proof(
            work,
            plan,
            scope,
            verb,
            limits,
            events,
            population.len(),
            &active,
            &mut proofs.attributed,
        )?;
        if (
            // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "Unavailability is sticky for this Converger and independently disables every later compilation-key lookup.")]
            proofs.attributed.is_empty() && self.unavailable.is_empty()
        ) && compilation == proofs.reuse_generation.and_then(|_| proof_cache::identity(work, plan))
        {
            self.proof_cache.insert(reuse_key, result);
        }
        let _ = proofs.results.insert(key, result);
        Ok(if proofs.attributed.is_empty() { result } else { None })
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
        attributed: &mut HashMap<u32, CompilerReason>,
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
        let outcome = super::invoke::run_cargo_selective(work, plan, verb, scope.roots, limits, self.first_round, events)?;
        if !outcome.succeeded
            && let Some(stdout) = outcome.stdout.as_deref()
        {
            let mut blamed = blame(stdout, &work.root, self.splices.guards());
            retain_blamed(&mut blamed, plan, scope.mutants);
            blamed.retain(|ordinal, reason| active.contains(ordinal) && !self.withdrawn.contains(ordinal) && !reason.needs_confirmation());
            attributed.extend(blamed);
        }
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
        proof_result(&outcome, plan, work, verb, active)
    }
}

#[ohno::error]
#[display("compile isolation stopped because its proof failed without attributable compiler errors:\n{failure_context}\n{stderr}")]
struct UnattributableProof {
    failure_context: String,
    stderr: String,
}

/// `None` means missing stdout; `Some(false)` means success; `Some(true)` means an active file
/// has attributable compiler errors. `Err` means a failed build supplied no such attribution.
fn proof_result(
    outcome: &super::invoke::Compiled,
    plan: &Plan,
    work: &Workspace,
    verb: &[&str],
    active: &HashSet<u32>,
) -> Result<Option<bool>> {
    let Some(stdout) = outcome.stdout.as_deref() else {
        return Ok(None);
    };
    if outcome.succeeded {
        return Ok(Some(false));
    }
    let contexts = failure_contexts(stdout, plan, &work.root, verb);
    if !contexts.iter().any(|context| {
        plan.mutants.iter().any(|mutant| {
            active.contains(&mutant.ordinal)
                && context
                    .files
                    .iter()
                    .any(|file| super::blame::matches_reported_file(Utf8Path::new(file), mutant.file.as_ref(), &work.root))
        })
    }) {
        let cause = UnattributableProof::new(outcome.failure_context.clone(), outcome.stderr.clone());
        return Err(error!("{cause}").caused_by(cause));
    }
    Ok(Some(true))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::error::Error as _;

    use serde_json::{Value, from_str, from_value, json};

    use super::*;
    use crate::error::error;

    #[test]
    fn noncompiler_and_unrelated_proof_failures_never_convict_a_subset() {
        let root = Utf8Path::new("workspace");
        let work = Workspace::adopt(root.to_owned(), root.join("target"));
        let mut plan = plan(root, 2);
        plan.mutants[1].file = Utf8Path::new("src/other.rs").into();
        let mut outcome = super::super::invoke::Compiled {
            succeeded: false,
            failure_context: "Cargo finished with signal 9".into(),
            stdout: Some(String::new()),
            stderr: "linker failed: disk full".into(),
        };
        let noncompiler = proof_result(&outcome, &plan, &work, &["check", "--lib"], &HashSet::from_iter([1])).unwrap_err();
        let cause = noncompiler.source().unwrap().downcast_ref::<UnattributableProof>().unwrap();
        assert_eq!(cause.failure_context, outcome.failure_context);
        assert_eq!(cause.stderr, outcome.stderr);
        outcome.stdout = Some(error_stream());
        assert_eq!(
            proof_result(&outcome, &plan, &work, &["check", "--lib"], &HashSet::from_iter([1])).unwrap(),
            Some(true)
        );
        let unrelated = proof_result(&outcome, &plan, &work, &["check", "--lib"], &HashSet::from_iter([2])).unwrap_err();
        let cause = unrelated.source().unwrap().downcast_ref::<UnattributableProof>().unwrap();
        assert_eq!(cause.failure_context, outcome.failure_context);
        assert_eq!(cause.stderr, outcome.stderr);
        outcome.succeeded = true;
        assert_eq!(
            proof_result(&outcome, &plan, &work, &["check", "--lib"], &HashSet::from_iter([1])).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn proof_result_uses_component_suffixes_only_for_absolute_paths_outside_the_workspace() {
        let root = Utf8Path::new(if cfg!(windows) { "C:\\prepared" } else { "/prepared" });
        let work = Workspace::adopt(root.to_owned(), root.join("target"));
        let mut plan = plan(root, 1);
        plan.mutants[0].file = Utf8Path::new("crates/pkg/src/lib.rs").into();
        let outside = if cfg!(windows) { "C:\\elsewhere" } else { "/elsewhere" };
        for (file, allowed) in [
            ("crates/pkg/src/lib.rs".to_owned(), true),
            (root.join("crates/pkg/src/lib.rs").to_string(), true),
            (format!("{outside}/crates/pkg/src/lib.rs"), true),
            ("elsewhere/crates/pkg/src/lib.rs".to_owned(), false),
            (root.join("other/crates/pkg/src/lib.rs").to_string(), false),
            (format!("{outside}/src/lib.rs"), false),
            (format!("{outside}/crates/other/src/lib.rs"), false),
        ] {
            let mut diagnostic: Value = from_str(&error_stream()).unwrap();
            diagnostic["message"]["spans"][0]["file_name"] = file.clone().into();
            let outcome = super::super::invoke::Compiled {
                succeeded: false,
                failure_context: "compiler failed".into(),
                stdout: Some(diagnostic.to_string()),
                stderr: "compiler stderr".into(),
            };
            let actual = proof_result(&outcome, &plan, &work, &["check", "--lib"], &HashSet::from_iter([1]));
            if allowed {
                assert_eq!(actual.unwrap(), Some(true), "{file}");
            } else {
                assert_eq!(
                    actual.unwrap_err().message(),
                    "compile isolation stopped because its proof failed without attributable compiler errors:\ncompiler failed\ncompiler stderr",
                    "{file}"
                );
            }
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct CostWork {
        pub(super) index_mutants: usize,
        pub(super) index_packages: usize,
        pub(super) index_dependencies: usize,
        pub(super) ordinal_lookups: usize,
        pub(super) metadata: usize,
        pub(super) dependent_entries: usize,
        scan_mutants: usize,
        scan_packages: usize,
    }

    thread_local! {
        static COST_WORK: std::cell::RefCell<CostWork> = std::cell::RefCell::default();
    }

    pub(super) fn cost_work(update: impl FnOnce(&mut CostWork)) {
        COST_WORK.with_borrow_mut(update);
    }

    fn take_cost_work() -> CostWork {
        COST_WORK.with_borrow_mut(core::mem::take)
    }

    fn scan_cost(plan: &Plan, previous: &HashSet<u32>, background: &HashSet<u32>, active: &[u32]) -> (usize, usize, bool) {
        let active = active.iter().copied().collect::<HashSet<_>>();
        let changed = previous
            .symmetric_difference(&active)
            .filter(|ordinal| !background.contains(ordinal))
            .copied()
            .collect::<HashSet<_>>();
        let mut files = HashSet::default();
        let mut packages = HashSet::default();
        let mut nonlocal = false;
        for mutant in &plan.mutants {
            cost_work(|work| work.scan_mutants += 1);
            nonlocal |= active.contains(&mutant.ordinal) && may_have_nonlocal_effects(mutant);
            if changed.contains(&mutant.ordinal) {
                let _ = files.insert(mutant.file.as_ref());
                let _ = packages.insert(mutant.package.as_ref());
            }
        }
        let mut rebuilt = packages.clone();
        for (package, dependencies) in &plan.reach {
            cost_work(|work| work.scan_packages += 1);
            if dependencies.iter().any(|dependency| packages.contains(dependency.as_str())) {
                let _ = rebuilt.insert(package.as_str());
            }
        }
        (rebuilt.len(), files.len(), !nonlocal)
    }

    #[test]
    fn indexed_cost_matches_scan_across_active_background_and_historical_ordinals() {
        let mut plan = plan(Utf8Path::new("indexed-cost"), 6);
        for (mutant, (ordinal, package, file, mutator)) in plan.mutants.iter_mut().zip([
            (1, "leaf", "shared.rs", "literal.int_to_zero"),
            (2, "leaf", "shared.rs", "stmt.delete_assign"),
            (3, "middle", "middle.rs", "literal.int_to_zero"),
            (4, "unmapped", "unmapped.rs", "literal.int_to_zero"),
            (0, "history-a", "history-a.rs", "literal.int_to_zero"),
            (0, "history-b", "history-b.rs", "fn_value.zero"),
        ]) {
            mutant.ordinal = ordinal;
            mutant.package = package.into();
            mutant.file = Utf8Path::new(file).into();
            mutant.mutator = mutator.into();
        }
        plan.reach = [
            ("middle", vec!["leaf", "middle"]),
            ("top", vec!["middle"]),
            ("overlap", vec!["leaf", "middle"]),
            ("unrelated", vec!["unrelated"]),
        ]
        .into_iter()
        .map(|(package, dependencies)| (package.to_owned(), dependencies.into_iter().map(str::to_owned).collect()))
        .collect();
        let costs = ProofCostIndex::new(&plan);
        let ordinals = [0, 1, 2, 3, 4, 99];
        let population = |mask| {
            ordinals
                .iter()
                .enumerate()
                .filter_map(|(bit, ordinal)| (mask & (1 << bit) != 0).then_some(*ordinal))
                .collect::<Vec<_>>()
        };
        for previous in 0..64 {
            let previous = population(previous).into_iter().collect();
            for active in 0..64 {
                let mut active = population(active);
                active.extend(active.clone());
                for background in [vec![], vec![2], vec![0, 1, 3, 99]] {
                    let background = background.into_iter().collect();
                    assert_eq!(
                        costs.cost(&previous, &background, &active),
                        scan_cost(&plan, &previous, &background, &active),
                        "previous={previous:?}, background={background:?}, active={active:?}",
                    );
                }
            }
        }
        assert_eq!(costs.cost(&HashSet::default(), &HashSet::default(), &[1]), (3, 1, true));
        assert_eq!(costs.cost(&HashSet::default(), &HashSet::default(), &[4]), (1, 1, true));
        assert_eq!(costs.cost(&HashSet::from_iter([2]), &HashSet::default(), &[2]), (0, 0, false));
        assert_eq!(costs.cost(&HashSet::from_iter([2]), &HashSet::from_iter([2]), &[]), (0, 0, true));
    }

    #[test]
    fn indexed_cost_work_tracks_selected_metadata_and_reverse_reach_not_plan_population() {
        let mut plan = plan(Utf8Path::new("indexed-cost-work"), 8_192);
        for mutant in &mut plan.mutants[8..] {
            mutant.package = "cold".into();
        }
        plan.mutants[1].mutator = "stmt.delete_assign".into();
        plan.reach = (0..256)
            .map(|number| {
                (
                    format!("package-{number}"),
                    HashSet::from_iter([if number < 4 { "subject" } else { "cold" }.to_owned()]),
                )
            })
            .collect();
        let _ = take_cost_work();
        let costs = ProofCostIndex::new(&plan);
        assert_eq!(
            take_cost_work(),
            CostWork {
                index_mutants: 8_192,
                index_packages: 256,
                index_dependencies: 256,
                ..CostWork::default()
            },
        );
        assert_eq!(costs.mutants.len(), 8_192);
        assert_eq!(costs.mutants.values().map(Vec::len).sum::<usize>(), 8_192);
        assert_eq!(costs.dependents.len(), 2);
        assert_eq!(costs.dependents.values().map(Vec::len).sum::<usize>(), 256);

        let mut converger = Converger {
            proof_active: HashSet::from_iter([1, 2, 8_192]),
            proof_background: HashSet::from_iter([8_192]),
            ..Converger::default()
        };
        for active in [&[1][..], &[2][..], &[1, 2][..], &[1][..]] {
            assert_eq!(
                converger.proof_cost(&costs, active),
                scan_cost(&plan, &converger.proof_active, &converger.proof_background, active),
            );
            converger.proof_active = converger.proof_background.clone();
            converger.proof_active.extend(active);
        }
        assert_eq!(
            take_cost_work(),
            CostWork {
                ordinal_lookups: 10,
                metadata: 10,
                dependent_entries: 16,
                scan_mutants: 32_768,
                scan_packages: 1_024,
                ..CostWork::default()
            },
        );
    }

    #[derive(Default)]
    struct ProofEvents {
        observations: Vec<(usize, usize, Vec<std::path::PathBuf>)>,
        warnings: Vec<String>,
        activity: Vec<(u32, usize, usize)>,
    }

    impl Events for ProofEvents {
        fn phase(&mut self, _verb: &str, _detail: &str) {}

        fn mutant(&mut self, _mutant: &Mutant) {}

        fn warn(&mut self, warning: &str) {
            self.warnings.push(warning.to_owned());
        }

        fn convergence_activity(&mut self, round: u32, excluded: usize, proofs: usize) {
            self.activity.push((round, excluded, proofs));
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

    #[test]
    fn deferred_partitions_deduplicate_prune_withdrawals_and_wait_for_complete_eligibility() {
        let plan = plan(Utf8Path::new("deferred-partitions"), 5);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut proofs = Proofs::default();
        proofs.defer(&[]);
        proofs.defer(&population[..2]);
        proofs.defer(&[population[1], population[0]]);
        proofs.defer(&population[2..4]);
        proofs.defer(&population[4..]);
        assert_eq!(proofs.pending, [vec![1, 2], vec![3, 4], vec![5]]);
        let eligible = HashSet::from_iter([1, 3]);
        let withdrawn = HashSet::from_iter([2, 5]);
        let mut costed = Vec::new();
        assert_eq!(
            proofs.next_partition(&eligible, &withdrawn, |partition| {
                costed.push(partition.to_vec());
                (0, 0, false)
            }),
            Some(vec![1]),
        );
        assert!(costed.iter().all(|partition| partition == &[1]));
        assert_eq!(proofs.pending, [vec![3, 4]]);
        assert_eq!(
            proofs.next_partition(&eligible, &withdrawn, |_| panic!("ineligible partition")),
            None
        );
        assert_eq!(
            proofs.next_partition(&HashSet::from_iter([3, 4]), &withdrawn, |_| (0, 0, false)),
            Some(vec![3, 4]),
        );
        assert_eq!(proofs.pending, Vec::<Vec<u32>>::new());
    }

    #[test]
    fn proof_cost_prioritizes_body_statement_and_loop_edits_after_equal_rebuild_costs() {
        let mut plan = plan(Utf8Path::new("proof-cost"), 1);
        let converger = Converger::default();
        for (mutator, nonlocal) in [
            ("fn_value.zero", true),
            ("stmt.delete_assign", true),
            ("loop.delete_continue", true),
            ("parameter.default_shadow", true),
            ("cond.always_true", true),
            ("literal.int_to_zero", false),
            ("custom.fn_value.", false),
            ("custom.loop.", false),
        ] {
            plan.mutants[0].mutator = mutator.into();
            assert_eq!(
                converger.proof_cost(&ProofCostIndex::new(&plan), &[1]),
                (1, 1, !nonlocal),
                "{mutator}"
            );
        }
    }

    #[test]
    fn deferred_proof_transport_errors_propagate_before_narrowing_or_publishing_blame() {
        let work = memory_workspace();
        let plan = plan(&work.root, 3);
        let mut proofs = Proofs {
            results: HashMap::from_iter([(vec![1, 2, 3], Some(true))]),
            pending: vec![vec![2]],
            ..Proofs::default()
        };
        let mut converger = Converger::default();
        let failure = super::super::with_compiler(
            |_, _, _, _| Err(error!("deferred proof transport failed")),
            || {
                converger.isolate_candidates(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: false,
                    },
                    &["check"],
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                    &HashSet::from_iter([1, 2, 3]),
                    &mut proofs,
                )
            },
        )
        .unwrap_err();
        assert_eq!(failure.message(), "deferred proof transport failed");
        assert!(converger.withdrawn.is_empty());
        assert!(proofs.attributed.is_empty());
        assert_eq!(proofs.local, 1);
    }

    #[test]
    fn isolation_orchestration_reuses_only_proofs_from_the_same_frozen_source_generation() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 1);
        let path = Utf8PathBuf::from("src/lib.rs");
        plan.files.push(crate::discover::TargetFile {
            absolute: work.root.join(&path),
            path: path.clone(),
            package: "subject".into(),
            source: None,
        });
        let mut converger = Converger {
            subset_oracle: Some(|_| Some(false)),
            ..Converger::default()
        };
        proof_cache::with_identity(Some([7; 32]), || {
            for source in ["fn value() -> u32 { 1 }", "fn value() -> u32 { 2 }"] {
                plan.files[0].source = Some(source.into());
                let _previous = plan.digests.insert(path.clone(), crate::discover::digest(source.as_bytes()));
                for round in 0..2 {
                    converger.begin_convergence();
                    assert!(
                        converger
                            .isolate_scoped(
                                &work,
                                &plan,
                                &error_stream(),
                                &["check", "--tests"],
                                None,
                                BuildLimits::default(),
                                &mut ProofEvents::default(),
                            )
                            .unwrap()
                            .is_empty()
                    );
                    assert_eq!(converger.isolation_budget.proofs, usize::from(round == 0));
                    assert!(converger.withdrawn.is_empty());
                }
            }
        });
        assert_eq!(converger.proof_roots.len(), 2);
    }

    #[test]
    fn definitive_proofs_reuse_across_rounds_without_spending_another_launch() {
        let work = memory_workspace();
        let plan = plan(&work.root, 2);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let scope = BuildScope {
            roots: None,
            mutants: None,
            publish_progress: false,
        };
        for oracle in [
            (|_: &HashSet<u32>| Some(false)) as super::super::SubsetOracle,
            |_| Some(true),
            |_| None,
        ] {
            let mut converger = Converger {
                subset_oracle: Some(oracle),
                ..Converger::default()
            };
            proof_cache::with_identity(Some([7; 32]), || {
                let mut first = Proofs {
                    reuse_generation: Some([1; 32]),
                    ..Proofs::default()
                };
                let original = converger
                    .subset_fails(
                        &work,
                        &plan,
                        scope,
                        &["check", "--tests"],
                        BuildLimits::default(),
                        &mut ProofEvents::default(),
                        &population,
                        &population,
                        &mut first,
                    )
                    .unwrap();
                assert_eq!(first.local, 1);
                converger.begin_convergence();
                let mut second = Proofs {
                    reuse_generation: Some([1; 32]),
                    ..Proofs::default()
                };
                assert_eq!(
                    converger
                        .subset_fails(
                            &work,
                            &plan,
                            scope,
                            &["check", "--tests"],
                            BuildLimits::default(),
                            &mut ProofEvents::default(),
                            &population,
                            &population,
                            &mut second,
                        )
                        .unwrap(),
                    original
                );
                assert_eq!(
                    converger
                        .subset_fails(
                            &work,
                            &plan,
                            scope,
                            &["check", "--tests"],
                            BuildLimits::default(),
                            &mut ProofEvents::default(),
                            &population,
                            &population,
                            &mut second,
                        )
                        .unwrap(),
                    original,
                    "a cross-round cache hit remains a definitive local verdict on repeated queries"
                );
                let launches = usize::from(original.is_none());
                assert_eq!(second.local, launches);
                assert_eq!(converger.isolation_budget.proofs, launches);
                assert_eq!(converger.proof_roots.len(), 1 + launches);
            });
        }
    }

    #[test]
    fn cross_round_reuse_declines_unknown_and_changed_compilation_identities() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let scope = BuildScope {
            roots: None,
            mutants: None,
            publish_progress: false,
        };
        let mut converger = Converger {
            subset_oracle: Some(|_| Some(false)),
            ..Converger::default()
        };
        for compilation in [Some([1; 32]), None, Some([2; 32])] {
            proof_cache::with_identity(compilation, || {
                let mut proofs = Proofs {
                    reuse_generation: Some([1; 32]),
                    ..Proofs::default()
                };
                assert_eq!(
                    converger
                        .subset_fails(
                            &work,
                            &plan,
                            scope,
                            &["check"],
                            BuildLimits::default(),
                            &mut ProofEvents::default(),
                            &population,
                            &population,
                            &mut proofs,
                        )
                        .unwrap(),
                    Some(false)
                );
                assert_eq!(proofs.local, 1);
            });
        }
        assert_eq!(converger.proof_roots.len(), 3);
    }

    #[test]
    fn actual_proof_results_reuse_only_after_an_unchanged_complete_compilation_context() {
        use std::cell::Cell;
        use std::rc::Rc;

        use crate::exec::proof_context::{Operation, with_operations};

        for succeeded in [true, false] {
            for after_first in [Some([1; 32]), Some([2; 32]), None] {
                let work = memory_workspace();
                let plan = plan(&work.root, 1);
                let population = plan.mutants.iter().collect::<Vec<_>>();
                let identity = Rc::new(Cell::new(Some([1; 32])));
                let calls = Rc::new(Cell::new(0));
                let compiler_identity = Rc::clone(&identity);
                let compiler_calls = Rc::clone(&calls);
                let observed_identity = Rc::clone(&identity);
                let mut converger = Converger::default();
                super::super::with_compiler(
                    move |_, _, _, _| {
                        compiler_calls.set(compiler_calls.get() + 1);
                        if compiler_calls.get() == 1 {
                            compiler_identity.set(after_first);
                        }
                        let stdout = if succeeded { String::new() } else { error_stream() };
                        Ok(Some(super::super::tests::scripted_output(succeeded, &stdout, "")))
                    },
                    || {
                        with_operations(
                            move |operation| match operation {
                                Operation::Identity(_) => observed_identity.get(),
                                Operation::Seal(_, _) | Operation::Invalidate => None,
                            },
                            || {
                                for round in 0..3 {
                                    converger.begin_convergence();
                                    let mut proofs = Proofs {
                                        reuse_generation: Some([7; 32]),
                                        ..Proofs::default()
                                    };
                                    assert_eq!(
                                        converger
                                            .subset_fails(
                                                &work,
                                                &plan,
                                                BuildScope {
                                                    roots: None,
                                                    mutants: None,
                                                    publish_progress: false
                                                },
                                                &["check", "--tests"],
                                                BuildLimits::default(),
                                                &mut ProofEvents::default(),
                                                &population,
                                                &population,
                                                &mut proofs,
                                            )
                                            .unwrap(),
                                        Some(!succeeded)
                                    );
                                    let expected = match after_first {
                                        Some(value) if value == [1; 32] => 1,
                                        Some(_) => (round + 1).min(2),
                                        None => round + 1,
                                    };
                                    assert_eq!(calls.get(), expected, "round {round}, context {after_first:?}");
                                }
                            },
                        );
                    },
                );
            }
        }
    }

    #[test]
    fn transient_context_changes_cannot_publish_a_proof_for_a_later_return_to_the_old_identity() {
        use std::cell::Cell;
        use std::rc::Rc;

        use crate::exec::proof_context::{Operation, with_operations};

        for scripted_oracle in [false, true] {
            let work = memory_workspace();
            let plan = plan(&work.root, 1);
            let population = plan.mutants.iter().collect::<Vec<_>>();
            let reads = Rc::new(Cell::new(0));
            let observed_reads = Rc::clone(&reads);
            let calls = Rc::new(Cell::new(0));
            let compiler_calls = Rc::clone(&calls);
            let mut converger = Converger {
                subset_oracle: scripted_oracle.then_some((|_| Some(true)) as super::super::SubsetOracle),
                ..Converger::default()
            };
            super::super::with_compiler(
                move |_, _, _, _| {
                    compiler_calls.set(compiler_calls.get() + 1);
                    let succeeded = compiler_calls.get() != 1;
                    let stdout = if succeeded { String::new() } else { error_stream() };
                    Ok(Some(super::super::tests::scripted_output(succeeded, &stdout, "")))
                },
                || {
                    with_operations(
                        move |operation| match operation {
                            Operation::Identity(_) => {
                                observed_reads.set(observed_reads.get() + 1);
                                Some(if observed_reads.get() == 2 { [2; 32] } else { [1; 32] })
                            }
                            Operation::Seal(_, _) | Operation::Invalidate => None,
                        },
                        || {
                            for round in 0..3 {
                                converger.begin_convergence();
                                let mut proofs = Proofs {
                                    reuse_generation: Some([7; 32]),
                                    ..Proofs::default()
                                };
                                let verdict = converger
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
                                        &mut ProofEvents::default(),
                                        &population,
                                        &population,
                                        &mut proofs,
                                    )
                                    .unwrap();
                                assert_eq!(verdict, Some(scripted_oracle || round == 0));
                                assert_eq!(proofs.local, usize::from(round < 2));
                            }
                        },
                    );
                },
            );
            assert_eq!(calls.get(), if scripted_oracle { 0 } else { 2 });
            if scripted_oracle {
                assert_eq!(converger.proof_roots.len(), 2);
            }
        }
    }

    #[test]
    fn oracle_side_effects_cannot_publish_stale_verdicts_when_the_original_identity_returns() {
        use std::cell::Cell;

        use crate::exec::proof_context::{Operation, with_operations};

        thread_local! {
            static CONTEXT: Cell<[u8; 32]> = const { Cell::new([1; 32]) };
            static CALLS: Cell<usize> = const { Cell::new(0) };
        }
        CONTEXT.set([1; 32]);
        CALLS.set(0);
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut converger = Converger {
            subset_oracle: Some(|_| {
                let first = CALLS.replace(CALLS.get() + 1) == 0;
                if first {
                    CONTEXT.set([2; 32]);
                }
                Some(first)
            }),
            ..Converger::default()
        };
        with_operations(
            |operation| match operation {
                Operation::Identity(_) => Some(CONTEXT.get()),
                _ => None,
            },
            || {
                for round in 0..3 {
                    CONTEXT.set([1; 32]);
                    converger.begin_convergence();
                    let mut proofs = Proofs {
                        reuse_generation: Some([7; 32]),
                        ..Proofs::default()
                    };
                    assert_eq!(
                        converger
                            .subset_fails(
                                &work,
                                &plan,
                                BuildScope {
                                    roots: None,
                                    mutants: None,
                                    publish_progress: false
                                },
                                &["check"],
                                BuildLimits::default(),
                                &mut ProofEvents::default(),
                                &population,
                                &population,
                                &mut proofs,
                            )
                            .unwrap(),
                        Some(round == 0)
                    );
                    assert_eq!(proofs.local, usize::from(round < 2));
                }
            },
        );
        assert_eq!(CALLS.get(), 2);
    }

    #[test]
    fn attributed_compiler_errors_do_not_become_cross_round_subset_verdicts() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        let position = |line| crate::schema::Position::new(line, 1).unwrap();
        let guards = HashMap::from_iter([(
            1,
            (
                "src/lib.rs".into(),
                crate::schema::Guard {
                    site: position(10)..position(12),
                    mutated: Some(position(10)..position(11)),
                },
            ),
        )]);
        let mut converger = Converger {
            splices: super::super::Splices::seeded_guards_for_test(work.root.clone(), guards),
            ..Converger::default()
        };
        let diagnostic = json!({
            "reason": "compiler-message",
            "package_id": "subject 0.0.0",
            "target": {"name": "subject", "kind": ["lib"]},
            "message": {
                "level": "error", "message": "independent type error", "code": {"code": "E0308"},
                "spans": [{"file_name": "src/lib.rs", "is_primary": true,
                           "line_start": 10, "column_start": 1, "line_end": 10, "column_end": 2}]
            }
        })
        .to_string();
        let mut calls = 0;
        super::super::with_compiler(
            move |_, _, _, _| {
                calls += 1;
                Ok(Some(super::super::tests::scripted_output(
                    calls != 1,
                    if calls == 1 { &diagnostic } else { "{}" },
                    "",
                )))
            },
            || {
                proof_cache::with_identity(Some([1; 32]), || {
                    let population = plan.mutants.iter().collect::<Vec<_>>();
                    for round in 0..2 {
                        converger.begin_convergence();
                        let mut proofs = Proofs {
                            reuse_generation: Some([7; 32]),
                            ..Proofs::default()
                        };
                        assert_eq!(
                            converger
                                .subset_fails(
                                    &work,
                                    &plan,
                                    BuildScope {
                                        roots: None,
                                        mutants: None,
                                        publish_progress: false
                                    },
                                    &["check"],
                                    BuildLimits::default(),
                                    &mut ProofEvents::default(),
                                    &population,
                                    &population,
                                    &mut proofs,
                                )
                                .unwrap(),
                            if round == 0 { None } else { Some(false) }
                        );
                        assert_eq!(proofs.local, 1);
                        assert_eq!(proofs.attributed.len(), usize::from(round == 0));
                    }
                });
            },
        );
    }

    #[test]
    fn attributed_failures_stop_cached_subsets_and_interaction_minimization() {
        let work = memory_workspace();
        let plan = plan(&work.root, 3);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut converger = Converger {
            subset_oracle: Some(|_| panic!("an attributed failure ends proof search")),
            ..Converger::default()
        };
        let mut proofs = Proofs {
            attributed: HashMap::from_iter([(2, CompilerReason::isolated())]),
            results: HashMap::from_iter([(vec![1, 2, 3], Some(true))]),
            ..Proofs::default()
        };
        let scope = BuildScope {
            roots: None,
            mutants: None,
            publish_progress: false,
        };
        let mut events = ProofEvents::default();
        assert_eq!(
            converger
                .subset_fails(
                    &work,
                    &plan,
                    scope,
                    &["check"],
                    BuildLimits::default(),
                    &mut events,
                    &population,
                    &population,
                    &mut proofs,
                )
                .unwrap(),
            None,
        );
        assert_eq!(
            converger
                .minimize_interaction(
                    &work,
                    &plan,
                    scope,
                    &["check"],
                    BuildLimits::default(),
                    &mut events,
                    &population,
                    &population,
                    &mut proofs,
                )
                .unwrap(),
            None,
        );
        assert_eq!(proofs.local, 0);
        assert_eq!(converger.isolation_budget.proofs, 0);
    }

    #[test]
    fn item_narrowing_keeps_packages_files_and_item_paths_as_distinct_groups() {
        thread_local! {
            static QUERIES: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let work = memory_workspace();
        for (package, file, item) in [("b", "a.rs", "first"), ("a", "b.rs", "first"), ("a", "a.rs", "second")] {
            let mut plan = plan(&work.root, 5);
            for mutant in &mut plan.mutants {
                let (package, file, item) = if mutant.ordinal <= 3 {
                    ("a", "a.rs", "first")
                } else {
                    (package, file, item)
                };
                mutant.package = package.into();
                mutant.file = Utf8Path::new(file).into();
                mutant.item_path = item.into();
            }
            QUERIES.with_borrow_mut(Vec::clear);
            let mut converger = Converger {
                subset_oracle: Some(|active| {
                    let mut key = active.iter().copied().collect::<Vec<_>>();
                    key.sort_unstable();
                    QUERIES.with_borrow_mut(|queries| queries.push(key));
                    Some(active.contains(&1))
                }),
                ..Converger::default()
            };
            let mut proofs = Proofs::default();
            let result = converger
                .narrow_candidates(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: false,
                    },
                    &["check"],
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                    plan.mutants.iter().collect(),
                    &mut proofs,
                    &ProofCostIndex::new(&plan),
                )
                .unwrap();
            assert!(matches!(result, IsolationAttempt::Resolved(Isolation::Blamed(ordinals)) if ordinals == [1]));
            QUERIES.with_borrow(|queries| assert_eq!(*queries, [vec![1, 2, 3], vec![1]]));
        }
    }

    #[test]
    fn a_clean_cheaper_right_partition_keeps_the_failing_left_partition() {
        thread_local! {
            static QUERIES: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let work = memory_workspace();
        let mut plan = plan(&work.root, 4);
        for mutant in &mut plan.mutants {
            mutant.item_path = format!("item_{}", mutant.ordinal).into();
        }
        QUERIES.with_borrow_mut(Vec::clear);
        let mut converger = Converger {
            proof_active: HashSet::from_iter([3, 4]),
            subset_oracle: Some(|active| {
                let mut key = active.iter().copied().collect::<Vec<_>>();
                key.sort_unstable();
                QUERIES.with_borrow_mut(|queries| queries.push(key));
                Some(active.contains(&1))
            }),
            ..Converger::default()
        };
        let result = converger
            .narrow_candidates(
                &work,
                &plan,
                BuildScope {
                    roots: None,
                    mutants: None,
                    publish_progress: false,
                },
                &["check"],
                BuildLimits::default(),
                &mut ProofEvents::default(),
                plan.mutants.iter().collect(),
                &mut Proofs::default(),
                &ProofCostIndex::new(&plan),
            )
            .unwrap();
        assert!(matches!(result, IsolationAttempt::Resolved(Isolation::Blamed(ordinals)) if ordinals == [1]));
        QUERIES.with_borrow(|queries| assert_eq!(*queries, [vec![3, 4], vec![1, 2], vec![1]]));
    }

    #[test]
    fn actual_subset_diagnostics_attribute_only_active_unwithdrawn_independent_errors_and_cache_the_result() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 5);
        plan.mutants[4].package = "foreign".into();
        let position = |line| crate::schema::Position::new(line, 1).unwrap();
        let guards = (1..=5)
            .map(|ordinal| {
                (
                    ordinal,
                    (
                        "src/lib.rs".into(),
                        crate::schema::Guard {
                            site: position(ordinal * 10)..position(ordinal * 10 + 2),
                            mutated: (ordinal != 4).then(|| position(ordinal * 10)..position(ordinal * 10 + 1)),
                        },
                    ),
                )
            })
            .collect();
        let mut converger = Converger {
            splices: super::super::Splices::seeded_guards_for_test(work.root.clone(), guards),
            withdrawn: HashSet::from_iter([2]),
            proof_background: HashSet::from_iter([3]),
            ..Converger::default()
        };
        let diagnostics = (1..=5)
            .map(|ordinal| {
                json!({
                    "reason": "compiler-message",
                    "package_id": "subject 0.0.0",
                    "target": {"name": "subject", "kind": ["lib"]},
                    "message": {
                        "level": "error", "message": "independent type error", "code": {"code": "E0308"},
                        "spans": [{
                            "file_name": "src/lib.rs", "is_primary": true,
                            "line_start": ordinal * 10, "column_start": 1,
                            "line_end": ordinal * 10, "column_end": 2
                        }]
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut calls = 0;
        let mut proofs = Proofs::default();
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let active = [population[0], population[1], population[3], population[4]];
        let packages = ["subject".to_owned()];
        let scope = BuildScope {
            roots: None,
            mutants: Some(&packages),
            publish_progress: false,
        };
        super::super::with_compiler(
            move |_, _, _, _| {
                calls += 1;
                assert_eq!(calls, 1, "the populated proof must be cached");
                Ok(Some(super::super::tests::scripted_output(false, &diagnostics, "")))
            },
            || {
                let mut events = ProofEvents::default();
                assert_eq!(
                    converger
                        .subset_fails(
                            &work,
                            &plan,
                            scope,
                            &["check"],
                            BuildLimits::default(),
                            &mut events,
                            &population,
                            &active,
                            &mut proofs,
                        )
                        .unwrap(),
                    None,
                );
                assert_eq!(proofs.attributed.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
                assert_eq!(proofs.attributed[&1].code, "E0308");
                proofs.attributed.clear();
                assert_eq!(
                    converger
                        .subset_fails(
                            &work,
                            &plan,
                            scope,
                            &["check"],
                            BuildLimits::default(),
                            &mut events,
                            &population,
                            &active,
                            &mut proofs,
                        )
                        .unwrap(),
                    Some(true),
                );
                assert_eq!(proofs.local, 1);
            },
        );
    }

    #[test]
    fn oracle_proofs_replace_the_previous_schema_before_comparing_partition_rebuild_costs() {
        let work = memory_workspace();
        let plan = plan(&work.root, 5);
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut converger = Converger {
            subset_oracle: Some(|active| {
                assert_eq!(*active, HashSet::from_iter([1, 2]));
                Some(false)
            }),
            proof_active: HashSet::from_iter([5]),
            proof_background: HashSet::from_iter([4]),
            ..Converger::default()
        };
        assert_eq!(
            converger
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
                    &mut ProofEvents::default(),
                    &population,
                    &population[..2],
                    &mut Proofs::default(),
                )
                .unwrap(),
            Some(false),
        );
        assert_eq!(converger.proof_cost(&ProofCostIndex::new(&plan), &[1, 2]), (0, 0, true));
    }

    #[test]
    fn deletion_only_proof_diagnostics_still_require_independent_confirmation() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        let position = |line| crate::schema::Position::new(line, 1).unwrap();
        let mut converger = Converger {
            splices: super::super::Splices::seeded_guards_for_test(
                work.root.clone(),
                HashMap::from_iter([(
                    1,
                    (
                        "src/lib.rs".into(),
                        crate::schema::Guard {
                            site: position(10)..position(12),
                            mutated: None,
                        },
                    ),
                )]),
            ),
            ..Converger::default()
        };
        let diagnostics = json!({
            "reason": "compiler-message",
            "package_id": "subject 0.0.0",
            "target": {"name": "subject", "kind": ["lib"]},
            "message": {
                "level": "error", "message": "type error enclosing a deleted assignment", "code": {"code": "E0308"},
                "spans": [{
                    "file_name": "src/lib.rs", "is_primary": true,
                    "line_start": 10, "column_start": 1, "line_end": 10, "column_end": 2
                }]
            }
        })
        .to_string();
        let population = plan.mutants.iter().collect::<Vec<_>>();
        let mut proofs = Proofs::default();
        let result = super::super::with_compiler(
            move |_, _, _, _| Ok(Some(super::super::tests::scripted_output(false, &diagnostics, ""))),
            || {
                converger.subset_fails(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: false,
                    },
                    &["check"],
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                    &population,
                    &population,
                    &mut proofs,
                )
            },
        )
        .unwrap();
        assert_eq!(result, Some(true));
        assert_eq!(proofs.attributed.len(), 0);
    }

    // Population, failure-predicate mode, ordered compiler queries, and minimal surviving ordinals.

    #[test]
    fn far_reaching_partitions_reduce_proofs_without_changing_exclusions() {
        thread_local! {
            static QUERIES: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let work = memory_workspace();
        let mut traces = Vec::new();
        for classify in [false, true] {
            let mut plan = plan(&work.root, 8);
            for mutant in &mut plan.mutants {
                mutant.item_path = "shared_item".into();
                mutant.mutator = if classify && [5, 7].contains(&mutant.ordinal) {
                    "stmt.delete_assign"
                } else if mutant.ordinal == 2 {
                    "future.unclassified"
                } else {
                    "literal.int_to_zero"
                }
                .into();
            }
            QUERIES.with_borrow_mut(Vec::clear);
            let mut converger = Converger {
                subset_oracle: Some(|active| {
                    let mut key = active.iter().copied().collect::<Vec<_>>();
                    key.sort_unstable();
                    QUERIES.with_borrow_mut(|queries| queries.push(key));
                    Some(active.contains(&5) || active.contains(&7))
                }),
                ..Converger::default()
            };
            let isolated = converger
                .isolate_scoped(
                    &work,
                    &plan,
                    &error_stream(),
                    &["check"],
                    None,
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                )
                .unwrap();
            assert!(isolated.iter().all(|result| matches!(result, Isolation::Blamed(_))));
            assert_eq!(converger.withdrawn, HashSet::from_iter([5, 7]));
            assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([5, 7]));
            traces.push(QUERIES.with_borrow(Clone::clone));
        }
        assert_eq!(traces[0][0], (1..=8).collect::<Vec<_>>());
        assert_eq!(traces[1][0], traces[0][0]);
        assert_eq!((traces[0].len(), traces[1].len()), (9, 7), "{traces:?}");
    }

    #[test]
    fn retained_partitions_reduce_repeated_narrowing_without_extra_clean_sibling_proofs() {
        thread_local! {
            static FAILURES: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
            static QUERIES: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let work = memory_workspace();
        let plan = plan(&work.root, 16);
        for (failures, expected_builds) in [(vec![1], 6), (vec![1, 9], 11), (vec![1, 5, 9, 13], 21)] {
            FAILURES.with_borrow_mut(|values| values.clone_from(&failures));
            QUERIES.with_borrow_mut(Vec::clear);
            let mut converger = Converger {
                subset_oracle: Some(|active| {
                    let mut key = active.iter().copied().collect::<Vec<_>>();
                    key.sort_unstable();
                    QUERIES.with_borrow_mut(|queries| queries.push(key));
                    Some(FAILURES.with_borrow(|failures| failures.iter().any(|ordinal| active.contains(ordinal))))
                }),
                ..Converger::default()
            };
            let isolated = converger
                .isolate_scoped(
                    &work,
                    &plan,
                    &error_stream(),
                    &["check"],
                    None,
                    BuildLimits::default(),
                    &mut ProofEvents::default(),
                )
                .unwrap();
            assert!(isolated.iter().all(|result| matches!(result, Isolation::Blamed(_))));
            assert_eq!(converger.withdrawn, failures.iter().copied().collect());
            assert_eq!(converger.isolation_budget.proofs, expected_builds, "{failures:?}");
            QUERIES.with_borrow(|queries| {
                assert_eq!(queries.len(), expected_builds);
                assert_eq!(queries.iter().collect::<HashSet<_>>().len(), queries.len());
                if failures.len() > 1 {
                    assert!(queries.contains(&(9..=16).collect::<Vec<_>>()));
                    assert!(!queries.contains(&(2..=8).collect::<Vec<_>>()));
                }
            });
        }
    }

    #[test]
    fn retained_clean_partitions_do_not_hide_a_cross_partition_interaction_after_individual_blame() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 5);
        for mutant in &mut plan.mutants {
            mutant.mutator = match mutant.ordinal {
                1 => "stmt.delete_assign",
                5 => "future.unclassified",
                _ => "literal.int_to_zero",
            }
            .into();
        }
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&2) || active.contains(&1) && active.contains(&5))),
            ..Converger::default()
        };
        let isolated = converger
            .isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
            .unwrap();
        assert!(
            isolated
                .iter()
                .any(|result| matches!(result, Isolation::Blamed(ordinals) if ordinals == &[2]))
        );
        assert!(
            isolated
                .iter()
                .any(|result| matches!(result, Isolation::Interaction { members, .. } if members == &[1, 5]))
        );
        assert_eq!(converger.withdrawn, HashSet::from_iter([2]));
        assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([2]));
        assert!(converger.isolation_budget.proofs < MAX_ISOLATION_PROOFS);
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
            let mut accepted: Value = from_str(&error_stream()).unwrap();
            accepted["target"]["kind"] = json!(kinds);
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
        static ITERATION_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    /// Charges loop visits, not proofs, against the current thread's test-only progress allowance.
    pub(super) fn visit_isolation_iteration() {
        ITERATION_BUDGET.with(|budget| {
            if let Some(remaining) = budget.get() {
                budget.set(Some(
                    remaining
                        .checked_sub(1)
                        .expect("the deterministic isolation script has a finite iteration budget"),
                ));
            }
        });
    }

    /// Restores the outer progress allowance on normal return and unwind.
    fn with_iteration_budget<T>(iterations: usize, body: impl FnOnce() -> T) -> T {
        struct Restore(Option<usize>);
        impl Drop for Restore {
            fn drop(&mut self) {
                ITERATION_BUDGET.set(self.0);
            }
        }
        let _restore = Restore(ITERATION_BUDGET.replace(Some(iterations)));
        body()
    }

    #[test]
    fn recovered_isolation_not_reproduced_and_inconclusive_results_exit_the_current_tier_without_retrying_cached_proofs() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        for inconclusive in [false, true] {
            let mut converger = Converger {
                subset_oracle: Some(if inconclusive { |_| None } else { |_| Some(false) }),
                ..Converger::default()
            };
            let mut events = ProofEvents::default();
            // Either answer must leave this tier without revisiting its cached proof.
            let results = with_iteration_budget(1, || {
                converger.isolate_scoped(
                    &work,
                    &plan,
                    &error_stream(),
                    &["check", "--tests"],
                    None,
                    BuildLimits::default(),
                    &mut events,
                )
            })
            .unwrap();
            assert_eq!(converger.isolation_budget.proofs, 1);
            assert!(converger.withdrawn.is_empty());
            assert!(converger.census.is_empty());
            if inconclusive {
                assert!(matches!(results.as_slice(), [Isolation::Unresolved { ordinals, context }]
                    if ordinals == &[1] && context == "subject (lib)"));
                assert_eq!(
                    events.warnings,
                    ["left subject (lib) unresolved after 1 local and 1 invocation proof builds"]
                );
            } else {
                assert!(results.is_empty());
                assert_eq!(events.warnings.len(), 0);
            }
        }
    }

    #[test]
    fn recovered_isolation_stops_before_another_candidate_attempt_when_a_resolved_proof_exactly_exhausts_the_invocation_budget() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 2);
        plan.mutants[0].item_path = "first".into();
        plan.mutants[1].item_path = "second".into();
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1))),
            isolation_budget: IsolationBudget {
                contexts: 0,
                proofs: MAX_INVOCATION_ISOLATION_PROOFS - 2,
            },
            ..Converger::default()
        };
        // The last admissible proof must exit before another candidate-loop visit.
        let results = with_iteration_budget(1, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        })
        .unwrap();
        assert!(matches!(results.as_slice(), [Isolation::Blamed(ordinals)] if ordinals == &[1]));
        assert_eq!(converger.isolation_budget.proofs, MAX_INVOCATION_ISOLATION_PROOFS);
        assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
        assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
    }

    #[test]
    fn recovered_isolation_resets_the_local_proof_budget_for_distinct_contexts_sharing_the_same_cargo_verb() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 128);
        for mutant in &mut plan.mutants {
            mutant.package = if mutant.ordinal <= 64 { "alpha" } else { "beta" }.into();
            mutant.item_path = format!("item_{:03}", mutant.ordinal).into();
        }
        plan.reach = ["alpha", "beta"]
            .map(|package| (package.to_owned(), HashSet::from_iter([package.to_owned()])))
            .into_iter()
            .collect();
        let stdout = ["alpha", "beta"]
            .map(|package| {
                let mut message: Value = from_str(&error_stream()).unwrap();
                message["package_id"] = format!("{package} 0.0.0").into();
                message.to_string()
            })
            .join("\n");
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(!active.is_empty())),
            ..Converger::default()
        };
        // Headroom spans both contexts; the assertions pin the independently enforced proof cap.
        let results = with_iteration_budget(64, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &stdout,
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        })
        .unwrap();
        assert_eq!(converger.isolation_budget.proofs, 64);
        assert!(converger.withdrawn.iter().any(|ordinal| *ordinal <= 64));
        assert!(converger.withdrawn.iter().any(|ordinal| *ordinal > 64));
        assert!(results.iter().all(|result| matches!(result, Isolation::Blamed(_))));
        assert_eq!(converger.census.len(), converger.withdrawn.len());
    }

    #[test]
    fn recovered_isolation_skips_an_already_resolved_context_without_losing_a_later_independent_package() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 2);
        plan.mutants[0].package = "alpha".into();
        plan.mutants[1].package = "beta".into();
        plan.reach.clear();
        let stdout = [("alpha", "first"), ("alpha", "second"), ("beta", "third")]
            .map(|(package, target)| {
                let mut message: Value = from_str(&error_stream()).unwrap();
                message["package_id"] = format!("{package} 0.0.0").into();
                message["target"]["name"] = target.into();
                message.to_string()
            })
            .join("\n");
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(!active.is_empty())),
            ..Converger::default()
        };
        // Each independent package gets a visit; the duplicate alpha diagnostic must not get another.
        let results = with_iteration_budget(2, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &stdout,
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        })
        .unwrap();
        assert!(matches!(results.as_slice(), [Isolation::Blamed(first), Isolation::Blamed(second)] if first == &[1] && second == &[2]));
        assert_eq!(converger.withdrawn, HashSet::from_iter([1, 2]));
        assert_eq!(converger.isolation_budget.proofs, 2);
        assert_eq!(converger.isolation_budget.contexts, 2);
    }

    #[test]
    fn recovered_isolation_does_not_reintroduce_an_interaction_exclusion_when_widening_from_file_to_package() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 3);
        plan.mutants[2].file = Utf8PathBuf::from("src/other.rs").into();
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1) && active.contains(&2))),
            ..Converger::default()
        };
        // Permit file-to-package widening, but fail repeated cached interaction attempts.
        let results = with_iteration_budget(4, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        })
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], Isolation::Interaction { excluded, members } if members == &[1, 2] && members.contains(excluded)));
        assert!(converger.withdrawn.is_empty());
        assert!(converger.census.is_empty());
    }

    #[test]
    fn recovered_isolation_widens_past_an_unresolved_diagnostic_item_before_declaring_the_context_unresolved() {
        let work = memory_workspace();
        let mut plan = plan(&work.root, 2);
        plan.mutants[0].item_path = "first".into();
        plan.mutants[1].item_path = "second".into();
        let position = |line| crate::schema::Position::new(line, 1).unwrap();
        let guards = [1, 2]
            .map(|ordinal| {
                (
                    ordinal,
                    crate::schema::Guard {
                        site: position(ordinal * 10)..position(ordinal * 10 + 1),
                        mutated: None,
                    },
                )
            })
            .into_iter()
            .collect();
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&2))),
            ..Converger::default()
        };
        converger.splices.placed.insert("src/lib.rs".into(), (vec![1, 2], guards));
        let mut stream: Value = from_str(&error_stream()).unwrap();
        stream["message"]["spans"][0]["line_start"] = 10.into();
        // Permit the diagnostic-item miss and wider tiers needed to find the other item's singleton.
        let results = with_iteration_budget(4, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &stream.to_string(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        })
        .unwrap();
        assert!(matches!(results.as_slice(), [Isolation::Blamed(ordinals)] if ordinals == &[2]));
        assert_eq!(converger.withdrawn, HashSet::from_iter([2]));
        assert_eq!(converger.isolation_budget.proofs, 3);
    }

    #[test]
    fn recovered_isolation_returns_policy_persistence_failure_before_publishing_the_singleton_verdict() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/isolation-graph/application");
        let work = Workspace::adopt(root, "never-written-exclusion-target".into());
        let plan = plan(&work.root, 1);
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1))),
            compiler_exclusions: Some(from_value(json!({"version": 1, "mutants": {}})).unwrap()),
            exclusion_store: Some(|_, _| Err(error!("named singleton exclusion-store refusal"))),
            ..Converger::default()
        };
        let expected = "named singleton exclusion-store refusal";
        // Persistence refusal must exit the singleton's first eligible-population visit.
        let result = with_iteration_budget(1, || {
            converger.isolate_scoped(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
            )
        });
        let Err(failure) = result else {
            panic!("the injected exclusion store must refuse the singleton verdict");
        };
        assert_eq!(failure.message(), expected);
        assert!(converger.withdrawn.is_empty());
        assert!(converger.census.is_empty());
        assert_eq!(converger.isolation_budget.proofs, 1);
    }

    #[test]
    fn recovered_actual_proof_execution_reports_exact_round_historical_census_and_proof_count_before_the_injected_compiler() {
        let work = memory_workspace();
        let plan = plan(&work.root, 2);
        let mut converger = Converger {
            total_rounds: 11,
            historical_exclusions: 7,
            census: [(50, CompilerReason::isolated()), (60, CompilerReason::isolated())]
                .into_iter()
                .collect(),
            proof_background: HashSet::from_iter([2]),
            proof_active: HashSet::from_iter([99]),
            isolation_budget: IsolationBudget { contexts: 3, proofs: 13 },
            ..Converger::default()
        };
        let mut events = ProofEvents::default();
        let result = super::super::with_compiler(
            |_work, arguments, _budget, _events| {
                assert_eq!(arguments, ["check", "--tests", "--message-format=json", "--workspace"]);
                Err(error!("the in-memory proof compiler refuses execution"))
            },
            || {
                converger.run_proof(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: false,
                    },
                    &["check", "--tests"],
                    BuildLimits::default(),
                    &mut events,
                    17,
                    &HashSet::from_iter([1]),
                    &mut HashMap::default(),
                )
            },
        );
        assert_eq!(result.unwrap_err().message(), "the in-memory proof compiler refuses execution");
        assert_eq!(events.activity, [(11, 9, 13)]);
        assert_eq!(events.observations.len(), 0);
        assert_eq!(converger.proof_active, HashSet::from_iter([1, 2]));
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
                mutant.mutator = match mutant.ordinal {
                    1 | 3 => "stmt.delete_assign",
                    4 => "future.unclassified",
                    _ => "literal.int_to_zero",
                }
                .into();
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
    fn recovered_interaction_minimization_preserves_cross_partition_members_and_caches_repeated_proofs() {
        let root = Utf8PathBuf::from("synthetic-minimization");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 6);
        let population: Vec<_> = plan.mutants.iter().collect();
        let mut converger = Converger {
            subset_oracle: Some(|active| Some([1, 3, 5].iter().all(|ordinal| active.contains(ordinal)))),
            ..Converger::default()
        };
        let mut proofs = Proofs::default();
        let members = converger
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
        assert_eq!(members, Some(vec![1, 3, 5]));
        assert_eq!(proofs.local, 25);
        assert_eq!(proofs.results.len(), 25);
        assert_eq!(converger.isolation_budget.proofs, 25);
        assert_eq!(proofs.results[&vec![1, 3, 4, 5, 6]], Some(true));
        assert_eq!(proofs.results[&vec![1, 3, 5, 6]], Some(true));
        assert_eq!(proofs.results[&vec![1, 3, 5]], Some(true));
        for pair in [vec![1, 3], vec![1, 5], vec![3, 5]] {
            assert_eq!(proofs.results[&pair], Some(false));
        }
        assert!(converger.withdrawn.is_empty());
    }

    #[test]
    fn recovered_interaction_minimization_replaces_the_population_with_a_failing_chunk() {
        let root = Utf8PathBuf::from("synthetic-chunk-minimization");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = plan(&root, 6);
        let population: Vec<_> = plan.mutants.iter().collect();
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1) && active.contains(&2))),
            ..Converger::default()
        };
        let mut proofs = Proofs::default();
        let members = converger
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
        assert_eq!(members, Some(vec![1, 2]));
        assert_eq!(proofs.local, 4);
        assert_eq!(proofs.results.len(), 4);
        assert_eq!(proofs.results[&vec![1, 2, 3]], Some(true));
        assert_eq!(proofs.results[&vec![1, 2]], Some(true));
        assert_eq!(proofs.results[&vec![1]], Some(false));
        assert_eq!(proofs.results[&vec![2]], Some(false));
    }

    #[test]
    fn recovered_affected_components_expand_from_both_edge_directions_without_admitting_empty_edges() {
        let reach = HashMap::from_iter([
            ("root".into(), HashSet::from_iter(["leaf".into(), "spare".into()])),
            ("next".into(), HashSet::from_iter(["root".into(), "last".into()])),
            ("disconnected".into(), HashSet::from_iter(["outside".into()])),
            ("empty".into(), HashSet::default()),
        ]);
        let expected = HashSet::from_iter(["root".into(), "leaf".into(), "spare".into(), "next".into(), "last".into()]);
        for seed in ["root", "leaf", "last"] {
            let mut affected = HashSet::from_iter([seed.into()]);
            extend_affected_packages(&mut affected, &reach);
            assert_eq!(affected, expected, "{seed}");
        }
    }

    #[test]
    fn recovered_background_guards_exclude_ineligible_mutants_even_without_a_failure_context() {
        let root = Utf8PathBuf::from("synthetic-background");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = plan(&root, 4);
        plan.mutants[1].ordinal = 0;
        let mut converger = Converger {
            withdrawn: HashSet::from_iter([3]),
            ..Converger::default()
        };
        let result = converger
            .isolate_scoped(
                &work,
                &plan,
                "",
                &["check"],
                None,
                BuildLimits::default(),
                &mut crate::testing::Recorder::default(),
            )
            .unwrap();
        assert!(result.is_empty());
        assert_eq!(converger.proof_background, HashSet::from_iter([1, 4]));
        assert_eq!(converger.isolation_budget.proofs, 0);
    }

    #[test]
    fn recovered_item_partitioning_does_not_spend_proofs_on_fragments_of_an_existing_item() {
        let root = Utf8PathBuf::from("synthetic-item-partitions");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = plan(&root, 6);
        for mutant in &mut plan.mutants {
            mutant.item_path = format!("item{}", (mutant.ordinal - 1) / 2).into();
        }
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&1) && active.contains(&2))),
            ..Converger::default()
        };
        let mut proofs = Proofs::default();
        let result = converger
            .isolate_candidates(
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
                &(1..=6).collect(),
                &mut proofs,
            )
            .unwrap();
        assert!(matches!(
            result,
            IsolationAttempt::Resolved(Isolation::Interaction { excluded: 1, members })
                if members == [1, 2]
        ));
        let mut schemas = proofs.results.keys().cloned().collect::<Vec<_>>();
        schemas.sort_unstable();
        assert_eq!(schemas, [vec![1], vec![1, 2], vec![1, 2, 3, 4, 5, 6], vec![2]]);
        assert_eq!(proofs.local, 4);
    }

    #[test]
    fn recovered_complement_reductions_decrease_granularity_before_spending_the_remaining_proof_budget() {
        for count in [12, 30] {
            let root = Utf8PathBuf::from("synthetic-granularity");
            let work = Workspace::adopt(root.clone(), root.join("target"));
            let plan = plan(&root, count);
            let population = plan.mutants.iter().collect::<Vec<_>>();
            let oracle: fn(&HashSet<u32>) -> Option<bool> = if count == 12 {
                |active| Some([2, 5, 8].iter().all(|ordinal| active.contains(ordinal)))
            } else {
                |active| Some(active.contains(&2) && active.contains(&18))
            };
            let mut converger = Converger {
                subset_oracle: Some(oracle),
                ..Converger::default()
            };
            let mut proofs = Proofs::default();
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
            assert_eq!(proofs.local, MAX_ISOLATION_PROOFS);
            let witness = if count == 12 {
                vec![1, 2, 5, 6, 7, 8]
            } else {
                (1..=4).chain(17..=24).collect()
            };
            assert_eq!(proofs.results.get(&witness), Some(&Some(true)), "{count}");
        }
    }

    #[test]
    fn recovered_diagnostic_item_tiers_have_stable_source_order_independent_of_hash_iteration() {
        let mut plan = plan(Utf8Path::new("synthetic-order"), 16);
        for mutant in &mut plan.mutants {
            mutant.item_path = format!("item{:02}", mutant.ordinal).into();
        }
        let envelopes = plan
            .mutants
            .iter()
            .map(|mutant| ((mutant.file.as_ref(), mutant.item_path.as_ref()), (1, 1)))
            .collect::<HashMap<_, _>>();
        assert!(!envelopes.keys().map(|(_file, item)| *item).is_sorted());
        let context = FailureContext {
            package: "subject".into(),
            target: "subject".into(),
            kind: "lib".into(),
            proof_verb: vec!["check".into()],
            files: HashSet::from_iter([Utf8PathBuf::from("src/lib.rs")]),
            codes: HashSet::default(),
            locations: HashMap::from_iter([(Utf8PathBuf::from("src/lib.rs"), vec![1])]),
        };
        let mut tiers = Vec::new();
        local_diagnostic_tiers(&Converger::default(), &context, &plan, &(1..=16).collect(), &envelopes, &mut tiers);
        assert_eq!(tiers, (1..=16).map(|ordinal| HashSet::from_iter([ordinal])).collect::<Vec<_>>());
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
    fn recovered_only_the_last_population_suppresses_a_repeated_isolation_tier() {
        let plan = plan(Utf8Path::new("root"), 2);
        let a = HashSet::from_iter([1]);
        let b = HashSet::from_iter([2]);
        let mut tiers = Vec::new();
        for population in [&a, &b, &a, &a] {
            push_isolation_tiers(&mut tiers, population.clone(), &plan, &HashSet::default());
        }
        assert_eq!(tiers, [a.clone(), b.clone(), a]);
        push_isolation_tiers(&mut tiers, b.clone(), &plan, &HashSet::default());
        assert_eq!(tiers.last(), Some(&b));
        assert_eq!(tiers.len(), 4);
    }

    #[test]
    fn oct25_likely_subset_is_compared_to_the_last_tier_not_the_first() {
        let mut plan = plan(Utf8Path::new("synthetic-likely-tiers"), 3);
        plan.mutants[0].mutator = "literal.bool_flip".into();
        plan.mutants[1].mutator = "literal.bool_flip".into();
        plan.mutants[2].mutator = "literal.int_to_one".into();
        let population = HashSet::from_iter([1, 2, 3]);
        let codes = HashSet::from_iter(["E0308".into()]);
        let likely = HashSet::from_iter([3]);
        let earlier = HashSet::from_iter([99]);
        let mut tiers = vec![earlier.clone(), likely.clone()];
        push_isolation_tiers(&mut tiers, population.clone(), &plan, &codes);
        assert_eq!(tiers, [earlier, likely.clone(), population.clone()]);
        let mut tiers = vec![likely.clone(), HashSet::from_iter([98])];
        push_isolation_tiers(&mut tiers, population.clone(), &plan, &codes);
        assert_eq!(tiers, [likely.clone(), HashSet::from_iter([98]), likely, population]);
    }

    #[test]
    fn oct25_context_tiers_deduplicate_a_likely_subset_repeated_after_a_local_population() {
        let mut plan = plan(Utf8Path::new("synthetic-bounded-tiers"), 3);
        for mutant in &mut plan.mutants {
            mutant.item_path = "local".into();
            mutant.mutator = "literal.bool_flip".into();
        }
        plan.mutants[0].mutator = "literal.int_to_one".into();
        plan.mutants[2].item_path = "outside".into();
        let converger = Converger::default();
        let by_package = isolation_population(&plan, &converger.withdrawn);
        let packages = vec!["subject".to_owned()];
        let dependencies = context_dependencies(&by_package, &packages);
        let mut context = context("subject", "lib");
        context.codes.insert("E0308".into());
        context.files.insert("src/lib.rs".into());
        context.locations.insert("src/lib.rs".into(), vec![10]);
        let envelopes = HashMap::from_iter([((Utf8Path::new("src/lib.rs"), "local"), (9, 11))]);
        let (tiers, diagnostic_end) = context_tiers(&converger, &context, &plan, &by_package, &packages, &dependencies, &envelopes);
        assert_eq!(
            tiers,
            [HashSet::from_iter([1]), HashSet::from_iter([1, 2]), HashSet::from_iter([1, 2, 3])]
        );
        assert_eq!(diagnostic_end, 2);
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

    pub(super) fn plan(root: &Utf8Path, mutants: usize) -> Plan {
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
        json!({
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
    fn cor2_positional_confirmation_preserves_the_failed_verb_even_with_frozen_roots() {
        let work = memory_workspace();
        let plan = plan(&work.root, 1);
        let roots = vec!["subject".to_owned()];
        for frozen in [false, true] {
            let mut converger = Converger::default();
            if frozen {
                converger.freeze_context(Some(&roots));
            }
            let result = super::super::with_compiler(
                |_work, arguments, _budget, _events| {
                    assert_eq!(
                        arguments,
                        [
                            "build",
                            "--lib",
                            "--features",
                            "special",
                            "--message-format=json",
                            "--package",
                            "subject"
                        ]
                    );
                    Err(error!("the positional proof retained its original invocation"))
                },
                || {
                    converger.isolate_with_candidates(
                        &work,
                        &plan,
                        &error_stream(),
                        &["build", "--lib", "--features", "special"],
                        Some(&roots),
                        BuildLimits::default(),
                        &mut ProofEvents::default(),
                        Some(&HashSet::from_iter([1])),
                    )
                },
            );
            assert_eq!(
                result.unwrap_err().message(),
                "the positional proof retained its original invocation"
            );
            assert_eq!(converger.isolation_budget.proofs, 1);
            assert!(converger.census.is_empty());
            assert!(converger.withdrawn.is_empty());
        }
    }

    #[test]
    fn cor2_inconclusive_positional_proofs_preserve_all_candidates_without_exclusion() {
        let work = memory_workspace();
        let plan = plan(&work.root, 2);
        let mut converger = Converger {
            subset_oracle: Some(|_| None),
            ..Converger::default()
        };
        let results = converger
            .isolate_with_candidates(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
                Some(&HashSet::from_iter([1])),
            )
            .unwrap();
        assert!(matches!(results.as_slice(), [Isolation::Unresolved { ordinals, .. }]
            if ordinals.iter().copied().collect::<HashSet<_>>() == HashSet::from_iter([1, 2])));
        assert!(converger.census.is_empty());
        assert!(converger.withdrawn.is_empty());
        assert!(converger.isolation_budget.proofs <= MAX_ISOLATION_PROOFS);
    }

    #[test]
    #[cfg_attr(miri, ignore = "checks the production 4096-candidate boundary with a native-scale population")]
    fn positional_tiers_are_first_intersect_dependencies_and_obey_the_inclusive_size_limit() {
        thread_local! {
            static PROOFS: std::cell::RefCell<Vec<Vec<u32>>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let work = memory_workspace();
        let mut plan = plan(&work.root, 4097);
        for mutant in &mut plan.mutants {
            mutant.item_path = "same_item".into();
        }
        for (positional, expected_first, expected_proofs) in [
            (HashSet::from_iter([2, 9999]), vec![2], 3),
            ((2..=4097).collect(), (2..=4097).collect(), 3),
            ((1..=4097).collect(), (1..=4096).collect(), 2),
            (HashSet::default(), (1..=4096).collect(), 2),
            (HashSet::from_iter([9999]), (1..=4096).collect(), 2),
        ] {
            PROOFS.with_borrow_mut(Vec::clear);
            let mut converger = Converger {
                subset_oracle: Some(|active| {
                    let mut active = active.iter().copied().collect::<Vec<_>>();
                    active.sort_unstable();
                    PROOFS.with_borrow_mut(|proofs| proofs.push(active));
                    None
                }),
                ..Converger::default()
            };
            let mut events = ProofEvents::default();
            let results = converger
                .isolate_with_candidates(
                    &work,
                    &plan,
                    &error_stream(),
                    &["check", "--tests"],
                    None,
                    BuildLimits::default(),
                    &mut events,
                    Some(&positional),
                )
                .unwrap();
            PROOFS.with_borrow(|proofs| {
                assert_eq!(proofs.first(), Some(&expected_first));
                assert_eq!(proofs.len(), expected_proofs);
                assert!(proofs.iter().all(|proof| !proof.is_empty() && proof.len() <= 4096));
            });
            let [Isolation::Unresolved { ordinals, .. }] = results.as_slice() else {
                panic!("inconclusive proofs must preserve the complete unresolved context: {results:?}");
            };
            assert_eq!(ordinals.iter().copied().collect::<HashSet<_>>(), (1..=4097).collect());
            assert!(converger.withdrawn.is_empty());
            assert!(converger.census.is_empty());
            assert_eq!(
                events.warnings,
                [format!(
                    "left subject (lib) unresolved after {expected_proofs} local and {expected_proofs} invocation proof builds"
                )]
            );
        }
    }

    #[test]
    fn a_confirmed_positional_tier_stops_before_proving_unrelated_diagnostic_tiers() {
        let work = memory_workspace();
        let plan = plan(&work.root, 3);
        let mut converger = Converger {
            subset_oracle: Some(|active| Some(active.contains(&2) || active.contains(&3))),
            ..Converger::default()
        };
        let results = converger
            .isolate_with_candidates(
                &work,
                &plan,
                &error_stream(),
                &["check", "--tests"],
                None,
                BuildLimits::default(),
                &mut ProofEvents::default(),
                Some(&HashSet::from_iter([2])),
            )
            .unwrap();
        assert!(matches!(results.as_slice(), [Isolation::Blamed(ordinals)] if ordinals == &[2]));
        assert_eq!(converger.withdrawn, HashSet::from_iter([2]));
        assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([2]));
        assert_eq!(converger.isolation_budget.proofs, 1);
    }

    #[test]
    fn recovered_diagnostic_envelopes_include_late_guards_and_both_item_boundaries() {
        let root = Utf8PathBuf::from("synthetic-envelope");
        let mut plan = plan(&root, 5);
        for mutant in &mut plan.mutants {
            mutant.item_path = "shared".into();
        }
        plan.mutants[0].file = Utf8PathBuf::from("missing.rs").into();
        plan.mutants[4].file = Utf8PathBuf::from("other.rs").into();
        let position = |line| crate::schema::Position::new(line, 1).unwrap();
        let guards = [(2, 20, 21), (3, 5, 6), (4, 30, 31)]
            .into_iter()
            .map(|(ordinal, start, end)| {
                (
                    ordinal,
                    crate::schema::Guard {
                        site: position(start)..position(end),
                        mutated: None,
                    },
                )
            })
            .collect();
        let mut converger = Converger::default();
        converger.splices.placed.insert("src/lib.rs".into(), (Vec::new(), guards));
        let envelopes = diagnostic_envelopes(&converger, &plan);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[&(Utf8Path::new("src/lib.rs"), "shared")], (5, 31));
        let dependencies = HashSet::from_iter([2, 3, 4, 5]);
        for (lines, included, expected) in [
            (vec![4], true, false),
            (vec![5], true, true),
            (vec![31], true, true),
            (vec![32], true, false),
            (vec![4, 6, 99], true, true),
            (vec![6], false, false),
        ] {
            let mut context = context("subject", "lib");
            if included {
                context.files.insert("src/lib.rs".into());
            }
            context.locations.insert("src/lib.rs".into(), lines);
            let mut tiers = Vec::new();
            local_diagnostic_tiers(&converger, &context, &plan, &dependencies, &envelopes, &mut tiers);
            assert_eq!(
                tiers,
                if expected {
                    vec![HashSet::from_iter([2, 3, 4])]
                } else {
                    Vec::new()
                }
            );
        }
    }

    #[test]
    fn recovered_context_tiers_partition_sorted_ordinals_at_the_exact_limit_without_duplicates() {
        let limit = MAX_ISOLATION_CANDIDATES;
        let mut plan = plan(Utf8Path::new("synthetic-tiers"), limit * 2 + 1);
        for mutant in &mut plan.mutants[..limit] {
            mutant.file = Utf8PathBuf::from("first.rs").into();
        }
        plan.mutants[0].mutator = "literal.bool_flip".into();
        plan.mutants.reverse();
        let converger = Converger::default();
        let by_package = isolation_population(&plan, &converger.withdrawn);
        let packages = vec!["subject".to_owned()];
        let dependencies = context_dependencies(&by_package, &packages);
        let mut context = context("subject", "lib");
        context.files.insert("first.rs".into());
        let (tiers, diagnostic_end) = context_tiers(
            &converger,
            &context,
            &plan,
            &by_package,
            &packages,
            &dependencies,
            &HashMap::default(),
        );
        let limit = u32::try_from(limit).unwrap();
        assert_eq!(diagnostic_end, 0);
        assert_eq!(
            tiers,
            [
                (1..=limit).collect::<HashSet<_>>(),
                (limit + 1..=limit * 2).collect(),
                HashSet::from_iter([limit * 2 + 1]),
            ]
        );
        context.codes.insert("E0308".into());
        let (tiers, _) = context_tiers(
            &converger,
            &context,
            &plan,
            &by_package,
            &packages,
            &dependencies,
            &HashMap::default(),
        );
        for (index, tier) in tiers.iter().enumerate() {
            assert!(!tiers[..index].contains(tier));
        }
        assert!(tiers.iter().all(|tier| tier.len() <= MAX_ISOLATION_CANDIDATES));
        assert_eq!(tiers[0], (2..=limit).collect());
        assert_eq!(tiers[1], (1..=limit).collect());
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
        let mut stream: Value = from_str(&error_stream()).expect("the static error stream is valid Cargo JSON");
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
            json!({"reason": "compiler-artifact"}).to_string(),
            json!({
                "reason": "compiler-message",
                "message": {"level": "error"}
            })
            .to_string(),
            json!({
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
            let mut stream: Value = from_str(&error_stream()).expect("static Cargo JSON parses");
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
            assert_eq!(events.warnings.len(), 0);
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
