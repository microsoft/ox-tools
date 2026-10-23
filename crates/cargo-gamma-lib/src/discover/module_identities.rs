// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unique logical module labels for package declaration evidence.

use camino::{Utf8Path, Utf8PathBuf};
use syn::Item;

use super::modules::{owned_directory, path_attribute};
use crate::cfg::CfgSet;
use crate::{HashMap, Result, paths};

#[derive(Clone, Debug)]
pub struct Edge {
    target: Utf8PathBuf,
    module: Option<Vec<String>>,
}

pub(super) fn declarations(path: &Utf8Path, ast: &syn::File, cfg: &CfgSet) -> Result<Vec<Edge>> {
    declarations_with(path, ast, cfg, Utf8Path::is_file, paths::physical)
}

fn declarations_with(
    path: &Utf8Path,
    ast: &syn::File,
    cfg: &CfgSet,
    mut is_file: impl FnMut(&Utf8Path) -> bool,
    mut resolve: impl FnMut(&Utf8Path) -> Result<Utf8PathBuf>,
) -> Result<Vec<Edge>> {
    let mut found = Vec::new();
    if let (Some(directory), Some(base)) = (owned_directory(path), path.parent()) {
        walk(&ast.items, &directory, base, Some(&[]), cfg, &mut found, &mut is_file, &mut resolve)?;
    }
    Ok(found)
}

#[expect(clippy::too_many_arguments, reason = "recursive module traversal shares its two filesystem seams")]
fn walk(
    items: &[Item],
    directory: &Utf8Path,
    base: &Utf8Path,
    module: Option<&[String]>,
    cfg: &CfgSet,
    found: &mut Vec<Edge>,
    is_file: &mut impl FnMut(&Utf8Path) -> bool,
    resolve: &mut impl FnMut(&Utf8Path) -> Result<Utf8PathBuf>,
) -> Result<()> {
    for item in items {
        let Item::Mod(node) = item else { continue };
        if cfg.skip_gate(&node.attrs) {
            continue;
        }
        // Only attributes whose module identity and file-selection effects we understand
        // preserve a label; an unknown attribute may expand or replace the declaration.
        let namespace = module
            .filter(|_| {
                node.attrs.iter().all(|attr| {
                    attr.path().get_ident().is_some_and(|name| {
                        matches!(
                            name.to_string().as_str(),
                            "cfg" | "path" | "doc" | "allow" | "expect" | "warn" | "deny" | "forbid" | "deprecated" | "macro_use"
                        )
                    })
                })
            })
            .map(|module| {
                module
                    .iter()
                    .cloned()
                    .chain(core::iter::once(node.ident.to_string()))
                    .collect::<Vec<_>>()
            });
        if let Some((_, items)) = &node.content {
            let nested = path_attribute(&node.attrs).map_or_else(|| directory.join(node.ident.to_string()), |path| base.join(path));
            walk(items, &nested, &nested, namespace.as_deref(), cfg, found, is_file, resolve)?;
            continue;
        }
        let name = node.ident.to_string();
        let candidates = path_attribute(&node.attrs).map_or_else(
            || vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")],
            |path| vec![base.join(path)],
        );
        let unresolved = candidates.first().cloned();
        let existing = candidates.into_iter().filter(|target| is_file(target)).collect::<Vec<_>>();
        if existing.is_empty()
            && let Some(target) = unresolved
        {
            found.push(Edge { target, module: None });
        }
        let unique = existing.len() == 1;
        for target in existing {
            found.push(Edge {
                target: resolve(&target)?,
                module: namespace.clone().filter(|_| unique),
            });
        }
    }
    Ok(())
}

