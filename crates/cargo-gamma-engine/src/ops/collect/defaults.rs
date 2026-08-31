// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::hash_map::Entry;

use syn::visit::{self, Visit};
use syn::{
    File, GenericParam, Generics, ItemEnum, ItemImpl, ItemStruct, ItemType, ItemUnion, Path, Type, TypeParamBound, UseTree, WherePredicate,
};

use crate::cfg::CfgSet;
use crate::{HashMap, HashSet};

/// The spellings in one file that name the standard-library `Default` trait.
///
/// Rust's prelude makes bare `Default` normally mean `core::default::Default`, but local items
/// and imports can shadow it in different namespaces. The collector cannot run name resolution,
/// so it records explicit standard aliases and explicit shadows before it decides whether a call,
/// bound, derive, or impl refers to the standard trait.
#[derive(Debug, Default)]
pub(super) struct DefaultPaths {
    aliases: HashSet<String>,
    module_aliases: HashMap<String, Vec<String>>,
    shadows: HashSet<String>,
    derive_shadows: HashSet<String>,
}

impl DefaultPaths {
    /// Collects standard-trait aliases and local shadows from a parsed file.
    pub(super) fn of(file: &File) -> Self {
        Self::of_in(file, &CfgSet::unconditional())
    }

    /// Collects bindings visible in this file's outer lexical scope.
    ///
    /// A child module owns its imports and declarations: letting its `Default` shadow the parent
    /// makes an unrelated outer `impl Default` look custom. Conditional items likewise introduce
    /// no binding when the selected build strips them.
    pub(super) fn of_in(file: &File, cfg: &CfgSet) -> Self {
        let mut paths = Self::default();

        for item in &file.items {
            if !cfg.holds_for(item_attrs(item)) {
                continue;
            }

            match item {
                syn::Item::Use(node) => paths.descend_use(&mut Vec::new(), &node.tree),
                syn::Item::Trait(node) if node.ident == "Default" => {
                    let _added = paths.shadows.insert("Default".to_owned());
                }
                syn::Item::Struct(node) => paths.note_type_shadow(&node.ident),
                syn::Item::Enum(node) => paths.note_type_shadow(&node.ident),
                syn::Item::Union(node) => paths.note_type_shadow(&node.ident),
                syn::Item::Type(node) => paths.note_type_shadow(&node.ident),
                syn::Item::Mod(node) => paths.note_type_shadow(&node.ident),
                _ => {}
            }
        }

        paths
    }

    /// Returns whether an implementation path names the standard `Default` trait.
    pub(super) fn is_standard_trait(&self, path: &Path) -> bool {
        let segments: Vec<String> = path.segments.iter().map(|segment| segment.ident.to_string()).collect();

        self.is_standard_trait_segments(&segments)
    }

    /// Returns whether the fallback spelling `Default::default()` resolves to this trait.
    ///
    /// A bare custom trait named `Default` shadows the prelude, so it is not the standard trait,
    /// but the fallback text would still recurse inside its own `default` method. A standard trait
    /// written through an alias recurses only when bare `Default` still resolves to that standard
    /// trait; qualified and differently aliased custom traits stay mutable.
    pub(super) fn is_fallback_trait(&self, path: &Path) -> bool {
        if path.segments.len() == 1 && path.segments.first().is_some_and(|segment| segment.ident == "Default") {
            return true;
        }

        self.is_standard_trait(path) && !self.shadows.contains("Default")
    }

    /// Returns whether a bare name resolves to the standard `Default` trait.
    fn is_standard_trait_name(&self, name: &str) -> bool {
        self.aliases.contains(name) || (name == "Default" && !self.shadows.contains(name))
    }

    /// Returns whether a complete associated-function path calls the standard default trait.
    pub(super) fn is_standard_default_callee(&self, path: &Path) -> bool {
        let mut segments: Vec<String> = path.segments.iter().map(|segment| segment.ident.to_string()).collect();

        if segments.pop().as_deref() != Some("default") {
            return false;
        }

        self.is_standard_callee_segments(&segments)
    }

    pub(super) fn is_standard_default_segments(&self, segments: &[String]) -> bool {
        let Some((method, qualifier)) = segments.split_last() else {
            return false;
        };

        method == "default" && self.is_standard_callee_segments(qualifier)
    }

    /// Returns whether a derive path invokes the built-in standard `Default` derive.
    fn is_standard_derive(&self, path: &Path) -> bool {
        path.is_ident("Default") && !self.derive_shadows.contains("Default")
    }

