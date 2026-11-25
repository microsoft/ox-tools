// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cargo_metadata::{DependencyKind, Metadata, Node, Package, PackageId};

use crate::ops::collect::ResourceDependency;
use crate::{HashMap, HashSet};

pub(super) type Dependency = ResourceDependency;

pub(super) fn extend_roots(roots: &mut HashSet<String>, dependencies: &[Dependency]) {
    let mut linked: HashMap<&str, Vec<&str>> = HashMap::default();
    for dependency in dependencies {
        if let Some(library) = &dependency.library_root {
            linked.entry(&dependency.crate_root).or_default().push(library);
        }
    }
    let mut pending: Vec<_> = roots.iter().cloned().collect();
    while let Some(root) = pending.pop() {
        for &library in linked.get(root.as_str()).into_iter().flatten() {
            // #[gamma::skip(cond.always_true, cond.negate, reason = "only newly inserted roots may be queued or dependency cycles repeat forever")]
            if roots.insert(library.to_owned()) {
                pending.push(library.to_owned());
            }
        }
    }
}

pub(super) fn of(metadata: &Metadata) -> Vec<Dependency> {
    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "Absent resolve metadata also leaves the dependency node inventory empty.")]
    if metadata.resolve.is_none() {
        return Vec::new();
    }
    let inventory = Inventory::new(metadata);
    let mut templates: HashMap<&PackageId, Option<DependencyTemplate>> = HashMap::default();
    let mut dependencies = Vec::new();
    for owner in metadata.workspace_packages() {
        let Some(node) = inventory.nodes.get(&owner.id) else {
            continue;
        };
        for edge in &node.deps {
            if !edge.dep_kinds.iter().any(|kind| kind.kind != DependencyKind::Build) {
                continue;
            }
            let Some(package) = inventory.packages.get(&edge.pkg) else {
                continue;
            };
            let mut aliases = vec![(edge.name.clone(), *package)];
            if let Some(node) = inventory.nodes.get(&package.id) {
                aliases.extend(node.deps.iter().filter_map(|nested| {
                    inventory
                        .packages
                        .get(&nested.pkg)
                        .map(|&package| (format!("{}::{}", edge.name, nested.name), package))
                }));
            }
            for (_, package) in &aliases {
                templates.entry(&package.id).or_insert_with(|| inventory.dependency(package));
            }
            for target in &owner.targets {
                for (alias, package) in &aliases {
                    if let Some(Some(template)) = templates.get(&package.id) {
                        dependencies.push(template.materialize(target.src_path.as_str(), alias));
                    }
                }
                // This conservative record-count default bounds inventory growth and matches the
                // namespace consumer's ceiling; it is not a measured package-capacity limit.
                // Exhaustion withholds optimization evidence, never selected mutation inputs.
                if dependencies.len() > 65536 {
                    return Vec::new();
                }
            }
        }
    }
    dependencies.sort_by(|left, right| {
        (&left.crate_root, &left.import_name, &left.package_id).cmp(&(&right.crate_root, &right.import_name, &right.package_id))
    });
    dependencies.dedup();
    dependencies
}

struct DependencyTemplate {
    package_id: String,
    package_name: String,
    version: String,
    source: String,
    features: Vec<String>,
    registry_closure: bool,
    library_root: Option<String>,
}

impl DependencyTemplate {
    fn materialize(&self, crate_root: &str, import_name: &str) -> Dependency {
        ResourceDependency {
            crate_root: crate_root.to_owned(),
            import_name: import_name.to_owned(),
            package_id: self.package_id.clone(),
            package_name: self.package_name.clone(),
            version: self.version.clone(),
            source: self.source.clone(),
            features: self.features.clone(),
            registry_closure: self.registry_closure,
            library_root: self.library_root.clone(),
        }
    }
}

/// Resolved-graph evidence keyed by package identity, reusable across import aliases.
///
/// Combines package and node data to identify workspace library roots and prove that the entire
/// transitive dependency closure stays in the configured registry.
struct Inventory<'a> {
    packages: HashMap<&'a PackageId, &'a Package>,
    nodes: HashMap<&'a PackageId, &'a Node>,
    workspace: HashSet<&'a PackageId>,
    traversal_steps: usize,
}

