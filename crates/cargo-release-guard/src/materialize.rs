// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Isolate source and remove every omitted-workspace dependency shortcut.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, TableLike, value};

use crate::config::Config;
use crate::model::{Candidates, Package, Report, Workspace};
use crate::{Result, fail, selection, source};

pub(crate) fn prepare(
    workspace: &Workspace,
    candidates: &Candidates,
    output: &Path,
    config: &Config,
    report: &mut Report,
) -> Result<PathBuf> {
    selection::validate(workspace, candidates, output, config)?;
    let selected: BTreeSet<_> = candidates.candidates.iter().map(|candidate| candidate.name.as_str()).collect();
    let retained: Vec<_> = workspace
        .packages
        .values()
        .filter(|package| !package.publishable() || selected.contains(package.name.as_str()))
        .collect();
    let omitted: Vec<_> = workspace
        .packages
        .values()
        .filter(|package| package.publishable() && !selected.contains(package.name.as_str()))
        .collect();
    validate_layout(workspace, &retained, &omitted)?;
    let directory = output.join("workspace");
    fs::create_dir(&directory)?;
    let mut excluded = candidates.artifact_roots.clone();
    excluded.push(output.to_owned());
    for relative in source::files(&workspace.root, &excluded)? {
        let original = workspace.root.join(&relative);
        if omitted.iter().any(|package| original.starts_with(package.directory()))
            || relative == Path::new("Cargo.lock")
            || relative.components().any(|part| part.as_os_str() == ".cargo")
        {
            continue;
        }
        source::copy_file(&original, &directory.join(&relative))?;
    }
    let mut root = workspace.document.clone();
    root.remove("patch");
    root.remove("replace");
    let mut workspace_table = root.get("workspace").and_then(Item::as_table).cloned().unwrap_or_default();
    workspace_table.remove("dependencies");
    workspace_table.remove("package");
    workspace_table.remove("default-members");
    workspace_table.remove("exclude");
    let resolver = workspace_table.get("resolver").and_then(Item::as_str).unwrap_or("1");
    if !matches!(resolver, "2" | "3") {
        return fail("release guard requires workspace resolver 2 or 3 to isolate production features");
    }
    let mut members = Array::new();
    for package in &retained {
        let manifest = package.manifest.strip_prefix(&workspace.root)?;
        let parent = manifest
            .parent()
            .expect("stripping the workspace root preserves the Cargo.toml filename");
        members.push(if parent.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            parent.to_string_lossy().into_owned()
        });
    }
    workspace_table["members"] = value(members);
    root["workspace"] = Item::Table(workspace_table);
    for package in retained {
        let mut document = package.document.clone();
        document.remove("workspace");
        document.remove("patch");
        document.remove("replace");
        materialize_package(&mut document, package, workspace, &directory, &omitted, report)?;
        rewrite_tables(document.as_table_mut(), package, workspace, &directory, report, false)?;
        if package.manifest == workspace.root.join("Cargo.toml") {
            for key in ["workspace", "profile"] {
                if let Some(item) = root.get(key) {
                    document[key] = item.clone();
                }
            }
            root = document;
        } else {
            fs::write(
                directory.join(package.manifest.strip_prefix(&workspace.root)?),
                document.to_string(),
            )?;
        }
    }
    for candidate in &candidates.candidates {
        let package = &workspace.packages[&candidate.name];
        let parent = package.directory();
        let path = directory.join(parent.strip_prefix(&workspace.root)?);
        let patch_key = config.patch_key(&candidate.registry)?;
        // Index URLs can contain authentication. Keep credentials in Cargo configuration, never manifests.
        if patch_key.contains(['@', '?', '#']) {
            return fail(
                "registry index URLs containing user information, query strings, or fragments are unsupported; configure Cargo authentication separately",
            );
        }
        root["patch"][&patch_key][&candidate.name]["path"] = value(path.to_string_lossy().as_ref());
    }
    fs::write(directory.join("Cargo.toml"), root.to_string())?;
    selection::validate(workspace, candidates, output, config)?;
    Ok(directory)
}

