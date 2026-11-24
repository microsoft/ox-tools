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
    _comment: Option<serde::de::IgnoredAny>,
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
    // A missing key uses the field default; an explicit null must not do so.
    T::deserialize(deserializer).map(Some)
}

fn read_policy(bytes: &[u8], path: impl fmt::Display) -> Result<PolicyFile, AppError> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    // Serde also accepts positional arrays for structs; policy fields must be named.
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
    fn an_exception_cannot_exempt_another_package() -> Result<(), AppError> {
        let policy = load(Some(Path::new("policy.json")), |_| {
            Ok(br#"{"exceptions":[{"package":"app","dependency":"wire","feature":"fakes"}]}"#.to_vec())
        })?;
        assert!(!policy.denies("app", Some("wire"), "fakes"));
        assert!(policy.denies("other", Some("wire"), "fakes"));
        Ok(())
    }

    #[test]
    fn rejects_invalid_policy_and_distinguishes_io_errors() -> Result<(), AppError> {
        let path = Path::new("policy.json");
        let invalid_features = "features must contain at least one feature and all names must be unique, nonempty strings";
        let invalid_exception = "each exception requires a package, a denied feature, and an optional dependency key";
        for (invalid, detail) in [
            ("null", "expected a JSON object"),
            ("[]", "expected a JSON object"),
            (
                r#"{"features":null}"#,
                "invalid type: null, expected a sequence at line 1 column 16",
            ),
            (r#"{"features":[]}"#, invalid_features),
            (r#"{"features":[" "]}"#, invalid_features),
            (r#"{"features":["fakes","fakes"]}"#, invalid_features),
            (
                r#"{"unknown":[]}"#,
                "unknown field `unknown`, expected one of `features`, `exceptions`, `$comment` at line 1 column 10",
            ),
            (r#"{"exceptions":[{"package":"app","feature":"not-denied"}]}"#, invalid_exception),
            (r#"{"exceptions":[{"package":" ","feature":"fakes"}]}"#, invalid_exception),
            (
                r#"{"exceptions":[{"package":"app","feature":"fakes","dependency":" "}]}"#,
                invalid_exception,
            ),
            (
                r#"{"exceptions":[{"package":"app","feature":"fakes","dependency":null}]}"#,
                "invalid type: null, expected a string at line 1 column 67",
            ),
            (
                r#"{"exceptions":[{"package":"app","feature":"fakes"},{"package":"app","feature":"fakes"}]}"#,
                "duplicate exception",
            ),
        ] {
            let result = load(Some(path), |_| Ok(invalid.as_bytes().to_vec()))
                .err()
                .expect("an invalid policy must fail");
            assert_eq!(result.to_string(), format!("invalid policy at policy.json: {detail}"), "{invalid}");
        }
        for (path, bundled, expected) in [
            (None, b"{}".as_slice(), "missing features"),
            (Some(path), b"{}".as_slice(), "missing features"),
            (Some(path), b"not json".as_slice(), "expected a JSON object"),
        ] {
            let error = load_with_defaults(path, |_| Ok(b"{}".to_vec()), bundled)
                .err()
                .expect("unusable bundled features must be reported");
            assert_eq!(error.to_string(), format!("invalid policy at bundled defaults.json: {expected}"));
        }
        for error in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
            let result = load(Some(path), |_| Err(io::Error::new(error, "scripted read failure")))
                .err()
                .expect("explicit policy must be readable");
            assert_eq!(result.to_string(), "could not read policy at policy.json: scripted read failure");
        }
        let with_bom = load(Some(path), |_| Ok(b"\xef\xbb\xbf{\"features\":[\"test-util\"]}".to_vec()))?;
        assert!(with_bom.denies("app", None, "test-util"));
        assert!(!with_bom.denies("app", None, "fakes"));
        Ok(())
    }
}
