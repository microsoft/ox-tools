// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cargo_metadata::{CargoOpt, Dependency, DependencyKind, Node, Package, PackageId};
use clap::{Parser, ValueEnum};
use ohno::{IntoAppError, bail};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

use super::Host;
use super::common::{Common, CommonArgs};
use crate::facts::CrateRef;
use crate::{HashMap, HashSet, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Deserialize, Serialize, Display, EnumString)]
#[value(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum DependencyType {
    /// Regular production dependencies
    Standard,

    /// Development-only dependencies
    Dev,

    /// Build-only dependencies
    Build,
}

#[derive(Parser, Debug)]
pub struct DepsArgs {
    /// Comma-separated list of dependency types to appraise
    #[arg(
        long = "dependency-types",
        value_delimiter = ',',
        value_name = "TYPES",
        default_value = "standard,dev,build"
    )]
    pub dependency_types: Option<Vec<DependencyType>>,

    /// Space or comma separated list of features to activate
    #[arg(short = 'F', long, value_name = "FEATURES", help_heading = "Feature Selection")]
    pub features: Vec<String>,

    /// Activate all available features
    #[arg(long, help_heading = "Feature Selection")]
    pub all_features: bool,

    /// Do not activate the `default` feature
    #[arg(long, help_heading = "Feature Selection")]
    pub no_default_features: bool,

    /// Process only the specified package
    #[arg(short = 'p', long, value_name = "SPEC", help_heading = "Package Selection")]
    pub package: Vec<String>,

    /// Process all packages in the workspace
    #[arg(long, help_heading = "Package Selection")]
    pub workspace: bool,

    #[command(flatten)]
    pub common: CommonArgs,
}

pub async fn process_dependencies<H: Host>(host: &mut H, args: &DepsArgs) -> Result<()> {
    let mut common = Common::new(host, &args.common).await?;

    // Configure features on the metadata command based on command-line options
    if args.all_features {
        _ = common.metadata_cmd.features(CargoOpt::AllFeatures);
    } else {
        if args.no_default_features {
            _ = common.metadata_cmd.features(CargoOpt::NoDefaultFeatures);
        }

        if !args.features.is_empty() {
            _ = common.metadata_cmd.features(CargoOpt::SomeFeatures(args.features.clone()));
        }
    }

    let metadata = common.metadata_cmd.exec().into_app_err("retrieving workspace metadata")?;
    let all_packages: HashMap<_, _> = metadata.packages.iter().map(|p| (&p.id, p)).collect();
    let resolve_index: HashMap<&PackageId, &Node> = metadata
        .resolve
        .as_ref()
        .map_or_else(HashMap::default, |r| r.nodes.iter().map(|n| (&n.id, n)).collect());

    // Validate package names if specified
    if !args.package.is_empty() {
        for pkg_name in &args.package {
            let found = metadata
                .workspace_members
                .iter()
                .filter_map(|id| all_packages.get(id).map(|p| &p.name))
                .any(|name| name == pkg_name);
            if !found {
                bail!("package '{pkg_name}' not found in workspace");
            }
        }
    }

    if !args.package.is_empty() {
        process_packages(
            args,
            &mut common,
            &all_packages,
            &resolve_index,
            metadata
                .workspace_members
                .iter()
                .filter_map(|id| all_packages.get(id).copied())
                .filter(|p| args.package.contains(&p.name)),
        )
        .await
    } else if args.workspace {
        process_packages(
            args,
            &mut common,
            &all_packages,
            &resolve_index,
            metadata.workspace_members.iter().filter_map(|id| all_packages.get(id).copied()),
        )
        .await
    } else if let Some(root) = metadata.root_package() {
        process_packages(args, &mut common, &all_packages, &resolve_index, core::iter::once(root)).await
    } else {
        // Virtual workspace, default to all members
        process_packages(
            args,
            &mut common,
            &all_packages,
            &resolve_index,
            metadata.workspace_members.iter().filter_map(|id| all_packages.get(id).copied()),
        )
        .await
    }
}

