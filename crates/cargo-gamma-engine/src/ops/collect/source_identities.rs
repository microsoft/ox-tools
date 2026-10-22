// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-labeled declaration graphs, kept separate for each crate root.

use std::collections::hash_map::Entry;

use super::default_identity::{Identities, Pending};
use crate::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct Sources {
    pending: HashMap<String, Pending>,
    duplicates: HashSet<String>,
    roots: HashMap<String, Identities>,
    contexts: HashMap<String, Option<(String, Vec<String>)>>,
    pub(super) bound: bool,
}

impl Sources {
    pub(super) fn insert(&mut self, source: &str, identities: Pending) {
        let source = source.replace('\\', "/");
        if self.duplicates.contains(&source) {
            return;
        }
        match self.pending.entry(source) {
            Entry::Occupied(entry) => {
                let (source, _old) = entry.remove_entry();
                let _added = self.duplicates.insert(source);
            }
            Entry::Vacant(entry) => {
                let _inserted = entry.insert(identities);
            }
        }
    }

    pub(super) fn absorb(&mut self, other: Self) {
        for source in other.duplicates {
            let _old = self.pending.remove(&source);
            let _added = self.duplicates.insert(source);
        }
        for (source, identities) in other.pending {
            self.insert(&source, identities);
        }
    }

    pub(super) fn bind(&mut self, modules: &[(String, String, Vec<String>)]) {
        self.bound = true;
        let mut roots: HashMap<_, _> = core::mem::take(&mut self.roots)
            .into_iter()
            .map(|(root, identities)| (root, Pending::from(identities)))
            .collect();
        for (source, root, module) in modules {
            let source = source.replace('\\', "/");
            let root = root.replace('\\', "/");
            let Some(identities) = self.pending.get(&source) else { continue };
            roots.entry(root.clone()).or_default().absorb_at(identities, module);
            let _entry = self
                .contexts
                .entry(source)
                .and_modify(|context| *context = None)
                .or_insert_with(|| Some((root.clone(), module.clone())));
        }
        self.roots = roots.into_iter().map(|(root, identities)| (root, identities.finish())).collect();
        self.pending.clear();
    }

    pub(super) fn alias(&mut self, source: &str, alias: &str) {
        if let Some(context) = self.contexts.get(&source.replace('\\', "/")).cloned() {
            let _entry = self
                .contexts
                .entry(alias.replace('\\', "/"))
                .and_modify(|value| *value = None)
                .or_insert(context);
        }
    }

    fn resolve(&self, source: &str, inline: &[String], path: &[String]) -> Option<(&Identities, Vec<String>)> {
        let (root, base) = self.contexts.get(&source.replace('\\', "/"))?.as_ref()?;
        let mut module = base.clone();
        module.extend_from_slice(inline);
        let mut path = path;
        match path.first()?.as_str() {
            "crate" => {
                module.clear();
                path = &path[1..];
            }
            "self" => path = &path[1..],
            "super" => {
                while path.first().is_some_and(|name| name == "super") {
                    let _parent = module.pop()?;
                    path = &path[1..];
                }
            }
            _ => {}
        }
        module.extend_from_slice(path);
        Some((self.roots.get(root)?, module))
    }

    pub(super) fn default_for(&self, source: &str, inline: &[String], path: &[String]) -> Option<bool> {
        let (identities, path) = self.resolve(source, inline, path)?;
        identities.default_for(&path)
    }

    pub(super) fn is_enum(&self, source: &str, inline: &[String], path: &[String]) -> bool {
        self.resolve(source, inline, path)
            .is_some_and(|(identities, path)| identities.is_enum(&path))
    }

    pub(super) fn defines(&self, source: &str, inline: &[String], path: &[String]) -> bool {
        self.resolve(source, inline, path)
            .is_some_and(|(identities, path)| identities.resolve_path(&path).is_some())
    }

