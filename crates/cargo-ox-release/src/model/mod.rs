// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serde data model for the release planner's three JSON contracts:
//! [`facts`] (emitted by fact-gathering), [`request`] (the model's classified
//! decisions), and [`plan`] (the resolver's canonical output).

pub mod facts;
pub mod plan;
pub mod request;
pub(crate) mod serde_helpers;

pub use facts::{ExternalDepChange, Facts, MacroCompileFixtureChange, PackageFact};
pub use plan::{
    Ambiguity, CascadeReasonOutput, ExposureProbe, MacroContractOutput, Plan, PlanStatus, RegressionEvidenceOutput, ReleaseOutput,
    ReleaseSource, SelectionDecisionOutput,
};
pub use request::Request;
pub(crate) use request::{ClassificationInput, CompileEntry, MacroContractInput, MeasuredInput, RegressionEntry, SelectionDecisionInput};

/// Returns a cleaned copy of `values`: each entry trimmed, empties dropped.
pub(crate) fn clean_string_list(values: &[String]) -> Vec<String> {
    values.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Normalizes a Cargo package identifier for graph comparisons: hyphens become
/// underscores, matching the resolver's repeated `.Replace('-', '_')`.
pub(crate) fn normalize_ident(identifier: &str) -> String {
    identifier.replace('-', "_")
}