async fn process_packages<'a, H: Host>(
    args: &DepsArgs,
    common: &mut Common<'_, H>,
    all_packages: &HashMap<&'a PackageId, &'a Package>,
    resolve_index: &HashMap<&'a PackageId, &'a Node>,
    target_packages: impl Iterator<Item = &'a Package>,
) -> Result<()> {
    let should_process = |dep_type: &DependencyType| args.dependency_types.as_ref().is_none_or(|d| d.is_empty() || d.contains(dep_type));

    // Collect all (CrateId, dependency_type) pairs, preserving duplicates
    let mut crate_dep_pairs: Vec<(CrateRef, DependencyType)> = Vec::new();

    let active_dep_types: Vec<_> = [DependencyType::Standard, DependencyType::Dev, DependencyType::Build]
        .into_iter()
        .filter(|dt| should_process(dt))
        .collect();

    for package in target_packages {
        for &dep_type in &active_dep_types {
            crate_dep_pairs.extend(build_transitive_deps(all_packages, resolve_index, &package.id, dep_type));
        }
    }

    // Fetch facts for each crate (no suggestions for deps command)
    let crate_refs: Vec<CrateRef> = crate_dep_pairs.into_iter().map(|(crate_ref, _)| crate_ref).collect();
    let facts = common.process_crates(&crate_refs, false).await?;

    // Report the facts
    common.report(facts)
}

/// Expand a set of features transitively using the package's feature declarations.
///
/// For each enabled feature, follows feature-to-feature activations (entries without
/// `:` or `/` separators) to compute the full set of active features.
fn expand_features(pkg: &Package, initial_features: &HashSet<String>) -> HashSet<String> {
    let mut expanded = initial_features.clone();
    let mut queue: Vec<String> = initial_features.iter().cloned().collect();

    while let Some(feature) = queue.pop() {
        if let Some(activations) = pkg.features.get(&feature) {
            for activation in activations {
                if activation.contains(':') || activation.contains('/') {
                    continue;
                }
                if expanded.insert(activation.clone()) {
                    queue.push(activation.clone());
                }
            }
        }
    }

    expanded
}

/// Find the dependency declaration in a package that matches the given library name and kind.
///
/// Falls back to matching any kind if no exact kind match is found.
fn find_dep_declaration<'a>(pkg: &'a Package, dep_lib_name: &str, kind: DependencyKind) -> Option<&'a Dependency> {
    pkg.dependencies
        .iter()
        .find(|d| d.rename.as_deref().unwrap_or(&d.name) == dep_lib_name && d.kind == kind)
        .or_else(|| {
            pkg.dependencies
                .iter()
                .find(|d| d.rename.as_deref().unwrap_or(&d.name) == dep_lib_name)
        })
}

/// Check whether an optional dependency is activated by the given set of expanded features.
fn is_optional_dep_active(expanded_features: &HashSet<String>, pkg: &Package, dep_lib_name: &str) -> bool {
    // Implicit feature: the dep name itself appears as an enabled feature
    if expanded_features.contains(dep_lib_name) {
        return true;
    }

    let slash_prefix = format!("{dep_lib_name}/");

    for feature_name in expanded_features {
        if let Some(activations) = pkg.features.get(feature_name) {
            for activation in activations {
                // dep:name syntax (edition 2021+)
                if activation.strip_prefix("dep:") == Some(dep_lib_name) {
                    return true;
                }
                // dep_name/feature syntax activates the optional dep (but dep_name?/feature does not)
                if activation.starts_with(slash_prefix.as_str()) {
                    return true;
                }
                // Pre-2021: listing an optional dep name directly in a feature activates it
                if activation == dep_lib_name && !activation.contains(':') && !activation.contains('/') {
                    return true;
                }
            }
        }
    }

    false
}

