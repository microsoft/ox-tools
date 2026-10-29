// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Declaration identities and alias links composable under labeled module roots.

use syn::visit::{self, Visit};
use syn::{File, Item, Type, UseTree};

use super::defaults::{DefaultPaths, derives_default, item_attrs};
use crate::cfg::CfgSet;
use crate::{HashMap, HashSet};

pub(super) fn nominal_attributes(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        attribute.path().get_ident().is_some_and(|name| {
            matches!(
                name.to_string().as_str(),
                "cfg"
                    | "doc"
                    | "derive"
                    | "repr"
                    | "allow"
                    | "expect"
                    | "warn"
                    | "deny"
                    | "forbid"
                    | "must_use"
                    | "deprecated"
                    | "non_exhaustive"
            )
        })
    })
}

fn known_derives(attributes: &[syn::Attribute], paths: &DefaultPaths) -> bool {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
        .all(|attribute| {
            attribute
                .parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .is_ok_and(|derives| {
                    derives.iter().all(|path| {
                        path.get_ident().is_some_and(|name| {
                            matches!(
                                name.to_string().as_str(),
                                "Clone" | "Copy" | "Debug" | "Eq" | "Hash" | "Ord" | "PartialEq" | "PartialOrd"
                            ) || name == "Default" && derives_default(attributes, paths)
                        })
                    })
                })
        })
}

#[derive(Clone, Debug, Default)]
pub(super) struct Identities {
    declarations: HashMap<Vec<String>, Option<bool>>,
    links: HashMap<Vec<String>, Option<Vec<String>>>,
    modules: HashSet<Vec<String>>,
    implementations: Vec<Vec<String>>,
    generic: HashSet<Vec<String>>,
    enums: HashSet<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Pending {
    identities: Identities,
}

impl Pending {
    pub(super) fn of(file: &File, cfg: &CfgSet) -> Self {
        let mut walk = Walk {
            identities: Identities::default(),
            module: Vec::new(),
            paths: DefaultPaths::of_in(file, cfg),
            cfg,
        };
        walk.visit_file(file);
        Self {
            identities: walk.identities,
        }
    }

    pub(super) fn finish(self) -> Identities {
        let mut identities = self.identities;
        for implementation in identities.implementations.clone() {
            if let Some(target) = identities.resolve_path(&implementation)
                && let Some(default) = identities.declarations.get_mut(&target)
                && default.is_some()
            {
                *default = (!identities.generic.contains(&target)).then_some(true);
            }
        }
        identities
    }

    pub(super) fn absorb_at(&mut self, other: &Self, module: &[String]) {
        let identities = &mut self.identities;
        let other = &other.identities;
        let local = |path: &[String]| module.iter().chain(path).cloned().collect::<Vec<_>>();
        let target = |path: &[String]| {
            if path.first().is_some_and(|name| name == "crate") {
                return Some(
                    path[
                // #[gamma::skip(literal.int_decrement, reason = "Sources::bind supplies absorb_at only original per-file Pending entries; composed roots are receivers, never re-absorbed as children. Keeping the leading crate marker thus does not cause a second module rebase. Identities::resolve_path strips that marker before every declaration/link lookup and visited-path check, so finish and subsequent queries resolve the same declaration identities.", tag = "equivalent")]
                1..]
                        .to_vec(),
                );
            }
            let mut prefix = module.to_vec();
            let mut path = path;
            while path.first().is_some_and(|name| name == "super") {
                let _parent = prefix.pop()?;
                path = &path[1..];
            }
            prefix.extend_from_slice(path);
            Some(prefix)
        };
        for (path, default) in &other.declarations {
            let _entry = identities
                .declarations
                .entry(local(path))
                .and_modify(|value| *value = None)
                .or_insert(*default);
        }
        for (path, link) in &other.links {
            let _entry = identities
                .links
                .entry(local(path))
                .and_modify(|value| *value = None)
                .or_insert_with(|| link.as_ref().and_then(|path| target(path)));
        }
        identities.modules.extend(other.modules.iter().map(|path| local(path)));
        identities.enums.extend(other.enums.iter().map(|path| local(path)));
        identities.generic.extend(other.generic.iter().map(|path| local(path)));
        identities
            .implementations
            .extend(other.implementations.iter().filter_map(|path| target(path)));
    }
}

impl From<Identities> for Pending {
    fn from(identities: Identities) -> Self {
        Self { identities }
    }
}

impl Identities {
    pub(super) fn of(file: &File, cfg: &CfgSet) -> Self {
        Pending::of(file, cfg).finish()
    }

