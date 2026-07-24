// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `--filter` / `--exclude-filter` metadata predicate language.
//!
//! A small, fixed set of predicates over cargo metadata — enough to express
//! every ad-hoc `cargo metadata` filter the recipes currently hand-roll,
//! and nothing more:
//!
//! - `lib` / `bin` — the member has a target of that kind.
//! - `dep:<name>` — the member declares `<name>` as a dependency.
//! - `metadata:<dotted.key>` — the `package.metadata.<dotted.key>` key exists.
//! - `metadata:<dotted.key>=<value>` — that key equals `<value>` (numeric
//!   compare when both sides parse as a number, else string compare).

use serde_json::Value;

use crate::error::{EachError, InvalidPredicateError};
use crate::workspace::Member;

/// A parsed filter predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// `lib`: member has a `lib` target.
    HasLib,
    /// `bin`: member has a `bin` target.
    HasBin,
    /// `dep:<name>`: member declares `<name>` as a dependency.
    DependsOn(String),
    /// `metadata:<dotted.key>`: the metadata key is present.
    MetadataPresent(String),
    /// `metadata:<dotted.key>=<value>`: the metadata key equals `<value>`.
    MetadataEquals(String, String),
}

impl Predicate {
    /// Parse a predicate from its command-line spelling.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if `spec` is not one of the recognized predicate
    /// forms (`lib`, `bin`, `dep:<name>`, `metadata:<key>[=<value>]`) or has an
    /// empty dependency name / metadata key.
    pub fn parse(spec: &str) -> Result<Self, EachError> {
        match spec {
            "lib" => Ok(Self::HasLib),
            "bin" => Ok(Self::HasBin),
            _ => {
                if let Some(name) = spec.strip_prefix("dep:") {
                    if name.is_empty() {
                        return Err(invalid(spec, "empty dependency name"));
                    }
                    Ok(Self::DependsOn(name.to_owned()))
                } else if let Some(rest) = spec.strip_prefix("metadata:") {
                    parse_metadata(spec, rest)
                } else {
                    Err(invalid(spec, "expected one of: lib, bin, dep:<name>, metadata:<key>[=<value>]"))
                }
            }
        }
    }

    /// Evaluate this predicate against a workspace member.
    #[must_use]
    pub fn matches(&self, member: &Member) -> bool {
        match self {
            Self::HasLib => member.has_lib,
            Self::HasBin => member.has_bin,
            Self::DependsOn(name) => member.dependencies.contains(name),
            Self::MetadataPresent(key) => lookup(&member.metadata, key).is_some(),
            Self::MetadataEquals(key, expected) => lookup(&member.metadata, key).is_some_and(|v| value_equals(v, expected)),
        }
    }
}

/// Parse the portion of a `metadata:` predicate after the prefix.
fn parse_metadata(spec: &str, rest: &str) -> Result<Predicate, EachError> {
    if let Some((key, value)) = rest.split_once('=') {
        if key.is_empty() {
            return Err(invalid(spec, "empty metadata key"));
        }
        Ok(Predicate::MetadataEquals(key.to_owned(), value.to_owned()))
    } else {
        if rest.is_empty() {
            return Err(invalid(spec, "empty metadata key"));
        }
        Ok(Predicate::MetadataPresent(rest.to_owned()))
    }
}

fn invalid(spec: &str, reason: &str) -> EachError {
    InvalidPredicateError::new(spec.to_owned(), reason.to_owned()).into()
}

/// Walk a dotted key path (`coverage-gate.min-lines-percent`) into a JSON
/// metadata value.
fn lookup<'v>(metadata: &'v Value, dotted_key: &str) -> Option<&'v Value> {
    let mut node = metadata;
    for segment in dotted_key.split('.') {
        node = node.get(segment)?;
    }
    Some(node)
}