/// Compute the features to activate on a dependency based on the parent's declaration and features.
fn compute_dep_features(parent_pkg: &Package, dep_decl: &Dependency, parent_expanded_features: &HashSet<String>) -> HashSet<String> {
    let mut features = HashSet::default();

    if dep_decl.uses_default_features {
        _ = features.insert("default".to_string());
    }

    for f in &dep_decl.features {
        _ = features.insert(f.clone());
    }

    // Propagate features from parent feature declarations (dep/feature and dep?/feature syntax)
    let dep_lib_name = dep_decl.rename.as_deref().unwrap_or(&dep_decl.name);
    let prefix = format!("{dep_lib_name}/");
    let weak_prefix = format!("{dep_lib_name}?/");

    for feature_name in parent_expanded_features {
        if let Some(activations) = parent_pkg.features.get(feature_name) {
            for activation in activations {
                if let Some(dep_feature) = activation.strip_prefix(prefix.as_str()) {
                    _ = features.insert(dep_feature.to_string());
                } else if let Some(dep_feature) = activation.strip_prefix(weak_prefix.as_str()) {
                    _ = features.insert(dep_feature.to_string());
                }
            }
        }
    }

    features
}

/// Build the transitive closure of dependencies starting from a target package.
///
/// Uses the resolved dependency graph from `cargo metadata` to walk exact `PackageId`s,
/// avoiding ambiguity when multiple versions of the same crate exist (e.g., syn 1.x and 2.x).
///
/// Feature-aware: only follows optional dependencies whose activating features are enabled,
/// and propagates the correct feature set to each dependency based on the parent's declaration
/// rather than the unified `Node.features` from cargo metadata. This avoids false positives
/// caused by workspace feature unification.
///
/// Dev/build dependencies only apply at the first hop; their transitive deps are Normal.
fn build_transitive_deps<'a>(
    all_packages: &HashMap<&'a PackageId, &'a Package>,
    resolve_index: &HashMap<&'a PackageId, &'a Node>,
    target_package_id: &PackageId,
    dependency_type: DependencyType,
) -> HashSet<(CrateRef, DependencyType)> {
    let initial_kind = match dependency_type {
        DependencyType::Standard => DependencyKind::Normal,
        DependencyType::Dev => DependencyKind::Development,
        DependencyType::Build => DependencyKind::Build,
    };

    let mut result = HashSet::default();
    let mut visited_features: HashMap<&PackageId, HashSet<String>> = HashMap::default();
    let mut queue: Vec<(&PackageId, HashSet<String>)> = Vec::new();

    // Seed the queue with the target package's direct deps of the requested kind
    if let Some(target_pkg) = all_packages.get(target_package_id)
        && let Some(node) = resolve_index.get(target_package_id)
    {
        let root_features: HashSet<String> = node.features.iter().map(ToString::to_string).collect();
        let expanded_root = expand_features(target_pkg, &root_features);

        for node_dep in &node.deps {
            if node_dep.dep_kinds.iter().any(|dk| dk.kind == initial_kind) {
                if let Some(dep_decl) = find_dep_declaration(target_pkg, &node_dep.name, initial_kind) {
                    if dep_decl.optional && !is_optional_dep_active(&expanded_root, target_pkg, &node_dep.name) {
                        continue;
                    }
                    let features = compute_dep_features(target_pkg, dep_decl, &expanded_root);
                    queue.push((&node_dep.pkg, features));
                } else {
                    queue.push((&node_dep.pkg, HashSet::default()));
                }
            }
        }
    }

    while let Some((pkg_id, activated_features)) = queue.pop() {
        // Only re-process if we have new features to consider
        let was_seen = visited_features.contains_key(pkg_id);
        let entry = visited_features.entry(pkg_id).or_default();
        let prev_len = entry.len();
        entry.extend(activated_features);
        if was_seen && entry.len() == prev_len {
            continue;
        }
        let all_features = entry.clone();

        if let Some(pkg) = all_packages.get(pkg_id) {
            _ = result.insert((CrateRef::new(&pkg.name, Some(pkg.version.clone())), dependency_type));

            let expanded = expand_features(pkg, &all_features);

            // Follow Normal edges for transitive deps, filtered by feature activation
            if let Some(node) = resolve_index.get(pkg_id) {
                for node_dep in &node.deps {
                    if node_dep.dep_kinds.iter().any(|dk| dk.kind == DependencyKind::Normal) {
                        if let Some(dep_decl) = find_dep_declaration(pkg, &node_dep.name, DependencyKind::Normal) {
                            if dep_decl.optional && !is_optional_dep_active(&expanded, pkg, &node_dep.name) {
                                continue;
                            }
                            let dep_features = compute_dep_features(pkg, dep_decl, &expanded);
                            queue.push((&node_dep.pkg, dep_features));
                        } else {
                            queue.push((&node_dep.pkg, HashSet::default()));
                        }
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn make_package(json: &str) -> Package {
        serde_json::from_str(json).expect("valid Package JSON")
    }

    fn make_dep(json: &str) -> Dependency {
        serde_json::from_str(json).expect("valid Dependency JSON")
    }

    const MINIMAL_PKG: &str = r#"{
        "name": "test-pkg",
        "version": "0.1.0",
        "id": "test-pkg 0.1.0 (path+file:///test)",
        "source": null,
        "dependencies": [],
        "targets": [],
        "features": {},
        "manifest_path": "/test/Cargo.toml",
        "categories": [],
        "keywords": [],
        "edition": "2021",
        "metadata": null
    }"#;

    #[test]
    fn expand_features_empty() {
        let pkg = make_package(MINIMAL_PKG);
        let result = expand_features(&pkg, &HashSet::default());
        assert!(result.is_empty());
    }

    #[test]
    fn expand_features_transitive() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "default": ["a"],
                "a": ["b"],
                "b": []
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        let initial: HashSet<String> = core::iter::once("default".to_string()).collect();
        let expanded = expand_features(&pkg, &initial);
        assert!(expanded.contains("default"));
        assert!(expanded.contains("a"));
        assert!(expanded.contains("b"));
    }

    #[test]
    fn expand_features_skips_dep_syntax() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["dep:serde", "itoa/serde", "b"],
                "b": []
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        let initial: HashSet<String> = core::iter::once("extra".to_string()).collect();
        let expanded = expand_features(&pkg, &initial);
        assert!(expanded.contains("extra"));
        assert!(expanded.contains("b"));
        // dep:serde and itoa/serde are dependency syntax, not features
        assert!(!expanded.contains("dep:serde"));
        assert!(!expanded.contains("itoa/serde"));
        assert_eq!(expanded.len(), 2);
    }

    #[test]
    fn find_dep_declaration_by_name_and_kind() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [
                {"name": "serde", "req": "^1", "kind": null, "optional": true, "uses_default_features": true, "features": [], "target": null, "rename": null},
                {"name": "serde", "req": "^1", "kind": "dev", "optional": false, "uses_default_features": true, "features": ["derive"], "target": null, "rename": null}
            ],
            "targets": [], "features": {},
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        let normal = find_dep_declaration(&pkg, "serde", DependencyKind::Normal);
        assert!(normal.is_some());
        assert!(normal.unwrap().optional);

        let dev = find_dep_declaration(&pkg, "serde", DependencyKind::Development);
        assert!(dev.is_some());
        assert!(!dev.unwrap().optional);
    }

    #[test]
    fn find_dep_declaration_with_rename() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [
                {"name": "serde", "req": "^1", "kind": null, "optional": false, "uses_default_features": true, "features": [], "target": null, "rename": "my_serde"}
            ],
            "targets": [], "features": {},
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        // Match by renamed name
        let found = find_dep_declaration(&pkg, "my_serde", DependencyKind::Normal);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "serde");

        // Original name should NOT match
        let not_found = find_dep_declaration(&pkg, "serde", DependencyKind::Normal);
        assert!(not_found.is_none());
    }

    #[test]
    fn is_optional_dep_active_dep_colon_syntax() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["dep:once_cell"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        let features: HashSet<String> = core::iter::once("extra".to_string()).collect();
        assert!(is_optional_dep_active(&features, &pkg, "once_cell"));
        assert!(!is_optional_dep_active(&features, &pkg, "serde"));
    }

    #[test]
    fn is_optional_dep_active_implicit_feature() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {},
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        // Pre-2021 style: feature name matches dep name
        let features: HashSet<String> = core::iter::once("serde".to_string()).collect();
        assert!(is_optional_dep_active(&features, &pkg, "serde"));
        assert!(!is_optional_dep_active(&features, &pkg, "other"));
    }

    #[test]
    fn is_optional_dep_active_not_active() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["dep:once_cell"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        // Without the "extra" feature, once_cell should NOT be active
        let empty: HashSet<String> = HashSet::default();
        assert!(!is_optional_dep_active(&empty, &pkg, "once_cell"));
    }

    #[test]
    fn is_optional_dep_active_slash_syntax() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["itoa/serde"],
                "weak": ["itoa?/serde"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        // dep/feature syntax activates the optional dep
        let features: HashSet<String> = core::iter::once("extra".to_string()).collect();
        assert!(is_optional_dep_active(&features, &pkg, "itoa"));

        // dep?/feature (weak) syntax does NOT activate the optional dep
        let weak_features: HashSet<String> = core::iter::once("weak".to_string()).collect();
        assert!(!is_optional_dep_active(&weak_features, &pkg, "itoa"));
    }

    #[test]
    fn compute_dep_features_basic() {
        let pkg = make_package(MINIMAL_PKG);
        let dep = make_dep(
            r#"{
            "name": "serde", "req": "^1", "kind": null,
            "optional": false, "uses_default_features": true,
            "features": ["derive"], "target": null, "rename": null
        }"#,
        );

        let parent_features = HashSet::default();
        let features = compute_dep_features(&pkg, &dep, &parent_features);
        assert!(features.contains("default"));
        assert!(features.contains("derive"));
        assert_eq!(features.len(), 2);
    }

    #[test]
    fn compute_dep_features_no_default() {
        let pkg = make_package(MINIMAL_PKG);
        let dep = make_dep(
            r#"{
            "name": "serde", "req": "^1", "kind": null,
            "optional": false, "uses_default_features": false,
            "features": ["derive"], "target": null, "rename": null
        }"#,
        );

        let parent_features = HashSet::default();
        let features = compute_dep_features(&pkg, &dep, &parent_features);
        assert!(!features.contains("default"));
        assert!(features.contains("derive"));
        assert_eq!(features.len(), 1);
    }

    #[test]
    fn compute_dep_features_propagates_from_parent() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "serde": ["dep:serde", "itoa/serde"],
                "extra": ["itoa?/extra_feature"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );
        let dep = make_dep(
            r#"{
            "name": "itoa", "req": "^1", "kind": null,
            "optional": false, "uses_default_features": false,
            "features": [], "target": null, "rename": null
        }"#,
        );

        let parent_features: HashSet<String> = ["serde".to_string(), "extra".to_string()].into_iter().collect();
        let features = compute_dep_features(&pkg, &dep, &parent_features);
        // itoa/serde from "serde" feature, itoa?/extra_feature from "extra" feature
        assert!(features.contains("serde"));
        assert!(features.contains("extra_feature"));
    }

    #[test]
    fn is_optional_dep_active_bare_dep_name_in_feature() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["serde"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        // Pre-2021 style: a feature that lists the optional dependency's bare name activates it.
        let features: HashSet<String> = core::iter::once("extra".to_string()).collect();
        assert!(is_optional_dep_active(&features, &pkg, "serde"));
    }

    #[test]
    fn is_optional_dep_active_ignores_unrelated_bare_feature_activations() {
        let pkg = make_package(
            r#"{
            "name": "test-pkg", "version": "0.1.0",
            "id": "test-pkg 0.1.0 (path+file:///test)", "source": null,
            "dependencies": [], "targets": [],
            "features": {
                "extra": ["other"]
            },
            "manifest_path": "/test/Cargo.toml", "categories": [], "keywords": [],
            "edition": "2021", "metadata": null
        }"#,
        );

        let features: HashSet<String> = core::iter::once("extra".to_string()).collect();
        assert!(!is_optional_dep_active(&features, &pkg, "serde"));
    }

    fn make_node(json: &str) -> Node {
        serde_json::from_str(json).expect("valid Node JSON")
    }

    /// Builds a package with the given name, dependency declarations and features.
    fn package(name: &str, dependencies: &str, features: &str) -> Package {
        make_package(&format!(
            r#"{{
                "name": "{name}", "version": "1.0.0",
                "id": "{name} 1.0.0 (path+file:///{name})", "source": null,
                "dependencies": [{dependencies}], "targets": [],
                "features": {features},
                "manifest_path": "/{name}/Cargo.toml", "categories": [], "keywords": [],
                "edition": "2021", "metadata": null
            }}"#
        ))
    }

    /// A normal, non-optional dependency declaration on `name`, with no features.
    fn plain_dep(name: &str) -> String {
        format!(
            r#"{{"name": "{name}", "req": "^1", "kind": null, "optional": false,
                 "uses_default_features": false, "features": [], "target": null, "rename": null}}"#
        )
    }

    /// A resolve node for `name` whose dependency edges of the given kind point at `deps`.
    fn node_of_kind(name: &str, deps: &[&str], kind: &str) -> Node {
        let edges = deps
            .iter()
            .map(|dep| {
                format!(
                    r#"{{"name": "{dep}", "pkg": "{dep} 1.0.0 (path+file:///{dep})",
                         "dep_kinds": [{{"kind": {kind}, "target": null}}]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        make_node(&format!(
            r#"{{
                "id": "{name} 1.0.0 (path+file:///{name})",
                "dependencies": [],
                "deps": [{edges}],
                "features": []
            }}"#
        ))
    }

    /// A resolve node for `name` whose normal dependency edges point at `deps`.
    fn node(name: &str, deps: &[&str]) -> Node {
        node_of_kind(name, deps, "null")
    }

    #[test]
    fn build_transitive_deps_walks_every_edge_case() {
        // The graph deliberately contains one instance of every awkward edge:
        //   root -> optoff   inactive optional dependency, skipped
        //   root -> kept     ordinary dependency, reached twice with the same features
        //   root -> other    ordinary dependency that also depends on `kept`
        //   root -> lonely   a package with no entry in the resolve graph
        //   root -> ghost    a resolve edge with no matching declaration, and no package either
        //   kept -> toptoff  inactive optional dependency of a transitive package
        //   kept -> tghost   transitive resolve edge with no matching declaration
        let root = package(
            "root",
            &format!(
                r#"{{"name": "optoff", "req": "^1", "kind": null, "optional": true,
                     "uses_default_features": false, "features": [], "target": null, "rename": null}},{},{},{}"#,
                plain_dep("kept"),
                plain_dep("other"),
                plain_dep("lonely")
            ),
            "{}",
        );
        let kept = package(
            "kept",
            r#"{"name": "toptoff", "req": "^1", "kind": null, "optional": true,
                "uses_default_features": false, "features": [], "target": null, "rename": null}"#,
            "{}",
        );
        let other = package("other", &plain_dep("kept"), "{}");
        let lonely = package("lonely", "", "{}");
        let tghost = package("tghost", "", "{}");
        let optoff = package("optoff", "", "{}");
        let toptoff = package("toptoff", "", "{}");

        let packages = [&root, &kept, &other, &lonely, &tghost, &optoff, &toptoff];
        let all_packages: HashMap<&PackageId, &Package> = packages.iter().map(|p| (&p.id, *p)).collect();

        let root_node = node("root", &["optoff", "kept", "other", "lonely", "ghost"]);
        let mut kept_node = node("kept", &["toptoff", "tghost"]);
        // A dev-only edge: transitive walking follows normal edges only, so this is ignored.
        kept_node.deps.append(&mut node_of_kind("kept", &["devonly"], r#""dev""#).deps);
        let other_node = node("other", &["kept"]);
        let nodes = [&root_node, &kept_node, &other_node];
        let resolve_index: HashMap<&PackageId, &Node> = nodes.iter().map(|n| (&n.id, *n)).collect();

        let result = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Standard);

        let mut names: Vec<_> = result.iter().map(|(r, _)| r.name().to_string()).collect();
        names.sort();

        // `optoff` and `toptoff` are inactive optional dependencies, `ghost` has no package.
        assert_eq!(names, vec!["kept", "lonely", "other", "tghost"]);
        assert!(result.iter().all(|(_, kind)| *kind == DependencyType::Standard));
    }

    #[test]
    fn build_transitive_deps_returns_nothing_for_an_unknown_target() {
        let root = package("root", "", "{}");
        let all_packages: HashMap<&PackageId, &Package> = HashMap::default();
        let resolve_index: HashMap<&PackageId, &Node> = HashMap::default();

        let result = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Standard);

        assert!(result.is_empty(), "a target that is not in the metadata has no dependencies");
    }

    #[test]
    fn build_transitive_deps_respects_the_requested_first_hop_kind() {
        let normal_dep = plain_dep("normal");
        let build_dep = r#"{"name": "buildonly", "req": "^1", "kind": "build", "optional": false,
                           "uses_default_features": false, "features": [], "target": null, "rename": null}"#;
        let dev_dep = r#"{"name": "devonly", "req": "^1", "kind": "dev", "optional": false,
                         "uses_default_features": false, "features": [], "target": null, "rename": null}"#;
        let root = package("root", &format!("{normal_dep},{build_dep},{dev_dep}"), "{}");
        let normal = package("normal", "", "{}");
        let buildonly = package("buildonly", "", "{}");
        let devonly = package("devonly", "", "{}");

        let packages = [&root, &normal, &buildonly, &devonly];
        let all_packages: HashMap<&PackageId, &Package> = packages.iter().map(|p| (&p.id, *p)).collect();

        let mut root_node = node("root", &["normal"]);
        root_node.deps.append(&mut node_of_kind("root", &["buildonly"], r#""build""#).deps);
        root_node.deps.append(&mut node_of_kind("root", &["devonly"], r#""dev""#).deps);
        let nodes = [&root_node];
        let resolve_index: HashMap<&PackageId, &Node> = nodes.iter().map(|n| (&n.id, *n)).collect();

        let standard = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Standard);
        let build = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Build);
        let dev = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Dev);

        assert_eq!(standard.iter().map(|(r, _)| r.name()).collect::<Vec<_>>(), vec!["normal"]);
        assert_eq!(build.iter().map(|(r, _)| r.name()).collect::<Vec<_>>(), vec!["buildonly"]);
        assert_eq!(dev.iter().map(|(r, _)| r.name()).collect::<Vec<_>>(), vec!["devonly"]);
        assert!(standard.iter().all(|(_, kind)| *kind == DependencyType::Standard));
        assert!(build.iter().all(|(_, kind)| *kind == DependencyType::Build));
        assert!(dev.iter().all(|(_, kind)| *kind == DependencyType::Dev));
    }

    #[test]
    fn build_transitive_deps_revisits_seen_packages_when_new_features_arrive() {
        let root = package("root", &format!("{},{}", plain_dep("feature_path"), plain_dep("plain_path")), "{}");
        let plain_path = package("plain_path", &plain_dep("shared"), "{}");
        let feature_path = package(
            "feature_path",
            r#"{"name": "shared", "req": "^1", "kind": null, "optional": false,
               "uses_default_features": false, "features": ["extra"], "target": null, "rename": null}"#,
            "{}",
        );
        let shared = package(
            "shared",
            r#"{"name": "activated", "req": "^1", "kind": null, "optional": true,
               "uses_default_features": false, "features": [], "target": null, "rename": null}"#,
            r#"{"extra": ["dep:activated"]}"#,
        );
        let activated = package("activated", "", "{}");

        let packages = [&root, &plain_path, &feature_path, &shared, &activated];
        let all_packages: HashMap<&PackageId, &Package> = packages.iter().map(|p| (&p.id, *p)).collect();

        let root_node = node("root", &["feature_path", "plain_path"]);
        let plain_path_node = node("plain_path", &["shared"]);
        let feature_path_node = node("feature_path", &["shared"]);
        let shared_node = node("shared", &["activated"]);
        let nodes = [&root_node, &plain_path_node, &feature_path_node, &shared_node];
        let resolve_index: HashMap<&PackageId, &Node> = nodes.iter().map(|n| (&n.id, *n)).collect();

        let result = build_transitive_deps(&all_packages, &resolve_index, &root.id, DependencyType::Standard);
        let mut names: Vec<_> = result.iter().map(|(r, _)| r.name().to_string()).collect();
        names.sort();

        assert_eq!(names, vec!["activated", "feature_path", "plain_path", "shared"]);
    }
}
