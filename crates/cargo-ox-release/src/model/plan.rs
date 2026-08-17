// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The plan contract: the resolver's canonical output.
//!
//! A resolved plan carries the ordered release set; a blocked plan carries the
//! ambiguities that must be classified before the plan can resolve. Both carry
//! the echoed selection decisions and macro contracts.

use serde::{Deserialize, Serialize};

use crate::version::ChangeType;

/// Whether a plan resolved to a release set or is blocked on ambiguities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    /// The plan resolved to an ordered release set.
    Resolved,
    /// The plan is blocked until the listed ambiguities are classified.
    Blocked,
}

/// Whether a release was user-seeded or derived by the cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseSource {
    /// Seeded directly by an accepted release token.
    User,
    /// Pulled in or elevated by the dependency cascade.
    Cascade,
}

/// The resolver's canonical output document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Whether the plan resolved or is blocked.
    pub status: PlanStatus,
    /// The release mode the plan was resolved under.
    pub mode: String,
    /// Echoed selection decisions, sorted by package.
    pub selection_decisions: Vec<SelectionDecisionOutput>,
    /// The ordered release set (empty when blocked).
    pub releases: Vec<ReleaseOutput>,
    /// Echoed macro contracts, sorted by package.
    pub macro_contracts: Vec<MacroContractOutput>,
    /// Ambiguities that block the plan (empty when resolved). Each is a
    /// kind-specific record; the shape varies by `kind`.
    pub ambiguities: Vec<Ambiguity>,
    /// Non-blocking warnings.
    pub warnings: Vec<String>,
}

/// A single release in the resolved set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseOutput {
    /// Directory identifier.
    pub folder: String,
    /// Cargo package name.
    pub name: String,
    /// Current version.
    pub from: String,
    /// Next version.
    pub to: String,
    /// The release's change type.
    pub change_type: ChangeType,
    /// Whether the release was user-seeded or derived by the cascade.
    pub source: ReleaseSource,
    /// Whether the release retains a manual-review flag.
    pub manual_review: bool,
    /// Whether this release breaks a macro contract.
    pub contract_breaking: bool,
    /// Why dependents pulled this release in / elevated it.
    pub cascade_reasons: Vec<CascadeReasonOutput>,
}

/// One edge that contributed to a release's presence or change type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CascadeReasonOutput {
    /// The upstream package name driving this edge.
    pub target: String,
    /// The upstream package's resolved version.
    pub version: String,
    /// Whether this edge is breaking.
    pub breaking: bool,
    /// The edge classification (`type`, `macroPublic`, `macroRuntime`, ...).
    pub edge_class: String,
    /// The per-edge judgment (`typeExposed`, `contractBreaking`, ...).
    pub judgment: String,
    /// Where the judgment came from (`releaseFacts`, `macroContracts`, ...).
    pub judgment_source: String,
}

/// An echoed selection decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionDecisionOutput {
    /// The package the decision is about.
    pub package: String,
    /// `accept` or `decline`.
    pub decision: String,
    /// The canonical selection reason.
    pub reason: String,
    /// The evidence lines.
    pub evidence: Vec<String>,
    /// Normalized regression-probe outcomes (for `behavior-fix`).
    pub regression_evidence: Vec<RegressionEvidenceOutput>,
}

/// A normalized regression-evidence probe outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegressionEvidenceOutput {
    /// The probe kind (`consumer-runtime`, `consumer-compile`,
    /// `packaged-artifact`).
    pub kind: String,
    /// The probe command / description.
    pub probe: String,
    /// The measured `baseline->current` outcome, or `inconclusive`.
    pub outcome: String,
}

/// An echoed macro contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroContractOutput {
    /// The proc-macro package.
    pub package: String,
    /// The declared verdict (`compatible`, `nonbreaking`, `breaking`).
    pub verdict: String,
    /// The verdict floor derived from measured compile evidence.
    pub derived_verdict: String,
    /// The resolver-computed review scope.
    pub reviewed: Vec<String>,
    /// The evidence lines.
    pub evidence: Vec<String>,
}