    pub(super) fn same_type(&self, source: &str, inline: &[String], left: &[String], right: &[String]) -> bool {
        let Some((identities, left)) = self.resolve(source, inline, left) else {
            return false;
        };
        let Some((_, right)) = self.resolve(source, inline, right) else {
            return false;
        };
        identities
            .resolve_path(&left)
            .is_some_and(|left| identities.resolve_path(&right).as_ref() == Some(&left))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::cfg::CfgSet;

    fn identities(source: &str) -> Pending {
        Pending::of(&syn::parse_file(source).unwrap(), &CfgSet::unconditional())
    }

    fn path(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).into()).collect()
    }

    #[test]
    fn labeled_roots_resolve_relative_paths_without_cross_crate_leakage() {
        let mut sources = Sources::default();
        sources.insert("src\\lib.rs", identities("#[derive(Default)] struct Root; enum Choice { A }"));
        sources.insert(
            "src\\child.rs",
            identities("use super::Root as Parent; type Alias = crate::Root; struct Local;"),
        );
        sources.insert("other.rs", identities("struct Root;"));
        sources.bind(&[
            ("src/lib.rs".into(), "src/lib.rs".into(), Vec::new()),
            ("src/child.rs".into(), "src/lib.rs".into(), path(&["child"])),
            ("other.rs".into(), "other.rs".into(), Vec::new()),
            ("absent.rs".into(), "src/lib.rs".into(), Vec::new()),
        ]);
        assert!(sources.bound);
        for name in ["Parent", "Alias"] {
            assert_eq!(sources.default_for("src\\child.rs", &[], &path(&[name])), Some(true));
            assert!(sources.same_type("src/child.rs", &[], &path(&[name]), &path(&["crate", "Root"])));
        }
        assert_eq!(sources.default_for("other.rs", &[], &path(&["Root"])), Some(false));
        assert!(sources.defines("src/lib.rs", &path(&["child"]), &path(&["self", "Local"])));
        assert!(sources.defines("src/child.rs", &[], &path(&["super", "Root"])));
        assert!(sources.is_enum("src/child.rs", &[], &path(&["crate", "Choice"])));
        assert!(!sources.is_enum("src/lib.rs", &[], &path(&["Root"])));
        assert!(!sources.defines("src/lib.rs", &[], &path(&["super", "Root"])));
        assert!(!sources.defines("src/lib.rs", &[], &[]));
        assert!(!sources.same_type("unknown.rs", &[], &path(&["Root"]), &path(&["Root"])));
        assert!(!sources.same_type("src/lib.rs", &[], &path(&["Root"]), &[]));
        assert!(!sources.same_type("src/lib.rs", &[], &path(&["Root"]), &path(&["Choice"])));
        sources.alias("src\\child.rs", "virtual\\child.rs");
        assert_eq!(sources.default_for("virtual/child.rs", &[], &path(&["Parent"])), Some(true));
        sources.alias("src/child.rs", "virtual/child.rs");
        assert!(!sources.defines("virtual/child.rs", &[], &path(&["Parent"])));
        sources.alias("missing.rs", "unused.rs");
        assert!(!sources.defines("unused.rs", &[], &path(&["Root"])));
    }

    #[test]
    fn duplicate_sources_and_bindings_fail_closed_even_after_absorption() {
        let mut left = Sources::default();
        left.insert("duplicate.rs", identities("struct First;"));
        left.insert("duplicate.rs", identities("struct Second;"));
        left.insert("duplicate.rs", identities("struct Third;"));
        left.insert("shared.rs", identities("struct Shared;"));
        let mut right = Sources::default();
        right.insert("shared.rs", identities("struct Different;"));
        right.insert("bad.rs", identities("struct Bad;"));
        right.insert("bad.rs", identities("struct Other;"));
        right.insert("unique.rs", identities("struct Unique;"));
        left.insert("bad.rs", identities("struct Old;"));
        left.absorb(right);
        left.bind(&[
            ("duplicate.rs".into(), "root".into(), Vec::new()),
            ("shared.rs".into(), "root".into(), Vec::new()),
            ("bad.rs".into(), "root".into(), Vec::new()),
            ("unique.rs".into(), "root".into(), Vec::new()),
            ("unique.rs".into(), "second".into(), Vec::new()),
        ]);
        for (file, name) in [
            ("duplicate.rs", "First"),
            ("shared.rs", "Shared"),
            ("bad.rs", "Old"),
            ("unique.rs", "Unique"),
        ] {
            assert!(!left.defines(file, &[], &path(&[name])), "{file}");
        }
    }

    #[test]
    fn rebinding_finishes_deferred_implementations_against_new_module_aliases() {
        let mut sources = Sources::default();
        sources.insert("implementation.rs", identities("impl Default for crate::actual::Alias {}"));
        sources.bind(&[("implementation.rs".into(), "root".into(), path(&["implementation"]))]);
        assert_eq!(
            sources.default_for("implementation.rs", &[], &path(&["crate", "actual", "Alias"])),
            None
        );

        sources.insert("actual.rs", identities("pub struct Actual; pub type Alias = Actual;"));
        sources.bind(&[("actual.rs".into(), "root".into(), path(&["actual"]))]);
        assert_eq!(sources.default_for("actual.rs", &[], &path(&["Actual"])), Some(true));
        assert_eq!(
            sources.default_for("implementation.rs", &[], &path(&["crate", "actual", "Alias"])),
            Some(true)
        );
        assert!(sources.same_type("actual.rs", &[], &path(&["Actual"]), &path(&["Alias"])));

        sources.bind(&[]);
        assert_eq!(sources.default_for("actual.rs", &[], &path(&["Alias"])), Some(true));
    }
}
