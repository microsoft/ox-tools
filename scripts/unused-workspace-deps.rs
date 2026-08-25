#!/usr/bin/env -S cargo +nightly -Zscript
---
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

[package]
edition = "2024"

[dependencies]
automation = { path = "../crates/automation" }
ohno = { version = "0.4", features = ["app-err"] }
argh = "0.1"
toml_edit = { version = "0.25", features = ["parse", "display"] }
---

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use argh::FromArgs;
use ohno::{AppError, IntoAppError};
use toml_edit::{DocumentMut, Item, Value};

/// Report (and optionally remove) entries of `[workspace.dependencies]` that no
/// workspace member inherits with `{ workspace = true }`.
///
/// `cargo udeps` only sees dependencies that a crate actually declares, so a stale
/// entry in the workspace dependency catalog stays invisible to it. This script
/// closes that gap by comparing the catalog against every member manifest.
#[derive(FromArgs)]
struct Args {
    /// path to the workspace root Cargo.toml (defaults to the repository root)
    #[argh(option)]
    manifest_path: Option<PathBuf>,

    /// remove the unused entries from the workspace manifest instead of only reporting them
    #[argh(switch)]
    fix: bool,

    /// name of a workspace dependency to keep even when unused (repeatable)
    #[argh(option)]
    allow: Vec<String>,
}

/// Dependency tables a member manifest can inherit workspace dependencies from.
const DEP_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

fn main() {
    let args: Args = argh::from_env();

    let manifest_path = args.manifest_path.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("script directory always has a parent")
            .join("Cargo.toml")
    });

    if let Err(e) = run(&manifest_path, &args) {
        eprintln!("❌ {e}");
        std::process::exit(1);
    }
}

fn run(manifest_path: &Path, args: &Args) -> Result<(), AppError> {
    let workspace_root = manifest_path
        .parent()
        .ok_or_else(|| ohno::app_err!("manifest path has no parent directory: {}", manifest_path.display()))?;

    let mut manifest = read_manifest(manifest_path)?;
    let declared = declared_workspace_deps(&manifest)?;

    // `automation` links its own copy of `ohno`, so map across the crate boundary
    // instead of relying on `?` to convert.
    let packages = automation::list_packages(workspace_root).map_err(|e| ohno::app_err!("failed to list workspace packages: {e}"))?;
    let mut member_manifests: BTreeSet<PathBuf> = packages.iter().map(|pkg| PathBuf::from(&pkg.manifest_path)).collect();

    // A workspace root can be a package itself, in which case its own dependency
    // tables also inherit from the catalog.
    member_manifests.insert(manifest_path.to_path_buf());

    let mut used = BTreeSet::new();
    for member in &member_manifests {
        let doc = read_manifest(member)?;
        collect_inherited_deps(&doc, &mut used);
    }

    let allowed: BTreeSet<&str> = args.allow.iter().map(String::as_str).collect();
    let unused: Vec<&String> = declared
        .iter()
        .filter(|name| !used.contains(name.as_str()) && !allowed.contains(name.as_str()))
        .collect();

    println!();
    println!("=== Unused Workspace Dependencies ===");
    println!();
    println!("Manifest: {}", manifest_path.display());
    println!("Members checked: {}", member_manifests.len());
    println!("Declared in [workspace.dependencies]: {}", declared.len());
    println!("Inherited by at least one member:      {}", used.len());
    if !allowed.is_empty() {
        println!("Explicitly allowed: {}", args.allow.join(", "));
    }
    println!();

    if unused.is_empty() {
        println!("✅ No unused workspace dependencies found.");
        return Ok(());
    }

    println!("Found {} unused entr{}:", unused.len(), if unused.len() == 1 { "y" } else { "ies" });
    for name in &unused {
        println!("  - {name}");
    }
    println!();

    if !args.fix {
        ohno::bail!(
            "{} unused workspace dependenc{} found; re-run with --fix to remove them",
            unused.len(),
            if unused.len() == 1 { "y" } else { "ies" }
        );
    }

    let removed = remove_workspace_deps(&mut manifest, &unused)?;
    std::fs::write(manifest_path, manifest.to_string()).into_app_err("failed to write workspace manifest")?;

    println!("🧹 Removed {removed} entr{} from {}.", if removed == 1 { "y" } else { "ies" }, manifest_path.display());
    println!("   Run `cargo check --workspace` to update Cargo.lock.");

    Ok(())
}

