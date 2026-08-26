// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Detection: which `[workspace.dependencies]` entries no member inherits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Dependency tables a member manifest can inherit workspace dependencies from.
const DEP_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Key under `[workspace.metadata]` holding this tool's configuration.
const METADATA_KEY: &str = "ensure-no-unused-workspace-deps";

/// What a manifest turned out to be.
pub enum Catalog {
    /// The manifest has no `[workspace]` table, so it declares no catalog.
    NotAWorkspace,

    /// The manifest is a workspace root. The catalog may still be empty.
    Workspace(WorkspaceCatalog),
}

/// The `[workspace.dependencies]` catalog of a workspace root, with the
/// allow-list that accompanies it.
pub struct WorkspaceCatalog {
    /// Catalog entry names, in the order the manifest declares them.
    pub declared: Vec<String>,

    /// Names configured as deliberate exceptions.
    pub allowed: BTreeSet<String>,
}

/// Read a manifest's text.
pub fn read_manifest_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

/// Parse manifest text that came from `path`.
pub fn parse_manifest(text: &str, path: &Path) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

/// Read and parse a manifest.
pub fn read_manifest(path: &Path) -> Result<DocumentMut> {
    parse_manifest(&read_manifest_text(path)?, path)
}

/// Classify a parsed manifest and, when it is a workspace root, collect its
/// catalog and allow-list.
pub fn catalog(manifest: &DocumentMut) -> Catalog {
    let Some(workspace) = manifest.get("workspace").and_then(Item::as_table_like) else {
        return Catalog::NotAWorkspace;
    };

    let declared = workspace
        .get("dependencies")
        .and_then(Item::as_table_like)
        .map(|table| table.iter().map(|(key, _)| key.to_owned()).collect())
        .unwrap_or_default();

    let allowed = workspace
        .get("metadata")
        .and_then(Item::as_table_like)
        .and_then(|metadata| metadata.get(METADATA_KEY))
        .and_then(Item::as_table_like)
        .and_then(|config| config.get("allowed"))
        .and_then(Item::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).map(ToOwned::to_owned).collect())
        .unwrap_or_default();

    Catalog::Workspace(WorkspaceCatalog { declared, allowed })
}

/// Collect the catalog keys that member manifests inherit.
///
/// `members` are manifest paths as reported by `cargo metadata`; a manifest that
/// cannot be read or parsed fails the run rather than being silently treated as
/// inheriting nothing, which would turn a read error into false accusations.
pub fn inherited(members: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut used = BTreeSet::new();

    for member in members {
        let doc = read_manifest(member)?;
        collect_inherited(&doc, &mut used);
    }

    Ok(used)
}

/// Record every catalog key a single manifest inherits.
fn collect_inherited(doc: &DocumentMut, used: &mut BTreeSet<String>) {
    for name in DEP_TABLES {
        if let Some(table) = doc.get(name).and_then(Item::as_table_like) {
            collect_from_dep_table(table, used);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and its dev/build siblings.
    let Some(targets) = doc.get("target").and_then(Item::as_table_like) else {
        return;
    };

    for target in targets.iter().filter_map(|(_, target)| target.as_table_like()) {
        for name in DEP_TABLES {
            if let Some(table) = target.get(name).and_then(Item::as_table_like) {
                collect_from_dep_table(table, used);
            }
        }
    }
}

/// Record the inheriting declarations of one dependency table.
fn collect_from_dep_table(table: &dyn TableLike, used: &mut BTreeSet<String>) {
    for (name, spec) in table.iter() {
        if inherits_from_workspace(spec) {
            used.insert(name.to_owned());
        }
    }
}

/// True for `dep = { workspace = true, .. }` and the dotted `dep.workspace = true`
/// form. Both are table-like to `toml_edit`, so one lookup covers each.
fn inherits_from_workspace(spec: &Item) -> bool {
    spec.as_table_like()
        .and_then(|table| table.get("workspace"))
        .and_then(Item::as_value)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Split the catalog into the entries nobody inherits and the allow-list entries
/// that suppressed nothing.
///
/// Declaration order is preserved so the report reads alongside the manifest.
pub fn partition(catalog: &WorkspaceCatalog, used: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let uninherited: Vec<String> = catalog
        .declared
        .iter()
        .filter(|name| !used.contains(name.as_str()))
        .cloned()
        .collect();

    let unused = uninherited
        .iter()
        .filter(|name| !catalog.allowed.contains(name.as_str()))
        .cloned()
        .collect();
    let stale = catalog.allowed.iter().filter(|name| !uninherited.contains(name)).cloned().collect();

    (unused, stale)
}
