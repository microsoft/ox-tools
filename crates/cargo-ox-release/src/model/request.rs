// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The request contract: the model's mode, accepted tokens, and per-package
//! selection decisions, classifications, and macro contracts.
//!
//! Every container is strongly typed. Fields tolerate the JSON shapes a
//! producer may emit — a single-element array unwrapped to a scalar, empty
//! lists and strings written as `null`, and numeric fields arriving as numbers,
//! strings, or bools — through the deserializers in
//! [`crate::model::serde_helpers`]. Validating the *values* (a recognized
//! reason, non-empty evidence, a covered review scope) is the resolver's job.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::model::PackageFact;
use crate::model::serde_helpers::{flex_int, flexible_vec, null_string, opt_flexible_vec};

/// A release request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Release mode: `targeted`, `changed`, or `all`. Absent defaults to
    /// `targeted`.
    #[serde(default)]
    pub(crate) mode: Option<String>,
    /// Accepted release tokens (`folder`, `folder@breaking`, `folder@1.2.3`).
    #[serde(default, deserialize_with = "flexible_vec")]
    pub(crate) tokens: Vec<String>,
    /// Per-candidate selection decisions, keyed by folder. `None` distinguishes
    /// an absent container (an error in changed/all mode) from an empty one.
    #[serde(default)]
    pub(crate) selection_decisions: Option<BTreeMap<String, SelectionDecisionInput>>,
    /// Per-package objective classifications, keyed by folder or name.
    #[serde(default)]
    pub(crate) classifications: BTreeMap<String, ClassificationInput>,
    /// Per-macro contract attestations, keyed by folder or name.
    #[serde(default)]
    pub(crate) macro_contracts: BTreeMap<String, MacroContractInput>,
    /// Whether to downgrade pin-below-required-version errors to warnings.
    #[serde(default)]
    pub(crate) force: bool,
}

impl Request {
    /// Looks up a classification by folder, then by name.
    pub(crate) fn classification(&self, fact: &PackageFact) -> Option<&ClassificationInput> {
        self.classifications
            .get(&fact.folder)
            .or_else(|| self.classifications.get(&fact.name))
    }

    /// Looks up a macro contract by folder, then by name.
    pub(crate) fn macro_contract(&self, fact: &PackageFact) -> Option<&MacroContractInput> {
        self.macro_contracts
            .get(&fact.folder)
            .or_else(|| self.macro_contracts.get(&fact.name))
    }
}

/// A supplied selection decision. Field-level defaults keep an incomplete
/// object parseable so the resolver can report the specific rule it violates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelectionDecisionInput {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) decision: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) reason: String,
    #[serde(default, deserialize_with = "flexible_vec")]
    pub(crate) evidence: Vec<String>,
    #[serde(default, deserialize_with = "flexible_vec")]
    pub(crate) regression_evidence: Vec<RegressionEntry>,
}

/// One regression-evidence entry: an object, or a bare string the resolver
/// rejects with a specific message.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum RegressionEntry {
    Text(String),
    Item(RegressionEvidenceInput),
}

/// The object form of a regression-evidence entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegressionEvidenceInput {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) kind: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) probe: String,
    #[serde(default, deserialize_with = "lenient_measured")]
    pub(crate) baseline: Option<MeasuredInput>,
    #[serde(default, deserialize_with = "lenient_measured")]
    pub(crate) current: Option<MeasuredInput>,
}

/// The measured half of a before/after probe.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeasuredInput {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) result: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) revision: String,
    #[serde(default, deserialize_with = "flex_int")]
    pub(crate) exit_code: Option<i64>,
}

/// Deserializes a probe measurement leniently: only a JSON object yields a
/// [`MeasuredInput`]; a string, number, bool, array, `null`, or absent value
/// yields `None`, which the resolver grades as an incomplete measurement (and
/// reports as an actionable ambiguity) rather than rejecting the whole request.
fn lenient_measured<'de, D>(deserializer: D) -> Result<Option<MeasuredInput>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        Some(object @ serde_json::Value::Object(_)) => MeasuredInput::deserialize(object).map(Some).map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// A supplied classification: a bare change-type string, or a detailed object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum ClassificationInput {
    Simple(String),
    Detailed(ClassificationObj),
}

/// The object form of a classification.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClassificationObj {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) change_type: String,
    /// Present iff the request supplied `manualReview`; the resolver rejects a
    /// value that contradicts the resolver-owned flag.
    #[serde(default)]
    pub(crate) manual_review: Option<bool>,
}

/// A supplied macro contract: a bare verdict string (which the resolver then
/// rejects for missing its required channels/evidence), or a detailed object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum MacroContractInput {
    Verdict(String),
    /// Boxed to keep the enum's variants similarly sized.
    Detailed(Box<MacroContractObj>),
}

/// The object form of a macro contract. `reviewedPackages`, `channels`, and
/// `evidence` are `Option` so the resolver can distinguish "absent/null" (an
/// error) from "present but empty" (a different error).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MacroContractObj {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) verdict: String,
    #[serde(default, deserialize_with = "opt_flexible_vec")]
    pub(crate) reviewed_packages: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) channels: Option<MacroChannels>,
    #[serde(default, deserialize_with = "opt_flexible_vec")]
    pub(crate) evidence: Option<Vec<String>>,
    #[serde(default, deserialize_with = "flexible_vec")]
    pub(crate) compile_evidence: Vec<CompileEntry>,
}