fn read_manifest(path: &Path) -> Result<DocumentMut, AppError> {
    let text = std::fs::read_to_string(path).into_app_err_with(|| format!("failed to read {}", path.display()))?;
    text.parse::<DocumentMut>()
        .into_app_err_with(|| format!("failed to parse {}", path.display()))
}

/// Names declared in the root manifest's `[workspace.dependencies]` table.
fn declared_workspace_deps(manifest: &DocumentMut) -> Result<Vec<String>, AppError> {
    let Some(table) = workspace_deps_table(manifest) else {
        ohno::bail!("the manifest has no [workspace.dependencies] table");
    };

    Ok(table.iter().map(|(key, _)| key.to_owned()).collect())
}

fn workspace_deps_table(manifest: &DocumentMut) -> Option<&dyn toml_edit::TableLike> {
    manifest.get("workspace")?.as_table_like()?.get("dependencies")?.as_table_like()
}

/// Record every dependency a manifest inherits from the workspace catalog.
fn collect_inherited_deps(doc: &DocumentMut, used: &mut BTreeSet<String>) {
    for table_name in DEP_TABLES {
        if let Some(item) = doc.get(table_name) {
            collect_from_dep_table(item, used);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and friends.
    let Some(targets) = doc.get("target").and_then(Item::as_table_like) else {
        return;
    };

    for (_, target) in targets.iter() {
        let Some(target) = target.as_table_like() else {
            continue;
        };

        for table_name in DEP_TABLES {
            if let Some(item) = target.get(table_name) {
                collect_from_dep_table(item, used);
            }
        }
    }
}

fn collect_from_dep_table(item: &Item, used: &mut BTreeSet<String>) {
    let Some(table) = item.as_table_like() else {
        return;
    };

    for (name, spec) in table.iter() {
        if inherits_from_workspace(spec) {
            used.insert(name.to_owned());
        }
    }
}

/// True for `dep = { workspace = true, .. }` and the dotted `dep.workspace = true` form.
fn inherits_from_workspace(spec: &Item) -> bool {
    spec.as_table_like()
        .and_then(|t| t.get("workspace"))
        .and_then(Item::as_value)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Remove the named entries, carrying any comments that belong to them over to the
/// next surviving entry so section headers such as `# external dependencies` survive.
fn remove_workspace_deps(manifest: &mut DocumentMut, names: &[&String]) -> Result<usize, AppError> {
    let table = manifest
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .and_then(|t| t.get_mut("dependencies"))
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| ohno::app_err!("the manifest has no [workspace.dependencies] table"))?;

    let order: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    let doomed: BTreeSet<&str> = names.iter().map(|name| name.as_str()).collect();

    let mut removed = 0;
    let mut carried = String::new();

    for name in &order {
        let prefix = table
            .key(name)
            .and_then(|key| key.leaf_decor().prefix())
            .and_then(|prefix| prefix.as_str())
            .unwrap_or("")
            .to_owned();

        if doomed.contains(name.as_str()) {
            carried.push_str(&comment_prefix(&prefix));
            table.remove(name);
            removed += 1;
        } else if !carried.is_empty() {
            let merged = format!("{carried}{prefix}");
            if let Some(mut key) = table.key_mut(name) {
                key.leaf_decor_mut().set_prefix(merged);
            }
            carried.clear();
        }
    }

    if !carried.is_empty() {
        // The removed entries were the trailing ones, so their comments have no
        // following key to sit in front of. Append them after the last surviving
        // entry's value instead, which keeps them at the end of the table where they
        // were written; attaching them to that key's *prefix* would move them up and
        // mislabel the entry they landed in front of.
        //
        // When every entry is removed the table is left empty and there is nothing to
        // anchor them to, so comments that only headed removed entries go with them.
        let trailing = table.iter().last().map(|(key, _)| key.to_owned());
        if let Some(last) = trailing
            && let Some(value) = table.get_mut(&last).and_then(Item::as_value_mut)
        {
            let suffix = value.decor().suffix().and_then(|s| s.as_str()).unwrap_or("").to_owned();
            value.decor_mut().set_suffix(format!("{suffix}{carried}"));
        }
    }

    Ok(removed)
}

/// A removed entry's decor is only worth keeping when it carries comments, such as
/// the `# external dependencies` header that groups the entries after it.
fn comment_prefix(prefix: &str) -> String {
    if prefix.lines().any(|line| line.trim_start().starts_with('#')) {
        prefix.to_owned()
    } else {
        String::new()
    }
}
