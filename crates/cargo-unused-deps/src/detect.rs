// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Detection: which `[workspace.dependencies]` entries no member inherits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Dependency tables a member manifest can inherit workspace dependencies from.
const DEP_TABLES: [(&str, Section); 3] = [
    ("dependencies", Section::Normal),
    ("dev-dependencies", Section::Development),
    ("build-dependencies", Section::Build),
];

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

    /// Dependency keys declared by at least one member.
    pub declarations: BTreeSet<String>,

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

/// One dependency a member declares.
#[derive(Debug, Clone)]
pub struct Declared {
    /// The declaration's key, as written.
    pub name: String,

    /// Where it was declared.
    pub section: Section,

    /// Target-table predicate, or none for an unconditional declaration.
    pub target: Option<String>,

    /// Whether a feature definition requires this dependency declaration.
    pub feature_referenced: bool,
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
    let feature_references = feature_references(doc);

    for (table, section) in DEP_TABLES {
        if let Some(item) = doc.get(table).and_then(Item::as_table_like) {
            collect_declared(item, section, None, &feature_references, &mut declared);
        }
    }

    if let Some(targets) = doc.get("target").and_then(Item::as_table_like) {
        for (target_name, target) in targets.iter().filter_map(|(name, target)| Some((name, target.as_table_like()?))) {
            for (table, section) in DEP_TABLES {
                if let Some(item) = target.get(table).and_then(Item::as_table_like) {
                    collect_declared(item, section, Some(target_name), &feature_references, &mut declared);
                }
            }
        }
    }

    declared
}

/// Record one dependency table's declarations.
fn collect_declared(
    table: &dyn TableLike,
    section: Section,
    target: Option<&str>,
    feature_references: &FeatureReferences,
    into: &mut Vec<Declared>,
) {
    for (key, spec) in table.iter() {
        into.push(Declared {
            name: key.to_owned(),
            section,
            target: target.map(str::to_owned),
            feature_referenced: feature_references.explicit.contains(key)
                || (is_optional(spec) && feature_references.implicit_optional.contains(key)),
        });
    }
}

/// Dependency references carried by the feature table.
#[derive(Default)]
struct FeatureReferences {
    /// Unambiguous `dep:name`, `name/feature`, and `name?/feature` references.
    explicit: BTreeSet<String>,
    /// Bare names that can denote an implicit optional-dependency feature.
    implicit_optional: BTreeSet<String>,
}

fn feature_references(doc: &DocumentMut) -> FeatureReferences {
    let Some(features) = doc.get("features").and_then(Item::as_table_like) else {
        return FeatureReferences::default();
    };
    let feature_names: BTreeSet<&str> = features.iter().map(|(name, _)| name).collect();
    let mut references = FeatureReferences::default();

    for value in features
        .iter()
        .filter_map(|(_, feature)| feature.as_array())
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(name) = feature_dependency(value) {
            references.explicit.insert(name.to_owned());
        } else if !value.contains('/') && !feature_names.contains(value) {
            references.implicit_optional.insert(value.to_owned());
        }
    }

    references
}

/// Extract a dependency key from `dep:name`, `name/feature`, or `name?/feature`.
fn feature_dependency(value: &str) -> Option<&str> {
    if let Some(name) = value.strip_prefix("dep:") {
        return Some(name);
    }
    value.split_once('/').map(|(name, _)| name.strip_suffix('?').unwrap_or(name))
}

/// Whether a dependency declaration sets `optional = true`.
fn is_optional(spec: &Item) -> bool {
    spec.as_table_like()
        .and_then(|table| table.get("optional"))
        .and_then(Item::as_value)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Read a manifest's text.
pub fn read_manifest_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).context(format!("failed to read {}", path.display()))
}

/// Parse manifest text that came from `path`.
pub fn parse_manifest(text: &str, path: &Path) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().context(format!("failed to parse {}", path.display()))
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

    let allowed = allowed_names(configured, &format!("workspace.metadata.{METADATA_KEY}"))?;

    Ok(Catalog::Workspace(WorkspaceCatalog { declared, allowed }))
}

/// Read package-local source-finding suppressions.
pub fn package_allowed(manifest: &DocumentMut) -> Result<BTreeSet<String>> {
    let configured = manifest
        .get("package")
        .and_then(Item::as_table_like)
        .and_then(|package| package.get("metadata"))
        .and_then(Item::as_table_like)
        .and_then(|metadata| metadata.get(METADATA_KEY))
        .and_then(Item::as_table_like)
        .and_then(|config| config.get("allowed"));

    allowed_names(configured, &format!("package.metadata.{METADATA_KEY}"))
}