    /// Records one imported binding and the complete path it came from.
    fn imported(&mut self, binding: String, prefix: &[String], source: String) {
        let mut path = prefix.to_vec();

        if source != "self" {
            path.push(source);
        }

        let path = self.expand_module_alias(&path);

        if is_standard_trait_path(&path) {
            let _added = self.aliases.insert(binding);
        } else if is_standard_module_path(&path) {
            let _replaced = self.module_aliases.insert(binding.clone(), path);

            if binding == "Default" {
                let _added = self.shadows.insert(binding);
            }
        } else if binding == "Default" {
            let _added = self.shadows.insert(binding.clone());
            let _added = self.derive_shadows.insert(binding);
        }
    }

    /// Records every binding one `use` tree introduces.
    fn descend_use(&mut self, prefix: &mut Vec<String>, tree: &UseTree) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.descend_use(prefix, &path.tree);
                let _popped = prefix.pop();
            }

            UseTree::Name(name) => self.imported(name.ident.to_string(), prefix, name.ident.to_string()),
            UseTree::Rename(rename) => self.imported(rename.rename.to_string(), prefix, rename.ident.to_string()),

            UseTree::Group(group) => {
                for item in &group.items {
                    self.descend_use(prefix, item);
                }
            }

            UseTree::Glob(_) => {}
        }
    }

    /// Resolves a direct or imported spelling of the standard trait.
    fn is_standard_trait_segments(&self, segments: &[String]) -> bool {
        if let [name] = segments
            && self.is_standard_trait_name(name)
        {
            return true;
        }

        is_standard_trait_path(&self.expand_module_alias(segments))
    }

    /// Resolves the qualifier of an associated call through the type namespace.
    fn is_standard_callee_segments(&self, segments: &[String]) -> bool {
        if let [name] = segments
            && (self.aliases.contains(name) || (name == "Default" && !self.shadows.contains(name)))
        {
            return true;
        }

        is_standard_trait_path(&self.expand_module_alias(segments))
    }

    /// Resolves the first segment of a path through one or more imported standard modules.
    fn expand_module_alias(&self, segments: &[String]) -> Vec<String> {
        let mut expanded = segments.to_vec();

        for _ in 0..segments.len() {
            let first = expanded
                .first()
                .expect("alias expansion cannot consume more segments than the original path contains");
            let Some(prefix) = self.module_aliases.get(first) else {
                break;
            };

            let _spliced = expanded.splice(..1, prefix.iter().cloned());
        }

        expanded
    }
}

/// Returns whether a path spells the standard `Default` trait without imports.
fn is_standard_trait_path(path: &[String]) -> bool {
    matches!(
        path,
        [root, module, name]
            if matches!(root.as_str(), "std" | "core") && module == "default" && name == "Default"
    )
}

/// Returns whether a path names a standard module that can be imported under an alias.
fn is_standard_module_path(path: &[String]) -> bool {
    matches!(path, [root] if matches!(root.as_str(), "std" | "core"))
        || matches!(path, [root, module] if matches!(root.as_str(), "std" | "core") && module == "default")
}

impl DefaultPaths {
    /// Records a source declaration that occupies the bare type/module name `Default`.
    fn note_type_shadow(&mut self, ident: &syn::Ident) {
        if ident == "Default" {
            let _added = self.shadows.insert("Default".to_owned());
        }
    }
}

/// Returns the outer attributes of an item.
fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(node) => &node.attrs,
        syn::Item::Enum(node) => &node.attrs,
        syn::Item::ExternCrate(node) => &node.attrs,
        syn::Item::Fn(node) => &node.attrs,
        syn::Item::ForeignMod(node) => &node.attrs,
        syn::Item::Impl(node) => &node.attrs,
        syn::Item::Macro(node) => &node.attrs,
        syn::Item::Mod(node) => &node.attrs,
        syn::Item::Static(node) => &node.attrs,
        syn::Item::Struct(node) => &node.attrs,
        syn::Item::Trait(node) => &node.attrs,
        syn::Item::TraitAlias(node) => &node.attrs,
        syn::Item::Type(node) => &node.attrs,
        syn::Item::Union(node) => &node.attrs,
        syn::Item::Use(node) => &node.attrs,
        _ => &[],
    }
}

