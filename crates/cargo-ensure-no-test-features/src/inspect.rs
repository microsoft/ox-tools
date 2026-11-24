// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use ohno::AppError;
use serde::Deserialize;

use crate::policy::Policy;

#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    workspace_members: Vec<String>,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    publish: Option<Vec<String>>,
    features: BTreeMap<String, Vec<String>>,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    rename: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    kind: Option<Kind>,
    target: Option<String>,
    features: Vec<String>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Normal,
    Build,
    Dev,
}

pub(crate) fn check(json: &[u8], policy: &Policy) -> Result<BTreeSet<String>, AppError> {
    let metadata =
        serde_json::from_slice(json).map_err(|error| AppError::new(format!("could not parse cargo metadata output: {error}")))?;
    inspect(&metadata, policy)
}

fn inspect(metadata: &Metadata, policy: &Policy) -> Result<BTreeSet<String>, AppError> {
    let mut packages = BTreeMap::new();
    for package in &metadata.packages {
        if packages.insert(package.id.as_str(), package).is_some() {
            return Err(AppError::new(format!("duplicate package ID '{}' in cargo metadata", package.id)));
        }
    }

    let mut violations = BTreeSet::new();
    for id in &metadata.workspace_members {
        let package = packages
            .get(id.as_str())
            .ok_or_else(|| AppError::new(format!("missing package for workspace member '{id}' in cargo metadata")))?;
        if !package.publish.as_ref().is_some_and(Vec::is_empty) {
            inspect_package(package, &metadata.workspace_root, policy, &mut violations);
        }
    }
    Ok(violations)
}

