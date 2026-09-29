// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Discovery authority carried inside the interchange format's free-form configuration.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::elements::{MutantResult, SourceProvenance, VerdictProvenance};

/// A versioned population description, retaining unsupported versions without trusting them.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Population {
    /// A population whose completeness rules this reader understands.
    Known(Scope),
    /// Future metadata, preserved but never used to retire identities.
    Unknown(Value),
}

impl<'de> Deserialize<'de> for Population {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let version = value
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| serde::de::Error::custom("population.version must be a non-negative integer"))?;
        if version != 1 {
            return Ok(Self::Unknown(value));
        }
        let scope: Scope =
            serde_json::from_value(value).map_err(|cause| serde::de::Error::custom(format!("invalid population: {cause}")))?;
        scope.validate().map_err(serde::de::Error::custom)?;
        Ok(Self::Known(scope))
    }
}

impl Population {
    pub(crate) const fn known(&self) -> Option<&Scope> {
        match self {
            Self::Known(scope) => Some(scope),
            Self::Unknown(_) => None,
        }
    }
}

/// Resolved shaping contexts and explicit discovery completeness, independent of verdicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub version: u32,
    pub selection: SelectionKind,
    /// Every field, including unknown additive fields, participates in the context digest.
    pub contexts: BTreeMap<String, Value>,
    pub context: Option<String>,
    pub complete_files: BTreeSet<String>,
    pub reductions: BTreeSet<String>,
    /// Original complete assertions consumed by a merge, not assertions made by that merge.
    pub assertions: Vec<PopulationAssertion>,
    /// Audited exact request, independent of the population's shaping context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<ExactSelection>,
}

/// Identities explicitly requested by a replay, with its optional input report digest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExactSelection {
    pub ids: BTreeSet<String>,
    pub parent_report: Option<String>,
}

/// Why the report contains its selected candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionKind {
    Discovery,
    Survivors,
    Diff,
    ExactIds,
    Merged,
}

/// One complete file population, with its original discovery authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PopulationAssertion {
    pub file: String,
    pub context: String,
    pub ids: BTreeSet<String>,
    pub discovered: SourceProvenance,
}

/// The discovery generation under which an observation belongs to a population.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PopulationOrigin {
    pub context: String,
    pub discovered: SourceProvenance,
}

/// An original observation retained so staged merges can apply later population evidence.
///
/// Source bytes stay in the standard file table. Their digest prevents an older location from
/// being drawn over another generation, including when an observation is currently not rendered.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub file: String,
    pub source_key: String,
    pub source: SourceProvenance,
    pub mutant: MutantResult,
    pub verdict: VerdictProvenance,
}

impl Scope {
    pub(crate) fn discovery(shaping: Value) -> Self {
        let key = context_key(&shaping);
        Self {
            version: 1,
            selection: SelectionKind::Discovery,
            contexts: [(key.clone(), shaping)].into(),
            context: Some(key),
            complete_files: BTreeSet::new(),
            reductions: BTreeSet::new(),
            assertions: Vec::new(),
            exact: None,
        }
    }