/// Names type parameters whose bounds explicitly promise the standard `Default` trait.
pub(super) fn standard_defaulted_parameters(generics: &Generics, defaults: &DefaultPaths) -> Vec<String> {
    let mut names = Vec::new();

    for parameter in &generics.params {
        let GenericParam::Type(parameter) = parameter else {
            continue;
        };

        if parameter.bounds.iter().any(|bound| standard_default_bound(bound, defaults)) {
            names.push(parameter.ident.to_string());
        }
    }

    if let Some(where_clause) = &generics.where_clause {
        for predicate in &where_clause.predicates {
            let WherePredicate::Type(predicate) = predicate else {
                continue;
            };
            let Type::Path(path) = &predicate.bounded_ty else {
                continue;
            };
            let Some(name) = path.path.get_ident() else {
                continue;
            };

            if predicate.bounds.iter().any(|bound| standard_default_bound(bound, defaults)) {
                let name = name.to_string();

                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }

    names
}

/// Returns whether one type bound names the standard `Default` trait.
fn standard_default_bound(bound: &TypeParamBound, defaults: &DefaultPaths) -> bool {
    matches!(bound, TypeParamBound::Trait(bound) if defaults.is_standard_trait(&bound.path))
}

/// What the workspace's own sources say about which of their types implement `Default`.
///
/// The `fn_value` family and its relatives reach for `Default::default()` whenever they cannot name
/// a value of a type. For a type this workspace defines, that guess can be checked without any type
/// resolution: the definition is in a file that was parsed anyway, and either it derives the
/// standard `Default`, something writes a standard `impl Default for` it, or it has none.
///
/// The index is evidence of *presence* and never proof of absence for anything it cannot see whole.
/// A type it has no definition for stays optimistic, because it may come from a dependency that
/// does implement `Default`. A definition it can see but whose `Default` a macro generates is the
/// one way it can be wrong, and it errs in the safe direction: a mutant is withheld that would have
/// compiled, which costs a little signal rather than producing a wrong verdict.
#[derive(Debug, Default)]
pub struct Defaults {
    /// The local bindings that decide whether `Default` means the standard trait.
    paths: DefaultPaths,

    /// The `struct`, `enum` and `union` names this workspace defines.
    ///
    /// Type aliases are deliberately absent: an alias names someone else's type, so its `Default`
    /// is that type's business and nothing here can settle it.
    defined: HashSet<String>,

    /// Those of them that derive or implement the standard `Default` trait.
    defaulted: HashSet<String>,

    /// For each `type Alias<T, E = SomeError> = Result<T, E>`, the name of that default error type.
    ///
    /// A crate-wide `Result` alias is close to universal in real Rust, and it hides the error type
    /// from every signature that uses it. Recording what the alias fixed it to is what lets a
    /// one-argument `Result<T>` be reasoned about at all.
    ///
    /// The names are unqualified, and essentially every crate calls its alias `Result`, so a
    /// workspace of several crates routinely disagrees about what one key means. `None` records
    /// exactly that: the alias was seen naming more than one error type, so nothing can be
    /// concluded from it. Keeping the disagreement is what makes the index independent of the
    /// order the files happened to be parsed in.
    result_error: HashMap<String, Option<String>>,

    /// The selected configuration, used to ignore declarations that introduce no binding in this
    /// build.
    cfg: CfgSet,
}

/// Folds one alias into the map, demoting a key two files disagree about.
///
/// The last writer must not win. The partials are built by worker threads and folded in completion
/// order, so a map that overwrote on collision would give a different answer on every run over an
/// unchanged workspace — and the answer decides whether the `result.ok_to_err` family is emitted at
/// all, which is the largest screened family there is.
///
/// Demotion errs toward emitting, which is the safe direction: a mutant that turns out not to
/// compile costs a rollback round, whereas one that is withheld leaves a real gap in the suite
/// looking like a better score.
fn merge_alias(into: &mut HashMap<String, Option<String>>, alias: String, error: Option<String>) {
    match into.entry(alias) {
        Entry::Occupied(mut seen) => {
            if *seen.get() != error {
                *seen.get_mut() = None;
            }
        }
        Entry::Vacant(empty) => {
            let _inserted = empty.insert(error);
        }
    }
}

impl Defaults {
    /// Builds the index over one parsed file.
    ///
    /// A workspace's index is the [`absorb`](Self::absorb) of one of these per file, which is what
    /// lets the files be read on whichever thread happens to claim them.
    #[must_use]
    pub fn of(file: &File) -> Self {
        Self::of_in(file, &CfgSet::unconditional())
    }

    /// Builds the index over one parsed file for the selected configuration.
    #[must_use]
    pub fn of_in(file: &File, cfg: &CfgSet) -> Self {
        let mut index = Self {
            paths: DefaultPaths::of_in(file, cfg),
            cfg: cfg.clone(),
            ..Self::default()
        };

        index.visit_file(file);
        index
    }

    /// Returns whether the workspace defines this type and gives it no `Default`.
    ///
    /// Names are compared unqualified, so two crates in one workspace can both define a `Config`.
    /// Presence wins that collision: if either of them has a `Default`, neither is screened. The
    /// alternative would withhold a mutant that compiles, and this index exists to be conservative.
    #[must_use]
    pub fn lacks_default(&self, ty: &Type) -> bool {
        let Some(name) = name_of(ty) else {
            return false;
        };

        self.lacks_error_default(&name)
    }

    /// Folds another index into this one.
    ///
    /// Used to put together what several threads each learned from the files they parsed, in
    /// whatever order they finished. Every field resolves a collision without reference to that
    /// order, because the order is not reproducible and the result decides which mutants exist.
    ///
    /// `defined` and `defaulted` are unions, which is what makes presence win: a name one crate
    /// defines without a `Default` and another defines with one ends up in both sets, and is not
    /// screened. `result_error` cannot union, because its values are single names rather than
    /// membership, so a key two files disagree about is demoted to "unknown" instead — see
    /// [`merge_alias`].
    pub fn absorb(&mut self, other: Self) {
        self.defined.extend(other.defined);
        self.defaulted.extend(other.defaulted);

        for (alias, error) in other.result_error {
            merge_alias(&mut self.result_error, alias, error);
        }
    }

    /// Returns whether a type named by the index has no `Default`, given only its name.
    ///
    /// The by-name form exists for an error type reached through an alias, where what was recorded
    /// is a name rather than a syntax node.
    #[must_use]
    pub fn lacks_error_default(&self, name: &str) -> bool {
        self.defined.contains(name) && !self.defaulted.contains(name)
    }

    /// Returns the error type a `Result` alias fixed, given the alias's name.
    ///
    /// `None` covers both "no such alias" and "the workspace disagrees about this one", which are
    /// the same answer to the only question asked of it: nothing may be screened on this name.
    #[must_use]
    pub fn aliased_error(&self, alias: &str) -> Option<&str> {
        self.result_error.get(alias)?.as_deref()
    }

    fn note_derive(&mut self, name: &str, attributes: &[syn::Attribute]) {
        let _inserted = self.defined.insert(name.to_owned());

        if derives_default(attributes, &self.paths) {
            let _inserted = self.defaulted.insert(name.to_owned());
        }
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Defaults {
    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        if !self.cfg.holds_for(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.attrs);

        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        if !self.cfg.holds_for(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.attrs);

        visit::visit_item_enum(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast ItemUnion) {
        if !self.cfg.holds_for(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.attrs);

        visit::visit_item_union(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if !self.cfg.holds_for(&node.attrs) {
            return;
        }

        if let Some((path, _for)) = &node.trait_
            && self.paths.is_standard_trait(path)
            && let Some(name) = name_of(&node.self_ty)
        {
            let _inserted = self.defaulted.insert(name);
        }

        visit::visit_item_impl(self, node);
    }

    fn visit_item_type(&mut self, node: &'ast ItemType) {
        if !self.cfg.holds_for(&node.attrs) {
            return;
        }

        // Only an alias for `Result` matters, and only when it supplies its own error type as a
        // parameter default. `type Result<T> = core::result::Result<T, Error>` — with the error
        // written into the right-hand side rather than defaulted — is handled by the same read,
        // because the alias's target is what names the error either way.
        if name_of(&node.ty).as_deref() == Some("Result") {
            let defaulted = node.generics.params.iter().find_map(|param| match param {
                GenericParam::Type(ty) => ty.default.as_ref().and_then(|(_equals, ty)| name_of(ty)),
                _ => None,
            });

            let written = payload_name(&node.ty, 1);

            if let Some(error) = defaulted.or(written) {
                // One file can hold two aliases of the same name, in separate modules, just as two
                // files can. Both go through the same demotion.
                merge_alias(&mut self.result_error, node.ident.to_string(), Some(error));
            }
        }

        visit::visit_item_type(self, node);
    }
}

/// Returns whether a `#[derive(...)]` on an item lists `Default`.
fn derives_default(attributes: &[syn::Attribute], paths: &DefaultPaths) -> bool {
    let mut found = false;

    for attribute in attributes {
        if !attribute.path().is_ident("derive") {
            continue;
        }

        // Ignored rather than propagated: an attribute this cannot parse is one whose contents are
        // unknown, and the index's whole contract is that not knowing means staying optimistic.
        let _ignored = attribute.parse_nested_meta(|meta| {
            if paths.is_standard_derive(&meta.path) {
                found = true;
            }

            Ok(())
        });
    }

    found
}

/// The last segment of a type's path, which is the name the index is keyed by.
fn name_of(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
        Type::Paren(paren) => name_of(&paren.elem),
        Type::Group(group) => name_of(&group.elem),
        _ => None,
    }
}

/// The name of a type's `index`th generic argument.
fn payload_name(ty: &Type, index: usize) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };

    let syn::PathArguments::AngleBracketed(args) = &path.path.segments.last()?.arguments else {
        return None;
    };

    args.args
        .iter()
        .filter_map(|arg| match arg {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .nth(index)
        .and_then(name_of)
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    fn index(sources: &[&str]) -> Defaults {
        let mut defaults = Defaults::default();

        for source in sources {
            defaults.absorb(Defaults::of(&syn::parse_file(source).unwrap()));
        }

        defaults
    }

    #[test]
    fn a_type_the_workspace_defines_without_a_default_is_reported() {
        let defaults = index(&["pub struct Error { code: u8 }"]);
        let ty: Type = parse_quote!(Error);

        assert!(defaults.lacks_default(&ty));
    }

    #[test]
    fn a_derived_default_is_seen() {
        let defaults = index(&["#[derive(Debug, Default)] pub struct Error;"]);
        let ty: Type = parse_quote!(Error);

        assert!(!defaults.lacks_default(&ty));
    }

    /// An `enum` is indexed the same way a `struct` is -- its own derive is read, and the type is
    /// no longer merely optimistic about having a `Default`.
    #[test]
    fn an_enum_with_a_derived_default_is_seen() {
        let defaults = index(&["#[derive(Default)] pub enum Choice { #[default] A, B }"]);
        let ty: Type = parse_quote!(Choice);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_written_default_impl_is_seen_from_another_file() {
        let defaults = index(&["pub struct Error;", "impl Default for Error { fn default() -> Self { Self } }"]);
        let ty: Type = parse_quote!(Error);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn only_standard_default_implementations_and_derives_are_indexed() {
        let ty: Type = parse_quote!(Error);
        let standard_alias = index(&[
            "pub struct Error;",
            "use core::default::Default as StdDefault; impl StdDefault for Error { fn default() -> Self { Self } }",
        ]);
        let standard_module_alias = index(&[
            "pub struct Error;",
            "use core::default as defaults; impl defaults::Default for Error { fn default() -> Self { Self } }",
        ]);
        let standard_derive = index(&["struct Default;", "#[derive(Default)] pub struct Error;"]);
        let custom = index(&[
            "pub struct Error;",
            "mod custom { pub trait Default { fn default() -> Self; } } impl custom::Default for Error { fn default() -> Self { Self } }",
        ]);
        let custom_alias = index(&[
            "pub struct Error;",
            "mod custom { pub trait Default { fn default() -> Self; } } use custom::Default as Alias; impl Alias for Error { fn default() -> Self { Self } }",
        ]);
        let custom_derive = index(&["#[derive(custom::Default)] pub struct Error;"]);

        assert!(!standard_alias.lacks_default(&ty));
        assert!(!standard_module_alias.lacks_default(&ty));
        assert!(!standard_derive.lacks_default(&ty));
        assert!(custom.lacks_default(&ty));
        assert!(custom_alias.lacks_default(&ty));
        assert!(custom_derive.lacks_default(&ty));
    }

    #[test]
    fn nested_and_disabled_default_shadows_do_not_poison_an_outer_implementation() {
        let source = "
            struct Error;
            mod nested {
                trait Default {}
                use custom::Default;
            }
            #[cfg(any())]
            trait Default {}
            impl Default for Error {
                fn default() -> Self { Self }
            }
        ";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::parse("unix"));
        let ty: Type = parse_quote!(Error);

        assert!(!defaults.lacks_default(&ty));
    }

    /// A `union` named `Default` shadows the prelude exactly like a `struct`, `enum`, `type` or
    /// `mod` of that name does — `note_type_shadow` treats every kind of type-namespace
    /// declaration alike.
    #[test]
    fn a_union_named_default_shadows_the_prelude() {
        let source = "
            struct Error;
            union Default { flag: u8 }
            impl Default for Error {
                fn default() -> Self { Self }
            }
        ";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());
        let ty: Type = parse_quote!(Error);

        assert!(defaults.lacks_default(&ty), "a custom `Default` union shadows the standard trait");
    }

    /// A grouped `use` such as `use std::{fmt, default::Default}` introduces every item in the
    /// group, not only the first, and a glob import introduces no binding this module can
    /// interpret — but must not panic doing nothing with it.
    #[test]
    fn grouped_and_glob_use_trees_are_both_handled() {
        let defaults = index(&["pub struct Error;
             use std::{fmt, default::Default as StdDefault};
             impl StdDefault for Error { fn default() -> Self { Self } }
             use std::collections::*;"]);
        let ty: Type = parse_quote!(Error);

        assert!(
            !defaults.lacks_default(&ty),
            "the aliased Default inside the group must still resolve"
        );
    }

    /// Importing a custom, non-standard item under the bare name `Default` shadows the prelude —
    /// the same outcome a locally *declared* `Default` produces, reached through the import path
    /// instead of a declaration.
    #[test]
    fn importing_a_custom_default_directly_shadows_the_prelude() {
        let source = "
            struct Error;
            mod custom { pub trait Default { fn default() -> Self; } }
            use custom::Default;
            impl Default for Error {
                fn default() -> Self { Self }
            }
        ";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());
        let ty: Type = parse_quote!(Error);

        assert!(defaults.lacks_default(&ty), "importing a custom Default directly is still a shadow");
    }

    /// Aliasing the `core::default` module itself to the bare name `Default` — an unusual but
    /// legal import — makes `Default` mean a module rather than the trait, and so must shadow the
    /// prelude exactly as a locally declared or imported one does.
    #[test]
    fn aliasing_the_default_module_to_the_bare_name_shadows_the_prelude() {
        let source = "
            struct Error;
            use core::default as Default;
            impl Default for Error {
                fn default() -> Self { Self }
            }
        ";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());
        let ty: Type = parse_quote!(Error);

        assert!(
            defaults.lacks_default(&ty),
            "aliasing the default module to `Default` shadows the bare trait name"
        );
    }

    /// A single-segment alias of `std` or `core` itself (rather than of `std::default`) is
    /// recorded as a standard module too, and a path built through it resolves exactly as one
    /// built directly on `std`/`core` would.
    #[test]
    fn a_single_segment_standard_module_alias_still_resolves() {
        let defaults = index(&["pub struct Error;
             use std as MyStd;
             impl MyStd::default::Default for Error { fn default() -> Self { Self } }"]);
        let ty: Type = parse_quote!(Error);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_type_the_workspace_does_not_define_stays_optimistic() {
        let defaults = index(&["pub struct Error;"]);
        let ty: Type = parse_quote!(Utf8Error);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_qualified_path_is_keyed_by_its_last_segment() {
        let defaults = index(&["pub struct Error;"]);
        let ty: Type = parse_quote!(crate::error::Error);

        assert!(defaults.lacks_default(&ty));
    }

    #[test]
    fn a_name_two_crates_disagree_about_stays_optimistic() {
        let defaults = index(&["pub struct Config { a: u8 }", "#[derive(Default)] pub struct Config;"]);
        let ty: Type = parse_quote!(Config);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn an_alias_is_not_a_definition() {
        let defaults = index(&["pub type Handle = std::fs::File;"]);
        let ty: Type = parse_quote!(Handle);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_result_alias_records_the_error_it_defaults_to() {
        let defaults = index(&["pub type Result<T, E = Error> = core::result::Result<T, E>;"]);

        assert_eq!(defaults.aliased_error("Result"), Some("Error"));
    }

    #[test]
    fn a_result_alias_records_the_error_written_into_its_target() {
        let defaults = index(&["pub type Result<T> = core::result::Result<T, Error>;"]);

        assert_eq!(defaults.aliased_error("Result"), Some("Error"));
    }

    #[test]
    fn an_alias_for_something_other_than_a_result_is_not_recorded() {
        let defaults = index(&["pub type Pairs = Vec<(u8, u8)>;"]);

        assert_eq!(defaults.aliased_error("Pairs"), None);
    }

    /// Two crates that each call their alias `Result` leave the name meaning nothing, whichever
    /// order their files were folded in.
    ///
    /// The partials are built by worker threads and folded as they finish, so an index that let
    /// the last writer win would give a different population — and a different score — on two runs
    /// over an unchanged workspace.
    #[test]
    fn two_crates_disagreeing_about_result_demote_the_alias_in_either_order() {
        const ONE: &str = "pub type Result<T> = core::result::Result<T, ThisError>;";
        const TWO: &str = "pub type Result<T> = core::result::Result<T, ThatError>;";

        assert_eq!(index(&[ONE, TWO]).aliased_error("Result"), None);
        assert_eq!(index(&[TWO, ONE]).aliased_error("Result"), None);
    }

    /// Two crates that agree keep the answer, since there is nothing to disagree about.
    #[test]
    fn two_crates_agreeing_about_result_keep_the_error_they_agree_on() {
        const SAME: &str = "pub type Result<T> = core::result::Result<T, Error>;";

        assert_eq!(index(&[SAME, SAME]).aliased_error("Result"), Some("Error"));
    }

    /// A demoted alias stays demoted, rather than being revived by a later partial that happens to
    /// name one of the two errors.
    #[test]
    fn an_alias_already_demoted_is_not_revived_by_a_later_agreement() {
        let defaults = index(&[
            "pub type Result<T> = core::result::Result<T, ThisError>;",
            "pub type Result<T> = core::result::Result<T, ThatError>;",
            "pub type Result<T> = core::result::Result<T, ThisError>;",
        ]);

        assert_eq!(defaults.aliased_error("Result"), None);
    }

    /// One file holding two aliases of the same name in separate modules is the same collision.
    #[test]
    fn one_file_disagreeing_with_itself_demotes_the_alias_too() {
        let defaults = index(&["mod one { pub type Result<T> = core::result::Result<T, ThisError>; }
             mod two { pub type Result<T> = core::result::Result<T, ThatError>; }"]);

        assert_eq!(defaults.aliased_error("Result"), None);
    }

    /// `is_standard_default_callee` is only ever true for a path whose last segment is literally
    /// `default`; anything else — including a path that otherwise looks like it names the trait —
    /// is refused before the qualifier is even inspected.
    #[test]
    fn a_callee_path_not_ending_in_default_is_refused() {
        let paths = DefaultPaths::default();
        let path: Path = parse_quote!(Default::new);

        assert!(!paths.is_standard_default_callee(&path));
    }

    /// `is_standard_default_segments` mirrors `is_standard_default_callee` for a segment list
    /// that has already been split off a method name; an empty list has no last segment to
    /// split and so is refused outright, and a non-`default` final segment is refused exactly as
    /// the path form is.
    #[test]
    fn empty_or_non_default_segments_are_refused() {
        let paths = DefaultPaths::default();

        assert!(!paths.is_standard_default_segments(&[]));
        assert!(!paths.is_standard_default_segments(&["Default".to_owned(), "new".to_owned()]));
        assert!(paths.is_standard_default_segments(&["Default".to_owned(), "default".to_owned()]));
    }

    /// `item_attrs` reads the attributes of every item kind it recognizes, including the ones no
    /// other test in this module happens to declare, and falls back to an empty slice for a kind
    /// it does not (a `Verbatim` item, which `syn` produces for tokens it does not interpret).
    #[test]
    fn item_attrs_reads_every_recognized_item_kind_and_falls_back_for_the_rest() {
        let labeled = |item: syn::Item| {
            assert_eq!(
                item_attrs(&item).len(),
                1,
                "expected exactly the one `#[allow(dead_code)]` attribute"
            );
        };

        labeled(parse_quote!(
            #[allow(dead_code)]
            extern crate core;
        ));
        labeled(parse_quote!(
            #[allow(dead_code)]
            extern "C" {}
        ));
        labeled(parse_quote!(
            #[allow(dead_code)]
            macro_rules! m {
                () => {};
            }
        ));
        labeled(parse_quote!(
            #[allow(dead_code)]
            trait Alias = Clone;
        ));
        labeled(parse_quote!(
            #[allow(dead_code)]
            union U {
                a: u8,
            }
        ));

        let verbatim = syn::Item::Verbatim(proc_macro2::TokenStream::new());

        assert!(
            item_attrs(&verbatim).is_empty(),
            "an unrecognized item kind has no attributes to read"
        );
    }

    /// A `where` clause is read the same way inline bounds are: a plain type bound reports its
    /// parameter, and every other predicate shape -- a lifetime bound, a bound on a type this index
    /// cannot key by a single name, and a bound on a multi-segment path -- is passed over rather than
    /// mistaken for one.
    #[test]
    fn a_where_clause_bound_also_names_its_parameter_as_defaulted() {
        let defaults = DefaultPaths::default();
        let item: syn::ItemFn = parse_quote! {
            fn f<'a, T, U, V>() where T: Default, 'a: 'static, (U,): Default, some::U: Default, V: Clone {}
        };

        let names = standard_defaulted_parameters(&item.sig.generics, &defaults);

        assert_eq!(names, vec!["T".to_owned()]);
    }

    /// The same parameter named twice -- once inline and once in the `where` clause -- is reported
    /// only once.
    #[test]
    fn a_where_clause_repeating_an_inline_bound_is_not_reported_twice() {
        let defaults = DefaultPaths::default();
        let item: syn::ItemFn = parse_quote! {
            fn f<T: Default>() where T: Default {}
        };

        let names = standard_defaulted_parameters(&item.sig.generics, &defaults);

        assert_eq!(names, vec!["T".to_owned()]);
    }

    /// Every item kind `Defaults` indexes is skipped, along with its own derive, when a predicate
    /// controlling it does not hold -- matching the same rule every other item-level index in this
    /// crate applies.
    #[test]
    fn every_indexed_item_kind_is_skipped_when_its_predicate_does_not_hold() {
        let source = "
        #[cfg(not(unix))]
        #[derive(Debug)]
        struct AStruct;

        #[cfg(not(unix))]
        #[derive(Debug)]
        enum AnEnum { Variant }

        #[cfg(not(unix))]
        #[derive(Debug)]
        union AUnion { flag: u8 }

        struct Excluded;

        #[cfg(not(unix))]
        impl Default for Excluded {
            fn default() -> Self { Self }
        }

        #[cfg(not(unix))]
        type ExcludedResult<T = Excluded> = core::result::Result<T, Excluded>;
    ";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::parse("unix"));

        assert!(
            !defaults.lacks_default(&parse_quote!(AStruct)),
            "an excluded struct stays unknown, rather than being screened as missing a default"
        );
        assert!(!defaults.lacks_default(&parse_quote!(AnEnum)), "an excluded enum stays unknown too");
        assert!(
            !defaults.lacks_default(&parse_quote!(AUnion)),
            "an excluded union stays unknown too"
        );
        assert!(
            defaults.lacks_default(&parse_quote!(Excluded)),
            "the excluded impl must not register a `Default` this type does not really have here"
        );
        assert_eq!(
            defaults.aliased_error("ExcludedResult"),
            None,
            "the excluded alias must not be registered"
        );
    }

    /// A type alias's generic parameter can be a lifetime or a `const`, which carries no default
    /// type to read, rather than the type parameter the search is looking for.
    #[test]
    fn a_type_alias_with_no_type_parameter_reads_the_alias_target_instead() {
        let source = "struct Error; type MyResult<'a, const N: usize> = core::result::Result<u8, Error>;";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());

        assert!(defaults.lacks_default(&parse_quote!(Error)));
    }

    /// An attribute that is not `#[derive(...)]` is passed over, so a `#[derive(Default)]` sitting
    /// beside an unrelated attribute is still found.
    #[test]
    fn a_non_derive_attribute_beside_a_real_derive_does_not_hide_it() {
        let source = "#[allow(dead_code)] #[derive(Default)] struct S;";
        let defaults = Defaults::of_in(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());

        assert!(!defaults.lacks_default(&parse_quote!(S)));
    }

    /// `name_of` steps through a parenthesized type to the name inside, the same way it already
    /// does for any other wrapper it might be asked about.
    #[test]
    fn name_of_reads_through_a_parenthesized_type() {
        let ty: Type = parse_quote!((Error));

        assert_eq!(name_of(&ty), Some("Error".to_owned()));
    }

    /// `name_of` also steps through the invisible grouping `macro_rules!` hygiene can introduce,
    /// which never appears in ordinary source but is still one token away from a parenthesized type.
    #[test]
    fn name_of_reads_through_an_invisible_group() {
        let ty = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            elem: Box::new(parse_quote!(Error)),
        });

        assert_eq!(name_of(&ty), Some("Error".to_owned()));
    }

    /// `payload_name` refuses a type it cannot key by a path, a path with no generic arguments at
    /// all, and it steps over a non-type argument -- such as a lifetime -- to reach the type that
    /// follows it.
    #[test]
    fn payload_name_handles_non_path_types_bare_paths_and_non_type_arguments() {
        let tuple: Type = parse_quote!((u8, Error));
        let bare: Type = parse_quote!(Error);
        let with_lifetime: Type = parse_quote!(Result<'a, Error>);

        assert_eq!(payload_name(&tuple, 0), None);
        assert_eq!(payload_name(&bare, 0), None);
        assert_eq!(payload_name(&with_lifetime, 0), Some("Error".to_owned()));
    }
}
