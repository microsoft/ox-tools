// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use cargo_metadata::{DependencyKind, Metadata, Package};
use ohno::AppError;

use crate::policy::Policy;

pub(crate) fn check(json: &[u8], policy: &Policy) -> Result<BTreeSet<String>, AppError> {
    let metadata =
        serde_json::from_slice(json).map_err(|error| AppError::new(format!("could not parse cargo metadata output: {error}")))?;
    inspect(&metadata, policy)
}

fn inspect(metadata: &Metadata, policy: &Policy) -> Result<BTreeSet<String>, AppError> {
    let mut packages = BTreeMap::new();
    for package in &metadata.packages {
        if packages.insert(&package.id, package).is_some() {
            return Err(AppError::new(format!("duplicate package ID '{}' in cargo metadata", package.id)));
        }
    }

    let mut violations = BTreeSet::new();
    for id in &metadata.workspace_members {
        let package = packages
            .get(id)
            .ok_or_else(|| AppError::new(format!("missing package for workspace member '{id}' in cargo metadata")))?;
        if !package.publish.as_ref().is_some_and(Vec::is_empty) {
            inspect_package(package, metadata.workspace_root.as_std_path(), policy, &mut violations)?;
        }
    }
    Ok(violations)
}

fn inspect_package(package: &Package, workspace: &Path, policy: &Policy, violations: &mut BTreeSet<String>) -> Result<(), AppError> {
    let name = package.name.as_str();
    let manifest = package
        .manifest_path
        .as_std_path()
        .strip_prefix(workspace)
        .unwrap_or(package.manifest_path.as_std_path())
        .display();
    let mut dependencies = HashSet::new();
    let mut optional = HashSet::new();
    for dep in &package.dependencies {
        let kind = match dep.kind {
            DependencyKind::Development => continue,
            DependencyKind::Normal => "normal",
            DependencyKind::Build => "build",
            _ => return Err(AppError::new(format!("{manifest}: unknown dependency kind for '{}'", dep.name))),
        };
        let key = dep.rename.as_deref().unwrap_or(&dep.name);
        dependencies.insert(key);
        if dep.optional {
            optional.insert(key);
        }
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

    let mut pending = vec!["default"];
    let mut remaining: HashMap<_, _> = package.features.iter().map(|(key, edges)| (key.as_str(), edges)).collect();
    while let Some(feature) = pending.pop() {
        if let Some(edges) = remaining.remove(feature) {
            if policy.denies(name, None, feature) {
                violations.insert(format!("{manifest}: '{name}' enables its own '{feature}' through default features"));
            }
            for item in edges {
                if let Some((key, forwarded)) = item.split_once('/') {
                    let (key, weak) = key.strip_suffix('?').map_or((key, false), |key| (key, true));
                    if !weak && optional.contains(key) {
                        pending.push(key);
                    }
                    // Weak forwards are requests even when the dependency never activates.
                    if dependencies.contains(key) && policy.denies(name, Some(key), forwarded) {
                        violations.insert(format!(
                            "{manifest}: '{name}' forwards '{forwarded}' to dependency '{key}' through default features"
                        ));
                    }
                } else {
                    pending.push(item);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
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
          "version":1,"target_directory":"target","workspace_root":"workspace","workspace_members":["app","private"],
          "packages":[
            {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"workspace/Cargo.toml","publish":["internal"],
             "features":{"default":["alias","dep:wire","wire/fakes","test-dev/test-util"],
                         "alias":["other","test-util","wire?/insecure"],"other":["alias","wire/fakes"],
                         "test-util":[],"not-default":["wire/test-util"]},
             "dependencies":[
               {"name":"transport","req":"*","uses_default_features":true,"rename":"wire","kind":"build","target":"cfg(unix)","features":["insecure","Test-Util"],"optional":true},
               {"name":"dev","req":"*","uses_default_features":true,"rename":"test-dev","kind":"dev","features":["test-util"],"optional":false},
               {"name":"explicit","req":"*","uses_default_features":true,"kind":null,"features":["test-util"],"optional":false}]},
            {"id":"private","name":"private","version":"0.1.0","targets":[],"manifest_path":"private/Cargo.toml","publish":[],
             "features":{"default":["fakes"],"fakes":[]},"dependencies":[]},
            {"id":"external","name":"external","version":"0.1.0","targets":[],"manifest_path":"other/Cargo.toml","publish":null,
             "features":{"default":["fakes"],"fakes":[]},"dependencies":[]}
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
        assert_eq!(
            check(json, &load(Some(exceptions))?)?.into_iter().collect::<Vec<_>>(),
            [
                "Cargo.toml: 'app' forwards 'insecure' to dependency 'wire' through default features",
                "Cargo.toml: 'app' requests 'insecure' on dependency 'wire' (package 'transport', build, target cfg(unix))",
                "Cargo.toml: 'app' requests 'test-util' on dependency 'explicit' (package 'explicit', normal)",
            ]
        );
        let replace = r#"{"features":["different"]}"#;
        assert!(check(json, &load(Some(replace))?)?.is_empty());
        Ok(())
    }

    #[test]
    fn unrestricted_members_are_checked_and_absent_defaults_are_not() -> Result<(), AppError> {
        let json = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["member"],"packages":[
            {"id":"member","name":"member","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,"features":{},
             "dependencies":[{"name":"dep","req":"*","uses_default_features":true,"kind":"normal","features":["fakes"],"optional":false}]}
        ]}"#;
        let active = load(Some(r#"{"features":["fakes","default"]}"#))?;
        assert_eq!(
            check(json, &active)?.into_iter().collect::<Vec<_>>(),
            ["Cargo.toml: 'member' requests 'fakes' on dependency 'dep' (package 'dep', normal)"]
        );
        Ok(())
    }

    #[test]
    fn only_a_strong_forward_to_an_optional_dependency_enables_the_matching_local_feature() -> Result<(), AppError> {
        let active = load(None)?;
        for (request, optional, expected) in [
            (
                "transport/std",
                true,
                &["Cargo.toml: 'app' enables its own 'insecure' through default features"][..],
            ),
            ("transport?/std", true, &[][..]),
            ("dep:transport", true, &[][..]),
            ("transport/std", false, &[][..]),
        ] {
            let json = format!(
                r#"{{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["app"],"packages":[
                    {{"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,
                      "features":{{"default":["{request}"],"transport":["insecure"],"insecure":[]}},
                      "dependencies":[{{"name":"serde","req":"*","uses_default_features":true,"rename":"transport","kind":null,"features":[],"optional":{optional}}}]}}
                ]}}"#
            );
            assert_eq!(
                check(json.as_bytes(), &active)?.into_iter().collect::<Vec<_>>(),
                expected,
                "{request}, optional={optional}"
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_inconsistent_or_invalid_metadata() -> Result<(), AppError> {
        let active = load(None)?;
        let missing = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["missing"],"packages":[]}"#;
        assert_eq!(
            check(missing, &active)
                .expect_err("every member ID must resolve to a package")
                .to_string(),
            "missing package for workspace member 'missing' in cargo metadata"
        );
        let duplicates = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["app"],"packages":[
            {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,"features":{},"dependencies":[]},
            {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,"features":{},"dependencies":[]}
        ]}"#;
        assert_eq!(
            check(duplicates, &active).expect_err("package IDs must be unique").to_string(),
            "duplicate package ID 'app' in cargo metadata"
        );
        for (malformed, field) in [
            (
                r#"{"version":1,"target_directory":"target","workspace_members":[],"packages":[]}"#,
                "workspace_root",
            ),
            (
                r#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["app"],"packages":[
                {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,"features":{},
                 "dependencies":[{"name":"dep","req":"*","uses_default_features":true,"features":[],"optional":false}]}
            ]}"#,
                "kind",
            ),
        ] {
            assert!(
                check(malformed.as_bytes(), &active)
                    .expect_err("invalid cargo metadata must not be accepted")
                    .to_string()
                    .starts_with(&format!("could not parse cargo metadata output: missing field `{field}`")),
                "{malformed}"
            );
        }
        let unknown_kind = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["app"],"packages":[
            {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,"features":{},
             "dependencies":[{"name":"dep","req":"*","uses_default_features":true,"kind":"unknown","features":[],"optional":false}]}
        ]}"#;
        assert_eq!(
            check(unknown_kind, &active)
                .expect_err("unknown dependency kinds cannot be classified")
                .to_string(),
            "Cargo.toml: unknown dependency kind for 'dep'"
        );
        Ok(())
    }
}