    pub(super) fn is_enum(&self, path: &[String]) -> bool {
        self.resolve_path(path).is_some_and(|path| self.enums.contains(&path))
    }

    pub(super) fn default_for(&self, path: &[String]) -> Option<bool> {
        let path = self.resolve_path(path)?;
        self.declarations.get(&path).copied().flatten()
    }

    pub(super) fn local_module(&self, module: &[String], name: &str) -> bool {
        let mut path = module.to_vec();
        path.push(name.to_owned());
        self.modules.contains(&path)
    }

    pub(super) fn resolve_path(&self, path: &[String]) -> Option<Vec<String>> {
        let mut path = path.to_vec();
        if matches!(path.first().map(String::as_str), Some("crate" | "self")) {
            let _root = path.remove(0);
        }
        let mut visited = HashSet::default();
        let mut remaining = self.links.len().saturating_add(path.len()).saturating_add(1);
        loop {
            if path.first().is_some_and(|name| name == "crate") {
                let _root = path.remove(0);
            }
            if
            // #[gamma::skip(logical.or_remove_right, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            remaining == 0 || !visited.insert(path.clone()) {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
            // #[gamma::skip(all, reason = "this decreasing budget bounds alias cycles whose expanded paths never repeat")]
            remaining -= 1;
            if self.declarations.contains_key(&path) {
                return Some(path);
            }
            let mut replaced =
            // #[gamma::skip(literal.bool_flip, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            false;
            for end in (
                // #[gamma::skip(literal.int_decrement, reason = "Every link key is Walk::key(name), which always appends a name (even an empty introduced name), and absorb_at prefixes that nonempty key. The added empty-prefix lookup therefore cannot find a link and changes no resolution.", tag = "equivalent")]
                1..=path.len()
            )
                .rev()
            {
                if let Some(target) = self.links.get(&path[..end]) {
                    let _old = path.splice(..end, target.as_ref()?.iter().cloned());
                    replaced = true;
                    break;
                }
            }
            if
            // #[gamma::skip(cond.always_false, reason = "Identities::resolve_path retains its finite decreasing budget and visited-path guard. Removing the visited check still returns None at the budget for cycles; permitting one extra unchanged iteration after no link matches immediately hits visited (or the budget) and returns the same None. Positive declaration resolutions and chosen link replacements are unchanged.", tag = "equivalent")]
            !replaced {
                return
                // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
                None;
            }
        }
    }
}

struct Walk<'a> {
    identities: Identities,
    module: Vec<String>,
    paths: DefaultPaths,
    cfg: &'a CfgSet,
}