    pub(crate) fn merged() -> Self {
        Self {
            version: 1,
            selection: SelectionKind::Merged,
            contexts: BTreeMap::new(),
            context: None,
            complete_files: BTreeSet::new(),
            reductions: BTreeSet::new(),
            assertions: Vec::new(),
            exact: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(exact) = &self.exact
            && (self.selection != SelectionKind::ExactIds || exact.ids.is_empty())
        {
            return Err("exact selection must name a nonempty partial population".to_owned());
        }
        for (key, shaping) in &self.contexts {
            if !shaping.is_object() || *key != context_key(shaping) {
                return Err("population context key does not match its complete shaping record".to_owned());
            }
            validate_shaping(shaping)?;
        }
        match (self.selection, &self.context) {
            (SelectionKind::Merged, None) if self.complete_files.is_empty() => {}
            (SelectionKind::Merged, _) => return Err("merged population cannot assert a fresh complete snapshot".to_owned()),
            (_, Some(context)) if self.contexts.contains_key(context) => {}
            _ => return Err("population context is missing its shaping record".to_owned()),
        }
        if !self.complete_files.is_empty() && (self.selection != SelectionKind::Discovery || !self.reductions.is_empty()) {
            return Err("partial population cannot claim complete files".to_owned());
        }
        for assertion in &self.assertions {
            if self.selection != SelectionKind::Merged || !self.contexts.contains_key(&assertion.context) {
                return Err("inherited population assertion is missing its merged shaping context".to_owned());
            }
            if assertion.discovered.origin.is_empty() || assertion.discovered.lineage.is_empty() {
                return Err("inherited population assertion is missing its original authority".to_owned());
            }
        }
        Ok(())
    }
}

fn strings(value: &Value) -> bool {
    value.as_array().is_some_and(|values| values.iter().all(Value::is_string))
}

fn validate_shaping(shaping: &Value) -> Result<(), String> {
    for name in ["mutators", "files", "excludeFiles"] {
        if !shaping.get(name).is_some_and(strings) {
            return Err(format!("population shaping.{name} must be an array of strings"));
        }
    }
    for name in ["errors", "excludeTraitImpls"] {
        if shaping.get(name).is_some_and(|value| !strings(value)) {
            return Err(format!("population shaping.{name} must be an array of strings"));
        }
    }
    if shaping.get("idScheme").is_none_or(|value| value.as_u64().is_none()) {
        return Err("population shaping.idScheme must be a non-negative integer".to_owned());
    }
    if !shaping
        .get("features")
        .and_then(Value::as_object)
        .is_some_and(|features| features.values().all(strings))
    {
        return Err("population shaping.features must map packages to feature arrays".to_owned());
    }
    if !shaping.get("packages").and_then(Value::as_object).is_some_and(|packages| {
        packages
            .values()
            .all(|value| strings(value) && value.as_array().is_some_and(|parts| parts.len() == 2))
    }) {
        return Err("population shaping.packages must map names to directory/version pairs".to_owned());
    }
    if !shaping
        .get("opaque")
        .is_some_and(|value| value.is_null() || value.as_object().is_some_and(|parts| parts.values().all(Value::is_string)))
    {
        return Err("population shaping.opaque must be null or an object of digests".to_owned());
    }
    Ok(())
}

/// Hashes canonical JSON, including fields unknown to this version of Gamma.
pub(crate) fn context_key(shaping: &Value) -> String {
    let mut canonical = shaping.clone();
    canonical.sort_all_objects();
    let encoded = serde_json::to_vec(&canonical).expect("JSON values contain only serializable JSON data");
    blake3::hash(&encoded).to_hex().to_string()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn additive_shaping_fields_and_nested_values_participate_in_the_key() {
        let base = serde_json::json!({"mutators": ["m"], "cfg": {"b": 2, "a": 1},
            "idScheme": 1, "packages": {}, "features": {}, "files": [], "excludeFiles": [], "opaque": {}});
        let reordered: Value = serde_json::from_str(
            r#"{"cfg":{"a":1,"b":2},"mutators":["m"],"idScheme":1,"packages":{},"features":{},"files":[],"excludeFiles":[],"opaque":{}}"#,
        )
        .unwrap();
        assert_eq!(context_key(&base), context_key(&reordered));
        for field in ["lib", "noConstFns"] {
            let mut extended = base.clone();
            extended[field] = Value::Bool(true);
            assert_ne!(context_key(&base), context_key(&extended));
            let original = Scope::discovery(extended);
            let roundtrip: Population = serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
            assert_eq!(roundtrip.known().unwrap().contexts, original.contexts);
        }
    }

    #[test]
    fn malformed_supported_metadata_is_not_an_unknown_version() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"version": "1"}),
            serde_json::json!({"version": 1}),
        ] {
            let _ = serde_json::from_value::<Population>(value).unwrap_err();
        }
        let value = serde_json::json!({"version": 2, "future": true});
        let parsed: Population = serde_json::from_value(value.clone()).unwrap();
        assert!(parsed.known().is_none());
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
}
