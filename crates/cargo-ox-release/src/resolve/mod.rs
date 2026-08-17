// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The deterministic release resolver.
//!
//! It performs only mechanical work: token parsing, version arithmetic,
//! dependency closure, type-exposure and macro-contract propagation, pin
//! reconciliation, and topological ordering. Classifying source diffs and
//! reviewing proc-macro behavior are the caller's responsibility, supplied
//! through the request.
//!
//! All mutable state lives on [`Resolver`]. The public entry point is
//! [`resolve`].

mod cascade;
mod classify;
mod evidence;
mod macros;
mod selection;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use ohno::{AppError, bail};

use crate::model::{
    Ambiguity, Facts, MacroContractOutput, PackageFact, Plan, PlanStatus, Request, SelectionDecisionOutput, normalize_ident,
};
use crate::version::ChangeType;

/// The release mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Release only the explicitly tokenized packages plus their cascade.
    Targeted,
    /// Release every modified candidate (requires selection decisions).
    Changed,
    /// Consider every published package (requires selection decisions).
    All,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "targeted" => Ok(Self::Targeted),
            "changed" => Ok(Self::Changed),
            "all" => Ok(Self::All),
            other => bail!("Unknown release mode '{other}'."),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Targeted => "targeted",
            Self::Changed => "changed",
            Self::All => "all",
        }
    }

    fn requires_selection(self) -> bool {
        matches!(self, Self::Changed | Self::All)
    }
}

/// An accept/decline decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Accept,
    Decline,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
        }
    }
}

/// A validated selection decision for one candidate.
#[derive(Debug, Clone)]
pub(crate) struct SelectionDecision {
    pub(crate) folder: String,
    pub(crate) decision: Decision,
    pub(crate) reason: String,
    pub(crate) evidence: Vec<String>,
    pub(crate) regression_evidence: Vec<crate::model::RegressionEvidenceOutput>,
    pub(crate) evidence_issues: Vec<String>,
    pub(crate) regression_shown: bool,
}

/// A validated macro contract.
#[derive(Debug, Clone)]
pub(crate) struct MacroContract {
    /// The change type implied by the declared verdict.
    pub(crate) change_type: ChangeType,
    pub(crate) reviewed_packages: Vec<String>,
    /// Channel name → state (`unchanged`/`changed`/`notapplicable`).
    pub(crate) channels: BTreeMap<String, String>,
    pub(crate) evidence: Vec<String>,
    /// `owner|key` pairs actually evidenced.
    pub(crate) compile_evidence: Vec<evidence::CompileEvidenceEntry>,
    pub(crate) evidence_issues: Vec<String>,
    pub(crate) derived_floor: ChangeType,
    pub(crate) deciding_fixtures: Vec<String>,
}

impl MacroContract {
    fn channel(&self, name: &str) -> &str {
        self.channels.get(name).map_or("", String::as_str)
    }
}

/// A package's objective classification.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Classification {
    pub(crate) change_type: ChangeType,
    pub(crate) manual_review: bool,
}

/// One cascade edge recorded on a dependent's plan entry.
#[derive(Debug, Clone)]
pub(crate) struct CascadeReason {
    pub(crate) target: String,
    pub(crate) version: String,
    pub(crate) breaking: bool,
    pub(crate) edge_class: String,
    pub(crate) judgment: String,
    pub(crate) judgment_source: String,
}

/// A pending or resolved release.
#[derive(Debug, Clone)]
pub(crate) struct PlanEntry {
    pub(crate) source: String,
    pub(crate) requested_pin: Option<String>,
    pub(crate) effective_change_type: ChangeType,
    pub(crate) target_version: String,
    pub(crate) manual_review: bool,
    pub(crate) macro_contract_reviewed: bool,
    pub(crate) contract_breaking: bool,
    /// Cascade edges keyed by the upstream dependency folder.
    pub(crate) reasons: BTreeMap<String, CascadeReason>,
}