impl Walk<'_> {
    fn key(&self, name: String) -> Vec<String> {
        let mut key = self.module.clone();
        key.push(name);
        key
    }

    fn target(&self, mut names: Vec<String>) -> Option<Vec<String>> {
        let mut prefix = self.module.clone();
        match names.first()?.as_str() {
            "crate" => {
                return Some(names);
            }
            "self" => {
                let _root = names.remove(0);
            }
            "super" => {
                while names.first().is_some_and(|name| name == "super") {
                    if prefix.last().is_none_or(|name| name == "super") {
                        prefix.push("super".to_owned());
                    } else {
                        let _parent = prefix.pop();
                    }
                    let _root = names.remove(0);
                }
            }
            _ => {}
        }
        prefix.extend(names);
        Some(prefix)
    }

    fn link(&mut self, tree: &UseTree, prefix: &[String]) {
        match tree {
            UseTree::Path(path) => {
                let mut prefix = prefix.to_vec();
                prefix.push(path.ident.to_string());
                self.link(&path.tree, &prefix);
            }
            UseTree::Name(name) => {
                let mut target = prefix.to_vec();
                if name.ident != "self" {
                    target.push(name.ident.to_string());
                }
                let introduced = target.last().cloned().unwrap_or_default();
                self.insert_link(introduced, target);
            }
            UseTree::Rename(rename) => {
                let mut target = prefix.to_vec();
                if rename.ident != "self" {
                    target.push(rename.ident.to_string());
                }
                self.insert_link(rename.rename.to_string(), target);
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.link(tree, prefix);
                }
            }
            UseTree::Glob(_) => {}
        }
    }

    fn insert_link(&mut self, name: String, target: Vec<String>) {
        let key = self.key(name);
        let target = self.target(target);
        let _entry = self.identities.links.entry(key).and_modify(|value| *value = None).or_insert(target);
    }

    fn declaration(&mut self, name: String, attrs: &[syn::Attribute], generic: bool) {
        let key = self.key(name);
        if generic {
            let _inserted = self.identities.generic.insert(key.clone());
        }
        let default = (nominal_attributes(attrs) && known_derives(attrs, &self.paths))
            .then(|| derives_default(attrs, &self.paths))
            .filter(|derived| !generic || !derived);
        let _entry = self
            .identities
            .declarations
            .entry(key)
            .and_modify(|value| *value = None)
            .or_insert(default);
    }
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters describe their syntax")]
impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item(&mut self, node: &'ast Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            visit::visit_item(self, node);
        }
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let module = self.key(node.ident.to_string());
        let _inserted = self.identities.modules.insert(module);
        if let Some((_, items)) = &node.content {
            let mut file = syn::parse_file("").expect("empty Rust source always parses");
            file.items.clone_from(items);
            let paths = core::mem::replace(&mut self.paths, DefaultPaths::of_in(&file, self.cfg));
            self.module.push(node.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
            let _module = self.module.pop();
            self.paths = paths;
        }
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if nominal_attributes(&node.attrs) {
            let _added = self.identities.enums.insert(self.key(node.ident.to_string()));
        }
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        self.declaration(node.ident.to_string(), &node.attrs, !node.generics.params.is_empty());
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        if node.leading_colon.is_none() {
            self.link(&node.tree, &[]);
        }
    }

    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if node.generics.params.is_empty()
            && let Type::Path(path) = &*node.ty
            && path.qself.is_none()
            && path.path.leading_colon.is_none()
        {
            self.insert_link(
                node.ident.to_string(),
                path.path.segments.iter().map(|segment| segment.ident.to_string()).collect(),
            );
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if let Some((path, _)) = &node.trait_
            && self.paths.is_standard_trait(path)
            && let Type::Path(ty) = &*node.self_ty
            && let Some(target) = self.target(ty.path.segments.iter().map(|segment| segment.ident.to_string()).collect())
        {
            self.identities.implementations.push(target);
        }
    }

    fn visit_block(&mut self, _: &'ast syn::Block) {}
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::parse_file;

    use super::{Identities, Pending};
    use crate::HashMap;
    use crate::cfg::CfgSet;
    use crate::ops::collect::Defaults;

    #[test]
    fn longest_alias_prefix_wins_and_shorter_value_aliases_cannot_redirect_its_target() {
        let file = parse_file(
            "mod original { pub fn value() {} }
             use original::value as a;
             mod a {
                 pub struct Target;
                 pub mod nested { pub use super::Target as Exported; }
             }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(
            identities.resolve_path(&["a".into(), "nested".into(), "Exported".into()]),
            Some(vec!["a".into(), "Target".into()])
        );
    }

    #[test]
    fn growing_alias_cycles_obey_the_exact_derived_work_budget_before_declaring_a_match() {
        for (first, accepted) in [("b", true), ("a", false)] {
            let target = vec![first.to_owned(), "c".into(), "c".into()];
            let identities = Identities {
                declarations: HashMap::from_iter([(target.clone(), Some(false))]),
                links: HashMap::from_iter([
                    (vec!["a".into()], Some(vec!["b".into(), "c".into()])),
                    (vec!["b".into()], Some(vec!["a".into()])),
                ]),
                ..Identities::default()
            };
            assert_eq!(identities.resolve_path(&["a".into()]), accepted.then_some(target), "{first}");
        }
    }

    #[test]
    fn empty_self_import_targets_are_unknown_rather_than_panicking() {
        let identities = Identities::of(&parse_file("use self as Root;").unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.resolve_path(&["Root".into()]), None);
        assert_eq!(identities.default_for(&["Root".into()]), None);
    }

    #[test]
    fn configured_out_declarations_and_expanding_enum_attributes_supply_no_identity_proof() {
        let file = parse_file(
            "#[cfg(any())] struct Absent;
             mod nested { #[cfg(any())] enum Hidden { A } struct Present; }
             #[transform] enum Expanded { A }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::parse(""));
        assert_eq!(identities.default_for(&["Absent".into()]), None);
        assert_eq!(identities.default_for(&["nested".into(), "Hidden".into()]), None);
        assert_eq!(identities.default_for(&["nested".into(), "Present".into()]), Some(false));
        assert!(!identities.is_enum(&["Expanded".into()]));
    }

    #[test]
    fn labeled_merges_keep_generic_default_uncertainty_and_climb_multiple_missing_parents() {
        let cfg = CfgSet::unconditional();
        let generic = Pending::of(
            &parse_file("struct Generic<T>(T); impl<T> Default for Generic<T> { fn default() -> Self { panic!() } }").unwrap(),
            &cfg,
        );
        let mut root = Pending::default();
        root.absorb_at(&generic, &["child".into()]);
        assert_eq!(root.finish().default_for(&["child".into(), "Generic".into()]), None);
        let file = parse_file("use super::super::Shape as Escaped;").unwrap();
        let identities = Identities::of(&file, &cfg);
        assert_eq!(identities.resolve_path(&["Escaped".into()]), None);
        let identities = Identities::of(
            &parse_file("mod inner { use custom::Default; struct Opaque; } #[derive(Default)] struct Ready;").unwrap(),
            &cfg,
        );
        assert_eq!(identities.default_for(&["Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["inner".into(), "Opaque".into()]), Some(false));
    }

    #[test]
    fn rebased_double_parent_imports_resolve_the_root_instead_of_a_child_shadow() {
        let cfg = CfgSet::unconditional();
        let mut root = Pending::of(&parse_file("#[derive(Default)] struct Shape;").unwrap(), &cfg);
        let child = Pending::of(&parse_file("struct Shape; use super::super::Shape as Imported;").unwrap(), &cfg);
        root.absorb_at(&child, &["outer".into(), "child".into()]);
        let identities = root.finish();
        assert_eq!(identities.default_for(&["Shape".into()]), Some(true));
        assert_eq!(
            identities.default_for(&["outer".into(), "child".into(), "Shape".into()]),
            Some(false)
        );
        assert_eq!(
            identities.default_for(&["outer".into(), "child".into(), "Imported".into()]),
            Some(true)
        );
    }

    #[test]
    fn nested_nonstandard_default_imports_cannot_supply_a_standard_derive_proof() {
        let source = "mod nested { use custom::Default; #[derive(Default)] struct Local; }
                      #[derive(Default)] struct Outer;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["nested".into(), "Local".into()]), None);
        assert_eq!(identities.default_for(&["Outer".into()]), Some(true));
    }

    #[test]
    fn absolute_imports_do_not_resolve_through_same_named_source_modules() {
        let source = "mod external { #[derive(Default)] pub struct Shape; }
                      use ::external::Shape as Imported;
                      use external::Shape as Local;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["external".into(), "Shape".into()]), Some(true));
        assert_eq!(identities.default_for(&["Local".into()]), Some(true));
        assert_eq!(identities.default_for(&["Imported".into()]), None);
    }

    #[test]
    fn campaign_nominal_default_evidence_requires_every_derive_to_be_known() {
        for (attributes, expected) in [
            ("", Some(false)),
            ("#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]", Some(false)),
            ("#[derive(Clone, Default)]", Some(true)),
            ("#[doc = \"source declaration\"] #[repr(C)] #[derive(Debug)]", Some(false)),
            ("#[derive(Clone, Expand)]", None),
            ("#[derive(Default, Expand)]", None),
            ("#[derive(custom::Default)]", None),
            ("#[derive(Default = invalid)]", None),
            ("#[transform] #[derive(Default)]", None),
        ] {
            let source = format!("{attributes} struct Shape;");
            let identities = Identities::of(&parse_file(&source).unwrap(), &CfgSet::unconditional());
            assert_eq!(identities.default_for(&["Shape".into()]), expected, "{source}");
        }
        let source = "use custom::Default; #[derive(Default)] struct Shape;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["Shape".into()]), None);
    }

    #[test]
    fn campaign_union_and_enum_declarations_keep_distinct_default_and_variant_evidence() {
        let source = "
            union Missing { byte: u8 }
            union Ready { byte: u8 }
            impl Default for Ready { fn default() -> Self { Self { byte: 0 } } }
            #[derive(Default)] enum Choice { #[default] A, B }
            #[transform] enum Opaque { A }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for (name, expected_default, expected_enum) in [
            ("Missing", Some(false), false),
            ("Ready", Some(true), false),
            ("Choice", Some(true), true),
            ("Opaque", None, false),
        ] {
            assert_eq!(identities.default_for(&[name.into()]), expected_default, "{name}");
            assert_eq!(identities.is_enum(&[name.into()]), expected_enum, "{name}");
        }
    }

    #[test]
    fn campaign_grouped_self_imports_preserve_module_aliases_and_nested_parent_paths() {
        let source = "
            mod data { #[derive(Default)] pub struct Ready; pub struct Missing; }
            use self::data::{self as renamed, Ready, Missing as Absent};
            mod child {
                use super::data::{self, Ready as Present};
                pub mod leaf { use super::super::data::Ready as Deep; }
            }";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for (path, declaration, expected) in [
            ("Ready", "data::Ready", Some(true)),
            ("Absent", "data::Missing", Some(false)),
            ("renamed::Ready", "data::Ready", Some(true)),
            ("child::data::Missing", "data::Missing", Some(false)),
            ("child::Present", "data::Ready", Some(true)),
            ("child::leaf::Deep", "data::Ready", Some(true)),
            ("self::renamed::Ready", "data::Ready", Some(true)),
            ("crate::child::Present", "data::Ready", Some(true)),
        ] {
            let names = path.split("::").map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                identities.resolve_path(&names),
                Some(declaration.split("::").map(str::to_owned).collect()),
                "{path}"
            );
            assert_eq!(identities.default_for(&names), expected, "{path}");
        }
        assert!(identities.local_module(&[], "data"));
        assert!(identities.local_module(&["child".into()], "leaf"));
        assert!(!identities.local_module(&[], "renamed"));
        assert_eq!(identities.resolve_path(&["missing".into()]), None);
    }

    #[test]
    fn campaign_alias_ambiguity_cycles_and_external_roots_withhold_nominal_proof() {
        let source = "
            #[derive(Default)] struct Ready;
            struct Missing;
            use self::Ready as Ambiguous;
            use self::Missing as Ambiguous;
            type First = Second;
            type Second = First;
            use ::external::Ready as Foreign;
            type Generic<T> = Ready;
            type Absolute = ::external::Ready;";
        let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
        for name in ["Ambiguous", "First", "Second", "Foreign", "Generic", "Absolute"] {
            assert_eq!(identities.resolve_path(&[name.into()]), None, "{name}");
            assert_eq!(identities.default_for(&[name.into()]), None, "{name}");
        }
        assert_eq!(identities.default_for(&["Ready".into()]), Some(true));
        assert_eq!(identities.default_for(&["Missing".into()]), Some(false));
    }

    #[test]
    fn campaign_labeled_identity_merges_rebase_parent_links_and_implementations() {
        let cfg = CfgSet::unconditional();
        let mut root = Pending::of(
            &parse_file("struct Root; mod child {} #[derive(Default)] struct Ready;").unwrap(),
            &cfg,
        );
        let child = Pending::of(
            &parse_file(
                "use super::Root as Parent; use crate::Ready as Absolute;
                 struct Local; impl Default for Local { fn default() -> Self { Self } }
                 mod nested { use super::Local as Present; }
                 use super::super::Missing as Escaped;",
            )
            .unwrap(),
            &cfg,
        );
        root.absorb_at(&child, &["child".into()]);
        let identities = root.finish();
        for (path, expected) in [
            ("child::Parent", Some(false)),
            ("child::Absolute", Some(true)),
            ("child::Local", Some(true)),
            ("child::nested::Present", Some(true)),
            ("child::Escaped", None),
            ("Local", None),
        ] {
            assert_eq!(
                identities.default_for(&path.split("::").map(str::to_owned).collect::<Vec<_>>()),
                expected,
                "{path}"
            );
        }
        assert!(identities.local_module(&["child".into()], "nested"));
    }

    #[test]
    fn nested_reexports_preserve_root_declaration_default_identity() {
        let file = parse_file(
            "struct Missing; #[derive(Default)] struct Ready;
             mod child { use super::Missing as Absent; use crate::Ready as Present; }",
        )
        .unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(identities.default_for(&["child".into(), "Absent".into()]), Some(false));
        assert_eq!(identities.default_for(&["child".into(), "Present".into()]), Some(true));
    }

    #[test]
    fn prefix_expanding_alias_cycles_are_unknown_and_terminate() {
        let file = parse_file("type Alias = Alias::Nested;").unwrap();
        let identities = Identities::of(&file, &CfgSet::unconditional());
        assert_eq!(identities.resolve_path(&["Alias".into()]), None);
    }

    #[test]
    fn a_default_implementation_through_an_alias_targets_the_declaration_regardless_of_order() {
        for source in [
            "impl Default for Alias {} type Alias = Actual; struct Actual;",
            "struct Actual; type Alias = Actual; impl Default for Alias {}",
        ] {
            let identities = Identities::of(&parse_file(source).unwrap(), &CfgSet::unconditional());
            assert_eq!(identities.default_for(&["Actual".into()]), Some(true));
            assert_eq!(identities.default_for(&["Alias".into()]), Some(true));
        }
    }

    #[test]
    fn merged_local_reexports_keep_uniform_negative_evidence_but_unknown_targets_do_not() {
        let mut defaults =
            Defaults::of(&parse_file("mod implementation { pub struct Artifact; } pub use self::implementation::Artifact;").unwrap());
        defaults.absorb(Defaults::of(&parse_file("struct Unrelated;").unwrap()));
        assert!(!defaults.has_single_source_evidence());
        assert!(defaults.lacks_error_default("Artifact"));
        assert!(defaults.lacks_local_path(&["crate".into(), "catalog".into(), "Artifact".into()]));
        defaults.absorb(Defaults::of(&parse_file("pub use dependency::Artifact;").unwrap()));
        assert!(!defaults.lacks_error_default("Artifact"));
    }

    #[test]
    fn alias_implementations_and_conditional_derives_do_not_supply_negative_evidence_when_merged() {
        for sources in [
            ["type Alias = Actual; struct Actual;", "impl Default for Alias {}"],
            ["impl Default for Alias {}", "type Alias = Actual; struct Actual;"],
        ] {
            let mut defaults = Defaults::default();
            for source in sources {
                defaults.absorb(Defaults::of(&parse_file(source).unwrap()));
            }
            assert!(!defaults.lacks_error_default("Actual"));
        }
        let defaults =
            Defaults::of(&parse_file("#[cfg_attr(all(), derive(Default))] struct Conditional; pub use self::Conditional;").unwrap());
        assert!(!defaults.lacks_error_default("Conditional"));
        assert!(!defaults.lacks_local_path(&["Conditional".into()]));
    }
}
