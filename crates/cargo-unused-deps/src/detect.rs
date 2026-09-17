// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Detection: which `[workspace.dependencies]` entries no member inherits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Dependency tables a member manifest can inherit workspace dependencies from.
const DEP_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Key under `[workspace.metadata]` holding this tool's configuration.
const METADATA_KEY: &str = "unused-deps";

/// What a manifest turned out to be.
///
/// Selects the branch the check takes: a non-workspace manifest has no catalog
/// to be wrong about and passes, while a workspace root carries the entries and
/// allow-list that detection, reporting, and `--fix` all work from.
pub enum Catalog {
    /// The manifest has no `[workspace]` table, so it declares no catalog.
    NotAWorkspace,

    /// The manifest is a workspace root. The catalog may still be empty.
    Workspace(WorkspaceCatalog),
}

/// A workspace root's dependency catalog.
///
/// Carries the `[workspace.dependencies]` entry names together with the
/// allow-list configured beside them.
pub struct WorkspaceCatalog {
    /// Catalog entry names, in the order the manifest declares them.
    pub declared: Vec<String>,

    /// Names configured as deliberate exceptions.
    pub allowed: BTreeSet<String>,
}

/// Inheritance evidence collected from the workspace member manifests.
pub struct Inheritance {
    /// Catalog keys inherited by at least one member.
    pub keys: BTreeSet<String>,

    /// Manifest inputs whose contents support that conclusion.
    pub inputs: Vec<ManifestInput>,
}

/// One manifest and the contents read during detection.
pub struct ManifestInput {
    /// Path Cargo reported for the member manifest.
    pub path: PathBuf,

    /// Exact contents used to detect inherited catalog keys.
    pub contents: String,
}

/// Which manifest table declared a dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// `[dependencies]`, including its `[target.'cfg(…)']` forms.
    Normal,

    /// `[dev-dependencies]`, including its `[target.'cfg(…)']` forms.
    Development,

    /// `[build-dependencies]`, including its `[target.'cfg(…)']` forms.
    Build,
}

impl Section {
    /// The manifest table name.
    fn table(self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Development => "dev-dependencies",
            Self::Build => "build-dependencies",
        }
    }

    /// The section a table name denotes, if it denotes one.
    fn of_table(table: &str) -> Option<Self> {
        [Self::Normal, Self::Development, Self::Build]
            .into_iter()
            .find(|section| section.table() == table)
    }
}

/// One dependency a member declares.
#[derive(Debug, Clone)]
pub struct Declared {
    /// The declaration's key, as written.
    pub name: String,

    /// Where it was declared.
    pub section: Section,
}

impl Declared {
    /// The name rustc uses for the crate, which is the key with hyphens
    /// replaced. Diagnostics are matched on this form.
    pub fn extern_name(&self) -> String {
        self.name.replace('-', "_")
    }
}

/// Every dependency a member manifest declares, in every section and target table.
pub fn declared_dependencies(doc: &DocumentMut) -> Vec<Declared> {
    let mut declared = Vec::new();

    for table in DEP_TABLES {
        if let Some(item) = doc.get(table).and_then(Item::as_table_like) {
            collect_declared(item, table, &mut declared);
        }
    }

    if let Some(targets) = doc.get("target").and_then(Item::as_table_like) {
        for target in targets.iter().filter_map(|(_, target)| target.as_table_like()) {
            for table in DEP_TABLES {
                if let Some(item) = target.get(table).and_then(Item::as_table_like) {
                    collect_declared(item, table, &mut declared);
                }
            }
        }
    }

    declared
}

/// Record one dependency table's declarations.
fn collect_declared(table: &dyn TableLike, name: &str, into: &mut Vec<Declared>) {
    let Some(section) = Section::of_table(name) else {
        return;
    };

    for (key, _) in table.iter() {
        into.push(Declared {
            name: key.to_owned(),
            section,
        });
    }
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
    let text = read_manifest_text(path)?;
    parse_manifest(&text, path)
}