/// The resolver's borrowed inputs and owned working state.
pub(crate) struct Resolver<'a> {
    facts: &'a [PackageFact],
    request: &'a Request,
    folder_index: HashMap<String, usize>,
    mode: Mode,
    force: bool,
    selection_decisions: BTreeMap<String, SelectionDecision>,
    warnings: Vec<String>,
    ambiguities: Vec<Ambiguity>,
    ambiguity_keys: HashSet<String>,
    used_macro_contracts: BTreeMap<String, MacroContract>,
    plan: HashMap<String, PlanEntry>,
    queue: VecDeque<String>,
    token_folders: HashSet<String>,
}

/// Resolves a deterministic release plan from facts and a classified request.
///
/// This never publishes or mutates the workspace — it computes the plan
/// document only. A `blocked` plan lists the ambiguities that must be
/// classified before it can resolve. The inputs are borrowed, so a costly facts
/// snapshot can be reused across several requests.
///
/// # Errors
///
/// Returns an error for malformed input the resolver refuses outright (unknown
/// mode, unpublishable token, contradictory pin, missing required facts, and
/// the other hard failures). Recoverable gaps — an unreviewed macro, an
/// underclassified break — surface as a `blocked` plan, not an error.
pub fn resolve(facts: &Facts, request: &Request) -> Result<Plan, AppError> {
    let mut resolver = Resolver::new(facts, request)?;
    resolver.run()
}

impl<'a> Resolver<'a> {
    fn new(facts: &'a Facts, request: &'a Request) -> Result<Self, AppError> {
        if facts.schema_version != crate::model::facts::SCHEMA_VERSION {
            bail!("The facts document uses an unsupported schema version; regenerate it.");
        }
        let packages = facts.packages.as_slice();
        if packages.is_empty() {
            bail!("The facts document contains no workspace packages.");
        }
        let folder_index = packages.iter().enumerate().map(|(i, p)| (p.folder.clone(), i)).collect();

        let mode = match request.mode.as_deref() {
            Some(m) if !m.trim().is_empty() => Mode::parse(m)?,
            _ => Mode::Targeted,
        };
        let force = request.force;

        Ok(Self {
            facts: packages,
            request,
            folder_index,
            mode,
            force,
            selection_decisions: BTreeMap::new(),
            warnings: Vec::new(),
            ambiguities: Vec::new(),
            ambiguity_keys: HashSet::new(),
            used_macro_contracts: BTreeMap::new(),
            plan: HashMap::new(),
            queue: VecDeque::new(),
            token_folders: HashSet::new(),
        })
    }

    /// Looks up a fact by folder. The folder always originates from the facts
    /// list, so a miss is an internal invariant violation.
    fn fact(&self, folder: &str) -> &PackageFact {
        let index = self
            .folder_index
            .get(folder)
            .copied()
            .expect("plan and queue folders are always drawn from the facts list");
        &self.facts[index]
    }

    /// Finds the single package a release token identifies.
    fn find_package_fact(&self, identifier: &str) -> Result<usize, AppError> {
        let normalized = normalize_ident(identifier);
        let mut found = None;
        let mut count = 0;
        for (index, fact) in self.facts.iter().enumerate() {
            if fact.folder == identifier || fact.normalized_name() == normalized {
                found = Some(index);
                count += 1;
            }
        }
        match (count, found) {
            (1, Some(index)) => Ok(index),
            _ => bail!("Release token '{identifier}' matched {count} workspace packages."),
        }
    }

    /// Adds an ambiguity, de-duplicated by `key`.
    fn add_ambiguity(&mut self, key: String, ambiguity: Ambiguity) {
        if self.ambiguity_keys.insert(key) {
            self.ambiguities.push(ambiguity);
        }
    }