/// A blocking ambiguity. The record shape varies by `kind`; each variant
/// serializes with a `"kind"` tag plus its kind-specific fields, in a stable
/// field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Ambiguity {
    /// A classification below the breaking floor forced by an exposed external
    /// dependency requirement change.
    ExternalExposureUnderclassified {
        /// The package.
        package: String,
        /// The supplied (too-weak) classification.
        classified: String,
        /// The derived floor (`breaking`).
        derived_floor: String,
        /// The exposed dependency requirement changes.
        dependencies: Vec<ExposureProbe>,
        /// The request field to correct.
        required_input: String,
    },
    /// A selection reason weaker than the breaking floor an exposed external
    /// dependency forces.
    ExternalExposureUnderselected {
        /// The package.
        package: String,
        /// The supplied decision.
        decision: String,
        /// The supplied (too-weak) reason.
        reason: String,
        /// The derived floor (`breaking`).
        derived_floor: String,
        /// The exposed dependency requirement changes.
        dependencies: Vec<ExposureProbe>,
        /// The request field to correct.
        required_input: String,
    },
    /// A breaking/non-breaking classification with no own-source change to
    /// support it.
    OwnClassificationUnsupported {
        /// The package.
        package: String,
        /// The supplied (unsupported) classification.
        classified: String,
        /// The request field to correct.
        required_input: String,
    },
    /// A `breaking` selection reason contradicted by the objective
    /// classification.
    BreakingSelectionUnderclassified {
        /// The package.
        package: String,
        /// The supplied reason.
        reason: String,
        /// The objective classification's verdict name.
        objective_classification: String,
        /// The request field to correct.
        required_input: String,
    },
    /// Behavior-fix evidence that could not be read.
    BehaviorEvidenceInconclusive {
        /// The package.
        package: String,
        /// The supplied reason.
        reason: String,
        /// The specific reading problems.
        issues: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A behavior-fix reason with no probe demonstrating a fail→pass fix.
    BehaviorFixUndemonstrated {
        /// The package.
        package: String,
        /// The supplied reason.
        reason: String,
        /// The normalized probe outcomes.
        probes: Vec<RegressionEvidenceOutput>,
        /// The request field to correct.
        required_input: String,
    },
    /// A macro whose changed compile fixture was not evidenced by its contract.
    MacroCompileFixtureUnevidenced {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The unevidenced fixtures.
        fixtures: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A macro contract whose compile evidence could not be read.
    MacroCompileEvidenceInconclusive {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The specific reading problems.
        issues: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A declared macro verdict below the floor its compile evidence derives.
    MacroVerdictUnderclassified {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The declared verdict.
        declared_verdict: String,
        /// The derived verdict floor.
        derived_verdict: String,
        /// The fixtures that decided the floor.
        deciding_fixtures: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A macro that needs review but has no supplied contract.
    MacroContractUnreviewed {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The computed review scope.
        review_scope: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A macro contract that does not cover its computed review scope.
    MacroContractIncomplete {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The computed review scope.
        review_scope: Vec<String>,
        /// The packages the contract did review.
        reviewed: Vec<String>,
        /// The request field to correct.
        required_input: String,
    },
    /// A macro whose generated runtime paths changed but whose runtime partner
    /// is unknown.
    MacroRuntimeUnknown {
        /// The macro package.
        package: String,
        /// What triggered the requirement.
        trigger: String,
        /// The computed review scope.
        review_scope: Vec<String>,
        /// How to resolve the unknown runtime partner.
        required_input: String,
    },
}

impl Ambiguity {
    /// The `(package, kind, trigger)` key the resolver sorts ambiguities by.
    /// Variants without a trigger sort as if it were empty.
    pub(crate) fn sort_key(&self) -> (&str, &'static str, &str) {
        match self {
            Self::ExternalExposureUnderclassified { package, .. } => (package, "externalExposureUnderclassified", ""),
            Self::ExternalExposureUnderselected { package, .. } => (package, "externalExposureUnderselected", ""),
            Self::OwnClassificationUnsupported { package, .. } => (package, "ownClassificationUnsupported", ""),
            Self::BreakingSelectionUnderclassified { package, .. } => (package, "breakingSelectionUnderclassified", ""),
            Self::BehaviorEvidenceInconclusive { package, .. } => (package, "behaviorEvidenceInconclusive", ""),
            Self::BehaviorFixUndemonstrated { package, .. } => (package, "behaviorFixUndemonstrated", ""),
            Self::MacroCompileFixtureUnevidenced { package, trigger, .. } => (package, "macroCompileFixtureUnevidenced", trigger),
            Self::MacroCompileEvidenceInconclusive { package, trigger, .. } => (package, "macroCompileEvidenceInconclusive", trigger),
            Self::MacroVerdictUnderclassified { package, trigger, .. } => (package, "macroVerdictUnderclassified", trigger),
            Self::MacroContractUnreviewed { package, trigger, .. } => (package, "macroContractUnreviewed", trigger),
            Self::MacroContractIncomplete { package, trigger, .. } => (package, "macroContractIncomplete", trigger),
            Self::MacroRuntimeUnknown { package, trigger, .. } => (package, "macroRuntimeUnknown", trigger),
        }
    }
}

/// One exposed external dependency requirement change reported in an ambiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureProbe {
    /// The dependency crate name.
    pub name: String,
    /// The requirement at the release baseline.
    pub baseline_req: String,
    /// The requirement at the current revision.
    pub current_req: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn ambiguity_serializes_with_kind_tag_and_camel_case_fields() {
        let ambiguity = Ambiguity::MacroVerdictUnderclassified {
            package: "foo".to_string(),
            trigger: "macroPackageModified".to_string(),
            declared_verdict: "compatible".to_string(),
            derived_verdict: "breaking".to_string(),
            deciding_fixtures: vec!["crates/foo/tests/ui/x.rs".to_string()],
            required_input: "macroContracts.foo.verdict".to_string(),
        };
        let value = serde_json::to_value(&ambiguity).unwrap();
        assert_eq!(value["kind"], "macroVerdictUnderclassified");
        assert_eq!(value["package"], "foo");
        assert_eq!(value["declaredVerdict"], "compatible");
        assert_eq!(value["derivedVerdict"], "breaking");
        assert_eq!(value["requiredInput"], "macroContracts.foo.verdict");

        // Round-trips through the internally-tagged representation.
        let back: Ambiguity = serde_json::from_value(value).unwrap();
        assert_eq!(ambiguity, back);
    }

    #[test]
    fn ambiguity_fields_serialize_in_insertion_order() {
        // The blocked-plan record must keep a stable field order (`kind` first,
        // then the declared order), not an alphabetized one.
        let ambiguity = Ambiguity::MacroVerdictUnderclassified {
            package: "foo".to_string(),
            trigger: "t".to_string(),
            declared_verdict: "compatible".to_string(),
            derived_verdict: "breaking".to_string(),
            deciding_fixtures: Vec::new(),
            required_input: "macroContracts.foo.verdict".to_string(),
        };
        let json = serde_json::to_string(&ambiguity).unwrap();
        let order = [
            "\"kind\"",
            "\"package\"",
            "\"trigger\"",
            "\"declaredVerdict\"",
            "\"derivedVerdict\"",
            "\"decidingFixtures\"",
            "\"requiredInput\"",
        ];
        let positions: Vec<usize> = order.iter().map(|key| json.find(key).unwrap()).collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "ambiguity fields must serialize in insertion order; got {json}"
        );
    }
}