/// Classify a parsed manifest.
///
/// When the manifest is a workspace root, its catalog and allow-list are
/// collected as part of the classification.
///
/// # Errors
///
/// Returns an error when `workspace.dependencies` is present but not table-like,
/// or when `allowed` is not an array of strings. Treating either malformed
/// value as absent would make invalid configuration look clean.
pub fn catalog(manifest: &DocumentMut) -> Result<Catalog> {
    let Some(workspace) = manifest.get("workspace").and_then(Item::as_table_like) else {
        return Ok(Catalog::NotAWorkspace);
    };

    let declared = match workspace.get("dependencies") {
        None => Vec::new(),
        Some(item) => item
            .as_table_like()
            .ok_or_else(|| anyhow!("[workspace.dependencies] must be a table, found {}", item.type_name()))?
            .iter()
            .map(|(key, _)| key.to_owned())
            .collect(),
    };

    let configured = workspace
        .get("metadata")
        .and_then(Item::as_table_like)
        .and_then(|metadata| metadata.get(METADATA_KEY))
        .and_then(Item::as_table_like)
        .and_then(|config| config.get("allowed"));

    let mut allowed = BTreeSet::new();
    for value in array_of(configured, "allowed")? {
        let name = value.as_str().ok_or_else(|| {
            anyhow!(
                "[workspace.metadata.{METADATA_KEY}] allowed must contain only strings, found {}",
                value.type_name()
            )
        })?;

        allowed.insert(name.to_owned());
    }

    Ok(Catalog::Workspace(WorkspaceCatalog { declared, allowed }))
}

/// The values of a configured array, or none when the key is absent.
///
/// A present value of any other type is an error rather than a silent empty
/// list: a mis-typed key that behaves like an absent one is indistinguishable
/// from configuration that does not work.
fn array_of<'a>(configured: Option<&'a Item>, key: &str) -> Result<impl Iterator<Item = &'a Value>> {
    let array = match configured {
        None => None,
        Some(item) => Some(item.as_array().ok_or_else(|| {
            anyhow!(
                "[workspace.metadata.{METADATA_KEY}] {key} must be an array, found {}",
                item.type_name()
            )
        })?),
    };

    Ok(array.into_iter().flatten())
}

/// Collect the catalog keys that member manifests inherit.
///
/// `members` are manifest paths as reported by `cargo metadata`; a manifest that
/// cannot be read or parsed fails the run rather than being silently treated as
/// inheriting nothing, which would turn a read error into false accusations.
pub fn inherited(members: &[PathBuf]) -> Result<Inheritance> {
    let mut keys = BTreeSet::new();
    let mut inputs = Vec::with_capacity(members.len());

    for member in members {
        let contents = read_manifest_text(member)?;
        let doc = parse_manifest(&contents, member)?;
        collect_inherited(&doc, &mut keys);
        inputs.push(ManifestInput {
            path: member.clone(),
            contents,
        });
    }

    Ok(Inheritance { keys, inputs })
}

/// Record every catalog key a single manifest inherits.
fn collect_inherited(doc: &DocumentMut, inherited: &mut BTreeSet<String>) {
    for name in DEP_TABLES {
        if let Some(table) = doc.get(name).and_then(Item::as_table_like) {
            collect_from_dep_table(table, inherited);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and its dev/build siblings.
    let Some(targets) = doc.get("target").and_then(Item::as_table_like) else {
        return;
    };

    for target in targets.iter().filter_map(|(_, target)| target.as_table_like()) {
        for name in DEP_TABLES {
            if let Some(table) = target.get(name).and_then(Item::as_table_like) {
                collect_from_dep_table(table, inherited);
            }
        }
    }
}

/// Record the inheriting declarations of one dependency table.
///
/// The declaration key is recorded as written, never the package it resolves
/// to. Cargo matches inheritance by catalog key: a member writing
/// `rustdoc-types-v57 = { workspace = true }` can only be served by the catalog
/// key `rustdoc-types-v57`, whatever `package = "..."` rename that entry
/// carries. Normalizing to the resolved package name here would make every
/// renamed catalog entry look uninherited.
fn collect_from_dep_table(table: &dyn TableLike, inherited: &mut BTreeSet<String>) {
    for (name, spec) in table.iter() {
        if inherits_from_workspace(spec) {
            inherited.insert(name.to_owned());
        }
    }
}

/// Whether a dependency declaration inherits from the workspace.
///
/// True for `dep = { workspace = true, .. }` and for the dotted
/// `dep.workspace = true` form. Both are table-like to `toml_edit`, so one
/// lookup covers each.
fn inherits_from_workspace(spec: &Item) -> bool {
    spec.as_table_like()
        .and_then(|table| table.get("workspace"))
        .and_then(Item::as_value)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Split the catalog into unused entries and stale allow-list entries.
///
/// An entry is unused when no member inherits it and the allow-list does not
/// exempt it; an allow-list entry is stale when it suppresses nothing.
///
/// Declaration order is preserved so the report reads alongside the manifest.
pub fn partition(catalog: &WorkspaceCatalog, inherited: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
    let uninherited: Vec<String> = catalog
        .declared
        .iter()
        .filter(|name| !inherited.contains(name.as_str()))
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