    fn run(&mut self) -> Result<Plan, AppError> {
        self.gather_selection_decisions()?;

        let tokens: Vec<String> = self.request.tokens.clone();
        if tokens.is_empty() && self.mode == Mode::Targeted {
            bail!("Release mode 'targeted' requires at least one accepted package token.");
        }

        // Grade selection evidence before any token is expanded, so a plan
        // whose reasons are not demonstrated can never reach the release loop.
        let selection_folders: Vec<String> = self.selection_decisions.keys().cloned().collect();
        for folder in selection_folders {
            self.grade_selection_evidence(&folder)?;
        }

        for token in &tokens {
            self.process_token(token)?;
        }

        if self.mode.requires_selection() {
            self.verify_accepted_tokens()?;
            self.require_declined_macro_obligations()?;
        }

        if !self.ambiguities.is_empty() {
            return Ok(self.blocked_plan());
        }

        self.run_cascade()?;

        if !self.ambiguities.is_empty() {
            return Ok(self.blocked_plan());
        }

        self.resolved_plan()
    }

    fn gather_selection_decisions(&mut self) -> Result<(), AppError> {
        if !self.mode.requires_selection() {
            return Ok(());
        }
        let all_mode = self.mode == Mode::All;
        let candidate_folders: Vec<String> = self
            .facts
            .iter()
            .filter(|f| f.published && (all_mode || f.modified))
            .map(|f| f.folder.clone())
            .collect();

        let Some(decision_map) = self.request.selection_decisions.clone() else {
            bail!("Release mode '{}' requires selectionDecisions.", self.mode.as_str());
        };

        let candidate_set: HashSet<&String> = candidate_folders.iter().collect();
        let mut unknown: Vec<&String> = decision_map.keys().filter(|k| !candidate_set.contains(k)).collect();
        unknown.sort();
        if !unknown.is_empty() {
            let joined = unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            bail!(
                "Selection decisions contain unknown or non-candidate packages: {joined}. Use \
                 canonical folder identifiers."
            );
        }
        let mut missing: Vec<&String> = candidate_folders.iter().filter(|f| !decision_map.contains_key(*f)).collect();
        missing.sort();
        if !missing.is_empty() {
            let joined = missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            bail!("Selection decisions are missing candidate packages: {joined}.");
        }

        for folder in &candidate_folders {
            let input = decision_map.get(folder).expect("candidate is present after the missing-key check");
            let decision = self.parse_selection(folder, input)?;
            self.selection_decisions.insert(folder.clone(), decision);
        }
        Ok(())
    }

    fn verify_accepted_tokens(&self) -> Result<(), AppError> {
        for (folder, decision) in &self.selection_decisions {
            if decision.decision == Decision::Accept && !self.token_folders.contains(folder) {
                bail!("Accepted selection decision '{folder}' is missing a release token.");
            }
        }
        Ok(())
    }

    /// Grades the change-type sourcing so a resolved plan echoes exactly the
    /// packages it released. Emits the sorted selection decisions.
    fn selection_decision_output(&self) -> Vec<SelectionDecisionOutput> {
        self.selection_decisions
            .values()
            .map(|d| SelectionDecisionOutput {
                package: d.folder.clone(),
                decision: d.decision.as_str().to_string(),
                reason: d.reason.clone(),
                evidence: d.evidence.clone(),
                regression_evidence: d.regression_evidence.clone(),
            })
            .collect()
    }

    fn macro_contract_output(&self) -> Vec<MacroContractOutput> {
        self.used_macro_contracts
            .iter()
            .map(|(folder, contract)| MacroContractOutput {
                package: folder.clone(),
                verdict: contract.change_type.macro_verdict_name().to_string(),
                derived_verdict: contract.derived_floor.macro_verdict_name().to_string(),
                reviewed: self.emitted_review_scope(folder),
                evidence: contract.evidence.clone(),
            })
            .collect()
    }

    fn sorted_ambiguities(&self) -> Vec<Ambiguity> {
        let mut sorted = self.ambiguities.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        sorted
    }

    fn blocked_plan(&self) -> Plan {
        Plan {
            status: PlanStatus::Blocked,
            mode: self.mode.as_str().to_string(),
            selection_decisions: self.selection_decision_output(),
            releases: Vec::new(),
            macro_contracts: self.macro_contract_output(),
            ambiguities: self.sorted_ambiguities(),
            warnings: self.warnings.clone(),
        }
    }
}
