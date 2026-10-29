// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Namespace evidence allowances are conservative defaults, not measured capacities or Rust limits.
//! Source, scope, function-cache, and token allowances bound local traversal and retained syntax;
//! aggregate source and dependency allowances bound separate inventories. Matching scales do not
//! imply shared accounting, and these envelopes are not a combined memory-usage guarantee.

use std::sync::Arc;

use proc_macro2::{Delimiter, TokenTree};
use quote::ToTokens as _;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Attribute, File, Item, Macro, Meta, Token, UseTree};

use super::ResourceDependency;
use crate::cfg::CfgSet;
use crate::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct Namespaces {
    local: Modules,
    pending: HashMap<String, Option<Modules>>,
    roots: HashMap<String, Modules>,
    contexts: HashMap<String, Option<(String, Vec<String>)>>,
    bound: bool,
    dependencies: HashMap<(String, String), Option<ResourceDependency>>,
    costs: HashMap<String, usize>,
    local_cost: usize,
    exhausted: bool,
}

impl Namespaces {
    pub(super) fn of(file: &File, cfg: &CfgSet) -> Self {
        Self {
            local: Modules::of(file, cfg),
            local_cost: file.span().byte_range().len(),
            ..Self::default()
        }
    }

    pub(super) fn label(&mut self, source: &str) {
        if source.len() > 32768 {
            self.exhausted = true;
        }
        if
        // #[gamma::skip(cond.always_false, reason = "limit() independently checks sticky exhausted state and clears costs/evidence. Disabling label's first exhaustion return may insert a cost briefly, but limit discards it and the second exhaustion return still prevents insertion into pending. No query sees new evidence.", tag = "equivalent")]
        self.exhausted {
            return;
        }
        let _old = self.costs.insert(source.replace('\\', "/"), self.local_cost);
        self.limit();
        if self.exhausted {
            return;
        }
        let _old = self.pending.insert(source.replace('\\', "/"), Some(self.local.clone()));
    }

    pub(super) fn absorb(&mut self, other: Self) {
        self.costs(&other);
        if self.exhausted {
            return;
        }
        for (source, modules) in other.pending {
            let _entry = self.pending.entry(source).and_modify(|value| *value = None).or_insert(modules);
        }
    }

    pub(super) fn extend(&mut self, other: &Self) {
        self.costs(other);
        if self.exhausted {
            return;
        }
        for (source, modules) in &other.pending {
            let _entry = self
                .pending
                .entry(source.clone())
                .and_modify(|current| {
                    if current != modules {
                        *current = None;
                    }
                })
                .or_insert_with(|| modules.clone());
        }
    }

    pub(super) fn bind(&mut self, layouts: &[(String, String, Vec<String>)]) {
        self.bound = true;
        if self.exhausted
            || layouts.len() > 4096
            || layouts.iter().any(|(source, root, module)| {
                source.len() > 32768 || root.len() > 32768 || module.len() > 32 || module.iter().any(|part| part.len() > 256)
            })
        {
            self.exhausted = true;
            self.pending.clear();
            return;
        }
        for (source, root, module) in layouts {
            let source = source.replace('\\', "/");
            let root = root.replace('\\', "/");
            let Some(Some(modules)) = self.pending.get(&source) else { continue };
            self.roots.entry(root.clone()).or_default().absorb_at(modules, module);
            let _entry = self
                .contexts
                .entry(source)
                .and_modify(|value| *value = None)
                .or_insert_with(|| Some((root, module.clone())));
        }
        self.pending.clear();
    }

    pub(super) fn alias(&mut self, source: &str, alias: &str) {
        if source.len() > 32768 || alias.len() > 32768 {
            self.exhausted = true;
            return;
        }
        if let Some(context) = self.contexts.get(&source.replace('\\', "/")).cloned() {
            let _entry = self
                .contexts
                .entry(alias.replace('\\', "/"))
                .and_modify(|value| *value = None)
                .or_insert(context);
        }
    }

    pub(super) fn transparent(&self, source: &str, inline: &[String]) -> bool {
        if self.exhausted {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        if !self.bound {
            return self.local.transparent(inline, |namespace| self.opaque("", namespace));
        }
        let Some(Some((root, prefix))) = self.contexts.get(&source.replace('\\', "/")) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        let Some(modules) = self.roots.get(root) else {
            return
        // #[gamma::skip(literal.bool_flip, reason = "A bound context is installed only after its root is inserted, and external_target returns a root borrowed from roots.get_key_value. Roots are not removed without exhaustion, which prevents reaching these lookups. Successful import_path similarly requires modules(root) to have just succeeded. The later immutable root/module lookup cannot be absent.", tag = "equivalent")]
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        let module = prefix.iter().chain(inline).cloned().collect::<Vec<_>>();
        modules.transparent(&module, |namespace| self.opaque(root, namespace))
    }

    pub(super) fn function(&self, source: &str, inline: &[String], path: &[String]) -> Option<Function<'_>> {
        let (root, module) = if self.bound {
            let (root, prefix) = self.contexts.get(&source.replace('\\', "/"))?.as_ref()?;
            (root.as_str(), prefix.iter().chain(inline).cloned().collect())
        } else {
            ("", inline.to_vec())
        };
        self.function_in(root, &module, path)
    }

    pub(super) fn closed_methods_for(&self, source: &str, inline: &[String]) -> bool {
        if !self.bound {
            return self.closed_methods("", inline);
        }
        let Some(Some((root, prefix))) = self.contexts.get(&source.replace('\\', "/")) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        self.closed_methods(root, &prefix.iter().chain(inline).cloned().collect::<Vec<_>>())
    }

    pub(super) fn primitive_scope_for(&self, source: &str, inline: &[String]) -> bool {
        if !self.bound {
            return self.primitive_scope("", inline);
        }
        let Some(Some((root, prefix))) = self.contexts.get(&source.replace('\\', "/")) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        self.primitive_scope(root, &prefix.iter().chain(inline).cloned().collect::<Vec<_>>())
    }

    pub(super) fn width_contract_for(&self, source: &str, inline: &[String]) -> bool {
        if !self.bound {
            return self.width_contract("", inline, true);
        }
        let Some(Some((root, prefix))) = self.contexts.get(&source.replace('\\', "/")) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        self.width_contract(root, &prefix.iter().chain(inline).cloned().collect::<Vec<_>>(), true)
    }

    pub(super) fn function_in(&self, root: &str, module: &[String], path: &[String]) -> Option<Function<'_>> {
        let mut root = root;
        let mut modules = self.modules(root)?;
        let mut path = local_path(module, path)?;
        let mut visited = HashSet::default();
        for _ in 0..32 {
            if
            // #[gamma::skip(logical.or_remove_left, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            !visited.insert((root, path.clone())) || path.len() > 64 {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            let mut followed =
            // #[gamma::skip(literal.bool_flip, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            false;
            for length in (0..path.len()).rev() {
                let Some(Some(namespace)) = modules.scopes.get(&path[..length]) else {
                    continue;
                };
                if self.opaque(root, namespace) || namespace.wildcard || namespace.functions_exhausted {
                    return
                    // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                    None;
                }
                let suffix = &path[length..];
                if let Some(function) = namespace.functions.get(suffix) {
                    return Some(Function {
                        root: self.roots.get_key_value(root).map_or("", |(root, _)| root),
                        module: path[..length].to_vec(),
                        syntax: function.as_deref()?,
                        char_constructor: namespace.char_constructors.contains(suffix),
                    });
                }
                if let Some(import) = namespace.imports.get(&suffix[0]) {
                    let mut target = if let Some(path) = import_path(modules, &path[..length], import) {
                        path
                    } else {
                        let (external_root, path) = self.external_target(root, &path[..length], import)?;
                        root = external_root;
                        modules =
                        // #[gamma::skip(try.propagate_to_unwrap, reason = "A bound context is installed only after its root is inserted, and external_target returns a root borrowed from roots.get_key_value. Roots are not removed without exhaustion, which prevents reaching these lookups. Successful import_path similarly requires modules(root) to have just succeeded. The later immutable root/module lookup cannot be absent.", tag = "equivalent")]
                        self.modules(root)?;
                        path
                    };
                    target.extend_from_slice(&suffix[1..]);
                    path = target;
                    followed = true;
                    break;
                }
            }
            if
            // #[gamma::skip(cond.always_false, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            !followed {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
        }
        None
    }