/// Parse one metadata scope's allowed dependency names.
fn allowed_names(configured: Option<&Item>, scope: &str) -> Result<BTreeSet<String>> {
    let mut allowed = BTreeSet::new();
    for value in array_of(configured, scope, "allowed")? {
        let name = value
            .as_str()
            .ok_or_else(|| anyhow!("[{scope}] allowed must contain only strings, found {}", value.type_name()))?;

        allowed.insert(name.to_owned());
    }
    Ok(allowed)
}

/// The values of a configured array, or none when the key is absent.
///
/// A present value of any other type is an error rather than a silent empty
/// list: a mis-typed key that behaves like an absent one is indistinguishable
/// from configuration that does not work.
fn array_of<'a>(configured: Option<&'a Item>, scope: &str, key: &str) -> Result<impl Iterator<Item = &'a Value>> {
    let array = match configured {
        None => None,
        Some(item) => Some(
            item.as_array()
                .ok_or_else(|| anyhow!("[{scope}] {key} must be an array, found {}", item.type_name()))?,
        ),
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
    let mut declarations = BTreeSet::new();
    let mut inputs = Vec::with_capacity(members.len());

    for member in members {
        let contents = read_manifest_text(member)?;
        let doc = parse_manifest(&contents, member)?;
        collect_inherited(&doc, &mut keys);
        declarations.extend(declared_dependencies(&doc).into_iter().map(|dependency| dependency.name));
        inputs.push(ManifestInput {
            path: member.clone(),
            contents,
        });
    }

    Ok(Inheritance {
        keys,
        declarations,
        inputs,
    })
}

/// Record every catalog key a single manifest inherits.
fn collect_inherited(doc: &DocumentMut, inherited: &mut BTreeSet<String>) {
    for (name, _) in DEP_TABLES {
        if let Some(table) = doc.get(name).and_then(Item::as_table_like) {
            collect_from_dep_table(table, inherited);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and its dev/build siblings.
    let Some(targets) = doc.get("target").and_then(Item::as_table_like) else {
        return;
    };

    for target in targets.iter().filter_map(|(_, target)| target.as_table_like()) {
        for (name, _) in DEP_TABLES {
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
/// exempt it; an allow-list entry is stale when neither the catalog nor any
/// member declares its name.
///
/// Declaration order is preserved so the report reads alongside the manifest.
pub fn partition(catalog: &WorkspaceCatalog, inherited: &BTreeSet<String>, declarations: &BTreeSet<String>) -> (Vec<String>, Vec<String>) {
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
    let stale = catalog
        .allowed
        .iter()
        .filter(|name| !catalog.declared.contains(name) && !declarations.contains(name.as_str()))
        .cloned()
        .collect();

    (unused, stale)
}

#[cfg(test)]
mod tests {
    use super::{Section, declared_dependencies};

    #[test]
    fn declarations_include_target_specific_sections() {
        let manifest = r#"
[dependencies]
normal = "1"

[target.'cfg(windows)'.dev-dependencies]
development = "1"

[target.'cfg(unix)'.build-dependencies]
build = "1"
"#
        .parse()
        .expect("fixture manifest is valid");

        let declared = declared_dependencies(&manifest);
        assert_eq!(declared.len(), 3);
        assert_eq!(declared[0].section, Section::Normal);
        assert_eq!(declared[0].target, None);
        assert!(!declared[0].feature_referenced);
        assert_eq!(declared[1].section, Section::Development);
        assert_eq!(declared[1].target.as_deref(), Some("cfg(windows)"));
        assert_eq!(declared[2].section, Section::Build);
        assert_eq!(declared[2].target.as_deref(), Some("cfg(unix)"));
    }

    #[test]
    fn dependencies_required_by_features_are_identified() {
        let manifest = r#"
[dependencies]
required = "1"
bare_required = "1"
plain = { version = "1", optional = true }
explicit = { version = "1", optional = true }
forwarded = { version = "1", optional = true }
weak = { version = "1", optional = true }
bare = { version = "1", optional = true }
shadowed = { version = "1", optional = true }

[features]
api = ["required/std", "bare_required", "dep:explicit", "forwarded/derive", "weak?/std", "bare", "shadowed"]
shadowed = []
"#
        .parse()
        .expect("fixture manifest is valid");

        let declared = declared_dependencies(&manifest);
        assert!(declared[0].feature_referenced, "required dependency features need the declaration");
        assert!(
            !declared[1].feature_referenced,
            "a bare required dependency is not an implicit optional feature"
        );
        assert!(!declared[2].feature_referenced);
        assert!(declared[3].feature_referenced);
        assert!(declared[4].feature_referenced);
        assert!(declared[5].feature_referenced);
        assert!(
            declared[6].feature_referenced,
            "bare implicit optional feature enables the dependency"
        );
        assert!(
            !declared[7].feature_referenced,
            "a same-named explicit feature shadows the implicit optional feature"
        );
    }
}
