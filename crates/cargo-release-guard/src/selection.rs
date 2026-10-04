// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Explicit publication intent, independent of source-impact analysis.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::model::{Candidate, Candidates, Package, Workspace};
use crate::{Result, fail, source};

pub(crate) fn registry(package: &Package, requested: Option<&str>, default: &str) -> Result<String> {
    if !package.publishable() {
        return fail(format!("{} is private and cannot be selected for publication", package.name));
    }
    if let Some(requested) = requested {
        if package
            .publish
            .as_ref()
            .is_some_and(|allowed| !allowed.iter().any(|name| name == requested))
        {
            return fail(format!("{} does not allow publication to {requested}", package.name));
        }
        return Ok(requested.into());
    }
    match &package.publish {
        None => Ok(default.into()),
        Some(allowed) if allowed.len() == 1 => Ok(allowed[0].clone()),
        Some(_) => fail(format!("{} permits multiple registries; pass --registry", package.name)),
    }
}

pub(crate) fn select(
    workspace: &Workspace,
    base: &str,
    override_list: Option<&Path>,
    requested_registry: Option<&str>,
    config: &Config,
    output: &Path,
    diagnostics: &mut Vec<String>,
) -> Result<Candidates> {
    let head_commit = source::git_text(&workspace.root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let base_ref = source::git_text(
        &workspace.root,
        &["rev-parse", "--verify", "--end-of-options", &format!("{base}^{{commit}}")],
    )?;
    let base_commit = source::git_text(&workspace.root, &["merge-base", &base_ref, &head_commit])?;
    let historical = source::historical_workspace(workspace, &base_commit, output, diagnostics)?;
    for package in workspace.packages.values().filter(|package| package.publishable()) {
        if historical
            .packages
            .get(&package.name)
            .is_some_and(|old| package.version < old.version)
        {
            return fail(format!(
                "version regression for {}: {} is below the PR base",
                package.name, package.version
            ));
        }
    }
    let mut candidates = Vec::new();
    if let Some(path) = override_list {
        let names: Vec<String> = serde_json::from_slice(&fs::read(path)?)?;
        let mut seen = BTreeSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                return fail(format!("duplicate explicit candidate: {name}"));
            }
            let Some(package) = workspace.packages.get(&name) else {
                return fail(format!("unknown explicit candidate: {name}"));
            };
            candidates.push(Candidate {
                registry: registry(package, requested_registry, &config.default_registry)?,
                name,
                version: package.version.to_string(),
                reason: "explicit".into(),
            });
        }
    } else {
        candidates = infer(workspace, &historical, requested_registry, &config.default_registry)?;
    }
    candidates.sort_by(|left, right| left.name.cmp(&right.name));
    let artifact_roots = vec![output.to_owned()];
    Ok(Candidates {
        schema_version: 1,
        base_commit,
        head_commit,
        source_root: workspace.root.clone(),
        source_digest: source::digest(&workspace.root, &artifact_roots)?,
        configuration_digest: config.fingerprint()?,
        artifact_roots,
        candidates,
    })
}

fn infer(workspace: &Workspace, base: &Workspace, requested: Option<&str>, default: &str) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    for package in workspace.packages.values().filter(|package| package.publishable()) {
        let old = base.packages.get(&package.name);
        let reason = match old {
            None => Some("new_package"),
            Some(old) if !old.publishable() => Some("newly_publishable"),
            Some(old) if package.version > old.version => Some("version_advance"),
            Some(old) if newly_allowed(package, old, requested) => Some("new_registry_destination"),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            candidates.push(Candidate {
                name: package.name.clone(),
                version: package.version.to_string(),
                registry: registry(package, requested, default)?,
                reason: reason.into(),
            });
        }
    }
    Ok(candidates)
}

fn newly_allowed(package: &Package, old: &Package, requested: Option<&str>) -> bool {
    let Some(previous) = &old.publish else { return false };
    if let Some(registry) = requested {
        return !previous.iter().any(|name| name == registry)
            && package
                .publish
                .as_ref()
                .is_none_or(|current| current.iter().any(|name| name == registry));
    }
    package
        .publish
        .as_ref()
        .is_none_or(|current| current.iter().any(|name| !previous.contains(name)))
}

pub(crate) fn validate(workspace: &Workspace, candidates: &Candidates, output: &Path, config: &Config) -> Result<()> {
    if candidates.schema_version != 1 {
        return fail("unsupported candidate report schema_version");
    }
    if candidates.source_root != workspace.root {
        return fail("candidate report belongs to a different source workspace");
    }
    if candidates.configuration_digest != config.fingerprint()? {
        return fail("Cargo configuration snapshot mismatch: regenerate the candidate report");
    }
    for root in &candidates.artifact_roots {
        if workspace.root.starts_with(root) || !root.join(".release-guard-owned").is_file() {
            return fail("candidate report contains an invalid artifact exclusion");
        }
    }
    let mut excluded = candidates.artifact_roots.clone();
    excluded.push(output.to_owned());
    if source::digest(&workspace.root, &excluded)? != candidates.source_digest
        || source::git_text(&workspace.root, &["rev-parse", "--verify", "HEAD^{commit}"])? != candidates.head_commit
    {
        return fail("source snapshot mismatch: regenerate the candidate report for the current checkout");
    }
    let mut names = BTreeSet::new();
    for candidate in &candidates.candidates {
        if !names.insert(&candidate.name) {
            return fail(format!("duplicate candidate in report: {}", candidate.name));
        }
        let Some(package) = workspace.packages.get(&candidate.name) else {
            return fail(format!("candidate no longer exists: {}", candidate.name));
        };
        if package.version.to_string() != candidate.version {
            return fail(format!("candidate version mismatch: {}", candidate.name));
        }
        registry(package, Some(&candidate.registry), "crates-io")?;
    }
    Ok(())
}