fn validate_layout(workspace: &Workspace, retained: &[&Package], omitted: &[&Package]) -> Result<()> {
    for package in omitted {
        if package.manifest == workspace.root.join("Cargo.toml") {
            return fail("an omitted publishable package at the workspace root is unsupported; use a virtual workspace");
        }
    }
    for left in retained {
        for right in omitted {
            let left = left.directory();
            let right = right.directory();
            if left.starts_with(right) || right.starts_with(left) {
                return fail("nested retained and omitted package directories cannot be safely isolated");
            }
        }
    }
    Ok(())
}

fn materialize_package(
    document: &mut DocumentMut,
    package: &Package,
    workspace: &Workspace,
    output: &Path,
    omitted: &[&Package],
    report: &mut Report,
) -> Result<()> {
    let directory = package.directory();
    let inherited = workspace.document.get("workspace").and_then(|item| item.get("package"));
    let package_table = document["package"]
        .as_table_mut()
        .ok_or_else(|| std::io::Error::other("package table is missing"))?;
    package_table.remove("workspace");
    for (key, item) in package_table.iter_mut() {
        let is_inherited = item.get("workspace").and_then(Item::as_bool) == Some(true);
        if is_inherited {
            *item = inherited
                .and_then(|table| table.get(key.get()))
                .cloned()
                .ok_or_else(|| std::io::Error::other(format!("missing inherited package field: {key}")))?;
        }
        if ["readme", "license-file", "build"].contains(&key.get()) && item.is_str() {
            let base = if is_inherited { &workspace.root } else { directory };
            *item = value(
                isolated_path(base, item.as_str().expect("checked is_str"), workspace, output, omitted)?
                    .to_string_lossy()
                    .as_ref(),
            );
        }
    }
    if document.get("lints").and_then(|item| item.get("workspace")).and_then(Item::as_bool) == Some(true) {
        document["lints"] = workspace
            .document
            .get("workspace")
            .and_then(|item| item.get("lints"))
            .cloned()
            .ok_or_else(|| std::io::Error::other("missing inherited workspace lints"))?;
    }
    if let Some(lib) = document.get_mut("lib") {
        materialize_target(lib, directory, workspace, output, omitted)?;
    }
    for key in ["bin", "test", "bench", "example"] {
        if let Some(item) = document.get_mut(key)
            && let Some(array) = item.as_array()
        {
            let mut targets = ArrayOfTables::new();
            for entry in array {
                let entries = entry
                    .as_inline_table()
                    .ok_or_else(|| std::io::Error::other(format!("package {} has a non-table entry in {key} targets", package.name)))?;
                let mut table = Table::new();
                for (key, entry) in entries {
                    table[key] = Item::Value(entry.clone());
                }
                targets.push(table);
            }
            *item = Item::ArrayOfTables(targets);
        }
        if let Some(targets) = document.get_mut(key).and_then(Item::as_array_of_tables_mut) {
            for target in targets.iter_mut() {
                if let Some(path) = target.get("path").and_then(Item::as_str) {
                    target["path"] = value(
                        isolated_path(directory, path, workspace, output, omitted)?
                            .to_string_lossy()
                            .as_ref(),
                    );
                }
                if (key == "bench" || (key == "example" && target.get("harness").and_then(Item::as_bool) == Some(false)))
                    && target.get("test").and_then(Item::as_bool) == Some(true)
                {
                    target["test"] = value(false);
                    report.diagnostics.push(format!(
                        "{} {key} {}: disabled test execution in W_b; {}",
                        package.name,
                        target.get("name").and_then(Item::as_str).unwrap_or("<unnamed>"),
                        if key == "example" {
                            "harness-free examples are compiled separately, never run"
                        } else {
                            "benchmarks are outside the ordinary test suite"
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

fn materialize_target(item: &mut Item, base: &Path, workspace: &Workspace, output: &Path, omitted: &[&Package]) -> Result<()> {
    if let Some(path) = item.get("path").and_then(Item::as_str) {
        item["path"] = value(isolated_path(base, path, workspace, output, omitted)?.to_string_lossy().as_ref());
    }
    Ok(())
}

fn isolated_path(base: &Path, path: &str, workspace: &Workspace, output: &Path, omitted: &[&Package]) -> Result<PathBuf> {
    let original = base.join(path);
    source::reject_links(&original)?;
    let canonical = original.canonicalize()?;
    if !canonical.starts_with(&workspace.root) || omitted.iter().any(|package| canonical.starts_with(package.directory())) {
        return fail(format!("package data or target path escapes retained source: {path}"));
    }
    let target = output.join(canonical.strip_prefix(&workspace.root)?);
    if !target.is_file() {
        return fail(format!("package data or target is not part of the source snapshot: {path}"));
    }
    Ok(target)
}

fn rewrite_tables(
    table: &mut dyn TableLike,
    owner: &Package,
    workspace: &Workspace,
    output: &Path,
    report: &mut Report,
    in_target: bool,
) -> Result<()> {
    for kind in ["dependencies", "build-dependencies", "dev-dependencies"] {
        if let Some(dependencies) = table.get_mut(kind).and_then(Item::as_table_like_mut) {
            for (name, item) in dependencies.iter_mut() {
                *item = dependency(name.get(), item, kind == "dev-dependencies", owner, workspace, output, report)?;
            }
        }
    }
    if !in_target && let Some(targets) = table.get_mut("target") {
        let targets = targets
            .as_table_like_mut()
            .ok_or_else(|| std::io::Error::other("target dependency configuration must be a table"))?;
        for (_, target) in targets.iter_mut() {
            let target = target
                .as_table_like_mut()
                .ok_or_else(|| std::io::Error::other("target dependency entry must be a table"))?;
            rewrite_tables(target, owner, workspace, output, report, true)?;
        }
    }
    Ok(())
}

fn dependency(
    alias: &str,
    item: &Item,
    dev: bool,
    owner: &Package,
    workspace: &Workspace,
    output: &Path,
    report: &mut Report,
) -> Result<Item> {
    let inherited = item.get("workspace").and_then(Item::as_bool) == Some(true);
    let mut table = if inherited {
        inherit_dependency(alias, item, workspace)?
    } else {
        dependency_table(item)?
    };
    let source_shortcut = table.contains_key("path") || table.contains_key("git");
    let name = table.get("package").and_then(Item::as_str).unwrap_or(alias).to_owned();
    if let Some(destination) = workspace
        .packages
        .get(&name)
        .filter(|package| package.publishable() || table.contains_key("path"))
    {
        if let Some(path) = table.get("path").and_then(Item::as_str) {
            let base = if inherited { &workspace.root } else { owner.directory() };
            let manifest = base.join(path).join("Cargo.toml").canonicalize()?;
            if manifest != destination.manifest {
                return fail(format!(
                    "dependency {alias} has the same name as a workspace package but a different local source"
                ));
            }
        }
        if destination.publishable() {
            if table.get("version").and_then(Item::as_str).is_none() {
                if !dev && owner.publishable() {
                    return fail(format!(
                        "{} has a versionless production dependency on {name}; declare an honest registry requirement",
                        owner.name
                    ));
                }
                table["version"] = value(destination.version.to_string());
                report.derived_requirements.push(format!(
                    "{} test/support dependency {alias}: derived {} from {name}'s declared package version",
                    owner.name, destination.version
                ));
            }
            let requirement = table
                .get("version")
                .and_then(Item::as_str)
                .expect("version supplied or derived above");
            semver::VersionReq::parse(requirement)?;
            table.remove("path");
            if source_shortcut
                && table.get("registry").is_none()
                && table.get("registry-index").is_none()
                && let Some(allowed) = &destination.publish
            {
                if allowed.len() == 1 && allowed[0] != "crates-io" {
                    table["registry"] = value(&allowed[0]);
                } else if allowed.len() > 1 {
                    return fail(format!(
                        "dependency {alias} requires an explicit registry because {name} has multiple publication destinations"
                    ));
                }
            }
        } else {
            if owner.publishable() && !dev {
                return fail(format!(
                    "{} has an unpublished production dependency on private package {name}",
                    owner.name
                ));
            }
            let directory = destination.directory();
            table["path"] = value(output.join(directory.strip_prefix(&workspace.root)?).to_string_lossy().as_ref());
            table.remove("registry");
            table.remove("registry-index");
        }
        for key in ["git", "branch", "tag", "rev"] {
            table.remove(key);
        }
    } else if table.contains_key("path") {
        return fail(format!(
            "external path dependency {} -> {alias} cannot be safely isolated; make support packages workspace members",
            owner.name
        ));
    }
    Ok(Item::Table(table))
}

fn inherit_dependency(alias: &str, item: &Item, workspace: &Workspace) -> Result<Table> {
    let entry = workspace
        .document
        .get("workspace")
        .and_then(|item| item.get("dependencies"))
        .and_then(|item| item.get(alias))
        .ok_or_else(|| std::io::Error::other(format!("workspace dependency {alias} is missing")))?;
    let mut table = dependency_table(entry)?;
    let local = item
        .as_table_like()
        .expect("the caller checked workspace=true on this dependency table");
    for (key, entry) in local.iter().filter(|(key, _)| *key != "workspace") {
        if key == "default-features"
            && entry.as_bool() == Some(false)
            && table.get("default-features").and_then(Item::as_bool) != Some(false)
        {
            // On pre-2024 editions Cargo ignores this attempt to disable inherited defaults.
            // Edition 2024 rejects it during discovery, before materialization.
            continue;
        }
        if key == "features" {
            let mut features = table.get("features").and_then(Item::as_array).cloned().unwrap_or_default();
            for feature in entry
                .as_array()
                .ok_or_else(|| std::io::Error::other("dependency features must be an array"))?
            {
                if let Some(feature) = feature.as_str()
                    && !features.iter().any(|existing| existing.as_str() == Some(feature))
                {
                    features.push(feature);
                }
            }
            table["features"] = value(features);
        } else {
            table[key] = entry.clone();
        }
    }
    Ok(table)
}

fn dependency_table(item: &Item) -> Result<Table> {
    let mut table = Table::new();
    if let Some(version) = item.as_str() {
        table["version"] = value(version);
    } else if let Some(entries) = item.as_table_like() {
        for (key, value) in entries.iter() {
            table[key] = value.clone();
        }
    } else {
        return fail("dependency must be a version string or a table");
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use toml_edit::value;

    use super::{dependency, dependency_table, materialize_package, prepare, rewrite_tables, validate_layout};
    use crate::config::Config;
    use crate::model::{Candidate, Candidates, Report};
    use crate::{source, test_support};

    #[test]
    fn manifest_write_failure_preserves_source_and_conflicting_output_files() {
        let directory = test_support::directory();
        let mut workspace = test_support::workspace(directory.path(), &[("consumer", "")]);
        fs::write(workspace.root.join(".gitignore"), "target/\n").unwrap();
        fs::write(workspace.root.join("blocked"), "preserved source").unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&workspace.root)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let output = source::claim_output(&workspace.root.join("target").join("write-failure")).unwrap();
        let config = Config::load(&workspace.root, &output).unwrap();
        workspace.packages.get_mut("consumer").unwrap().manifest = workspace.root.join("blocked").join("Cargo.toml");
        let head = source::git_text(&workspace.root, &["rev-parse", "HEAD"]).unwrap();
        let candidates = Candidates {
            schema_version: 1,
            base_commit: head.clone(),
            head_commit: head,
            source_root: workspace.root.clone(),
            source_digest: source::digest(&workspace.root, std::slice::from_ref(&output)).unwrap(),
            configuration_digest: config.fingerprint().unwrap(),
            artifact_roots: vec![output.clone()],
            candidates: vec![Candidate {
                name: "consumer".into(),
                version: "1.0.0".into(),
                registry: "crates-io".into(),
                reason: "test".into(),
            }],
        };
        prepare(&workspace, &candidates, &output, &config, &mut Report::new("test")).unwrap_err();
        assert_eq!(
            fs::read_to_string(output.join("workspace").join("blocked")).unwrap(),
            "preserved source"
        );
        assert_eq!(
            source::digest(&workspace.root, std::slice::from_ref(&output)).unwrap(),
            candidates.source_digest
        );
    }

    #[test]
    fn dependency_validation_rejects_missing_inheritance_alias_mismatch_and_external_paths() {
        let directory = test_support::directory();
        let workspace = test_support::workspace(directory.path(), &[("consumer", ""), ("helper", "")]);
        let owner = &workspace.packages["consumer"];
        let mut report = Report::new("test");
        for (text, diagnostic) in [
            ("workspace=true", "workspace dependency"),
            ("path='../consumer'\nversion='1'", "different local source"),
        ] {
            let item = toml_edit::Item::Table(text.parse::<toml_edit::DocumentMut>().unwrap().as_table().clone());
            assert!(
                dependency("helper", &item, false, owner, &workspace, directory.path(), &mut report)
                    .unwrap_err()
                    .to_string()
                    .contains(diagnostic)
            );
        }
        let item = toml_edit::Item::Table("path='../outside'".parse::<toml_edit::DocumentMut>().unwrap().as_table().clone());
        assert!(
            dependency("external", &item, false, owner, &workspace, directory.path(), &mut report)
                .unwrap_err()
                .to_string()
                .contains("external path dependency")
        );
        assert!(
            dependency_table(&value(12))
                .unwrap_err()
                .to_string()
                .contains("version string or a table")
        );
    }

    #[test]
    fn ambiguous_destinations_and_private_production_edges_fail_before_resolution() {
        let directory = test_support::directory();
        let mut workspace = test_support::workspace(directory.path(), &[("consumer", ""), ("helper", "")]);
        workspace.packages.get_mut("helper").unwrap().publish = Some(vec!["first".into(), "second".into()]);
        let item = toml_edit::Item::Table(
            "path='../helper'\nversion='1'"
                .parse::<toml_edit::DocumentMut>()
                .unwrap()
                .as_table()
                .clone(),
        );
        let mut report = Report::new("test");
        assert!(
            dependency(
                "helper",
                &item,
                false,
                &workspace.packages["consumer"],
                &workspace,
                directory.path(),
                &mut report
            )
            .unwrap_err()
            .to_string()
            .contains("multiple publication destinations")
        );
        workspace.packages.get_mut("helper").unwrap().publish = Some(vec!["crates-io".into()]);
        let rewritten = dependency(
            "helper",
            &item,
            false,
            &workspace.packages["consumer"],
            &workspace,
            directory.path(),
            &mut report,
        )
        .unwrap();
        assert!(rewritten.get("path").is_none());
        assert!(rewritten.get("registry").is_none());
        workspace.packages.get_mut("helper").unwrap().publish = Some(Vec::new());
        assert!(
            dependency(
                "helper",
                &item,
                false,
                &workspace.packages["consumer"],
                &workspace,
                directory.path(),
                &mut report
            )
            .unwrap_err()
            .to_string()
            .contains("unpublished production")
        );
    }

    #[test]
    fn unsafe_layouts_and_unmaterialized_package_data_are_rejected() {
        let directory = test_support::directory();
        let mut workspace = test_support::workspace(directory.path(), &[("consumer", ""), ("helper", "")]);
        let mut helper = workspace.packages["helper"].clone();
        helper.manifest = workspace.root.join("Cargo.toml");
        assert!(
            validate_layout(&workspace, &[&workspace.packages["consumer"]], &[&helper])
                .unwrap_err()
                .to_string()
                .contains("workspace root")
        );
        helper.manifest = workspace.packages["consumer"].directory().join("nested").join("Cargo.toml");
        assert!(
            validate_layout(&workspace, &[&workspace.packages["consumer"]], &[&helper])
                .unwrap_err()
                .to_string()
                .contains("nested retained")
        );
        let package = workspace.packages["consumer"].clone();
        let mut document = package.document.clone();
        document["lib"]["path"] = value("src/lib.rs");
        let output = directory.path().join("output");
        let mut report = Report::new("test");
        assert!(
            materialize_package(&mut document, &package, &workspace, &output, &[], &mut report)
                .unwrap_err()
                .to_string()
                .contains("not part of the source snapshot")
        );
        workspace.document["workspace"]["package"]["license"] = value("MIT");
        document = package.document.clone();
        document["package"]["license"]["workspace"] = value(true);
        document["lib"]["doctest"] = value(false);
        materialize_package(&mut document, &package, &workspace, &output, &[], &mut report).unwrap();
        assert_eq!(document["package"]["license"].as_str(), Some("MIT"));
    }

    #[test]
    fn malformed_target_dependency_shapes_fail_closed() {
        let directory = test_support::directory();
        let workspace = test_support::workspace(directory.path(), &[("consumer", "")]);
        let mut report = Report::new("test");
        for (text, diagnostic) in [
            ("target=42", "target dependency configuration must be a table"),
            ("[target]\ninvalid=42", "target dependency entry must be a table"),
        ] {
            let mut document = text.parse::<toml_edit::DocumentMut>().unwrap();
            assert!(
                rewrite_tables(
                    document.as_table_mut(),
                    &workspace.packages["consumer"],
                    &workspace,
                    directory.path(),
                    &mut report,
                    false
                )
                .unwrap_err()
                .to_string()
                .contains(diagnostic)
            );
        }
    }
}