impl<'a> Inventory<'a> {
    fn new(metadata: &'a Metadata) -> Self {
        Self {
            packages: metadata.packages.iter().map(|package| (&package.id, package)).collect(),
            nodes: metadata
                .resolve
                .iter()
                .flat_map(|resolve| &resolve.nodes)
                .map(|node| (&node.id, node))
                .collect(),
            workspace: metadata.workspace_members.iter().collect(),
            // A deduplicated walk pops at most one root plus every dependency edge.
            traversal_steps: metadata.resolve.iter().flat_map(|resolve| &resolve.nodes).fold(
                // #[gamma::skip(literal.int_increment, tag = "equivalent", reason = "The deduplicated traversal already needs at most one root plus every dependency edge; one extra step only observes an empty pending stack.")]
                1_usize,
                |steps, node| steps.saturating_add(node.deps.len()),
            ),
        }
    }

    fn dependency(&self, package: &Package) -> Option<DependencyTemplate> {
        let node = self.nodes.get(&package.id)?;
        // #[gamma::skip(logical.and_remove_left, tag = "equivalent", reason = "Cargo metadata guarantees a null package source for every workspace member.")]
        let library_root = if package.source.is_none() && self.workspace.contains(&package.id) {
            let mut libraries = package
                .targets
                .iter()
                .filter(|target| target.kind.iter().any(|kind| matches!(kind, cargo_metadata::TargetKind::Lib)));
            match (libraries.next(), libraries.next()) {
                (Some(library), None) => Some(library.src_path.to_string()),
                _ => None,
            }
        } else {
            None
        };
        Some(DependencyTemplate {
            package_id: package.id.to_string(),
            package_name: package.name.to_string(),
            version: package.version.to_string(),
            source: package.source.as_ref().map_or_else(String::new, ToString::to_string),
            features: node.features.iter().map(ToString::to_string).collect(),
            registry_closure: self.registry_closure(&package.id),
            library_root,
        })
    }

    fn registry_closure(&self, start: &PackageId) -> bool {
        let mut pending = vec![start];
        let mut seen = HashSet::default();
        for _step in
            // #[gamma::skip(range.exclusive_to_inclusive, tag = "equivalent", reason = "One root plus every dependency edge bounds all pending pops, including duplicates; an extra empty pop cannot change the closure verdict.")]
            0..self.traversal_steps
        {
            let Some(id) = pending.pop() else {
                return true;
            };
            if !seen.insert(id) {
                continue;
            }
            // A conservative traversal default, not a registry-validity or measured-cost limit:
            // oversized closures withhold provenance rather than guessing that every input is locked.
            if seen.len() > 256 {
                return false;
            }
            let Some(package) = self.packages.get(id) else {
                return false;
            };
            if package
                .source
                .as_ref()
                .is_none_or(|source| source.to_string() != "registry+https://github.com/rust-lang/crates.io-index")
            {
                return false;
            }
            let Some(node) = self.nodes.get(id) else {
                return false;
            };
            pending.extend(node.deps.iter().map(|dependency| &dependency.pkg));
        }
        pending.is_empty()
    }
}

#[cfg(test)]
fn dependency(metadata: &Metadata, root: &str, alias: &str, package: &Package) -> Option<Dependency> {
    Some(Inventory::new(metadata).dependency(package)?.materialize(root, alias))
}