/// The six macro contract channels. A missing channel defaults to an empty
/// string, which the resolver rejects as an unclassified channel.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MacroChannels {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) exported_macros: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) accepted_syntax: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) compile_behavior: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) generated_api: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) generated_runtime_paths: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) hygiene: String,
}

/// One compile-evidence entry: an object, or a bare string the resolver
/// rejects.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum CompileEntry {
    Text(String),
    Item(CompileEvidenceInput),
}

/// The object form of a compile-evidence entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompileEvidenceInput {
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) owner_package: String,
    #[serde(default, deserialize_with = "null_string")]
    pub(crate) path: String,
    #[serde(default, deserialize_with = "lenient_measured")]
    pub(crate) baseline: Option<MeasuredInput>,
    #[serde(default, deserialize_with = "lenient_measured")]
    pub(crate) current: Option<MeasuredInput>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tokens_accepts_a_bare_scalar() {
        // A single token may arrive unwrapped as a bare scalar.
        let request: Request = serde_json::from_str(r#"{"mode":"targeted","tokens":"alpha@breaking"}"#).unwrap();
        assert_eq!(request.tokens, vec!["alpha@breaking"]);
    }

    #[test]
    fn tokens_accepts_an_array_and_defaults_to_empty() {
        let request: Request = serde_json::from_str(r#"{"tokens":["a","b"]}"#).unwrap();
        assert_eq!(request.tokens, vec!["a", "b"]);
        let request: Request = serde_json::from_str("{}").unwrap();
        assert!(request.tokens.is_empty());
    }

    #[test]
    fn exit_code_accepts_int_float_string_and_bool() {
        let parse = |json: &str| serde_json::from_str::<MeasuredInput>(json).unwrap().exit_code;
        assert_eq!(parse(r#"{"exitCode":101}"#), Some(101));
        assert_eq!(parse(r#"{"exitCode":0.0}"#), Some(0));
        assert_eq!(parse(r#"{"exitCode":"101"}"#), Some(101));
        assert_eq!(parse(r#"{"exitCode":true}"#), Some(1));
        assert_eq!(parse(r#"{"exitCode":null}"#), None);
        assert_eq!(parse("{}"), None);
    }

    #[test]
    fn classification_accepts_a_string_or_an_object() {
        let simple: ClassificationInput = serde_json::from_str(r#""patch""#).unwrap();
        assert!(matches!(simple, ClassificationInput::Simple(s) if s == "patch"));
        let detailed: ClassificationInput = serde_json::from_str(r#"{"changeType":"breaking","manualReview":false}"#).unwrap();
        let ClassificationInput::Detailed(obj) = detailed else {
            panic!("expected the detailed form");
        };
        assert_eq!(obj.change_type, "breaking");
        assert_eq!(obj.manual_review, Some(false));
    }

    #[test]
    fn macro_contract_accepts_a_verdict_string_or_an_object() {
        let verdict: MacroContractInput = serde_json::from_str(r#""breaking""#).unwrap();
        assert!(matches!(verdict, MacroContractInput::Verdict(v) if v == "breaking"));
        let detailed: MacroContractInput = serde_json::from_str(r#"{"verdict":"compatible","reviewedPackages":"only_one"}"#).unwrap();
        let MacroContractInput::Detailed(obj) = detailed else {
            panic!("expected the detailed form");
        };
        // A single reviewed package unwraps to a scalar; it must still parse.
        assert_eq!(obj.reviewed_packages.as_deref(), Some(["only_one".to_string()].as_slice()));
    }

    #[test]
    fn regression_and_compile_entries_distinguish_object_from_string() {
        let item: RegressionEntry = serde_json::from_str(r#"{"kind":"consumer-runtime","probe":"p"}"#).unwrap();
        assert!(matches!(item, RegressionEntry::Item(_)));
        let text: RegressionEntry = serde_json::from_str(r#""oops""#).unwrap();
        assert!(matches!(text, RegressionEntry::Text(_)));

        let item: CompileEntry = serde_json::from_str(r#"{"ownerPackage":"o","path":"p"}"#).unwrap();
        assert!(matches!(item, CompileEntry::Item(_)));
        let text: CompileEntry = serde_json::from_str(r#""oops""#).unwrap();
        assert!(matches!(text, CompileEntry::Text(_)));
    }

    #[test]
    fn measurement_tolerates_a_non_object_shape() {
        // A mis-shaped baseline/current must parse (yielding None) so the
        // resolver can grade it as an incomplete measurement and report an
        // actionable ambiguity, rather than failing the whole request.
        let entry: RegressionEntry =
            serde_json::from_str(r#"{"kind":"consumer-runtime","probe":"p","baseline":"fail","current":101}"#).unwrap();
        let RegressionEntry::Item(item) = entry else {
            panic!("expected the object form");
        };
        assert!(item.baseline.is_none());
        assert!(item.current.is_none());

        let compile: CompileEntry = serde_json::from_str(r#"{"ownerPackage":"o","path":"p","current":"fail"}"#).unwrap();
        let CompileEntry::Item(item) = compile else {
            panic!("expected the object form");
        };
        assert!(item.current.is_none());

        // A well-formed object still parses into a measurement.
        let entry: RegressionEntry =
            serde_json::from_str(r#"{"kind":"consumer-runtime","probe":"p","baseline":{"result":"fail","revision":"r","exitCode":101}}"#)
                .unwrap();
        let RegressionEntry::Item(item) = entry else {
            panic!("expected the object form");
        };
        assert_eq!(item.baseline.unwrap().result, "fail");
    }
}