    pub(super) fn dependencies(&mut self, dependencies: &[ResourceDependency]) {
        // Count matches the discovery inventory ceiling; field and total-text caps separately
        // bound metadata cloning, independently of the aggregate source envelope.
        if dependencies.len() > 65536
            || dependencies.iter().any(|dependency| {
                dependency.crate_root.len() > 32768
                    || dependency.import_name.len() > 256
                    || dependency.package_name.len() > 256
                    || dependency.package_id.len() > 32768
                    || dependency.source.len() > 1024
                    || dependency.version.len() > 256
                    || dependency.library_root.as_ref().is_some_and(|root| root.len() > 32768)
                    || dependency.features.len() > 256
                    || dependency.features.iter().any(|feature| feature.len() > 256)
            })
            || dependencies
                .iter()
                .map(|dependency| {
                    dependency.crate_root.len()
                        + dependency.import_name.len()
                        + dependency.package_name.len()
                        + dependency.package_id.len()
                        + dependency.source.len()
                        + dependency.version.len()
                        + dependency.library_root.as_ref().map_or(0, String::len)
                        + dependency.features.iter().map(String::len).sum::<usize>()
                })
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            self.exhausted = true;
            self.dependencies.clear();
            return;
        }
        for dependency in dependencies {
            let key = (dependency.crate_root.replace('\\', "/"), dependency.import_name.clone());
            let _entry = self
                .dependencies
                .entry(key)
                .and_modify(|value| *value = None)
                .or_insert_with(|| Some(dependency.clone()));
        }
    }

    pub(super) fn dependency(&self, root: &str, alias: &str, package: &str, version: &str) -> bool {
        self.modules(root).is_some_and(|modules| {
            modules
                .scopes
                .values()
                .flatten()
                .all(|namespace| !namespace.nominal.contains(alias))
        }) && self
            .dependencies
            .get(&(root.to_owned(), alias.to_owned()))
            .is_some_and(|dependency| {
                matches!(dependency, Some(dependency) if dependency.package_name == package && dependency.version == version
                && dependency.source == "registry+https://github.com/rust-lang/crates.io-index"
                && !dependency.package_id.is_empty() && dependency.registry_closure)
            })
    }

    pub(super) fn primitive_scope(&self, root: &str, module: &[String]) -> bool {
        let Some(modules) = self.modules(root) else {
            return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        let Some(Some(namespace)) = modules.scopes.get(module) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        !namespace.functions_exhausted
            && modules.transparent(module, |namespace| self.opaque(root, namespace))
            &&
            // #[gamma::skip(collection.reverse_array, reason = "These arrays feed pure membership/all predicates without mutation, so reversing their iteration order does not change the boolean result or retained state.", tag = "equivalent")]
            ["str", "String", "Vec", "char", "usize", "bool", "Some", "None", "Option"]
                .iter()
                .all(|name| {
                    !namespace.nominal.contains(*name)
                        && !namespace.values.contains(*name)
                        && !namespace.imports.contains_key(*name)
                        && !namespace.functions.contains_key(&vec![(*name).to_owned()])
                })
    }

    fn modules(&self, root: &str) -> Option<&Modules> {
        if self.exhausted {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        if root.is_empty() && !self.bound {
            Some(&self.local)
        } else {
            self.roots.get(root)
        }
    }

    /// Keeps modeled method spellings independent of unknown extension traits.
    ///
    /// Local traits must be checked explicitly; other imports need recognized standard or pinned
    /// dependency contracts before they can preserve method-resolution evidence.
    pub(super) fn closed_methods(&self, root: &str, module: &[String]) -> bool {
        let Some(Some(namespace)) = self.modules(root).and_then(|modules| modules.scopes.get(module)) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        (
        // #[gamma::skip(logical.and_remove_right, reason = "Modules::transparent first rejects the current namespace wildcard and then tests opaque for every prefix including the current namespace. Thus each separately removed opaque/wildcard disjunct is implied by !transparent, and the removed closed-method !wildcard conjunct is implied by transparent. Conjoining the two redundant disjuncts also leaves the dominating !transparent disjunct unchanged.", tag = "equivalent")]
        self.modules(root).is_some_and(|modules| modules.transparent(module, |namespace| self.opaque(root, namespace)))
            && !namespace.wildcard) && namespace.trait_methods.values().map(Option::as_ref).all(nonconflicting_trait)
            && namespace.imports.values().chain(&namespace.anonymous).all(|path| {
            matches!(path.first().map(String::as_str), Some("std" | "core" | "alloc"))
                || matches!(path.as_slice(), [alias, name] if direct_alias(namespace, alias) && matches!(name.as_str(), "UnicodeWidthChar" | "UnicodeWidthStr")
                    && self.dependency(root, alias, "unicode-width", "0.2.2"))
                || matches!(path.as_slice(), [alias, name] if direct_alias(namespace, alias) && name == "Style" && self.dependency(root, alias, "owo-colors", "4.4.0"))
                || self.grouping_import(root, namespace, path)
                || self.nontrait(root, module, path)
        })
    }

    fn grouping_import(&self, root: &str, namespace: &Namespace, path: &[String]) -> bool {
        match path {
            [alias, name] if direct_alias(namespace, alias) => match name.as_str() {
                "Utf8Path" | "Utf8PathBuf" => self.dependency(root, alias, "camino", "1.2.5"),
                "Event" | "Parser" => self.dependency(root, alias, "saphyr-parser", "0.0.12"),
                "SimpleClock" | "SystemTimeExt" => self.dependency(root, alias, "tick", "0.7.0"),
                "Serialize" | "Serializer" | "Deserialize" | "Deserializer" => self.dependency(root, alias, "serde", "1.0.228"),
                _ => false,
            },
            [alias, module, name] => {
                direct_alias(namespace, alias) && module == "de" && name == "IgnoredAny" && self.dependency(root, alias, "serde", "1.0.228")
            }
            _ => false,
        }
    }

    fn scope_for(&self, source: &str, inline: &[String]) -> Option<(&str, &Namespace)> {
        let (root, module) = if self.bound {
            let (root, prefix) = self.contexts.get(&source.replace('\\', "/"))?.as_ref()?;
            (root.as_str(), prefix.iter().chain(inline).cloned().collect::<Vec<_>>())
        } else {
            ("", inline.to_vec())
        };
        Some((root, self.modules(root)?.scopes.get(&module)?.as_ref()?))
    }

    pub(super) fn local_record_for(&self, source: &str, inline: &[String], name: &str) -> bool {
        self.scope_for(source, inline)
            .is_some_and(|(_, namespace)| namespace.records.contains(name) && !namespace.imports.contains_key(name))
    }

    pub(super) fn stable_path_for(&self, source: &str, inline: &[String], ty: &syn::Type) -> bool {
        let Some((root, namespace)) = self.scope_for(source, inline) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        if !self.primitive_scope_for(source, inline) {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let syn::Type::Path(ty) = ty else {
            return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        if ty.qself.is_some() || ty.path.leading_colon.is_some() {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let path = ty.path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        if path.as_slice() == ["String"] {
            return true;
        }
        let path = if let [name] = path.as_slice() {
            if namespace.nominal.contains(name) {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            namespace.imports.get(name).map_or(path.as_slice(), Vec::as_slice)
        } else {
            path.as_slice()
        };
        matches!(path, [alias, name] if name == "Utf8PathBuf" && direct_alias(namespace, alias)
            && self.dependency(root, alias, "camino", "1.2.5"))
    }

    fn nontrait(&self, root: &str, module: &[String], import: &[String]) -> bool {
        let (mut root, mut path) = if let Some(path) = self.modules(root).and_then(|modules| import_path(modules, module, import)) {
            (root, path)
        } else if let Some(target) = self.external_target(root, module, import) {
            target
        } else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        let Some(mut modules) = self.modules(root) else {
            return
        // #[gamma::skip(literal.bool_flip, reason = "A bound context is installed only after its root is inserted, and external_target returns a root borrowed from roots.get_key_value. Roots are not removed without exhaustion, which prevents reaching these lookups. Successful import_path similarly requires modules(root) to have just succeeded. The later immutable root/module lookup cannot be absent.", tag = "equivalent")]
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        let mut visited = HashSet::default();
        for _ in 0..32 {
            if let Some(namespace) = modules.scopes.get(&path) {
                return namespace.is_some()
                    && modules.transparent(&path[..path.len().saturating_sub(1)], |namespace| self.opaque(root, namespace));
            }
            if
            // #[gamma::skip(logical.or_remove_right, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            path.len() > 64 || !visited.insert((root, path.clone())) {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            let mut followed =
            // #[gamma::skip(literal.bool_flip, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            false;
            for length in (
                // #[gamma::skip(range.exclusive_to_inclusive, reason = "Before the prefix loop, the function already checks modules.scopes.get(full_path) and returns if present. Extending the prefix range to the full path adds only that known-absent lookup, which continues. The earlier conservative production fix remains intact; its old test does not distinguish this now-redundant inclusive iteration.", tag = "equivalent")]
                0..path.len()
            )
                .rev()
            {
                let Some(Some(namespace)) = modules.scopes.get(&path[..length]) else {
                    continue;
                };
                if
                // #[gamma::skip(logical.or_remove_left, reason = "Modules::transparent first rejects the current namespace wildcard and then tests opaque for every prefix including the current namespace. Thus each separately removed opaque/wildcard disjunct is implied by !transparent, and the removed closed-method !wildcard conjunct is implied by transparent. Conjoining the two redundant disjuncts also leaves the dominating !transparent disjunct unchanged.", tag = "equivalent")]
                (
                    // #[gamma::skip(logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "Modules::transparent first rejects the current namespace wildcard and then tests opaque for every prefix including the current namespace. Thus each separately removed opaque/wildcard disjunct is implied by !transparent, and the removed closed-method !wildcard conjunct is implied by transparent. Conjoining the two redundant disjuncts also leaves the dominating !transparent disjunct unchanged.", tag = "equivalent")]
                    self.opaque(root, namespace) || namespace.wildcard
                ) || !modules.transparent(&path[..length], |namespace| self.opaque(root, namespace))
                {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                }
                let suffix = &path[length..];
                if suffix.len() == 1 && namespace.nominal.contains(&suffix[0]) {
                    return namespace.trait_methods.get(&suffix[0]).map_or_else(
                        || !namespace.trait_names.contains(&suffix[0]),
                        |methods| nonconflicting_trait(methods.as_ref()),
                    );
                }
                if namespace.functions.contains_key(suffix) {
                    return true;
                }
                if suffix.len() == 1 && namespace.macro_names.contains(&suffix[0]) {
                    return true;
                }
                if let Some(import) = namespace.imports.get(&suffix[0]) {
                    let mut target = if let Some(path) = import_path(modules, &path[..length], import) {
                        path
                    } else if let Some((external_root, path)) = self.external_target(root, &path[..length], import) {
                        root = external_root;
                        let Some(external_modules) = self.modules(root) else {
                            return
                        // #[gamma::skip(literal.bool_flip, reason = "A bound context is installed only after its root is inserted, and external_target returns a root borrowed from roots.get_key_value. Roots are not removed without exhaustion, which prevents reaching these lookups. Successful import_path similarly requires modules(root) to have just succeeded. The later immutable root/module lookup cannot be absent.", tag = "equivalent")]
                        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                        false;
                        };
                        modules = external_modules;
                        path
                    } else {
                        return
                        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                        false;
                    };
                    target.extend_from_slice(&suffix[1..]);
                    path = target;
                    followed = true;
                    break;
                }
            }
            if
            // #[gamma::skip(cond.always_false, reason = "A repeated (root,path) deterministically repeats the same immutable alias lookup and cannot later resolve. The unchanged finite iteration budget still returns None/false without the visited guard. Continuing an unchanged path after no alias was followed instead hits that visited guard and returns the same None/false.", tag = "equivalent")]
            !followed {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
        }
        false
    }

    fn external_target(&self, root: &str, module: &[String], path: &[String]) -> Option<(&str, Vec<String>)> {
        let alias = path.first()?;
        let namespace = self.modules(root)?.scopes.get(module)?.as_ref()?;
        if namespace.nominal.contains(alias)
            || namespace
                .imports
                .get(alias)
                .is_some_and(|target| target.as_slice() != [alias.clone()])
        {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        let dependency = self.dependencies.get(&(root.to_owned(), alias.clone()))?.as_ref()?;
        if dependency.package_id.is_empty() {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        let library = dependency.library_root.as_ref()?.replace('\\', "/");
        let (root, _) = self.roots.get_key_value(&library)?;
        Some((root, path[1..].to_vec()))
    }

    fn costs(&mut self, other: &Self) {
        if self.exhausted {
            return;
        }
        self.exhausted = other.exhausted;
        for (source, cost) in &other.costs {
            let _entry = self
                .costs
                .entry(source.clone())
                .and_modify(|current| *current = (*current).max(*cost))
                .or_insert(*cost);
        }
        self.limit();
    }

    fn limit(&mut self) {
        // Distinct source-count and byte defaults bound the merged index, beyond per-file checks.
        self.exhausted |= self.costs.len() > 4096
            || self
                .costs
                .values()
                .try_fold(0usize, |sum, cost| sum.checked_add(*cost))
                .is_none_or(|cost| cost > 8 * 1024 * 1024);
        if self.exhausted {
            self.local = Modules::default();
            self.pending.clear();
            self.roots.clear();
            self.contexts.clear();
            self.costs.clear();
        }
    }

    pub(super) fn width_contract(&self, root: &str, module: &[String], character: bool) -> bool {
        let Some(Some(namespace)) = self.modules(root).and_then(|modules| modules.scopes.get(module)) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        let expected = if character { "UnicodeWidthChar" } else { "UnicodeWidthStr" };
        namespace.imports.values().chain(&namespace.anonymous).any(|path| {
            matches!(path.as_slice(), [alias, name] if direct_alias(namespace, alias) && name == expected && self.dependency(root, alias, "unicode-width", "0.2.2"))
        })
    }

    pub(super) fn standard_macro(&self, root: &str, module: &[String], name: &str) -> bool {
        let Some(modules) = self.modules(root) else {
            return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
        };
        modules.transparent(module, |namespace| self.opaque(root, namespace))
            && (0..=module.len()).all(|length| {
                modules.scopes.get(&module[..length]).is_some_and(|namespace| {
                    namespace.as_ref().is_some_and(|namespace| {
                        !namespace.macro_names.contains(name) && !namespace.wildcard && !namespace.imports.contains_key(name)
                    })
                })
            })
    }

    /// Only narrowly admitted derive inputs may rely on the resolved Serde/derive dependency pair.
    /// Unknown derive expansion could introduce declarations or traits that invalidate the proof.
    fn opaque(&self, root: &str, namespace: &Namespace) -> bool {
        namespace.opaque
            || namespace
                .generated_modules
                .iter()
                .any(|name| self.dependencies.contains_key(&(root.to_owned(), name.clone())))
            || namespace.serde.iter().any(|paths| {
                paths.iter().any(|path| {
                    let path = if path.len() == 1 {
                        namespace.imports.get(&path[0]).map_or(path.as_slice(), Vec::as_slice)
                    } else {
                        path.as_slice()
                    };
                    !matches!(path, [alias, name] if direct_alias(namespace, alias) && matches!(name.as_str(), "Serialize" | "Deserialize")
                    && self.dependency(root, alias, "serde", "1.0.228")
                    && self.dependency(root, &format!("{alias}::serde_derive"), "serde_derive", "1.0.228")
                    && self.dependencies.get(&(root.to_owned(), alias.clone())).is_some_and(|dependency| {
                        matches!(dependency, Some(dependency) if dependency.features.iter().any(|feature| feature == "serde_derive"))
                    }))
                })
            })
    }
}

pub(super) struct Function<'a> {
    pub(super) root: &'a str,
    pub(super) module: Vec<String>,
    pub(super) syntax: &'a str,
    pub(super) char_constructor: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Modules {
    scopes: HashMap<Vec<String>, Option<Namespace>>,
}

impl Modules {
    fn of(file: &File, cfg: &CfgSet) -> Self {
        // The per-file default withholds evidence before walking an oversized declaration tree.
        if file.span().byte_range().len() > 1024 * 1024 {
            return Self {
                scopes: std::iter::once((
                    Vec::new(),
                    Some(Namespace {
                        opaque: true,
                        ..Namespace::default()
                    }),
                ))
                .collect(),
            };
        }
        let mut result = Self::default();
        result.items(&file.items, &mut Vec::new(), cfg);
        if file.attrs.iter().any(|attribute| attribute.path().is_ident("no_implicit_prelude"))
            && let Some(Some(namespace)) = result.scopes.get_mut(&Vec::new())
        {
            namespace.opaque = true;
        }
        result
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one bounded declaration pass classifies namespace-affecting item forms"
    )]
    fn items(&mut self, items: &[Item], module: &mut Vec<String>, cfg: &CfgSet) {
        // Independent depth and scope-count defaults bound recursion and index growth.
        if module.len() > 32 || self.scopes.len() > 4096 {
            return;
        }
        let mut namespace = Namespace::default();
        for item in items {
            if let Some(attributes) = item_attributes(item)
                && potentially_active(cfg, attributes)
                && !declaration_attributes(attributes)
            {
                if let Some(derives) = serde_attributes(attributes)
                    && serde_input(item)
                {
                    namespace.serde.push(derives);
                } else {
                    namespace.opaque = true;
                }
            }
            match item {
                Item::Struct(item)
                    if potentially_active(cfg, &item.attrs)
                        && (nominal_attributes(&item.attrs) || serde_attributes(&item.attrs).is_some()) =>
                {
                    if namespace.nominal.insert(item.ident.to_string()) {
                        let _new = namespace.records.insert(item.ident.to_string());
                    } else {
                        let _old = namespace.records.remove(&item.ident.to_string());
                    }
                    if !nominal_attributes(&item.attrs) {
                        let _new = namespace.serde_nominal.insert(item.ident.to_string());
                    }
                }
                Item::Enum(item)
                    if potentially_active(cfg, &item.attrs)
                        && (nominal_attributes(&item.attrs) || serde_attributes(&item.attrs).is_some()) =>
                {
                    let _old = namespace.records.remove(&item.ident.to_string());
                    let _new = namespace.nominal.insert(item.ident.to_string());
                    if !nominal_attributes(&item.attrs) {
                        let _new = namespace.serde_nominal.insert(item.ident.to_string());
                    }
                    if item.generics.params.is_empty() {
                        for variant in &item.variants {
                            if declaration_attributes(&variant.attrs)
                                && let syn::Fields::Unnamed(fields) = &variant.fields
                                && fields.unnamed.len() == 1
                                &&
                                // #[gamma::skip(iter.all_to_any, reason = "The immediately preceding fields.unnamed.len()==1 guard ensures all and any inspect the same single field and return the same predicate result.", tag = "equivalent")]
                                fields.unnamed.iter().all(|field| {
                                    declaration_attributes(&field.attrs)
                                        && matches!(&field.ty, syn::Type::Path(path) if path.qself.is_none() && path.path.is_ident("char"))
                                })
                            {
                                let path = vec![item.ident.to_string(), variant.ident.to_string()];
                                let _new = namespace.char_constructors.insert(path.clone());
                                namespace.function(
                                    path,
                                    &quote::quote!(
                                        fn value(character: char) -> char {
                                            character
                                        }
                                    ),
                                    // #[gamma::skip(literal.int_decrement, literal.int_increment, literal.int_to_one, literal.int_to_zero, reason = "The synthetic character-constructor function receives only a byte-cost value here. All mutated constants 0,1,63,65 and the original 64 are <=16384, and the fixed rendered function is also below that separate limit. The cached syntax and constructor flag match.", tag = "equivalent")]
                                    64,
                                );
                            }
                        }
                    }
                }
                Item::Type(item) if potentially_active(cfg, &item.attrs) && nominal_attributes(&item.attrs) => {
                    let _old = namespace.records.remove(&item.ident.to_string());
                    let _new = namespace.nominal.insert(item.ident.to_string());
                }
                Item::Union(item) if potentially_active(cfg, &item.attrs) && nominal_attributes(&item.attrs) => {
                    let _old = namespace.records.remove(&item.ident.to_string());
                    let _new = namespace.nominal.insert(item.ident.to_string());
                }
                Item::Trait(item) if potentially_active(cfg, &item.attrs) => {
                    let _old = namespace.records.remove(&item.ident.to_string());
                    namespace.traits = true;
                    let _new = namespace.trait_names.insert(item.ident.to_string());
                    let methods = (item.supertraits.is_empty() && declaration_attributes(&item.attrs))
                        .then(|| {
                            item.items
                                .iter()
                                .map(|item| match item {
                                    syn::TraitItem::Fn(function) if declaration_attributes(&function.attrs) => {
                                        Some(function.sig.ident.to_string())
                                    }
                                    _ => None,
                                })
                                .collect::<Option<HashSet<_>>>()
                        })
                        .flatten();
                    let _entry = namespace
                        .trait_methods
                        .entry(item.ident.to_string())
                        .and_modify(|value| *value = None)
                        .or_insert(methods);
                    let _new = namespace.nominal.insert(item.ident.to_string());
                }
                Item::Const(item) if potentially_active(cfg, &item.attrs) => {
                    let _new = namespace.values.insert(item.ident.to_string());
                }
                Item::Static(item) if potentially_active(cfg, &item.attrs) => {
                    let _new = namespace.values.insert(item.ident.to_string());
                }
                Item::Fn(function) if potentially_active(cfg, &function.attrs) && declaration_attributes(&function.attrs) => {
                    if cfg_invariant(&function.attrs) {
                        namespace.function(
                            vec![function.sig.ident.to_string()],
                            &function.to_token_stream(),
                            function.span().byte_range().len(),
                        );
                    } else {
                        let _old = namespace.functions.insert(vec![function.sig.ident.to_string()], None);
                    }
                }
                Item::Impl(implementation) if potentially_active(cfg, &implementation.attrs) && implementation.trait_.is_none() => {
                    if let syn::Type::Path(ty) = &*implementation.self_ty
                        && let Some(name) = ty.path.get_ident()
                        && implementation.generics.params.is_empty()
                    {
                        for item in &implementation.items {
                            if let syn::ImplItem::Fn(function) = item
                                && potentially_active(cfg, &function.attrs)
                            {
                                if cfg_invariant(&implementation.attrs) && cfg_invariant(&function.attrs) {
                                    namespace.function(
                                        vec![name.to_string(), function.sig.ident.to_string()],
                                        &function.to_token_stream(),
                                        function.span().byte_range().len(),
                                    );
                                } else {
                                    let _old = namespace
                                        .functions
                                        .insert(vec![name.to_string(), function.sig.ident.to_string()], None);
                                }
                            }
                        }
                    }
                }
                Item::Macro(item) if potentially_active(cfg, &item.attrs) => {
                    if item.mac.path.is_ident("macro_rules") {
                        if let Some(name) = &item.ident {
                            let name = name.to_string();
                            namespace.opaque |= !namespace.macro_names.insert(name.clone());
                            if harmless_definition(&item.mac)
                                && let Some(modules) = generated_modules(&item.mac)
                            {
                                let _new = namespace.harmless.insert(name);
                                namespace.generated_modules.extend(modules);
                            }
                        }
                    } else if item.mac.path.segments.last().is_some_and(|segment| segment.ident == "thread_local")
                        && !thread_local_body(&item.mac)
                    {
                        namespace.opaque = true;
                    } else {
                        namespace
                            .calls
                            .push(item.mac.path.segments.iter().map(|part| part.ident.to_string()).collect());
                    }
                    namespace.opaque |= !inert_attributes(&item.attrs);
                }
                Item::Use(item) if potentially_active(cfg, &item.attrs) => {
                    namespace.opaque |= !inert_attributes(&item.attrs);
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    imports(&item.tree, &mut Vec::new(), &mut namespace);
                }
                Item::ExternCrate(item) if potentially_active(cfg, &item.attrs) => {
                    let name = item.rename.as_ref().map_or(&item.ident, |(_, name)| name);
                    namespace.std_shadow |= name == "std";
                    namespace.opaque = true;
                }
                Item::TraitAlias(_) => namespace.opaque = true,
                Item::Mod(item) if potentially_active(cfg, &item.attrs) => {
                    let _old = namespace.records.remove(&item.ident.to_string());
                    let _new = namespace.nominal.insert(item.ident.to_string());
                    namespace.std_shadow |= matches!(item.ident.to_string().as_str(), "std" | "core" | "alloc");
                    namespace.opaque |= !inert_attributes(&item.attrs);
                    if let Some((_, items)) = &item.content
                        && inert_attributes(&item.attrs)
                    {
                        module.push(item.ident.to_string());
                        self.items(items, module, cfg);
                        let _parent = module.pop();
                    }
                }
                _ => {}
            }
        }
        namespace.opaque |= namespace.imports.keys().chain(&namespace.macro_names).any(|name| {
            matches!(
                name.as_str(),
                "Clone" | "Copy" | "Debug" | "Default" | "Eq" | "PartialEq" | "Ord" | "PartialOrd" | "Hash"
            )
        });
        let _entry = self
            .scopes
            .entry(module.clone())
            .and_modify(|value| *value = None)
            .or_insert(Some(namespace));
    }

    fn absorb_at(&mut self, other: &Self, prefix: &[String]) {
        for (module, namespace) in &other.scopes {
            let path = prefix.iter().chain(module).cloned().collect();
            let _entry = self
                .scopes
                .entry(path)
                .and_modify(|value| *value = None)
                .or_insert_with(|| namespace.clone());
        }
    }

    fn transparent(&self, module: &[String], opaque: impl Fn(&Namespace) -> bool) -> bool {
        let Some(Some(current)) = self.scopes.get(module) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        if current.wildcard {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let mut names = HashSet::default();
        let mut harmless = HashSet::default();
        for length in 0..=module.len() {
            let Some(Some(namespace)) = self.scopes.get(&module[..length]) else {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            };
            if opaque(namespace) || namespace.std_shadow {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            names.extend(namespace.macro_names.iter().cloned());
            for name in &namespace.macro_names {
                let _old = harmless.remove(name);
            }
            harmless.extend(namespace.harmless.iter().cloned());
            for call in &namespace.calls {
                let local = call.len() == 1 && harmless.contains(&call[0]);
                let standard = call.as_slice() == ["std", "thread_local"]
                    || call.as_slice() == ["thread_local"]
                        && !names.contains("thread_local")
                        && !namespace.wildcard
                        && namespace
                            .imports
                            .get("thread_local")
                            .is_none_or(|path| path.as_slice() == ["std", "thread_local"]);
                if !local && !standard {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                }
            }
        }
        true
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent uncertainty flags reject proofs rather than encode machine states"
)]
struct Namespace {
    opaque: bool,
    wildcard: bool,
    std_shadow: bool,
    macro_names: HashSet<String>,
    calls: Vec<Vec<String>>,
    harmless: HashSet<String>,
    generated_modules: HashSet<String>,
    imports: HashMap<String, Vec<String>>,
    anonymous: Vec<Vec<String>>,
    functions: HashMap<Vec<String>, Option<Arc<str>>>,
    char_constructors: HashSet<Vec<String>>,
    functions_exhausted: bool,
    nominal: HashSet<String>,
    records: HashSet<String>,
    values: HashSet<String>,
    traits: bool,
    trait_names: HashSet<String>,
    trait_methods: HashMap<String, Option<HashSet<String>>>,
    serde: Vec<Vec<Vec<String>>>,
    serde_nominal: HashSet<String>,
}

impl Namespace {
    fn function(&mut self, path: Vec<String>, tokens: &proc_macro2::TokenStream, bytes: usize) {
        // Count bounds retained bodies; text size bounds each render/cache entry. Both are
        // conservative defaults, with source size checked before rendering and text size afterward.
        if self.functions_exhausted {
            return;
        }
        if self.functions.len() == 256 {
            self.functions_exhausted = true;
            self.functions.clear();
            return;
        }
        let syntax = (bytes <= 16384)
            .then(|| tokens.to_string())
            .filter(|text| text.len() <= 16384)
            .map(Arc::from);
        let _entry = self.functions.entry(path).and_modify(|value| *value = None).or_insert(syntax);
    }
}

fn item_attributes(item: &Item) -> Option<&[Attribute]> {
    Some(match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => {
            return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
        }
    })
}

pub(super) fn declaration_attributes(attributes: &[Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        inert_meta(&attribute.meta)
            || attribute.path().get_ident().is_some_and(|name| {
                matches!(
                    name.to_string().as_str(),
                    "must_use" | "inline" | "cold" | "track_caller" | "test" | "repr"
                ) || name == "derive"
                    && attribute
                        .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                        .is_ok_and(|paths| {
                            paths.iter().all(|path| {
                                path.get_ident().is_some_and(|name| {
                                    matches!(
                                        name.to_string().as_str(),
                                        "Clone" | "Copy" | "Debug" | "Default" | "Eq" | "PartialEq" | "Ord" | "PartialOrd" | "Hash"
                                    )
                                })
                            })
                        })
            })
    })
}

fn imports(tree: &UseTree, prefix: &mut Vec<String>, namespace: &mut Namespace) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
            imports(&path.tree, prefix, namespace);
            let _parent = prefix.pop();
        }
        UseTree::Name(name) => {
            let mut path = prefix.clone();
            let alias = if name.ident == "self" {
                let Some(alias) = prefix.last() else {
                    namespace.opaque = true;
                    return;
                };
                alias.clone()
            } else {
                path.push(name.ident.to_string());
                name.ident.to_string()
            };
            namespace.std_shadow |= alias == "std";
            namespace.opaque |= namespace.imports.insert(alias, path).is_some();
        }
        UseTree::Rename(rename) => {
            let mut path = prefix.clone();
            if rename.ident != "self" {
                path.push(rename.ident.to_string());
            }
            namespace.std_shadow |= rename.rename == "std";
            if rename.rename == "_" {
                namespace.anonymous.push(path);
            } else {
                namespace.opaque |= namespace.imports.insert(rename.rename.to_string(), path).is_some();
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                imports(item, prefix, namespace);
            }
        }
        UseTree::Glob(_) => namespace.wildcard = true,
    }
}

fn inert_attributes(attributes: &[Attribute]) -> bool {
    attributes.iter().all(|attribute| inert_meta(&attribute.meta))
}

fn inert_meta(meta: &Meta) -> bool {
    let Some(name) = meta.path().get_ident() else {
        return
    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
    false;
    };
    match name.to_string().as_str() {
        "cfg" | "doc" | "allow" | "warn" | "deny" | "forbid" | "expect" | "coverage" => true,
        "cfg_attr" => meta.require_list().is_ok_and(|list| {
            Punctuated::<Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .is_ok_and(|arguments| arguments.len() >= 2 && arguments.iter().skip(1).all(inert_meta))
        }),
        _ => false,
    }
}

fn thread_local_body(mac: &Macro) -> bool {
    if mac.tokens.to_string().len() > 8192 {
        return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
    }
    syn::parse2::<File>(mac.tokens.clone()).is_ok_and(|file| {
        !file.items.is_empty()
            && file.items.iter().all(|item| {
                matches!(item, Item::Static(item) if inert_attributes(&item.attrs)
                    && !item.expr.to_token_stream().to_string().contains('!')
                    && namespace_input(&item.expr))
            })
    })
}

fn namespace_input(expression: &syn::Expr) -> bool {
    struct Input {
        remaining: usize,
        valid: bool,
    }
    #[expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]
    impl<'ast> Visit<'ast> for Input {
        fn visit_expr(&mut self, expression: &'ast syn::Expr) {
            if self.remaining == 0 {
                self.valid = false;
                return;
            }
            self.remaining -= 1;
            visit::visit_expr(self, expression);
        }
        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            self.valid &= inert_meta(&attribute.meta);
        }
    }
    let mut input = Input {
        remaining: 512,
        valid: true,
    };
    input.visit_expr(expression);
    input.valid
}

/// Preserves surrounding name-resolution evidence, not runtime purity of generated functions.
///
/// Identifier-only captures and isolated module bodies prevent arbitrary surrounding declarations.
/// Generated names cannot shadow modeled roots or types; restricting nested expansion, exports,
/// and attributes prevents declarations from escaping that boundary.
fn harmless_definition(mac: &Macro) -> bool {
    let tokens = mac
        .tokens
        .clone()
        .into_iter()
        .take(
            // #[gamma::skip(literal.int_increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
            257,
        )
        .collect::<Vec<_>>();
    if tokens.len() > 256 {
        return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
    }
    let mut cursor = 0;
    let mut arms = 0;
    while cursor < tokens.len() {
        if (
            // #[gamma::skip(logical.or_remove_left, reason = "The later let Some(TokenTree::Group(matcher))=tokens.get(cursor) else{return false} independently rejects the same non-group matcher before identifier/body validation.", tag = "equivalent")]
            !matches!(tokens.get(cursor), Some(TokenTree::Group(_)))
                || !matches!(tokens.get(cursor + 1), Some(TokenTree::Punct(p)) if p.as_char() == '=')
        ) || !matches!(tokens.get(cursor + 2), Some(TokenTree::Punct(p)) if p.as_char() == '>')
        {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let Some(TokenTree::Group(body)) = tokens.get(cursor + 3) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        let Some(TokenTree::Group(matcher)) = tokens.get(cursor) else {
            return
            // #[gamma::skip(literal.bool_flip, reason = "The matcher token was already required to be a Group by the preceding exact-shape guard. The changed second Group let-else fallback is unreachable. Malformed matchers fail the earlier guard.", tag = "equivalent")]
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        if !identifier_captures(matcher.stream()) || !only_modules(body.stream()) {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }

        // #[gamma::skip(assign.add_to_sub, reason = "arms is an unconstrained integer local, hence defaults to i32, not usize. The 256-token cap allows at most 64 complete arms. Decrementing produces -1..-64 without overflow rather than +1..+64; the only observation arms!=0 is identical.", tag = "equivalent")]
        arms +=
        // #[gamma::skip(literal.int_increment, reason = "arms is used only by arms!=0. The outer 256-token cap allows at most 64 complete four-token arms; incrementing by two therefore preserves zero/nonzero without overflow.", tag = "equivalent")]
        1;
        // #[gamma::skip(all, reason = "the arm cursor must advance or the same accepted macro arm is parsed forever")]
        cursor += 4;
        if matches!(tokens.get(cursor), Some(TokenTree::Punct(p)) if matches!(p.as_char(), ';' | ',')) {
            cursor += 1;
        }
    }
    arms != 0
}

fn identifier_captures(stream: proc_macro2::TokenStream) -> bool {
    fn bounded(stream: proc_macro2::TokenStream, remaining: &mut usize) -> bool {
        let tokens = stream
            .into_iter()
            .take(
                // #[gamma::skip(expr.increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
                remaining.saturating_add(
                    // #[gamma::skip(literal.int_increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
                    1,
                ),
            )
            .collect::<Vec<_>>();
        if
        // #[gamma::skip(cond.always_false, reason = "identifier_captures exposes only its boolean result. Removing the eager length guard still rejects over-budget input at its unchanged per-token zero guard (or a recursive failure propagated immediately). Its consumed private remaining counter is never observed after rejection.", tag = "equivalent")]
        tokens.len() > *remaining {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        for (index, token) in tokens.iter().enumerate() {
            if *remaining == 0 {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            *remaining -= 1;
            match token {
                TokenTree::Group(group) if !bounded(group.stream(), remaining) => {
                    return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
                }
                TokenTree::Punct(punctuation)
                    if punctuation.as_char() == ':'
                        && !matches!(tokens.get(index + 1), Some(TokenTree::Ident(name)) if name == "ident") =>
                {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                }
                _ => {}
            }
        }
        true
    }
    bounded(stream, &mut 256)
}

fn only_modules(stream: proc_macro2::TokenStream) -> bool {
    let tokens = stream.into_iter().collect::<Vec<_>>();
    let mut cursor = 0;
    while cursor < tokens.len() {
        while matches!(tokens.get(cursor), Some(TokenTree::Punct(p)) if p.as_char() == '#') {
            let Some(TokenTree::Group(attribute)) = tokens.get(cursor + 1) else {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            };
            if attribute.delimiter() != Delimiter::Bracket || !syn::parse2::<Meta>(attribute.stream()).is_ok_and(|meta| inert_meta(&meta)) {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            // #[gamma::skip(all, reason = "the attribute cursor must advance past each accepted attribute")]
            cursor += 2;
        }
        if matches!(tokens.get(cursor), Some(TokenTree::Ident(name)) if name == "pub") {
            cursor += 1;
        }
        if !matches!(tokens.get(cursor), Some(TokenTree::Ident(name)) if name == "mod") {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let Some(TokenTree::Ident(name)) = tokens.get(cursor + 1) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        if matches!(
            name.to_string().as_str(),
            "std"
                | "core"
                | "alloc"
                | "Vec"
                | "String"
                | "Option"
                | "Some"
                | "None"
                | "str"
                | "char"
                | "bool"
                | "usize"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "f32"
                | "f64"
        ) {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        let Some(TokenTree::Group(body)) = tokens.get(cursor + 2) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };
        if body.delimiter() != Delimiter::Brace || !safe_module_body(body.stream(), &mut 256) {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        // #[gamma::skip(all, reason = "the module cursor must advance past each accepted module declaration")]
        cursor += 3;
    }
    true
}

fn safe_module_body(stream: proc_macro2::TokenStream, remaining: &mut usize) -> bool {
    let tokens = stream
        .into_iter()
        .take(
            // #[gamma::skip(expr.increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
            remaining.saturating_add(
                // #[gamma::skip(literal.int_increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
                1,
            ),
        )
        .collect::<Vec<_>>();
    if tokens.len() > *remaining {
        return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
    }
    for (index, token) in tokens.iter().enumerate() {
        if *remaining == 0 {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        *remaining -= 1;
        match token {
            TokenTree::Punct(punctuation) if punctuation.as_char() == '!' => {
                return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
            }
            TokenTree::Punct(punctuation) if punctuation.as_char() == '#' => {
                let Some(TokenTree::Group(attribute)) = tokens.get(index + 1) else {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                };
                if attribute.delimiter() != Delimiter::Bracket
                    || !syn::parse2::<Meta>(attribute.stream()).is_ok_and(|meta| inert_meta(&meta))
                {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                }
            }
            TokenTree::Ident(name) if name == "macro_export" || name == "macro_use" => {
                return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
            }
            TokenTree::Group(group) if !safe_module_body(group.stream(), remaining) => {
                return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
            }
            _ => {}
        }
    }
    true
}

fn potentially_active(_: &CfgSet, attributes: &[Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        !attribute.path().is_ident("cfg")
            || !matches!(attribute.parse_args::<Meta>(), Ok(Meta::List(list)) if list.path.is_ident("any") && list.tokens.is_empty())
    })
}

fn cfg_invariant(attributes: &[Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        !attribute.path().is_ident("cfg")
            && (!attribute.path().is_ident("cfg_attr")
                || attribute
                    .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                    .is_ok_and(|parts| parts.iter().skip(1).all(|part| part.path().is_ident("coverage"))))
    })
}

fn direct_alias(namespace: &Namespace, alias: &str) -> bool {
    !namespace.nominal.contains(alias)
        && !namespace.generated_modules.contains(alias)
        && namespace.imports.get(alias).is_none_or(|path| path.as_slice() == [alias])
}

fn generated_modules(mac: &Macro) -> Option<HashSet<String>> {
    fn collect(stream: proc_macro2::TokenStream, names: &mut HashSet<String>, remaining: &mut usize) -> Option<()> {
        let tokens = stream
            .into_iter()
            .take(
                // #[gamma::skip(expr.increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
                remaining.saturating_add(
                    // #[gamma::skip(literal.int_increment, reason = "The take limit is already remaining+1 (or 257 for a budget of 256). Reading one more token still triggers the same immediate len>remaining rejection; otherwise the complete same accepted stream is read. These budgets start at <=1024 and only decrease, so the additional increment cannot overflow.", tag = "equivalent")]
                    1,
                ),
            )
            .collect::<Vec<_>>();
        if
        // #[gamma::skip(cond.always_false, reason = "generated_modules still spends each token through remaining.checked_sub(1)? after the removed eager length check. Any over-budget token stream returns None; private partially collected names are discarded. The old 1025-token case fails later in both versions.", tag = "equivalent")]
        tokens.len() > *remaining {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        for (index, token) in tokens.iter().enumerate() {
            *remaining = remaining.checked_sub(1)?;
            if let TokenTree::Ident(keyword) = token
                && keyword == "mod"
                && let Some(TokenTree::Ident(name)) = tokens.get(index + 1)
            {
                let _new = names.insert(name.to_string());
            }
            if let TokenTree::Group(group) = token {
                collect(group.stream(), names, remaining)?;
            }
        }
        Some(())
    }
    let mut names = HashSet::default();
    collect(mac.tokens.clone(), &mut names, &mut 1024)?;
    Some(names)
}

fn local_path(module: &[String], path: &[String]) -> Option<Vec<String>> {
    let mut result = module.to_vec();
    let mut path = path;
    match path.first()?.as_str() {
        "crate" => {
            result.clear();
            path = &path[1..];
        }

        "self" => path = &path[1..],
        "super" => {
            while path.first().is_some_and(|name| name == "super") {
                let _parent = result.pop()?;
                path = &path[1..];
            }
        }
        _ => {}
    }
    result.extend_from_slice(path);
    Some(result)
}

/// Derive inputs must be bounded inert data: macros, computed expressions, or opaque attributes
/// could introduce names or override the dependency contract relied on by namespace transparency.
fn serde_input(item: &Item) -> bool {
    struct Input {
        valid: bool,
        remaining: usize,
    }
    #[expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]
    impl<'ast> Visit<'ast> for Input {
        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            if attribute.path().is_ident("doc") {
                return;
            }
            if self.remaining ==
            // #[gamma::skip(literal.int_increment, reason = "The final serde_input predicate requires remaining!=0. Moving either zero guard to remaining==1 only rejects a node that would consume the last unit and therefore already fail that final predicate; smaller accepted inputs and larger rejected inputs are unchanged.", tag = "equivalent")]
            0
            {
                // #[gamma::skip(stmt.delete_assign, reason = "These assignments occur only inside remaining==0 branches that immediately return. The final independent remaining!=0 conjunct already forces false regardless of valid, so deleting the false assignment or changing it to true cannot admit the input.", tag = "equivalent")]
                self.valid =
                // #[gamma::skip(literal.bool_flip, reason = "These assignments occur only inside remaining==0 branches that immediately return. The final independent remaining!=0 conjunct already forces false regardless of valid, so deleting the false assignment or changing it to true cannot admit the input.", tag = "equivalent")]
                false;
                return;
            }
            self.remaining -= 1;
            if attribute.path().is_ident("serde") {
                let text = attribute.meta.to_token_stream().to_string();
                self.valid &= !text.contains("crate")
                    && !text.contains(
                        // #[gamma::skip(collection.reverse_array, reason = "These arrays feed pure membership/all predicates without mutation, so reversing their iteration order does not change the boolean result or retained state.", tag = "equivalent")]
                        ['!', '$', '#', '{', '}', '<', '>'],
                    );
            } else {
                self.valid &= declaration_attributes(core::slice::from_ref(attribute))
                    || attribute.path().is_ident("derive")
                    || attribute.path().is_ident("default");
            }
        }
        fn visit_type(&mut self, ty: &'ast syn::Type) {
            if self.remaining ==
            // #[gamma::skip(literal.int_increment, reason = "The final serde_input predicate requires remaining!=0. Moving either zero guard to remaining==1 only rejects a node that would consume the last unit and therefore already fail that final predicate; smaller accepted inputs and larger rejected inputs are unchanged.", tag = "equivalent")]
            0
            {
                // #[gamma::skip(stmt.delete_assign, reason = "These assignments occur only inside remaining==0 branches that immediately return. The final independent remaining!=0 conjunct already forces false regardless of valid, so deleting the false assignment or changing it to true cannot admit the input.", tag = "equivalent")]
                self.valid =
                // #[gamma::skip(literal.bool_flip, reason = "These assignments occur only inside remaining==0 branches that immediately return. The final independent remaining!=0 conjunct already forces false regardless of valid, so deleting the false assignment or changing it to true cannot admit the input.", tag = "equivalent")]
                false;
                return;
            }
            self.remaining -= 1;
            visit::visit_type(self, ty);
        }
        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.valid = false;
        }
        fn visit_expr(&mut self, expression: &'ast syn::Expr) {
            if !matches!(expression, syn::Expr::Lit(_)) {
                self.valid = false;
            }
        }
    }
    if item.span().byte_range().len() > 16384 {
        return
        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
        false;
    }
    let mut input = Input {
        valid: true,
        remaining: 512,
    };
    input.visit_item(item);
    input.valid && input.remaining != 0
}

/// Retains only recognizable derives without crate redirection or expansion-bearing options.
/// Their spellings still require exact dependency resolution before the namespace is trusted.
fn serde_attributes(attributes: &[Attribute]) -> Option<Vec<Vec<String>>> {
    let mut derives = Vec::new();
    for attribute in attributes {
        if attribute.path().is_ident("derive") {
            let paths = attribute
                .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                .ok()?;
            for path in paths {
                if path.get_ident().is_some_and(|name| {
                    matches!(
                        name.to_string().as_str(),
                        "Clone" | "Copy" | "Debug" | "Default" | "Eq" | "PartialEq" | "Ord" | "PartialOrd" | "Hash"
                    )
                }) {
                    continue;
                }
                if path.segments.len() > 2
                    || !matches!(path.segments.last()?.ident.to_string().as_str(), "Serialize" | "Deserialize")
                    || path.leading_colon.is_some()
                {
                    return
                    // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                    None;
                }
                derives.push(path.segments.iter().map(|part| part.ident.to_string()).collect());
            }
        } else if attribute.path().is_ident("serde") {
            let text = attribute.meta.to_token_stream().to_string();
            if text.contains("crate")
                || text.contains(
                    // #[gamma::skip(collection.reverse_array, reason = "These arrays feed pure membership/all predicates without mutation, so reversing their iteration order does not change the boolean result or retained state.", tag = "equivalent")]
                    ['!', '$', '#', '{', '}', '<', '>'],
                )
            {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
        } else if !declaration_attributes(core::slice::from_ref(attribute)) {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
    }
    (!derives.is_empty()).then_some(derives)
}

fn nominal_attributes(attributes: &[Attribute]) -> bool {
    attributes
        .iter()
        .all(|attribute| attribute.path().is_ident("derive") || declaration_attributes(core::slice::from_ref(attribute)))
}

/// Rejects extension methods with spellings used by the resource and grouping interpreters.
/// New modeled operations must join this conflict set so local traits cannot rebind their meaning.
fn nonconflicting_trait(methods: Option<&HashSet<String>>) -> bool {
    methods.is_some_and(|methods| {
        methods.iter().all(|method| {
            !matches!(
                method.as_str(),
                "len"
                    | "max"
                    | "min"
                    | "get"
                    | "chars"
                    | "next"
                    | "by_ref"
                    | "push"
                    | "width"
                    | "iter"
                    | "position"
                    | "map_or"
                    | "clone"
                    | "to_vec"
                    | "sort_by"
                    | "dedup"
                    | "collect"
                    | "filter"
                    | "filter_map"
                    | "map"
                    | "copied"
                    | "sort_unstable"
                    | "saturating_mul"
                    | "saturating_sub"
                    | "div_ceil"
            ) && !matches!(
                method.as_str(),
                "insert"
                    | "remove"
                    | "pop"
                    | "last"
                    | "last_mut"
                    | "is_empty"
                    | "extend"
                    | "clear"
                    | "truncate"
                    | "retain"
                    | "peek"
                    | "peek_mut"
            ) && !matches!(
                method.as_str(),
                "expect" | "unwrap_or" | "len_utf8" | "as_str" | "saturating_add" | "chunks" | "clone_from" | "contains"
            )
        })
    })
}

fn import_path(modules: &Modules, module: &[String], path: &[String]) -> Option<Vec<String>> {
    if matches!(path.first().map(String::as_str), Some("crate" | "self" | "super")) {
        return local_path(module, path);
    }
    let mut child = module.to_vec();
    child.push(path.first()?.clone());
    modules
        .scopes
        .contains_key(&child)
        .then(|| module.iter().chain(path).cloned().collect())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "resource_namespaces/coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    fn namespaces(source: &str) -> Namespaces {
        Namespaces::of(
            &syn::parse_file(source).expect("checked-in Rust fixture parses"),
            &CfgSet::unconditional(),
        )
    }

    fn attributes(source: &str) -> Vec<Attribute> {
        syn::parse_str::<syn::ItemStruct>(source)
            .expect("checked-in struct fixture parses")
            .attrs
    }

    fn macro_item(source: &str) -> Macro {
        syn::parse_str::<syn::ItemMacro>(source)
            .expect("checked-in macro fixture parses")
            .mac
    }

    fn tokens(source: &str) -> proc_macro2::TokenStream {
        source.parse().expect("checked-in token fixture has balanced delimiters")
    }

    fn dependency(alias: &str, package: &str, version: &str) -> ResourceDependency {
        ResourceDependency {
            crate_root: String::new(),
            import_name: alias.to_owned(),
            package_id: "registry package identity".to_owned(),
            package_name: package.to_owned(),
            version: version.to_owned(),
            source: "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
            features: Vec::new(),
            registry_closure: true,
            library_root: None,
        }
    }

    #[test]
    fn declaration_attributes_distinguish_builtin_derives_from_expanding_attributes() {
        for source in [
            "struct S;",
            "#[must_use] #[inline] #[cold] #[track_caller] #[test] #[repr(C)] struct S;",
            "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)] struct S;",
            "#[doc = \"help\"] #[allow(dead_code)] #[warn(unused)] #[deny(unsafe_code)] #[forbid(unused)] #[expect(dead_code)] #[coverage(off)] struct S;",
            "#[cfg_attr(unix, allow(dead_code), cfg_attr(windows, doc = \"help\"))] struct S;",
        ] {
            assert!(declaration_attributes(&attributes(source)), "{source}");
        }
        for source in [
            "#[custom] struct S;",
            "#[derive(Custom)] struct S;",
            "#[derive(Clone, custom::Debug)] struct S;",
            "#[derive(Clone, =)] struct S;",
            "#[custom::allow(dead_code)] struct S;",
            "#[cfg_attr(unix)] struct S;",
            "#[cfg_attr(unix, =)] struct S;",
            "#[cfg_attr] struct S;",
            "#[cfg_attr(unix, allow(dead_code), custom)] struct S;",
        ] {
            assert!(!declaration_attributes(&attributes(source)), "{source}");
        }
    }

    #[test]
    fn nominal_derives_do_not_make_declaration_attributes_inert() {
        let attrs = attributes("#[derive(Custom)] struct S;");
        assert!(nominal_attributes(&attrs));
        assert!(!declaration_attributes(&attrs));
        assert!(!inert_attributes(&attrs));
        assert!(!nominal_attributes(&attributes("#[custom] struct S;")));
        assert!(!inert_attributes(&attributes("#[inline] struct S;")));
    }

    #[test]
    fn only_definitely_empty_any_cfg_removes_namespace_declarations() {
        let cfg = CfgSet::parse("unix");
        for (source, active) in [
            ("struct S;", true),
            ("#[cfg(any())] struct S;", false),
            ("#[cfg(unix)] struct S;", true),
            ("#[cfg(windows)] struct S;", true),
            ("#[cfg(any(windows))] struct S;", true),
            ("#[cfg(all())] struct S;", true),
            ("#[cfg(\"unknown\")] struct S;", true),
            ("#[cfg_attr(unix, cfg(any()))] struct S;", true),
            ("#[cfg(unix)] #[cfg(any())] struct S;", false),
        ] {
            assert_eq!(potentially_active(&cfg, &attributes(source)), active, "{source}");
        }
    }

    #[test]
    fn function_cfg_invariance_only_accepts_coverage_cfg_attr_payloads() {
        for (source, invariant) in [
            ("struct S;", true),
            ("#[allow(dead_code)] struct S;", true),
            ("#[cfg(unix)] struct S;", false),
            ("#[cfg_attr(unix, coverage(off))] struct S;", true),
            ("#[cfg_attr(unix, coverage(off), coverage(on))] struct S;", true),
            ("#[cfg_attr(unix, coverage(off), inline)] struct S;", false),
            ("#[cfg_attr(unix, cfg(any()))] struct S;", false),
            ("#[cfg_attr(unix, =)] struct S;", false),
        ] {
            assert_eq!(cfg_invariant(&attributes(source)), invariant, "{source}");
        }
    }

    #[test]
    fn grouped_imports_preserve_self_renames_anonymous_paths_and_sibling_prefixes() {
        let ns = namespaces("use crate::outer::{self, A, nested::{B as C, self as N}, Trait as _, D};");
        let scope = ns.local.scopes[&Vec::new()].as_ref().expect("root scope exists");
        assert_eq!(scope.imports.len(), 5);
        for (alias, target) in [
            ("outer", path(&["crate", "outer"])),
            ("A", path(&["crate", "outer", "A"])),
            ("C", path(&["crate", "outer", "nested", "B"])),
            ("N", path(&["crate", "outer", "nested"])),
            ("D", path(&["crate", "outer", "D"])),
        ] {
            assert_eq!(scope.imports.get(alias), Some(&target));
        }
        assert_eq!(scope.anonymous, [path(&["crate", "outer", "Trait"])]);
        assert!(!scope.opaque);
        assert!(!scope.std_shadow);
        assert!(!scope.wildcard);
    }

    #[test]
    fn ambiguous_imports_and_standard_library_shadows_refuse_proofs() {
        for source in [
            "use a::X; use b::X;",
            "use a::X as Y; use b::X as Y;",
            "use self;",
            "use a as std;",
            "use a::std;",
            "mod std {}",
            "mod core {}",
            "mod alloc {}",
            "extern crate other;",
            "extern crate other as std;",
            "use other::Debug;",
            "use other::*;",
            "#![no_implicit_prelude]",
            "#[custom] use other::Thing;",
        ] {
            assert!(!namespaces(source).transparent("", &[]), "{source}");
        }
        for source in ["use a::X; use b::Y;", "mod ordinary {}", "#[cfg(any())] mod std {}"] {
            assert!(namespaces(source).transparent("", &[]), "{source}");
        }
    }

    #[test]
    fn local_paths_resolve_crate_self_and_repeated_super_without_underflow() {
        let base = path(&["outer", "inner"]);
        for (input, expected) in [
            (path(&["Thing"]), Some(path(&["outer", "inner", "Thing"]))),
            (path(&["self", "Thing"]), Some(path(&["outer", "inner", "Thing"]))),
            (path(&["crate", "Thing"]), Some(path(&["Thing"]))),
            (path(&["super", "Thing"]), Some(path(&["outer", "Thing"]))),
            (path(&["super", "super", "Thing"]), Some(path(&["Thing"]))),
            (path(&["super", "super", "super", "Thing"]), None),
            (Vec::new(), None),
            (path(&["self"]), Some(base.clone())),
            (path(&["crate"]), Some(Vec::new())),
        ] {
            assert_eq!(local_path(&base, &input), expected, "{input:?}");
        }
    }

    #[test]
    fn import_paths_require_a_known_child_unless_explicitly_relative() {
        let ns = namespaces("mod outer { mod child {} }");
        let module = path(&["outer"]);
        assert_eq!(
            import_path(&ns.local, &module, &path(&["child", "f"])),
            Some(path(&["outer", "child", "f"]))
        );
        assert_eq!(import_path(&ns.local, &module, &path(&["external", "f"])), None);
        assert_eq!(import_path(&ns.local, &module, &[]), None);
        assert_eq!(import_path(&ns.local, &module, &path(&["crate", "f"])), Some(path(&["f"])));
        assert_eq!(import_path(&ns.local, &module, &path(&["super", "f"])), Some(path(&["f"])));
    }

    #[test]
    fn function_lookup_follows_alias_chains_and_retains_defining_scope() {
        let ns = namespaces(
            "mod implementation { pub fn selected() -> char { 'a' } }
             use crate::implementation as renamed;
             use self::renamed::selected as first;
             use self::first as second;
             mod caller { use crate::second as target; }",
        );
        for (module, input) in [
            (Vec::new(), path(&["implementation", "selected"])),
            (Vec::new(), path(&["renamed", "selected"])),
            (Vec::new(), path(&["second"])),
            (path(&["caller"]), path(&["target"])),
            (path(&["caller"]), path(&["super", "second"])),
        ] {
            let function = ns.function("", &module, &input).expect("alias resolves to the declared function");
            assert_eq!(function.root, "");
            assert_eq!(function.module, path(&["implementation"]));
            let function: syn::ItemFn = syn::parse_str(function.syntax).expect("stored function syntax parses");
            assert_eq!(function.sig.ident, "selected");
            assert_eq!(function.block.to_token_stream().to_string(), "{ 'a' }");
        }
        assert!(ns.function("", &[], &path(&["missing"])).is_none());
        assert!(ns.function("", &[], &[]).is_none());
    }

    #[test]
    fn function_resolution_rejects_cycles_duplicates_and_unknown_namespaces() {
        for source in [
            "use b as a; use a as b;",
            "use self::b as a; use self::a as b;",
            "fn target() {} fn target() {}",
            "#[cfg(unix)] fn target() {}",
            "#[cfg_attr(unix, inline)] fn target() {}",
            "fn target() {} use unknown::*;",
            "#[custom] fn target() {}",
        ] {
            let ns = namespaces(source);
            assert!(ns.function("", &[], &path(&["target"])).is_none(), "{source}");
            assert!(ns.function("", &[], &path(&["a"])).is_none(), "{source}");
        }
        let ns = namespaces("#[cfg_attr(unix, coverage(off))] fn target() {}");
        assert!(ns.function("", &[], &path(&["target"])).is_some());
    }

    #[test]
    fn inherent_functions_and_char_variants_have_distinct_constructor_metadata() {
        let ns = namespaces(
            "struct Wrapper;
             impl Wrapper { fn make() -> char { 'w' } }
             enum Character { One(char), Empty, Pair(char, char), Text(String), Named { value: char } }
             enum Generic<T> { One(char), Other(T) }",
        );
        let inherent = ns
            .function("", &[], &path(&["Wrapper", "make"]))
            .expect("inherent function is available");
        assert!(!inherent.char_constructor);
        let constructor = ns
            .function("", &[], &path(&["Character", "One"]))
            .expect("single char tuple variant has a modeled constructor");
        assert!(constructor.char_constructor);
        assert!(constructor.syntax.contains("character"));
        for input in [
            path(&["Character", "Empty"]),
            path(&["Character", "Pair"]),
            path(&["Character", "Text"]),
            path(&["Character", "Named"]),
            path(&["Generic", "One"]),
        ] {
            assert!(ns.function("", &[], &input).is_none(), "{input:?}");
        }
    }

    #[test]
    fn generic_trait_and_cfg_dependent_impl_functions_are_not_modelled() {
        let ns = namespaces(
            "struct S; trait T { fn foreign(); }
             impl<T> S { fn generic() {} }
             impl T for S { fn foreign() {} }
             #[cfg(unix)] impl S { fn conditional_impl() {} }
             impl S { #[cfg(unix)] fn conditional_method() {} fn stable() {} }",
        );
        for name in ["generic", "foreign", "conditional_impl", "conditional_method"] {
            assert!(ns.function("", &[], &path(&["S", name])).is_none(), "{name}");
        }
        assert!(ns.function("", &[], &path(&["S", "stable"])).is_some());
    }

    #[test]
    fn function_cache_byte_boundary_and_duplicate_entries_fail_closed() {
        let mut namespace = Namespace::default();
        let body = tokens("fn selected() -> char { 's' }");
        namespace.function(path(&["at_limit"]), &body, 16384);
        namespace.function(path(&["too_large"]), &body, 16385);
        assert!(namespace.functions[&path(&["at_limit"])].is_some());
        assert!(namespace.functions[&path(&["too_large"])].is_none());
        namespace.function(path(&["at_limit"]), &body, 1);
        assert!(namespace.functions[&path(&["at_limit"])].is_none());
    }

    #[test]
    fn function_cache_exhaustion_discards_all_partial_proofs_permanently() {
        let mut namespace = Namespace::default();
        let body = tokens("fn selected() {}");
        for index in 0..256 {
            namespace.function(vec![index.to_string()], &body, 16);
        }
        assert!(!namespace.functions_exhausted);
        assert_eq!(namespace.functions.len(), 256);
        namespace.function(path(&["overflow"]), &body, 16);
        assert!(namespace.functions_exhausted);
        assert!(namespace.functions.is_empty());
        namespace.function(path(&["later"]), &body, 16);
        assert!(namespace.functions.is_empty());
    }

    #[test]
    fn records_exclude_enums_aliases_unions_traits_modules_and_duplicate_structs() {
        let ns = namespaces(
            "struct Record { x: char } struct Tuple(char); struct Unit;
             enum E { V } type Alias = Record; union U { x: char } trait T {}
             mod child { struct Nested; }
             struct Duplicate; struct Duplicate;
             struct Replaced; enum Replaced { V }
             struct Imported; use elsewhere::Imported;",
        );
        for name in ["Record", "Tuple", "Unit"] {
            assert!(ns.local_record_for("", &[], name), "{name}");
        }
        for name in [
            "E",
            "Alias",
            "U",
            "T",
            "child",
            "Duplicate",
            "Replaced",
            "Imported",
            "Missing",
            "Nested",
        ] {
            assert!(!ns.local_record_for("", &[], name), "{name}");
        }
        assert!(ns.local_record_for("", &path(&["child"]), "Nested"));
        assert!(!ns.local_record_for("", &path(&["missing"]), "Record"));
    }

    #[test]
    fn primitive_scope_detects_each_kind_of_value_and_type_shadow() {
        assert!(namespaces("struct Ordinary; fn ordinary() {}").primitive_scope_for("", &[]));
        for source in [
            "struct String;",
            "enum Vec { V }",
            "type str = char;",
            "union char { c: char }",
            "trait usize {}",
            "const Some: char = 's';",
            "static None: char = 'n';",
            "fn Option() {}",
            "use other::bool;",
        ] {
            assert!(!namespaces(source).primitive_scope_for("", &[]), "{source}");
        }
        assert!(namespaces("#[cfg(any())] struct String;").primitive_scope_for("", &[]));
        assert!(!namespaces("").primitive_scope_for("", &path(&["absent"])));
    }

    #[test]
    fn harmless_macro_rules_accept_multiple_arms_but_reject_expanding_bodies() {
        for source in [
            "macro_rules! safe { () => {}; }",
            "macro_rules! safe { ($name:ident) => { mod generated { pub fn f() {} } }; }",
            "macro_rules! safe { () => { mod first {} }; ($name:ident) => { pub mod second {} }, }",
            "macro_rules! safe { (($name:ident)) => { #[cfg(unix)] mod generated { #[allow(dead_code)] fn f() {} } }; }",
        ] {
            let mac = macro_item(source);
            assert!(harmless_definition(&mac), "{source}");
            assert!(generated_modules(&mac).is_some(), "{source}");
        }
        for source in [
            "macro_rules! unsafe_macro {}",
            "macro_rules! unsafe_macro { () -> {} }",
            "macro_rules! unsafe_macro { () => value }",
            "macro_rules! unsafe_macro { ($value:expr) => { mod generated {} } }",
            "macro_rules! unsafe_macro { () => { fn injected() {} } }",
            "macro_rules! unsafe_macro { () => { mod generated { invoke!(); } } }",
            "macro_rules! unsafe_macro { () => { #[custom] mod generated {} } }",
        ] {
            assert!(!harmless_definition(&macro_item(source)), "{source}");
        }
    }

    #[test]
    fn macro_identifier_capture_validation_recurses_into_groups() {
        for source in ["", "$name:ident", "($left:ident, [$right:ident])", "$($name:ident),*"] {
            assert!(identifier_captures(tokens(source)), "{source}");
        }
        for source in ["$name:expr", "($name:ident, [$other:tt])", "$name:", "$name:ident $other:path"] {
            assert!(!identifier_captures(tokens(source)), "{source}");
        }
    }

    #[test]
    fn macro_module_validation_rejects_every_primitive_and_prelude_shadow() {
        for source in [
            "mod std {}",
            "mod core {}",
            "mod alloc {}",
            "mod Vec {}",
            "mod String {}",
            "mod Option {}",
            "mod Some {}",
            "mod None {}",
            "mod str {}",
            "mod char {}",
            "mod bool {}",
            "mod usize {}",
            "mod isize {}",
            "mod u8 {}",
            "mod u16 {}",
            "mod u32 {}",
            "mod u64 {}",
            "mod u128 {}",
            "mod i8 {}",
            "mod i16 {}",
            "mod i32 {}",
            "mod i64 {}",
            "mod i128 {}",
            "mod f32 {}",
            "mod f64 {}",
        ] {
            assert!(!only_modules(tokens(source)), "{source}");
        }
        for source in [
            "",
            "mod ordinary {}",
            "pub mod first {} mod second {}",
            "#[doc = \"help\"] mod ordinary {}",
        ] {
            assert!(only_modules(tokens(source)), "{source}");
        }
        for source in [
            "mod",
            "mod 1 {}",
            "mod ordinary;",
            "mod ordinary ()",
            "pub(crate) mod ordinary {}",
            "# mod ordinary {}",
            "#(allow(dead_code)) mod ordinary {}",
            "#[custom] mod ordinary {}",
            "mod ordinary {} fn injected() {}",
            "mod ordinary { macro_export }",
            "mod ordinary { macro_use }",
        ] {
            assert!(!only_modules(tokens(source)), "{source}");
        }
    }

    #[test]
    fn safe_module_body_tracks_nested_tokens_attributes_and_budget_exactly() {
        let stream = tokens("fn f() { let x = 1; }");
        assert!(safe_module_body(stream.clone(), &mut 32));
        assert!(!safe_module_body(stream, &mut 1));
        assert!(safe_module_body(tokens(""), &mut 0));
        assert!(safe_module_body(tokens("value"), &mut 1));
        assert!(!safe_module_body(tokens("value"), &mut 0));
        for source in [
            "fn f() { nested!(); }",
            "#[custom] fn f() {}",
            "#(doc) fn f() {}",
            "#",
            "macro_export",
            "macro_use",
        ] {
            assert!(!safe_module_body(tokens(source), &mut 256), "{source}");
        }
        assert!(safe_module_body(tokens("#[allow(dead_code)] fn f() {}"), &mut 256));
    }

    #[test]
    fn generated_module_names_include_nested_and_later_arms_only() {
        let mac = macro_item("macro_rules! safe { () => { mod first { mod nested {} } }; ($name:ident) => { mod second {} }; }");
        assert_eq!(
            generated_modules(&mac),
            Some(path(&["first", "nested", "second"]).into_iter().collect())
        );
        assert_eq!(
            generated_modules(&macro_item("macro_rules! empty { () => {} }")),
            Some(HashSet::default())
        );
    }

    #[test]
    fn macro_visibility_obeys_ancestor_shadowing_and_call_qualification() {
        for (source, module, expected) in [
            ("macro_rules! safe { () => {} } safe!();", Vec::new(), true),
            ("macro_rules! safe { () => {} } mod child { safe!(); }", path(&["child"]), true),
            (
                "macro_rules! safe { () => {} } mod child { macro_rules! safe { () => { fn injected() {} } } safe!(); }",
                path(&["child"]),
                false,
            ),
            ("macro_rules! safe { () => {} } crate::safe!();", Vec::new(), false),
            ("unknown!();", Vec::new(), false),
            ("macro_rules! safe { () => {} } macro_rules! safe { () => {} }", Vec::new(), false),
        ] {
            assert_eq!(namespaces(source).transparent("", &module), expected, "{source}");
        }
    }

    #[test]
    fn thread_local_bodies_require_inert_static_initializers_without_macros() {
        for source in [
            "thread_local! { static KEY: char = 'a'; }",
            "thread_local! { #[allow(dead_code)] static FIRST: char = 'a'; static SECOND: usize = 2; }",
            "thread_local! { static KEY: usize = const { 3 }; }",
        ] {
            assert!(thread_local_body(&macro_item(source)), "{source}");
            assert!(namespaces(source).transparent("", &[]), "{source}");
        }
        for source in [
            "thread_local! {}",
            "thread_local! { const KEY: char = 'a'; }",
            "thread_local! { static KEY: char = make!(); }",
            "thread_local! { #[custom] static KEY: char = 'a'; }",
            "thread_local! { static KEY: usize = { #[custom] let value = 1; value }; }",
            "thread_local! { static KEY: char = 'a'; fn injected() {} }",
            "thread_local! { invalid syntax }",
        ] {
            assert!(!thread_local_body(&macro_item(source)), "{source}");
        }
    }

    #[test]
    fn thread_local_macro_recognition_checks_import_and_macro_shadowing() {
        for (source, expected) in [
            ("std::thread_local! { static KEY: char = 'a'; }", true),
            ("use std::thread_local; thread_local! { static KEY: char = 'a'; }", true),
            ("use custom::thread_local; thread_local! { static KEY: char = 'a'; }", false),
            (
                "macro_rules! thread_local { () => { fn injected() {} } } thread_local! { static KEY: char = 'a'; }",
                false,
            ),
            ("custom::thread_local! { static KEY: char = 'a'; }", false),
            ("std::thread_local! { static KEY: char = unknown!(); }", false),
        ] {
            assert_eq!(namespaces(source).transparent("", &[]), expected, "{source}");
        }
    }

    #[test]
    fn standard_macro_proofs_check_both_current_and_ancestor_scopes() {
        let clear = namespaces("mod child {}");
        assert!(clear.standard_macro("", &[], "vec"));
        assert!(clear.standard_macro("", &path(&["child"]), "vec"));
        for source in [
            "macro_rules! vec { () => {} } mod child {}",
            "use other::vec; mod child {}",
            "use other::*; mod child {}",
            "mod child { macro_rules! vec { () => {} } }",
            "mod child { use other::vec; }",
        ] {
            assert!(!namespaces(source).standard_macro("", &path(&["child"]), "vec"), "{source}");
        }
        assert!(!clear.standard_macro("", &path(&["absent"]), "vec"));
    }

    #[test]
    fn trait_method_closure_rejects_conflicting_and_unbounded_traits() {
        for source in [
            "trait Local { fn ordinary(&self); }",
            "trait Empty {}",
            "mod inner { pub struct Record; } use inner::Record;",
            "mod inner {} use self::inner;",
            "mod inner { pub fn f() {} } use inner::f;",
            "mod inner { macro_rules! m { () => {} } } use inner::m;",
            "use std::fmt::Formatter;",
        ] {
            assert!(namespaces(source).closed_methods_for("", &[]), "{source}");
        }
        for source in [
            "trait Local { fn len(&self); }",
            "trait Local: Other { fn ordinary(&self); }",
            "trait Local { type Associated; }",
            "trait Local { const VALUE: usize; }",
            "trait Local { #[custom] fn ordinary(&self); }",
            "trait Local {} trait Local {}",
            "use missing::Trait;",
            "use missing::Trait as _;",
            "mod inner { pub trait T { fn width(&self); } } use inner::T;",
            "use b as a; use a as b;",
        ] {
            assert!(!namespaces(source).closed_methods_for("", &[]), "{source}");
        }
    }

    #[test]
    fn every_recognized_conflicting_trait_method_blocks_closed_method_proofs() {
        for method in [
            "len",
            "max",
            "min",
            "get",
            "chars",
            "next",
            "by_ref",
            "push",
            "width",
            "iter",
            "position",
            "map_or",
            "clone",
            "to_vec",
            "sort_by",
            "dedup",
            "collect",
            "filter",
            "filter_map",
            "map",
            "copied",
            "sort_unstable",
            "saturating_mul",
            "saturating_sub",
            "div_ceil",
            "insert",
            "remove",
            "pop",
            "last",
            "last_mut",
            "is_empty",
            "extend",
            "clear",
            "truncate",
            "retain",
            "peek",
            "peek_mut",
            "expect",
            "unwrap_or",
            "len_utf8",
            "as_str",
            "saturating_add",
            "chunks",
            "clone_from",
            "contains",
        ] {
            let methods = path(&["ordinary", method]).into_iter().collect();
            assert!(!nonconflicting_trait(Some(&methods)), "{method}");
        }
        assert!(nonconflicting_trait(Some(&path(&["ordinary", "length"]).into_iter().collect())));
        assert!(nonconflicting_trait(Some(&HashSet::default())));
        assert!(!nonconflicting_trait(None));
    }

    #[test]
    fn binding_and_aliases_preserve_physical_root_prefix_and_inline_context() {
        let mut root = namespaces("mod outer;");
        root.label("src\\lib.rs");
        let mut child = namespaces("mod nested { pub struct Record; pub fn selected() -> char { 'n' } }");
        child.label("src\\outer.rs");
        root.absorb(child);
        root.bind(&[
            ("src\\lib.rs".to_owned(), "src\\lib.rs".to_owned(), Vec::new()),
            ("src\\outer.rs".to_owned(), "src\\lib.rs".to_owned(), path(&["outer"])),
        ]);
        assert!(!root.transparent("src\\outer.rs", &path(&["missing"])));
        root.alias("src\\outer.rs", "display\\outer.rs");
        assert!(root.transparent("display/outer.rs", &path(&["nested"])));
        assert!(root.closed_methods_for("display/outer.rs", &path(&["nested"])));
        assert!(root.primitive_scope_for("display/outer.rs", &path(&["nested"])));
        let function = root
            .function("display/outer.rs", &path(&["nested"]), &path(&["selected"]))
            .expect("alias retains nested defining context");
        assert_eq!(function.root, "src/lib.rs");
        assert_eq!(function.module, path(&["outer", "nested"]));
        assert!(root.local_record_for("display/outer.rs", &path(&["nested"]), "Record"));
        assert!(!root.local_record_for("display/outer.rs", &[], "Record"));
        assert!(root.function("unknown", &[], &path(&["selected"])).is_none());
        assert!(!root.primitive_scope_for("unknown", &[]));
        assert!(!root.closed_methods_for("unknown", &[]));
        assert!(!root.width_contract_for("unknown", &[]));
    }

    #[test]
    fn duplicate_bindings_aliases_and_absorbed_labels_invalidate_ambiguous_contexts() {
        let mut ns = namespaces("fn selected() {}");
        ns.label("source");
        ns.bind(&[("source".to_owned(), "root".to_owned(), Vec::new())]);
        ns.alias("source", "alias");
        assert!(ns.function("alias", &[], &path(&["selected"])).is_some());
        ns.alias("source", "alias");
        assert!(ns.function("alias", &[], &path(&["selected"])).is_none());
        ns.alias("missing", "untouched");
        assert!(!ns.transparent("untouched", &[]));

        let mut ns = namespaces("fn selected() {}");
        ns.label("source");
        ns.bind(&[
            ("source".to_owned(), "root".to_owned(), Vec::new()),
            ("source".to_owned(), "root".to_owned(), Vec::new()),
        ]);
        assert!(!ns.transparent("source", &[]));
        assert!(ns.function("source", &[], &path(&["selected"])).is_none());

        let mut left = namespaces("fn selected() {}");
        left.label("source");
        let mut right = namespaces("fn selected() {}");
        right.label("source");
        left.absorb(right);
        left.bind(&[("source".to_owned(), "root".to_owned(), Vec::new())]);
        assert!(!left.transparent("source", &[]));
    }

    #[test]
    fn extension_accepts_identical_snapshots_but_rejects_conflicting_source_contents() {
        let mut left = namespaces("fn selected() {}");
        left.label("source");
        let mut same = namespaces("fn selected() {}");
        same.label("source");
        left.extend(&same);
        left.bind(&[("source".to_owned(), "root".to_owned(), Vec::new())]);
        assert!(left.function("source", &[], &path(&["selected"])).is_some());

        let mut left = namespaces("fn selected() {}");
        left.label("source");
        let mut different = namespaces("fn different() {}");
        different.label("source");
        left.extend(&different);
        left.bind(&[("source".to_owned(), "root".to_owned(), Vec::new())]);
        assert!(!left.transparent("source", &[]));
    }

    #[test]
    fn dependency_identity_requires_exact_registry_version_closure_and_unique_edge() {
        let valid = dependency("width", "unicode-width", "0.2.2");
        let mut ns = namespaces("");
        ns.dependencies(core::slice::from_ref(&valid));
        assert!(ns.dependency("", "width", "unicode-width", "0.2.2"));
        assert!(!ns.dependency("", "other", "unicode-width", "0.2.2"));
        assert!(!ns.dependency("", "width", "other", "0.2.2"));
        assert!(!ns.dependency("", "width", "unicode-width", "0.2.1"));
        assert!(!ns.dependency("missing", "width", "unicode-width", "0.2.2"));
        for invalid in [
            ResourceDependency {
                package_id: String::new(),
                ..valid.clone()
            },
            ResourceDependency {
                registry_closure: false,
                ..valid.clone()
            },
            ResourceDependency {
                source: "git+https://example.invalid".to_owned(),
                ..valid.clone()
            },
            ResourceDependency {
                source: "registry+https://alternate.invalid".to_owned(),
                ..valid.clone()
            },
        ] {
            let mut ns = namespaces("");
            ns.dependencies(&[invalid]);
            assert!(!ns.dependency("", "width", "unicode-width", "0.2.2"));
        }
        ns.dependencies(&[valid]);
        assert!(!ns.dependency("", "width", "unicode-width", "0.2.2"));
        let mut shadow = namespaces("mod nested { struct width; }");
        shadow.dependencies(&[dependency("width", "unicode-width", "0.2.2")]);
        assert!(!shadow.dependency("", "width", "unicode-width", "0.2.2"));
    }

    #[test]
    fn width_contract_distinguishes_character_string_and_anonymous_trait_imports() {
        for (source, character, string) in [
            ("use width::UnicodeWidthChar;", true, false),
            ("use width::UnicodeWidthStr;", false, true),
            ("use width::{UnicodeWidthChar as _, UnicodeWidthStr as _};", true, true),
            ("use width::UnicodeWidthChar as Width;", true, false),
            ("use width::Other;", false, false),
            ("", false, false),
        ] {
            let mut ns = namespaces(source);
            ns.dependencies(&[dependency("width", "unicode-width", "0.2.2")]);
            assert_eq!(ns.width_contract("", &[], true), character, "{source}");
            assert_eq!(ns.width_contract("", &[], false), string, "{source}");
            assert_eq!(ns.width_contract_for("", &[]), character, "{source}");
        }
        for source in [
            "use fake as width; use width::UnicodeWidthChar;",
            "mod width {} use width::UnicodeWidthChar;",
            "macro_rules! generated { () => { mod width {} } } use width::UnicodeWidthChar;",
        ] {
            let mut ns = namespaces(source);
            ns.dependencies(&[dependency("width", "unicode-width", "0.2.2")]);
            assert!(!ns.width_contract("", &[], true), "{source}");
        }
    }

    #[test]
    fn grouping_imports_require_their_own_pinned_dependency_contracts() {
        for (alias, package, version, imports) in [
            ("paths", "camino", "1.2.5", "use paths::{Utf8Path, Utf8PathBuf};"),
            ("yaml", "saphyr-parser", "0.0.12", "use yaml::{Event, Parser};"),
            ("clock", "tick", "0.7.0", "use clock::{SimpleClock, SystemTimeExt};"),
            (
                "ser",
                "serde",
                "1.0.228",
                "use ser::{Serialize, Serializer, Deserialize, Deserializer}; use ser::de::IgnoredAny;",
            ),
            ("colors", "owo-colors", "4.4.0", "use colors::Style;"),
            ("width", "unicode-width", "0.2.2", "use width::{UnicodeWidthChar, UnicodeWidthStr};"),
        ] {
            let mut ns = namespaces(imports);
            assert!(!ns.closed_methods_for("", &[]), "{imports}");
            ns.dependencies(&[dependency(alias, package, version)]);
            assert!(ns.closed_methods_for("", &[]), "{imports}");
            let mut wrong_version = namespaces(imports);
            wrong_version.dependencies(&[dependency(alias, package, "0.0.0")]);
            assert!(!wrong_version.closed_methods_for("", &[]), "{imports}");
        }
    }

    #[test]
    fn stable_path_types_require_string_or_pinned_camino_without_qualification_ambiguity() {
        let mut ns = namespaces("use paths::Utf8PathBuf as Path;");
        ns.dependencies(&[dependency("paths", "camino", "1.2.5")]);
        for (source, expected) in [
            ("String", true),
            ("paths::Utf8PathBuf", true),
            ("Path", true),
            ("paths::Utf8Path", false),
            ("other::Utf8PathBuf", false),
            ("std::string::String", false),
            ("::String", false),
            ("<T as Trait>::Path", false),
            ("&str", false),
            ("(String,)", false),
        ] {
            let ty = syn::parse_str(source).expect("checked-in type fixture parses");
            assert_eq!(ns.stable_path_for("", &[], &ty), expected, "{source}");
        }
        let mut shadow = namespaces("struct Path; use paths::Utf8PathBuf as Path;");
        shadow.dependencies(&[dependency("paths", "camino", "1.2.5")]);
        assert!(!shadow.stable_path_for("", &[], &syn::parse_quote!(Path)));
        let mut wrong = namespaces("");
        wrong.dependencies(&[dependency("paths", "camino", "1.2.4")]);
        assert!(!wrong.stable_path_for("", &[], &syn::parse_quote!(paths::Utf8PathBuf)));
    }

    #[test]
    fn serde_derive_paths_require_supported_unqualified_or_single_alias_names() {
        for (source, expected) in [
            ("#[derive(Serialize)] struct S;", Some(vec![path(&["Serialize"])])),
            (
                "#[derive(Clone, ser::Serialize, ser::Deserialize)] struct S;",
                Some(vec![path(&["ser", "Serialize"]), path(&["ser", "Deserialize"])]),
            ),
            (
                "#[derive(Serialize)] #[derive(Deserialize)] struct S;",
                Some(vec![path(&["Serialize"]), path(&["Deserialize"])]),
            ),
            ("#[derive(Clone)] struct S;", None),
            ("#[derive(custom::Custom)] struct S;", None),
            ("#[derive(a::b::Serialize)] struct S;", None),
            ("#[derive(::ser::Serialize)] struct S;", None),
            ("#[derive(Serialize, =)] struct S;", None),
            ("#[derive(Serialize)] #[custom] struct S;", None),
            ("#[derive(Serialize)] #[serde(crate = \"other\")] struct S;", None),
        ] {
            assert_eq!(serde_attributes(&attributes(source)), expected, "{source}");
        }
    }

    #[test]
    fn serde_input_rejects_custom_attributes_macros_and_nonliteral_discriminants() {
        for (source, expected) in [
            (
                "#[derive(Serialize)] struct S { #[serde(rename = \"field\")] field: Vec<String> }",
                true,
            ),
            ("#[derive(Deserialize)] enum E { #[default] A = 1, B = 2 }", true),
            ("#[doc = \"crate ! $ # {} <>\"] struct S;", true),
            ("struct S { #[custom] field: char }", false),
            ("struct S { #[serde(crate = \"other\")] field: char }", false),
            ("struct S { field: make_type!() }", false),
            ("enum E { A = 1 + 2 }", false),
            ("enum E { A = VALUE }", false),
        ] {
            let item = syn::parse_str(source).expect("checked-in serde input fixture parses");
            assert_eq!(serde_input(&item), expected, "{source}");
        }
        for source in [
            "#[derive(Serialize)] #[serde(rename = \"!\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \"$\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \"#\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \"{\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \"}\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \"<\")] struct S;",
            "#[derive(Serialize)] #[serde(rename = \">\")] struct S;",
        ] {
            let item = syn::parse_str(source).expect("checked-in serde input fixture parses");
            assert!(!serde_input(&item), "{source}");
            assert!(serde_attributes(&attributes(source)).is_none(), "{source}");
        }
    }

    #[test]
    fn serde_namespace_proofs_require_both_pinned_packages_and_enabled_derive_feature() {
        for source in [
            "#[derive(ser::Serialize, ser::Deserialize)] struct Record { field: String }",
            "use ser::{Serialize, Deserialize}; #[derive(Clone, Serialize, Deserialize)] struct Record { field: String }",
        ] {
            let mut ns = namespaces(source);
            let mut ser = dependency("ser", "serde", "1.0.228");
            ser.features.push("serde_derive".to_owned());
            let derive = dependency("ser::serde_derive", "serde_derive", "1.0.228");
            assert!(!ns.transparent("", &[]), "{source}");
            ns.dependencies(core::slice::from_ref(&ser));
            assert!(!ns.transparent("", &[]), "{source}");
            ns.dependencies(core::slice::from_ref(&derive));
            assert!(ns.transparent("", &[]), "{source}");
            assert!(ns.local_record_for("", &[], "Record"));
            assert!(ns.primitive_scope_for("", &[]));

            let mut no_feature = namespaces(source);
            ser.features.clear();
            no_feature.dependencies(&[ser, derive]);
            assert!(!no_feature.transparent("", &[]), "{source}");
        }
    }

    #[test]
    fn generated_modules_only_poison_matching_dependency_aliases() {
        let mut ns = namespaces("macro_rules! safe { () => { mod generated {} } } safe!();");
        assert!(ns.transparent("", &[]));
        ns.dependencies(&[dependency("other", "other", "1.0.0")]);
        assert!(ns.transparent("", &[]));
        ns.dependencies(&[dependency("generated", "other", "1.0.0")]);
        assert!(!ns.transparent("", &[]));
    }

    #[test]
    fn external_function_aliases_follow_bound_workspace_library_roots() {
        let mut consumer = namespaces("use dep::selected as target;");
        consumer.label("consumer\\lib.rs");
        let mut library = namespaces("pub fn selected() -> char { 'l' }");
        library.label("library\\lib.rs");
        consumer.absorb(library);
        consumer.bind(&[
            ("consumer\\lib.rs".to_owned(), "consumer\\lib.rs".to_owned(), Vec::new()),
            ("library\\lib.rs".to_owned(), "library\\lib.rs".to_owned(), Vec::new()),
        ]);
        let mut edge = dependency("dep", "workspace-library", "0.1.0");
        edge.crate_root = "consumer\\lib.rs".to_owned();
        edge.library_root = Some("library\\lib.rs".to_owned());
        consumer.dependencies(&[edge]);
        let function = consumer
            .function("consumer\\lib.rs", &[], &path(&["target"]))
            .expect("workspace alias resolves using normalized library root");
        assert_eq!(function.root, "library/lib.rs");
        assert!(function.module.is_empty());
        assert!(function.syntax.contains("'l'"));
        assert!(consumer.closed_methods_for("consumer\\lib.rs", &[]));
        assert!(consumer.function("consumer\\lib.rs", &[], &path(&["absent"])).is_none());
    }

    fn workspace_namespaces(consumer: &str, library: &str) -> Namespaces {
        let mut consumer_source = namespaces(consumer);
        consumer_source.label("consumer\\lib.rs");
        let mut dependency_source = namespaces(library);
        dependency_source.label("library\\lib.rs");
        consumer_source.absorb(dependency_source);
        consumer_source.bind(&[
            ("consumer\\lib.rs".into(), "consumer\\lib.rs".into(), Vec::new()),
            ("library\\lib.rs".into(), "library\\lib.rs".into(), Vec::new()),
        ]);
        let mut edge = dependency("dep", "workspace-library", "0.1.0");
        edge.crate_root = "consumer\\lib.rs".into();
        edge.library_root = Some("library\\lib.rs".into());
        consumer_source.dependencies(&[edge]);
        consumer_source
    }

    #[test]
    fn external_targets_require_an_unshadowed_alias_and_an_unambiguous_bound_library() {
        for (consumer, expected) in [
            ("", Some(("library/lib.rs", path(&["nested", "selected"])))),
            ("use dep;", Some(("library/lib.rs", path(&["nested", "selected"])))),
            ("struct dep;", None),
            ("use other as dep;", None),
            ("use dep::nested as dep;", None),
        ] {
            let ns = workspace_namespaces(consumer, "pub mod nested { pub fn selected() {} }");
            assert_eq!(
                ns.external_target("consumer/lib.rs", &[], &path(&["dep", "nested", "selected"])),
                expected,
                "{consumer}"
            );
            assert!(ns.external_target("consumer/lib.rs", &[], &[]).is_none());
            assert!(ns.external_target("unknown", &[], &path(&["dep"])).is_none());
            assert!(ns.external_target("consumer/lib.rs", &path(&["absent"]), &path(&["dep"])).is_none());
        }
        for fault in 0..4 {
            let mut ns = workspace_namespaces("", "pub fn selected() {}");
            let edge = ns
                .dependencies
                .get_mut(&("consumer/lib.rs".into(), "dep".into()))
                .expect("workspace_namespaces registers the consumer dependency");
            match fault {
                0 => *edge = None,
                1 => edge.as_mut().expect("the fresh edge is unambiguous").package_id.clear(),
                2 => edge.as_mut().expect("the fresh edge is unambiguous").library_root = None,
                3 => edge.as_mut().expect("the fresh edge is unambiguous").library_root = Some("absent\\lib.rs".into()),
                _ => unreachable!("the four faults are enumerated by this test"),
            }
            assert!(
                ns.external_target("consumer/lib.rs", &[], &path(&["dep", "selected"])).is_none(),
                "fault {fault}"
            );
        }
    }

    #[test]
    fn workspace_reexports_preserve_suffixes_and_check_traits_in_the_defining_namespace() {
        for (library, expected) in [
            ("pub mod actual { pub struct Record; } pub use crate::actual as exposed;", true),
            ("pub mod actual { pub fn Record() {} } pub use crate::actual as exposed;", true),
            (
                "pub mod actual { pub trait Record { fn ordinary(&self); } } pub use crate::actual as exposed;",
                true,
            ),
            (
                "pub mod actual { pub trait Record { fn len(&self); } } pub use crate::actual as exposed;",
                false,
            ),
            (
                "pub mod actual { pub trait Record: Unknown {} } pub use crate::actual as exposed;",
                false,
            ),
            ("pub mod actual {} pub use crate::actual as exposed;", false),
            (
                "pub mod actual { use unknown::*; pub struct Record; } pub use crate::actual as exposed;",
                false,
            ),
            (
                "#[custom] pub mod actual { pub struct Record; } pub use crate::actual as exposed;",
                false,
            ),
            ("pub use crate::other as exposed; pub use crate::exposed as other;", false),
        ] {
            let ns = workspace_namespaces("use dep::exposed::Record;", library);
            assert_eq!(
                ns.nontrait("consumer/lib.rs", &[], &path(&["dep", "exposed", "Record"])),
                expected,
                "{library}"
            );
            assert_eq!(ns.closed_methods_for("consumer\\lib.rs", &[]), expected, "{library}");
        }
    }

    #[test]
    fn cost_merging_uses_maximum_per_source_and_exhaustion_invalidates_existing_proofs() {
        let mut ns = namespaces("fn selected() {}");
        ns.costs.insert("source".to_owned(), 100);
        let mut other = Namespaces::default();
        other.costs.insert("source".to_owned(), 90);
        other.costs.insert("second".to_owned(), 50);
        ns.costs(&other);
        assert_eq!(ns.costs.get("source"), Some(&100));
        assert_eq!(ns.costs.get("second"), Some(&50));
        other.costs.insert("source".to_owned(), 110);
        ns.costs(&other);
        assert_eq!(ns.costs.get("source"), Some(&110));
        ns.costs.clear();
        ns.costs.insert("limit".to_owned(), 8 * 1024 * 1024);
        ns.limit();
        assert!(ns.function("", &[], &path(&["selected"])).is_some());
        ns.costs.insert("overflow".to_owned(), 1);
        ns.limit();
        assert!(ns.exhausted);
        assert!(!ns.transparent("", &[]));
        assert!(ns.function("", &[], &path(&["selected"])).is_none());
        assert!(ns.costs.is_empty());
        assert!(ns.local.scopes.is_empty());
    }

    #[test]
    fn label_alias_and_layout_length_boundaries_fail_closed() {
        let mut ns = namespaces("fn selected() {}");
        let at_limit = "s".repeat(32768);
        ns.label(&at_limit);
        assert!(!ns.exhausted);
        let too_long = "s".repeat(32769);
        ns.label(&too_long);
        assert!(ns.exhausted);
        assert!(!ns.transparent("", &[]));

        for (source, root, module, valid) in [
            (at_limit.clone(), "root".to_owned(), Vec::new(), true),
            ("source".to_owned(), at_limit.clone(), Vec::new(), true),
            ("source".to_owned(), too_long.clone(), Vec::new(), false),
            (too_long.clone(), "root".to_owned(), Vec::new(), false),
            ("source".to_owned(), "root".to_owned(), vec!["m".repeat(256)], true),
            ("source".to_owned(), "root".to_owned(), vec!["m".repeat(257)], false),
            ("source".to_owned(), "root".to_owned(), vec!["m".to_owned(); 32], true),
            ("source".to_owned(), "root".to_owned(), vec!["m".to_owned(); 33], false),
        ] {
            let mut ns = namespaces("");
            ns.bind(&[(source, root, module)]);
            assert_eq!(!ns.exhausted, valid);
        }
        let mut ns = namespaces("");
        ns.alias(&at_limit, &at_limit);
        assert!(!ns.exhausted);
        ns.alias("source", &too_long);
        assert!(ns.exhausted);
    }

    #[test]
    fn direct_dependency_aliases_reject_local_import_and_generated_module_redirection() {
        for (source, expected) in [
            ("", true),
            ("use dep;", true),
            ("use other as dep;", false),
            ("struct dep;", false),
            ("mod dep {}", false),
            ("macro_rules! generated { () => { mod dep {} } }", false),
            ("use dep::Thing;", true),
        ] {
            let ns = namespaces(source);
            let namespace = ns.local.scopes[&Vec::new()].as_ref().expect("root scope exists");
            assert_eq!(direct_alias(namespace, "dep"), expected, "{source}");
        }
    }

    #[test]
    fn dependency_field_limits_accept_the_boundary_and_reject_one_byte_beyond() {
        for (field, limit) in [
            ("crate_root", 32768),
            ("import_name", 256),
            ("package_name", 256),
            ("package_id", 32768),
            ("source", 1024),
            ("version", 256),
            ("library_root", 32768),
            ("feature", 256),
        ] {
            for (length, accepted) in [(limit, true), (limit + 1, false)] {
                let mut edge = dependency("dep", "package", "1.0.0");
                let value = "x".repeat(length);
                match field {
                    "crate_root" => edge.crate_root = value,
                    "import_name" => edge.import_name = value,
                    "package_name" => edge.package_name = value,
                    "package_id" => edge.package_id = value,
                    "source" => edge.source = value,
                    "version" => edge.version = value,
                    "library_root" => edge.library_root = Some(value),
                    "feature" => edge.features = vec![value],
                    _ => unreachable!("field cases are enumerated by this test"),
                }
                let key = (edge.crate_root.clone(), edge.import_name.clone());
                let mut ns = namespaces("fn selected() {}");
                ns.dependencies(&[edge]);
                assert_eq!(ns.exhausted, !accepted, "{field} length {length}");
                assert_eq!(ns.dependencies.contains_key(&key), accepted, "{field} length {length}");
                assert_eq!(
                    ns.function("", &[], &path(&["selected"])).is_some(),
                    accepted,
                    "{field} length {length}"
                );
            }
        }
    }

    #[test]
    fn dependency_feature_count_and_aggregate_budget_fail_closed() {
        for (count, accepted) in [(256, true), (257, false)] {
            let mut edge = dependency("dep", "package", "1.0.0");
            edge.features = vec!["feature".to_owned(); count];
            let mut ns = namespaces("");
            ns.dependencies(&[edge]);
            assert_eq!(ns.dependency("", "dep", "package", "1.0.0"), accepted);
            assert_eq!(ns.exhausted, !accepted);
        }
        let mut edge = dependency("dep", "package", "1.0.0");
        edge.features = vec!["f".repeat(256); 256];
        let mut ns = namespaces("");
        ns.dependencies(&[dependency("valid", "package", "1.0.0")]);
        assert!(ns.dependency("", "valid", "package", "1.0.0"));
        ns.dependencies(&vec![edge; 128]);
        assert!(ns.exhausted);
        assert!(ns.dependencies.is_empty());
        assert!(!ns.dependency("", "valid", "package", "1.0.0"));
    }

    #[test]
    fn cost_overflow_and_absorbed_exhaustion_never_preserve_local_proofs() {
        let mut ns = namespaces("fn selected() {}");
        ns.costs.insert("first".to_owned(), usize::MAX);
        ns.costs.insert("second".to_owned(), 1);
        ns.limit();
        assert!(ns.exhausted);
        assert!(ns.function("", &[], &path(&["selected"])).is_none());

        let mut exhausted = Namespaces {
            exhausted: true,
            ..Namespaces::default()
        };
        exhausted.label("ignored");
        assert!(exhausted.pending.is_empty());
        let mut ns = namespaces("fn selected() {}");
        ns.absorb(exhausted);
        assert!(ns.exhausted);
        assert!(!ns.transparent("", &[]));
        assert!(ns.function("", &[], &path(&["selected"])).is_none());
    }
}