fn inspect_package(package: &Package, workspace: &std::path::Path, policy: &Policy, violations: &mut BTreeSet<String>) {
    let name = &package.name;
    let manifest = package
        .manifest_path
        .strip_prefix(workspace)
        .unwrap_or(&package.manifest_path)
        .display();
    let mut dependencies = HashSet::new();
    for dep in &package.dependencies {
        if dep.kind == Some(Kind::Dev) {
            continue;
        }
        let key = dep.rename.as_deref().unwrap_or(&dep.name);
        dependencies.insert(key);
        let kind = if dep.kind == Some(Kind::Build) { "build" } else { "normal" };
        for feature in &dep.features {
            if policy.denies(name, Some(key), feature) {
                let target = dep.target.as_ref().map_or_else(String::new, |target| format!(", target {target}"));
                violations.insert(format!(
                    "{manifest}: '{name}' requests '{feature}' on dependency '{key}' (package '{}', {kind}{target})",
                    dep.name
                ));
            }
        }
    }

    if !package.features.contains_key("default") {
        return;
    }
    let mut pending = vec!["default"];
    let mut visited = HashSet::new();
    while let Some(feature) = pending.pop() {
        if !visited.insert(feature) {
            continue;
        }
        if policy.denies(name, None, feature) {
            violations.insert(format!("{manifest}: '{name}' enables its own '{feature}' through default features"));
        }
        for item in package.features.get(feature).into_iter().flatten() {
            if let Some((key, forwarded)) = item.split_once('/') {
                let key = key.strip_suffix('?').unwrap_or(key);
                if dependencies.contains(key) && policy.denies(name, Some(key), forwarded) {
                    violations.insert(format!(
                        "{manifest}: '{name}' forwards '{forwarded}' to dependency '{key}' through default features"
                    ));
                }
            } else if !item.starts_with("dep:") {
                pending.push(item);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::policy;

    fn load(text: Option<&str>) -> Result<Policy, AppError> {
        policy::load(text.map(|_| Path::new("policy.json")), |_| {
            Ok(text.expect("only an explicit policy is read").as_bytes().to_vec())
        })
    }

    #[test]
    fn finds_only_production_requests_and_reachable_defaults() -> Result<(), AppError> {
        let json = br#"{
          "workspace_root":"workspace","workspace_members":["app","private"],
          "packages":[
            {"id":"app","name":"app","manifest_path":"workspace/Cargo.toml","publish":["internal"],
             "features":{"default":["alias","dep:wire","wire/fakes","test-dev/test-util"],
                         "alias":["other","test-util","wire?/insecure"],"other":["alias","wire/fakes"],
                         "test-util":[],"not-default":["wire/test-util"]},
             "dependencies":[
               {"name":"transport","rename":"wire","kind":"build","target":"cfg(unix)","features":["insecure","Test-Util"]},
               {"name":"dev","rename":"test-dev","kind":"dev","features":["test-util"]},
               {"name":"explicit","kind":null,"features":["test-util"]}]},
            {"id":"private","name":"private","manifest_path":"private/Cargo.toml","publish":[],
             "features":{"default":["fakes"]},"dependencies":[]},
            {"id":"external","name":"external","manifest_path":"other/Cargo.toml","publish":null,
             "features":{"default":["fakes"]},"dependencies":[]}
          ]
        }"#;
        let actual = check(json, &load(None)?)?;
        assert_eq!(
            actual.into_iter().collect::<Vec<_>>(),
            [
                "Cargo.toml: 'app' enables its own 'test-util' through default features",
                "Cargo.toml: 'app' forwards 'fakes' to dependency 'wire' through default features",
                "Cargo.toml: 'app' forwards 'insecure' to dependency 'wire' through default features",
                "Cargo.toml: 'app' requests 'insecure' on dependency 'wire' (package 'transport', build, target cfg(unix))",
                "Cargo.toml: 'app' requests 'test-util' on dependency 'explicit' (package 'explicit', normal)",
            ]
        );
        let exceptions = r#"{"exceptions":[
            {"package":"app","feature":"test-util"},
            {"package":"app","feature":"fakes","dependency":"wire"},
            {"package":"app","feature":"insecure","dependency":"transport"}
        ]}"#;
        assert_eq!(check(json, &load(Some(exceptions))?)?.len(), 3);
        let replace = r#"{"features":["different"]}"#;
        assert!(check(json, &load(Some(replace))?)?.is_empty());
        Ok(())
    }

    #[test]
    fn unrestricted_members_are_checked_and_absent_defaults_are_not() -> Result<(), AppError> {
        let json = br#"{"workspace_root":".","workspace_members":["member"],"packages":[
            {"id":"member","name":"member","manifest_path":"Cargo.toml","publish":null,"features":{},
             "dependencies":[{"name":"dep","kind":"normal","features":["fakes"]}]}
        ]}"#;
        let active = load(Some(r#"{"features":["fakes","default"]}"#))?;
        assert_eq!(
            check(json, &active)?.into_iter().collect::<Vec<_>>(),
            ["Cargo.toml: 'member' requests 'fakes' on dependency 'dep' (package 'dep', normal)"]
        );
        Ok(())
    }

    #[test]
    fn rejects_inconsistent_or_invalid_metadata() -> Result<(), AppError> {
        let active = load(None)?;
        let missing = br#"{"workspace_root":".","workspace_members":["missing"],"packages":[]}"#;
        assert!(
            check(missing, &active)
                .expect_err("every member ID must resolve to a package")
                .to_string()
                .contains("missing package")
        );
        let duplicates = br#"{"workspace_root":".","workspace_members":["app"],"packages":[
            {"id":"app","name":"app","manifest_path":"Cargo.toml","publish":null,"features":{},"dependencies":[]},
            {"id":"app","name":"app","manifest_path":"Cargo.toml","publish":null,"features":{},"dependencies":[]}
        ]}"#;
        assert!(
            check(duplicates, &active)
                .expect_err("package IDs must be unique")
                .to_string()
                .contains("duplicate package ID")
        );
        for malformed in [
            "{}",
            r#"{"workspace_root":".","workspace_members":["app"],"packages":[
                {"id":"app","name":"app","manifest_path":"Cargo.toml","features":{},"dependencies":[]}
            ]}"#,
            r#"{"workspace_root":".","workspace_members":["app"],"packages":[
                {"id":"app","name":"app","manifest_path":"Cargo.toml","publish":null,"features":{},
                 "dependencies":[{"name":"dep","features":[]}]}
            ]}"#,
        ] {
            assert!(
                check(malformed.as_bytes(), &active)
                    .expect_err("invalid cargo metadata must not be accepted")
                    .to_string()
                    .contains("could not parse cargo metadata output"),
                "{malformed}"
            );
        }
        Ok(())
    }
}