pub(super) fn layouts(roots: &[Utf8PathBuf], declarations: &[(Utf8PathBuf, Vec<Edge>)]) -> Vec<(String, String, Vec<String>)> {
    let mut edges = HashMap::<&Utf8Path, Option<&[Edge]>>::default();
    for (source, declared) in declarations {
        let _entry = edges
            .entry(source.as_path())
            .and_modify(|value| *value = None)
            .or_insert(Some(declared));
    }
    let mut found = Vec::new();
    for root in roots {
        let mut modules = HashMap::<Utf8PathBuf, Vec<String>>::default();
        let mut owners = HashMap::<Vec<String>, Utf8PathBuf>::default();
        let mut queue = vec![(root.clone(), Vec::<String>::new())];
        let mut uncertain = false;
        while let Some((file, module)) = queue.pop() {
            if let Some(existing) = modules.get(&file) {
                uncertain |= existing != &module;
                // #[gamma::skip(loop.delete_continue, reason = "a revisited file can otherwise enqueue a cycle with ever-longer module paths")]
                continue;
            }
            if owners.get(&module).is_some_and(|owner| owner != &file) {
                uncertain = true;
            }
            let _old = owners.insert(module.clone(), file.clone());
            let _old = modules.insert(file.clone(), module.clone());
            let Some(Some(declared)) = edges.get(file.as_path()) else {
                uncertain = true;
                continue;
            };
            for edge in *declared {
                if let Some(path) = &edge.module {
                    queue.push((edge.target.clone(), module.iter().chain(path).cloned().collect()));
                } else {
                    uncertain = true;
                }
            }
        }
        // Layouts are authoritative crate-wide evidence. An unknown edge or conflicting
        // owner can hide a competing declaration, invalidating even otherwise known labels.
        if !uncertain {
            found.extend(
                modules
                    .into_iter()
                    .map(|(source, module)| (source.to_string(), root.to_string(), module)),
            );
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod survivor_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::error;

    #[test]
    fn parent_relative_module_paths_retain_the_complete_crate_layout() {
        let root = Utf8PathBuf::from("source/bin/tool/main.rs");
        let child = Utf8PathBuf::from("source/shared.rs");
        let ast = syn::parse_file("#[path = \"../../shared.rs\"] mod shared;").unwrap();
        let mut resolved = Vec::new();
        let edges = declarations_with(
            &root,
            &ast,
            &CfgSet::unconditional(),
            |_| true,
            |target| {
                resolved.push(target.to_owned());
                Ok(child.clone())
            },
        )
        .unwrap();
        assert_eq!(resolved, [Utf8PathBuf::from("source/bin/tool/../../shared.rs")]);
        let layout = layouts(core::slice::from_ref(&root), &[(root.clone(), edges), (child.clone(), vec![])]);
        assert_eq!(layout.len(), 2);
        assert!(layout.contains(&(root.to_string(), root.to_string(), vec![])));
        assert!(layout.contains(&(child.to_string(), root.to_string(), vec!["shared".into()])));
    }

    #[test]
    fn repeated_roots_deduplicate_layouts_without_merging_distinct_crate_identities() {
        let root = Utf8PathBuf::from("lib.rs");
        let child = Utf8PathBuf::from("shared.rs");
        let declarations = [
            (
                root.clone(),
                vec![Edge {
                    target: child.clone(),
                    module: Some(vec!["shared".to_owned()]),
                }],
            ),
            (child.clone(), vec![]),
        ];
        assert_eq!(
            layouts(&[root.clone(), child.clone(), root.clone()], &declarations),
            [
                (root.to_string(), root.to_string(), vec![]),
                (child.to_string(), root.to_string(), vec!["shared".to_owned()]),
                (child.to_string(), child.to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn module_identity_resolution_errors_are_reported_instead_of_dropping_evidence() {
        let ast = syn::parse_file("mod child;").unwrap();
        let error = declarations_with(
            Utf8Path::new("source/lib.rs"),
            &ast,
            &CfgSet::unconditional(),
            |target| target == Utf8Path::new("source/child.rs"),
            |_| Err(error!("could not resolve module child")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("could not resolve module child"));
    }

    #[test]
    fn missing_duplicate_and_cyclic_module_inputs_never_prove_a_unique_crate_layout() {
        let root = Utf8PathBuf::from("lib.rs");
        let child = Utf8PathBuf::from("child.rs");
        let edge = |target| Edge {
            target,
            module: Some(vec!["child".to_owned()]),
        };
        let roots = core::slice::from_ref(&root);
        assert!(layouts(roots, &[(root.clone(), vec![edge(child)])]).is_empty());
        assert!(layouts(roots, &[(root.clone(), vec![]), (root.clone(), vec![])]).is_empty());
        assert!(layouts(roots, &[(root.clone(), vec![edge(root.clone())])]).is_empty());
    }
}