/// Compare a metadata node to an expected string. When both the node's
/// scalar rendering and the expected value parse as `f64`, compare
/// numerically (so `0` matches `0.0`); otherwise compare as strings.
fn value_equals(node: &Value, expected: &str) -> bool {
    let rendered = match node {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return false,
    };
    match (rendered.parse::<f64>(), expected.parse::<f64>()) {
        // Exact equality is intended: both sides are short decimal literals
        // (metadata values and the user's expected string), so `0` matches
        // `0.0` without any epsilon fuzz being meaningful here.
        #[expect(clippy::float_cmp, reason = "comparing exact parsed values of short decimal literals")]
        (Ok(a), Ok(b)) => a == b,
        _ => rendered == expected,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn member_with(deps: &[&str], metadata: Value, has_lib: bool, has_bin: bool) -> Member {
        Member {
            name: "m".to_owned(),
            version: "0.1.0".to_owned(),
            manifest_path: PathBuf::from("/ws/m/Cargo.toml"),
            has_lib,
            has_bin,
            dependencies: deps.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>(),
            metadata,
        }
    }

    #[test]
    fn parses_kind_predicates() {
        assert_eq!(Predicate::parse("lib").expect("lib"), Predicate::HasLib);
        assert_eq!(Predicate::parse("bin").expect("bin"), Predicate::HasBin);
    }

    #[test]
    fn parses_dep_predicate() {
        assert_eq!(Predicate::parse("dep:loom").expect("dep"), Predicate::DependsOn("loom".to_owned()));
    }

    #[test]
    fn parses_metadata_predicates() {
        assert_eq!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent").expect("present"),
            Predicate::MetadataPresent("coverage-gate.min-lines-percent".to_owned())
        );
        assert_eq!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0").expect("equals"),
            Predicate::MetadataEquals("coverage-gate.min-lines-percent".to_owned(), "0".to_owned())
        );
    }

    #[test]
    fn rejects_unknown_and_empty() {
        Predicate::parse("nonsense").expect_err("unknown predicate must error");
        Predicate::parse("dep:").expect_err("empty dependency name must error");
        Predicate::parse("metadata:").expect_err("empty metadata key must error");
    }

    #[test]
    fn kind_predicates_match() {
        let m = member_with(&[], Value::Null, true, false);
        assert!(Predicate::HasLib.matches(&m));
        assert!(!Predicate::HasBin.matches(&m));
    }

    #[test]
    fn dep_predicate_matches() {
        let m = member_with(&["loom", "serde"], Value::Null, true, false);
        assert!(Predicate::DependsOn("loom".to_owned()).matches(&m));
        assert!(!Predicate::DependsOn("tokio".to_owned()).matches(&m));
    }

    #[test]
    fn metadata_present_and_equals() {
        let m = member_with(&[], json!({ "coverage-gate": { "min-lines-percent": 0 } }), true, false);
        assert!(Predicate::parse("metadata:coverage-gate.min-lines-percent").expect("p").matches(&m));
        // numeric-aware: `0` matches the JSON number `0`.
        assert!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0")
                .expect("p")
                .matches(&m)
        );
        assert!(
            Predicate::parse("metadata:coverage-gate.min-lines-percent=0.0")
                .expect("p")
                .matches(&m)
        );
        assert!(
            !Predicate::parse("metadata:coverage-gate.min-lines-percent=50")
                .expect("p")
                .matches(&m)
        );
        assert!(!Predicate::parse("metadata:missing.key").expect("p").matches(&m));
    }

    #[test]
    fn metadata_string_equals() {
        let m = member_with(&[], json!({ "role": "script-only" }), true, false);
        assert!(Predicate::parse("metadata:role=script-only").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:role=library").expect("p").matches(&m));
    }

    #[test]
    fn metadata_bool_equals() {
        let m = member_with(&[], json!({ "flag": true }), true, false);
        assert!(Predicate::parse("metadata:flag=true").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:flag=false").expect("p").matches(&m));
    }

    #[test]
    fn metadata_non_scalar_never_equals() {
        // Object / array metadata values can be *present* but never compare
        // equal to a scalar expected string.
        let m = member_with(&[], json!({ "obj": { "a": 1 }, "arr": [1, 2] }), true, false);
        assert!(Predicate::parse("metadata:obj").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:obj=x").expect("p").matches(&m));
        assert!(!Predicate::parse("metadata:arr=x").expect("p").matches(&m));
    }

    #[test]
    fn rejects_empty_metadata_key_before_equals() {
        Predicate::parse("metadata:=value").expect_err("empty key with value must error");
    }
}
