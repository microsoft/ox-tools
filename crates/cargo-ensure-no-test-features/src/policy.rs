// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashSet;
use std::path::Path;
use std::{fmt, io};

use ohno::AppError;
use serde::{Deserialize, Deserializer};

const BUNDLED: &[u8] = include_bytes!("../defaults.json");
const BUNDLED_LABEL: &str = "bundled defaults.json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default, deserialize_with = "present")]
    features: Option<Vec<String>>,
    #[serde(default)]
    exceptions: Vec<Exception>,
    #[serde(default, rename = "$comment")]
    _comment: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Exception {
    package: String,
    feature: String,
    #[serde(default, deserialize_with = "present")]
    dependency: Option<String>,
}

pub(crate) struct Policy {
    features: HashSet<String>,
    exceptions: Vec<Exception>,
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn read_policy(bytes: &[u8], path: impl fmt::Display) -> Result<PolicyFile, AppError> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(AppError::new(format!("invalid policy at {path}: expected a JSON object")));
    }
    serde_json::from_slice(bytes).map_err(|error| AppError::new(format!("invalid policy at {path}: {error}")))
}

impl Policy {
    fn new(
        features: Vec<String>,
        exceptions: Vec<Exception>,
        feature_path: impl fmt::Display,
        exception_path: impl fmt::Display,
    ) -> Result<Self, AppError> {
        let invalid = |message| AppError::new(format!("invalid policy at {exception_path}: {message}"));
        let mut denied = HashSet::new();
        if features.is_empty()
            || features
                .into_iter()
                .any(|feature| feature.trim().is_empty() || !denied.insert(feature))
        {
            return Err(AppError::new(format!(
                "invalid policy at {feature_path}: features must contain at least one feature and all names must be unique, nonempty strings"
            )));
        }
        let mut seen = HashSet::new();
        for entry in &exceptions {
            let dependency = entry.dependency.as_deref();
            if entry.package.trim().is_empty() || !denied.contains(&entry.feature) || dependency.is_some_and(|key| key.trim().is_empty()) {
                return Err(invalid(
                    "each exception requires a package, a denied feature, and an optional dependency key",
                ));
            }
            if !seen.insert((&entry.package, dependency, &entry.feature)) {
                return Err(invalid("duplicate exception"));
            }
        }
        Ok(Self {
            features: denied,
            exceptions,
        })
    }

    pub(crate) fn denies(&self, package: &str, dependency: Option<&str>, feature: &str) -> bool {
        self.features.contains(feature)
            && !self
                .exceptions
                .iter()
                .any(|entry| entry.package == package && entry.dependency.as_deref() == dependency && entry.feature == feature)
    }
}

pub(crate) fn load(path: Option<&Path>, read: impl FnOnce(&Path) -> io::Result<Vec<u8>>) -> Result<Policy, AppError> {
    load_with_defaults(path, read, BUNDLED)
}

fn load_with_defaults(path: Option<&Path>, read: impl FnOnce(&Path) -> io::Result<Vec<u8>>, bundled: &[u8]) -> Result<Policy, AppError> {
    if let Some(path) = path {
        let label = path.display();
        let bytes = read(path).map_err(|error| AppError::new(format!("could not read policy at {label}: {error}")))?;
        let active = read_policy(&bytes, &label)?;
        if let Some(features) = active.features {
            return Policy::new(features, active.exceptions, &label, &label);
        }
        return Policy::new(default_features(bundled)?, active.exceptions, BUNDLED_LABEL, label);
    }

    Policy::new(default_features(bundled)?, Vec::new(), BUNDLED_LABEL, BUNDLED_LABEL)
}

fn default_features(bundled: &[u8]) -> Result<Vec<String>, AppError> {
    read_policy(bundled, BUNDLED_LABEL)?
        .features
        .ok_or_else(|| AppError::new(format!("invalid policy at {BUNDLED_LABEL}: missing features")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_policy_and_distinguishes_io_errors() -> Result<(), AppError> {
        let path = Path::new("policy.json");
        for invalid in [
            "null",
            "[]",
            r#"{"features":null}"#,
            r#"{"features":[]}"#,
            r#"{"features":[" "]}"#,
            r#"{"features":["fakes","fakes"]}"#,
            r#"{"unknown":[]}"#,
            r#"{"exceptions":[{"package":"app","feature":"not-denied"}]}"#,
            r#"{"exceptions":[{"package":"app","feature":"fakes","dependency":null}]}"#,
            r#"{"exceptions":[{"package":"app","feature":"fakes"},{"package":"app","feature":"fakes"}]}"#,
        ] {
            assert!(load(Some(path), |_| Ok(invalid.as_bytes().to_vec())).is_err(), "{invalid}");
        }
        assert!(load_with_defaults(None, |_| unreachable!(), b"{}").is_err());
        assert!(load_with_defaults(Some(path), |_| Ok(b"{}".to_vec()), b"{}").is_err());
        assert!(load_with_defaults(Some(path), |_| Ok(b"{}".to_vec()), b"not json").is_err());
        for error in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
            let result = load(Some(path), |_| Err(error.into()))
                .err()
                .expect("explicit policy must be readable");
            assert!(result.to_string().contains("could not read policy at policy.json"));
        }
        let with_bom = load(Some(path), |_| Ok(b"\xef\xbb\xbf{\"features\":[\"test-util\"]}".to_vec()))?;
        assert!(with_bom.denies("app", None, "test-util"));
        assert!(!with_bom.denies("app", None, "fakes"));
        Ok(())
    }
}