#[cfg(test)]
fn registry_closure(metadata: &Metadata, start: &PackageId) -> bool {
    Inventory::new(metadata).registry_closure(start)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::from_str;

    use super::*;

    fn metadata() -> Metadata {
        from_str(include_str!("../../tests/fixtures/correctness/resource-dependencies.json")).unwrap()
    }

    #[test]
    fn recovered_library_root_accepts_a_lib_among_multiple_crate_kinds() {
        let metadata = metadata();
        let mut package = metadata.packages[1].clone();
        package.targets[0].kind = from_str(r#"["lib", "rlib"]"#).unwrap();
        let dependency = dependency(&metadata, "src/root.rs", "renamed", &package).unwrap();
        assert_eq!(dependency.library_root.as_deref(), Some(package.targets[0].src_path.as_str()));
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "65,536-record boundary workload; smaller dependency tests remain enabled under Miri"
    )]
    fn recovered_dependency_inventory_limit_counts_records_before_deduplication() {
        let template = metadata();
        for (count, retained) in [(65_535, true), (65_536, true), (65_537, false), (65_538, false)] {
            let mut metadata = template.clone();
            let owner = &template.packages[0];
            let leaf = &template.packages[2];
            metadata.workspace_members = vec![owner.id.clone()];
            metadata.packages = vec![owner.clone(), leaf.clone()];
            metadata.packages[0].targets = vec![owner.targets[0].clone(); count];
            let resolve = metadata.resolve.as_mut().unwrap();
            resolve.nodes.retain(|node| node.id == owner.id || node.id == leaf.id);
            let edge = resolve.nodes[0]
                .deps
                .iter()
                .find(|edge| edge.pkg == leaf.id && edge.dep_kinds.iter().any(|kind| kind.kind != DependencyKind::Build))
                .unwrap()
                .clone();
            resolve.nodes[0].deps = vec![edge];
            let records = of(&metadata);
            assert_eq!(records.len(), usize::from(retained), "{count} records before deduplication");
            if retained {
                assert_eq!(records[0].package_id, leaf.id.to_string());
                assert_eq!(records[0].crate_root, owner.targets[0].src_path.as_str());
            }
        }
    }

    fn registry_chain(count: usize) -> (Metadata, PackageId) {
        let mut metadata = metadata();
        let package = metadata.packages[2].clone();
        let node = metadata.resolve.as_ref().unwrap().nodes[2].clone();
        let edge = metadata.resolve.as_ref().unwrap().nodes[0].deps[0].clone();
        let ids = (0..count)
            .map(|index| PackageId {
                repr: format!("registry+https://github.com/rust-lang/crates.io-index#chain-{index}@1.0.0"),
            })
            .collect::<Vec<_>>();
        metadata.packages = ids
            .iter()
            .map(|id| {
                let mut package = package.clone();
                package.id.clone_from(id);
                package
            })
            .collect();
        metadata.resolve.as_mut().unwrap().nodes = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut node = node.clone();
                node.id.clone_from(id);
                node.deps = ids
                    .get(index + 1)
                    .map(|next| {
                        let mut edge = edge.clone();
                        edge.pkg.clone_from(next);
                        vec![edge]
                    })
                    .unwrap_or_default();
                node
            })
            .collect();
        (metadata, ids[0].clone())
    }

    #[test]
    fn registry_provenance_accepts_the_exact_traversal_limit_and_refuses_larger_closures() {
        for (count, expected) in [(2, true), (255, true), (256, true), (257, false), (258, false)] {
            let (metadata, root) = registry_chain(count);
            assert_eq!(registry_closure(&metadata, &root), expected, "{count} packages");
        }
    }

    #[test]
    fn a_registry_cycle_finishes_without_reprocessing_its_dependencies() {
        let (mut metadata, root) = registry_chain(2);
        let nodes = &mut metadata.resolve.as_mut().unwrap().nodes;
        let mut back = nodes[0].deps[0].clone();
        back.pkg.clone_from(&root);
        nodes[1].deps.push(back);
        assert!(registry_closure(&metadata, &root));
    }

    #[test]
    fn revisiting_a_registry_package_does_not_abandon_pending_registry_branches() {
        let (mut metadata, root) = registry_chain(4);
        let nodes = &mut metadata.resolve.as_mut().unwrap().nodes;
        let edge = nodes[0].deps[0].clone();
        let ids = nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>();
        // Visit the shared branch through package 1 twice while package 3 remains pending;
        // its later nonregistry status must still be inspected after the duplicate.
        nodes[0].deps = [3, 1, 2]
            .map(|index| {
                let mut edge = edge.clone();
                edge.pkg.clone_from(&ids[index]);
                edge.name = format!("branch_{index}");
                edge
            })
            .to_vec();
        nodes[1].deps.clear();
        nodes[2].deps = vec![edge];
        nodes[3].deps.clear();
        for node in nodes {
            node.dependencies = node.deps.iter().map(|dependency| dependency.pkg.clone()).collect();
        }
        assert!(registry_closure(&metadata, &root));

        metadata.packages[3].source = None;
        assert!(!registry_closure(&metadata, &root));
    }

    #[test]
    fn a_registry_package_without_a_resolved_node_cannot_supply_provenance() {
        let (mut metadata, root) = registry_chain(2);
        assert!(registry_closure(&metadata, &root));
        metadata.resolve.as_mut().unwrap().nodes.pop();
        assert!(!registry_closure(&metadata, &root));
        let package = metadata.packages[0].clone();
        metadata.resolve = None;
        assert!(dependency(&metadata, "src/lib.rs", "alias", &package).is_none());
    }

    #[test]
    fn revisiting_a_registry_cycle_does_not_skip_a_pending_local_dependency() {
        let mut metadata = metadata();
        let leaf = metadata.packages[2].id.clone();
        let local = metadata.packages[1].id.clone();
        let mut cycle = metadata.resolve.as_ref().unwrap().nodes[0].deps[0].clone();
        cycle.pkg = leaf.clone();
        let mut untrusted = cycle.clone();
        untrusted.pkg = local;
        metadata.resolve.as_mut().unwrap().nodes[2].deps = vec![untrusted, cycle];
        assert!(!registry_closure(&metadata, &leaf));
        metadata.resolve.as_mut().unwrap().nodes[2].deps.remove(0);
        assert!(registry_closure(&metadata, &leaf));
    }

    #[test]
    fn resource_root_closure_visits_deep_chains_branches_and_cycles_without_unrelated_roots() {
        let template = of(&metadata()).remove(0);
        let mut dependencies = (0..130)
            .map(|index| Dependency {
                crate_root: format!("root-{index}"),
                library_root: Some(format!("root-{}", index + 1)),
                ..template.clone()
            })
            .collect::<Vec<_>>();
        for (owner, library) in [
            ("root-130", Some("root-0")),
            ("root-65", Some("branch")),
            ("root-65", Some("branch")),
            ("branch", Some("branch")),
            ("branch", None),
            ("unrelated", Some("unreachable")),
        ] {
            dependencies.push(Dependency {
                crate_root: owner.into(),
                library_root: library.map(str::to_owned),
                ..template.clone()
            });
        }
        dependencies.reverse();
        let mut roots = HashSet::from_iter(["root-0".to_owned()]);
        extend_roots(&mut roots, &dependencies);
        let mut expected: HashSet<_> = (0..=130).map(|index| format!("root-{index}")).collect();
        assert!(expected.insert("branch".into()));
        assert_eq!(roots, expected);
        extend_roots(&mut roots, &dependencies);
        assert_eq!(roots, expected);

        let mut empty = HashSet::default();
        extend_roots(&mut empty, &dependencies);
        assert!(empty.is_empty());
        extend_roots(&mut roots, &[]);
        assert_eq!(roots, expected);
    }

    #[test]
    fn resource_edges_preserve_target_alias_package_features_and_registry_provenance() {
        let metadata = metadata();
        let dependencies = of(&metadata);
        let identities = dependencies
            .iter()
            .map(|dependency| (dependency.crate_root.as_str(), dependency.import_name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            [
                ("C:/fixture/alpha/src/lib.rs", "dev_leaf"),
                ("C:/fixture/alpha/src/lib.rs", "renamed_beta"),
                ("C:/fixture/alpha/src/lib.rs", "renamed_beta::leaf"),
                ("C:/fixture/alpha/src/main.rs", "dev_leaf"),
                ("C:/fixture/alpha/src/main.rs", "renamed_beta"),
                ("C:/fixture/alpha/src/main.rs", "renamed_beta::leaf"),
                ("C:/fixture/beta/src/lib.rs", "leaf"),
            ]
        );
        for dependency in dependencies {
            if dependency.import_name == "renamed_beta" {
                assert_eq!(dependency.package_id, "path+file:///fixture/beta#1.2.3");
                assert_eq!(dependency.package_name, "beta");
                assert_eq!(dependency.version, "1.2.3");
                assert_eq!(dependency.source, "");
                assert_eq!(dependency.features, ["fast", "std"]);
                assert_eq!(dependency.library_root.as_deref(), Some("C:/fixture/beta/src/lib.rs"));
                assert!(!dependency.registry_closure);
            } else {
                assert_eq!(dependency.package_name, "registry-leaf");
                assert_eq!(dependency.version, "2.3.4");
                assert_eq!(dependency.source, "registry+https://github.com/rust-lang/crates.io-index");
                assert_eq!(dependency.features, ["alloc"]);
                assert_eq!(dependency.library_root, None);
                assert!(dependency.registry_closure);
            }
        }
    }

    #[test]
    fn resource_edges_are_sorted_deduplicated_and_skip_unresolved_nodes_without_stopping() {
        let mut metadata = metadata();
        let resolve = metadata.resolve.as_mut().unwrap();
        let duplicate = resolve.nodes[0].deps[0].clone();
        resolve.nodes[0].deps.insert(0, duplicate.clone());
        let mut missing = duplicate;
        missing.pkg = PackageId {
            repr: "missing-package".into(),
        };
        resolve.nodes[0].deps.insert(0, missing.clone());
        resolve.nodes[1].deps.insert(0, missing);
        resolve.nodes.reverse();
        let actual = of(&metadata);
        assert_eq!(actual, of(&self::metadata()));
        metadata
            .resolve
            .as_mut()
            .unwrap()
            .nodes
            .retain(|node| node.id != metadata.packages[0].id);
        let remaining = of(&metadata);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].import_name, "leaf");
        assert_eq!(remaining[0].crate_root, "C:/fixture/beta/src/lib.rs");
        metadata.resolve = None;
        assert_eq!(of(&metadata).len(), 0);
    }

    #[test]
    fn only_a_unique_workspace_library_can_supply_dependency_source() {
        let mut metadata = metadata();
        let root = "C:/fixture/alpha/src/main.rs";
        let beta = metadata.packages[1].clone();
        assert_eq!(
            dependency(&metadata, root, "alias", &beta).unwrap().library_root,
            Some("C:/fixture/beta/src/lib.rs".into())
        );
        let mut no_library = beta.clone();
        no_library.targets.clear();
        assert_eq!(dependency(&metadata, root, "alias", &no_library).unwrap().library_root, None);
        let mut ambiguous = beta.clone();
        ambiguous.targets.push(ambiguous.targets[0].clone());
        assert_eq!(dependency(&metadata, root, "alias", &ambiguous).unwrap().library_root, None);
        metadata.workspace_members.retain(|id| id != &beta.id);
        assert_eq!(dependency(&metadata, root, "alias", &beta).unwrap().library_root, None);
        metadata.resolve.as_mut().unwrap().nodes.retain(|node| node.id != beta.id);
        assert!(dependency(&metadata, root, "alias", &beta).is_none());
    }

    #[test]
    fn registry_closure_handles_cycles_and_refuses_missing_local_or_foreign_sources() {
        let mut metadata = metadata();
        let leaf_id = metadata.packages[2].id.clone();
        assert!(registry_closure(&metadata, &leaf_id));
        let mut cycle = metadata.resolve.as_ref().unwrap().nodes[0].deps[0].clone();
        cycle.pkg = leaf_id.clone();
        metadata.resolve.as_mut().unwrap().nodes[2].deps.push(cycle.clone());
        assert!(registry_closure(&metadata, &leaf_id));
        cycle.pkg = metadata.packages[1].id.clone();
        metadata.resolve.as_mut().unwrap().nodes[2].deps.push(cycle);
        assert!(!registry_closure(&metadata, &leaf_id));
        metadata.resolve.as_mut().unwrap().nodes[2].deps.pop();
        metadata.packages[2].source = Some(from_str("\"registry+https://example.test/index\"").unwrap());
        assert!(!registry_closure(&metadata, &leaf_id));
        metadata.packages[2].source = None;
        assert!(!registry_closure(&metadata, &leaf_id));
        metadata.packages.remove(2);
        assert!(!registry_closure(&metadata, &leaf_id));
        metadata.resolve = None;
        assert!(!registry_closure(&metadata, &leaf_id));
    }
}
