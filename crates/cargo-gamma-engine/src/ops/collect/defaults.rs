// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::hash_map::Entry;

use syn::visit::{self, Visit};
use syn::{
    Attribute, File, GenericParam, Generics, ImplItem, Item, ItemEnum, ItemImpl, ItemMod, ItemStruct, ItemType, ItemUnion, Path, TraitItem,
    Type, TypeParamBound, UseTree, WherePredicate,
};

use super::default_identity::{Identities, Pending};
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
    /// Collects standard-trait aliases and local shadows from a parsed file, ignoring `cfg`.
    ///
    /// Test fixtures use this form to build a `DefaultPaths` from bare source without constructing
    /// a [`CfgSet`].
    #[cfg(test)]
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
        // #[gamma::skip(iter.first_to_last, reason = "the right operand is evaluated only for a one-segment path, whose first and last segment are identical")]
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
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }

        self.is_standard_callee_segments(&segments)
    }

    pub(super) fn is_standard_default_segments(&self, segments: &[String]) -> bool {
        let Some((method, qualifier)) = segments.split_last() else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
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

            if
            // #[gamma::skip(cond.always_true, reason = "These guards control insertion of shadow-set entries for bindings other than Default. Every meaningful shadow query either checks the literal Default, or checks a name only after name==Default. Additional non-Default entries therefore cannot change any result.", tag = "equivalent")]
            binding == "Default" {
                let _added = self.shadows.insert(binding);
            }
        } else if
        // #[gamma::skip(cond.always_true, reason = "These guards control insertion of shadow-set entries for bindings other than Default. Every meaningful shadow query either checks the literal Default, or checks a name only after name==Default. Additional non-Default entries therefore cannot change any result.", tag = "equivalent")]
        binding == "Default" {
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
            && self.is_standard_trait_name(name)
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
                // #[gamma::skip(loop.break_to_continue, reason = "without an alias prefix expanded is unchanged, so continuing the bounded loop only repeats the same failed lookup")]
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
///
/// `pub(in crate::ops::collect)` so every discovery pre-pass — the stated-value audit, the
/// type/import index, and the fused phase-one pass that drives both — can gate its own descent by
/// the same predicate the collector itself reads before ever offering a mutant, rather than each
/// pre-pass reaching its own conclusion about which items the selected build actually contains.
pub(in crate::ops::collect) fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(node) => &node.attrs,
        Item::Enum(node) => &node.attrs,
        Item::ExternCrate(node) => &node.attrs,
        Item::Fn(node) => &node.attrs,
        Item::ForeignMod(node) => &node.attrs,
        Item::Impl(node) => &node.attrs,
        Item::Macro(node) => &node.attrs,
        Item::Mod(node) => &node.attrs,
        Item::Static(node) => &node.attrs,
        Item::Struct(node) => &node.attrs,
        Item::Trait(node) => &node.attrs,
        Item::TraitAlias(node) => &node.attrs,
        Item::Type(node) => &node.attrs,
        Item::Union(node) => &node.attrs,
        Item::Use(node) => &node.attrs,
        _ => &[],
    }
}

/// Returns the outer attributes of an associated item inside an `impl` block.
///
/// Shared with the same discovery pre-passes [`item_attrs`] serves, so an inactive associated
/// constant or method is excluded from the stated-value audit and the type/import index exactly as
/// the collector already excludes it from candidate collection.
pub(in crate::ops::collect) fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(node) => &node.attrs,
        ImplItem::Fn(node) => &node.attrs,
        ImplItem::Type(node) => &node.attrs,
        ImplItem::Macro(node) => &node.attrs,
        _ => &[],
    }
}

/// Returns the outer attributes of an associated item inside a `trait` block.
///
/// Shared with the same discovery pre-passes [`item_attrs`] serves; see its documentation.
pub(in crate::ops::collect) fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(node) => &node.attrs,
        TraitItem::Fn(node) => &node.attrs,
        TraitItem::Type(node) => &node.attrs,
        TraitItem::Macro(node) => &node.attrs,
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

#[derive(Debug, Default, Clone, Copy)]
enum Completeness {
    #[default]
    Empty,
    Complete,
    Partial,
}

impl Completeness {
    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Empty, other) => other,
            (this, Self::Empty) => this,
            (Self::Complete, Self::Complete) => Self::Complete,
            (Self::Complete | Self::Partial, Self::Partial | Self::Complete) => Self::Partial,
        }
    }

    const fn supplies_negative_evidence(self) -> bool {
        matches!(self, Self::Complete)
    }
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
    identities: Option<Identities>,
    sources: super::source_identities::Sources,
    resource_namespaces: super::resource_namespaces::Namespaces,
    local_imports: HashMap<String, Option<String>>,
    aliases: HashSet<String>,
    module_path: Vec<String>,
    uncertain_types: HashSet<String>,
    alias_targets: HashMap<String, HashSet<String>>,
    /// The local bindings that decide whether `Default` means the standard trait.
    paths: DefaultPaths,

    /// The `struct`, `enum` and `union` names this workspace defines.
    ///
    /// Type aliases are deliberately absent: an alias names someone else's type, so its `Default`
    /// is that type's business and nothing here can settle it.
    defined: HashSet<String>,

    /// Those of them that derive or implement the standard `Default` trait.
    defaulted: HashSet<String>,

    /// Unqualified names whose definitions disagree about implementing `Default`.
    ///
    /// Uniform collisions remain useful evidence: two `Config` definitions that both implement
    /// `Default` prove the property whichever one a bare name denotes, and two that both lack it
    /// prove the opposite. Only a mixed collision is undecidable.
    ambiguous: HashSet<String>,

    /// Names also introduced by aliases or re-exports, whose target declaration is unresolved.
    indirect: HashSet<String>,

    /// Names declared with type or const parameters.
    ///
    /// A bare use can fill omitted parameters from declaration defaults whose instantiated bounds
    /// are not visible at the use site, so the name alone is not positive `Default` evidence.
    generic: HashSet<String>,
    unproven_generic_types: HashSet<String>,

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

    /// Whether the indexed declarations are complete enough to supply negative evidence.
    completeness: Completeness,

    /// Lexical module nesting while visiting implementations.
    module_depth: usize,
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
    /// Builds declaration evidence labeled with the physical source that supplied it.
    #[must_use]
    pub fn of_source_in(file: &File, cfg: &CfgSet, source: &str) -> Self {
        let mut index = Self::of_in(file, cfg);
        index.sources.insert(source, Pending::of(file, cfg));
        index.resource_namespaces.label(source);
        index
    }

    /// Resolves the labeled evidence using unique, active source/module paths per crate root.
    ///
    /// Each tuple contains the source label, crate-root label, and logical module path.
    /// Sources without a unique module path must be omitted.
    pub fn bind_sources(&mut self, modules: &[(String, String, Vec<String>)]) {
        self.sources.bind(modules);
        self.resource_namespaces.bind(modules);
    }

    /// Adds the report spelling of an already-labeled physical source.
    pub fn alias_source(&mut self, source: &str, report: &str) {
        if source != report {
            self.sources.alias(source, report);
            self.resource_namespaces.alias(source, report);
        }
    }

    pub(crate) fn has_labeled_sources(&self) -> bool {
        self.sources.bound
    }

    pub(crate) fn resource_namespace_transparent(&self, source: &str, inline: &[String]) -> bool {
        self.resource_namespaces.transparent(source, inline)
    }

    pub(super) fn resource_sources(&self) -> &super::resource_namespaces::Namespaces {
        &self.resource_namespaces
    }

    /// Binds Cargo-resolved dependency evidence for bounded resource analysis.
    ///
    /// Missing or conflicting resolved edges
    /// never establish an external API contract.
    pub fn bind_resource_dependencies(&mut self, dependencies: &[super::ResourceDependency]) {
        self.resource_namespaces.dependencies(dependencies);
    }

    /// Shares span-free source evidence before binding a combined workspace layout.
    ///
    /// This does not share nominal type inference. Conflicting source evidence
    /// remains unknown, and dependency edges still require resolved identities.
    pub fn extend_resource_evidence(&mut self, other: &Self) {
        self.resource_namespaces.extend(&other.resource_namespaces);
    }

    pub(crate) fn source_default(&self, source: &str, inline: &[String], path: &[String]) -> Option<bool> {
        self.is_complete().then(|| self.sources.default_for(source, inline, path)).flatten()
    }

    pub(crate) fn source_is_enum(&self, source: &str, inline: &[String], path: &[String]) -> bool {
        self.sources.is_enum(source, inline, path)
    }

    pub(crate) fn source_defines(&self, source: &str, inline: &[String], path: &[String]) -> bool {
        self.sources.defines(source, inline, path)
    }

    pub(crate) fn source_same_type(&self, source: &str, inline: &[String], left: &[String], right: &[String]) -> bool {
        self.sources.same_type(source, inline, left, right)
    }

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
            identities: Some(Identities::of(file, cfg)),
            resource_namespaces: super::resource_namespaces::Namespaces::of(file, cfg),
            paths: DefaultPaths::of_in(file, cfg),
            cfg: cfg.clone(),
            completeness: Completeness::Complete,
            ..Self::default()
        };

        index.visit_file(file);
        index.note_default_aliases();
        index
    }

    /// Builds only the file-local path-resolution evidence, without negative type evidence.
    ///
    /// A standalone file cannot prove that a type lacks an implementation written in another
    /// file, so convenience collection stays optimistic until a workspace-wide index is available.
    #[must_use]
    pub fn optimistic_of_in(file: &File, cfg: &CfgSet) -> Self {
        let mut index = Self::of_in(file, cfg);
        index.completeness = Completeness::Partial;
        index
    }

    /// Returns whether the workspace defines this type and gives it no `Default`.
    ///
    /// Bare names are compared across the workspace, so two crates can both define a `Config`.
    /// Qualified paths stay unknown because this syntax-only index cannot prove which declaration
    /// they name. The union supplies negative evidence only when none of the bare-name definitions
    /// implements `Default`.
    #[must_use]
    pub fn lacks_default(&self, ty: &Type) -> bool {
        let Some(name) = unqualified_name_of(ty) else {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        };

        self.lacks_default_name(&name)
    }

    /// Returns whether this complete package index defines `name` without `Default`.
    #[must_use]
    pub(crate) fn lacks_default_name(&self, name: &str) -> bool {
        self.lacks_error_default(name)
    }

    /// Exact source-local paths are useful only before indexes with unknown module roots merge.
    pub(crate) fn lacks_local_path(&self, path: &[String]) -> bool {
        if !self.is_complete() {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        if let Some(identities) = &self.identities {
            return identities.default_for(path) == Some(false);
        }
        path.first().is_some_and(|root| root == "crate") && path.last().is_some_and(|name| self.lacks_error_default(name))
    }

    pub(crate) fn has_single_source_evidence(&self) -> bool {
        self.is_complete() && self.identities.is_some()
    }

    pub(crate) fn has_local_default(&self, path: &[String]) -> bool {
        self.identities.as_ref().and_then(|identities| identities.default_for(path)) == Some(true)
    }

    pub(crate) fn local_default(&self, path: &[String]) -> Option<bool> {
        self.identities.as_ref()?.default_for(path)
    }

    fn note_default_aliases(&mut self) {
        // Initial names plus all alias-target edges and at most one edge per local import bound
        // the popped work, including duplicate names. This assertion is test-only.
        #[cfg(test)]
        let mut remaining = self.defaulted.len() + self.alias_targets.values().map(HashSet::len).sum::<usize>() + self.local_imports.len();
        self.note_default_aliases_with(|_| {
            #[cfg(test)]
            {
                assert!(remaining > 0, "each default alias edge can be enqueued only once");
                remaining -= 1;
            }
        });
    }

    /// Reports each popped name before deduplication, including repeated names, so deterministic
    /// progress tests observe queued work rather than only newly discovered aliases.
    fn note_default_aliases_with(&mut self, mut on_visit: impl FnMut(&str)) {
        let mut pending = self.defaulted.iter().cloned().collect::<Vec<_>>();
        let mut seen = HashSet::default();
        while let Some(name) = pending.pop() {
            on_visit(&name);
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(targets) = self.alias_targets.get(&name) {
                for target in targets {
                    let _uncertain = self.uncertain_types.insert(target.clone());
                    pending.push(target.clone());
                }
            }
            if let Some(Some(target)) = self.local_imports.get(&name)
                && target != &name
            {
                let _uncertain = self.uncertain_types.insert(target.clone());
                pending.push(target.clone());
            }
        }
    }

    /// Folds another index into this one.
    ///
    /// Used to put together what several threads each learned from the files they parsed, in
    /// whatever order they finished. Every field resolves a collision without reference to that
    /// order, because the order is not reproducible and the result decides which mutants exist.
    ///
    /// `defined` and `defaulted` are unions. A name defined in both inputs becomes ambiguous only
    /// when the inputs disagree about whether it implements `Default`; uniform positive and
    /// negative collisions preserve their evidence. An empty accumulator adopts the other index's
    /// completeness, while any merge involving a partial index remains partial.
    /// `result_error` cannot union, because its values are single names rather than membership, so
    /// a key two files disagree about is demoted to "unknown" instead — see [`merge_alias`].
    pub fn absorb(&mut self, other: Self) {
        self.sources.absorb(other.sources);
        self.resource_namespaces.absorb(other.resource_namespaces);
        self.identities = if matches!(self.completeness, Completeness::Empty) {
            other.identities
        } else {
            None
        };
        self.completeness = self.completeness.merge(other.completeness);
        let collisions = self.defined.intersection(&other.defined).cloned().collect::<Vec<_>>();
        for name in collisions {
            if
            // #[gamma::skip(logical.or_remove_left, reason = "An existing ambiguity is already retained in self.ambiguous, and every incoming ambiguity is unconditionally extended immediately after the collision loop. Omitting or conjoining these redundant collision tests does not change the final set; default-status disagreement is the only new ambiguity this loop needs to add.", tag = "equivalent")]
            (
                // #[gamma::skip(logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "An existing ambiguity is already retained in self.ambiguous, and every incoming ambiguity is unconditionally extended immediately after the collision loop. Omitting or conjoining these redundant collision tests does not change the final set; default-status disagreement is the only new ambiguity this loop needs to add.", tag = "equivalent")]
                self.ambiguous.contains(&name) || other.ambiguous.contains(&name)
            ) || self.defaulted.contains(&name) != other.defaulted.contains(&name)
            {
                let _inserted = self.ambiguous.insert(name);
            }
        }
        self.ambiguous.extend(other.ambiguous);
        self.indirect.extend(other.indirect);
        self.defined.extend(other.defined);
        self.defaulted.extend(other.defaulted);
        self.generic.extend(other.generic);
        self.unproven_generic_types.extend(other.unproven_generic_types);
        self.aliases.extend(other.aliases);
        self.uncertain_types.extend(other.uncertain_types);
        for (name, targets) in other.alias_targets {
            self.alias_targets.entry(name).or_default().extend(targets);
        }
        for (name, target) in other.local_imports {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
            merge_alias(&mut self.local_imports, name, target);
        }

        for (alias, error) in other.result_error {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
            merge_alias(&mut self.result_error, alias, error);
        }
        self.note_default_aliases();
    }

    /// Returns whether a type named by the index has no `Default`, given only its name.
    ///
    /// The by-name form exists for an error type reached through an alias, where what was recorded
    /// is a name rather than a syntax node.
    #[must_use]
    pub fn lacks_error_default(&self, name: &str) -> bool {
        if !self.completeness.supplies_negative_evidence() || self.indirect.contains("*") {
            return
            // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
            false;
        }
        // At most one step per local import plus the terminal or repeated name.
        #[cfg(test)]
        let mut remaining = self.local_imports.len() + 1;
        self.lacks_error_default_with(name, |_| {
            #[cfg(test)]
            {
                assert!(remaining > 0, "local import lookup must stop at the first repeated name");
                remaining -= 1;
            }
        })
    }

    /// Reports each name lookup before cycle deduplication, including the terminating repeat,
    /// so tests can assert finite import traversal without changing evidence policy.
    fn lacks_error_default_with(&self, name: &str, mut on_visit: impl FnMut(&str)) -> bool {
        let mut name = name;
        let mut seen = HashSet::default();
        while {
            on_visit(name);
            seen.insert(name)
        } {
            if self.aliases.contains(name) || self.uncertain_types.contains(name) {
                return
                // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                false;
            }
            if self.indirect.contains(name) {
                let Some(Some(target)) = self.local_imports.get(name) else {
                    return
                    // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                    false;
                };
                if target != name {
                    if
                    // #[gamma::skip(logical.and_remove_left, reason = "Both defaulted and ambiguous names come from defined declarations; either already implies defined membership.", tag = "equivalent")]
                    self.defined.contains(name) && (self.defaulted.contains(name) || self.ambiguous.contains(name)) {
                        return
                        // #[gamma::skip(return_value.default, reason = "The inferred type is bool, whose Default value is already false.", tag = "equivalent")]
                        false;
                    }
                    name = target;
                    continue;
                }
            }
            return self.defined.contains(name) && !self.defaulted.contains(name);
        }
        false
    }

    /// Returns whether the workspace declares a type with this unqualified name.
    #[must_use]
    pub fn defines(&self, name: &str) -> bool {
        self.defined.contains(name)
    }

    /// Returns positive workspace evidence that the named type implements `Default`.
    ///
    /// Evidence is limited to an unambiguous, bare, non-generic type name indexed from a direct
    /// standard `Default` derive or implementation. `false` means only that positive evidence is
    /// unavailable: qualified, absolute, generic, ambiguous, and unknown paths remain undecided
    /// rather than proving that the type lacks `Default`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn has_default(&self, ty: &Type) -> bool {
        unqualified_name_of(ty).is_some_and(|name| self.has_default_name(&name))
    }

    /// Returns positive evidence for one non-generic type name in this package.
    pub(crate) fn has_default_name(&self, name: &str) -> bool {
        !self.ambiguous.contains(name)
            && !self.generic.contains(name)
            && (self.defaulted.contains(name) || (!self.completeness.supplies_negative_evidence() && self.defined.contains(name)))
    }

    /// Returns whether a declaration in this package explicitly implements `Default`.
    pub(crate) fn declares_default_name(&self, name: &str) -> bool {
        self.completeness.supplies_negative_evidence()
            && !self.ambiguous.contains(name)
            && !self.generic.contains(name)
            && self.defaulted.contains(name)
    }

    pub(crate) fn declares_generic_default_name(&self, name: &str) -> bool {
        self.completeness.supplies_negative_evidence()
            && !self.ambiguous.contains(name)
            && self.generic.contains(name)
            && self.defaulted.contains(name)
    }

    pub(crate) fn generic_default_is_unproven(&self, name: &str) -> bool {
        self.unproven_generic_types.contains(name) || self.uncertain_types.contains(name)
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.completeness.supplies_negative_evidence()
    }

    /// Prevents a partial package index from supplying negative declaration evidence.
    pub fn mark_incomplete(&mut self) {
        self.completeness = Completeness::Partial;
    }

    /// Returns the error type a `Result` alias fixed, given the alias's name.
    ///
    /// `None` covers both "no such alias" and "the workspace disagrees about this one", which are
    /// the same answer to the only question asked of it: nothing may be screened on this name.
    #[must_use]
    pub fn aliased_error(&self, alias: &str) -> Option<&str> {
        self.result_error.get(alias)?.as_deref()
    }

    fn note_derive(&mut self, name: &str, generics: &Generics, attributes: &[syn::Attribute]) {
        let defaulted = derives_default(attributes, &self.paths);
        if attributes.iter().any(|attribute| attribute.path().is_ident("cfg_attr")) {
            let _inserted = self.indirect.insert(name.to_owned());
            let _inserted = self.uncertain_types.insert(name.to_owned());
        }
        if self.defined.contains(name) && self.defaulted.contains(name) != defaulted {
            let _inserted = self.ambiguous.insert(name.to_owned());
        }
        let _inserted = self.defined.insert(name.to_owned());

        if defaulted {
            let _inserted = self.defaulted.insert(name.to_owned());
        }
        if !generics.params.is_empty() {
            let _inserted = self.generic.insert(name.to_owned());
        }
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Defaults {
    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.generics, &node.attrs);

        // #[gamma::skip(stmt.delete_call, reason = "`Defaults` has no callbacks for fields or field types, so descending cannot change this visitor's state")]
        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.generics, &node.attrs);

        // #[gamma::skip(stmt.delete_call, reason = "`Defaults` has no callbacks for variants or their contents, so descending cannot change this visitor's state")]
        visit::visit_item_enum(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast ItemUnion) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }

        self.note_derive(&node.ident.to_string(), &node.generics, &node.attrs);

        // #[gamma::skip(stmt.delete_call, reason = "`Defaults` has no callbacks for union fields or types, so descending cannot change this visitor's state")]
        visit::visit_item_union(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }

        if let Some((path, _for)) = &node.trait_
            && self.paths.is_standard_trait(path)
            && (self.module_depth == 0 || path.segments.len() > 1)
            && let Some(name) = name_of(&node.self_ty)
        {
            if !node.generics.params.is_empty() || node.generics.where_clause.is_some() {
                let _inserted = self.unproven_generic_types.insert(name.clone());
            }
            let _inserted = self.defaulted.insert(name);
        }

        // #[gamma::skip(stmt.delete_call, reason = "impl blocks cannot contain nested declarations handled by `Defaults`, so descent is unobservable")]
        visit::visit_item_impl(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }

        self.module_depth += 1;
        self.module_path.push(node.ident.to_string());
        visit::visit_item_mod(self, node);
        let _module = self.module_path.pop();
        self.module_depth -= 1;
    }

    fn visit_item_type(&mut self, node: &'ast ItemType) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }
        let _inserted = self.indirect.insert(node.ident.to_string());
        let _inserted = self.aliases.insert(node.ident.to_string());
        if let Some(target) = name_of(&node.ty) {
            let _target = self.alias_targets.entry(node.ident.to_string()).or_default().insert(target);
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
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                merge_alias(&mut self.result_error, node.ident.to_string(), Some(error));
            }
        }

        // #[gamma::skip(stmt.delete_call, reason = "`Defaults` has no callbacks for nodes nested in an alias target, so descent is unobservable")]
        visit::visit_item_type(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        fn names(tree: &syn::UseTree, into: &mut HashSet<String>) {
            match tree {
                syn::UseTree::Path(path) =>
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                {
                    names(&path.tree, into);
                }
                syn::UseTree::Name(name) => {
                    let _inserted = into.insert(name.ident.to_string());
                }
                syn::UseTree::Rename(rename) => {
                    let _inserted = into.insert(rename.rename.to_string());
                }
                syn::UseTree::Glob(_) => {
                    let _inserted = into.insert("*".to_owned());
                }
                syn::UseTree::Group(group) => {
                    for tree in &group.items {
                        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                        names(tree, into);
                    }
                }
            }
        }
        fn targets(tree: &syn::UseTree, prefix: &mut Vec<String>, into: &mut Vec<(String, Vec<String>)>) {
            match tree {
                syn::UseTree::Path(path) => {
                    prefix.push(path.ident.to_string());
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    targets(&path.tree, prefix, into);
                    let _last = prefix.pop();
                }
                syn::UseTree::Name(name) => {
                    let mut target = prefix.clone();
                    if name.ident != "self" {
                        target.push(name.ident.to_string());
                    }
                    if let Some(introduced) = target.last() {
                        into.push((introduced.clone(), target));
                    }
                }
                syn::UseTree::Rename(rename) => {
                    let mut target = prefix.clone();
                    if rename.ident != "self" {
                        target.push(rename.ident.to_string());
                    }
                    into.push((rename.rename.to_string(), target));
                }
                syn::UseTree::Group(group) => {
                    for tree in &group.items {
                        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                        targets(tree, prefix, into);
                    }
                }
                syn::UseTree::Glob(_) => {}
            }
        }
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }
        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
        names(&node.tree, &mut self.indirect);
        let mut imports = Vec::new();
        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
        targets(&node.tree, &mut Vec::new(), &mut imports);
        for (name, path) in imports {
            let local = node.leading_colon.is_none()
                && path.first().is_some_and(|root| {
                    matches!(root.as_str(), "crate" | "self" | "super")
                        || self
                            .identities
                            .as_ref()
                            .is_some_and(|identities| identities.local_module(&self.module_path, root))
                });
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
            merge_alias(&mut self.local_imports, name, local.then(|| path.last().cloned()).flatten());
        }
    }
}

/// Returns whether a `#[derive(...)]` on an item lists `Default`.
pub(super) fn derives_default(attributes: &[syn::Attribute], paths: &DefaultPaths) -> bool {
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
        // #[gamma::skip(option.none_to_some, reason = "`Type::default()` is an empty `Verbatim`, which every caller reduces back to no name")]
        _ => None,
    }
}

/// The name of a bare, non-generic type path, which can safely refer to the workspace-wide name
/// index.
fn unqualified_name_of(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) if
        // #[gamma::skip(match_guard.always_true, reason = "These sites independently require Path::get_ident/is_ident after the qself guard. On parsed syn paths, <Owner>::Name has a leading_colon, so get_ident/is_ident rejects it independently; <Owner as Trait>::Name contains at least two segments and is also rejected independently. The prior simple qualified-name negative assertions therefore never distinguished this guard. The proof is for parsed production inputs, not artificially inconsistent manually constructed paths.", tag = "equivalent")]
        path.qself.is_none() => path.path.get_ident().map(ToString::to_string),
        Type::Paren(paren) => unqualified_name_of(&paren.elem),
        Type::Group(group) => unqualified_name_of(&group.elem),
        _ => None,
    }
}

/// The name of a type's `index`th generic argument.
fn payload_name(ty: &Type, index: usize) -> Option<String> {
    let Type::Path(path) = ty else {
        return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
    };

    let syn::PathArguments::AngleBracketed(args) = &
    // #[gamma::skip(try.propagate_to_unwrap, reason = "payload_name is called on a source-parsed alias target. A parsed Type::Path has at least one segment, so segments.last() cannot fail on this call path.", tag = "equivalent")]
    path.path.segments.last()?.arguments
    else {
        return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use proc_macro2::TokenStream;
    use syn::{parse_file, parse_quote};

    use super::*;

    #[test]
    fn default_implementations_through_renamed_imports_prevent_false_negative_target_evidence() {
        for source in [
            "struct Error; use self::Error as Alias; impl Default for Alias { fn default() -> Self { Error } }",
            "struct Error; use self::Error as Middle; use self::Middle as Alias;
             impl Default for Alias { fn default() -> Self { Error } }",
        ] {
            let defaults = super::Defaults::of(&syn::parse_file(source).unwrap());
            for name in ["Alias", "Error"] {
                assert!(!defaults.lacks_error_default(name), "{source}: {name}");
            }
            assert!(defaults.generic_default_is_unproven("Error"));
        }
        let mut defaults = super::Defaults::of(&syn::parse_file("struct Error;").unwrap());
        assert!(defaults.lacks_error_default("Error"));
        defaults.absorb(super::Defaults::of(
            &syn::parse_file("use crate::Error as Alias; impl Default for Alias { fn default() -> Self { Self } }").unwrap(),
        ));
        assert!(!defaults.lacks_error_default("Error"));

        let defaults = super::Defaults::of(
            &syn::parse_file(
                "struct Error; mod implementation {
                    use crate::Error; impl core::default::Default for Error { fn default() -> Self { Self } }
                 }",
            )
            .unwrap(),
        );
        assert!(defaults.has_default_name("Error"));
        assert!(!defaults.generic_default_is_unproven("Error"));
    }

    #[test]
    fn imported_error_names_follow_unambiguous_local_targets_but_not_conflicting_defaults() {
        let mut defaults = super::Defaults::of(&syn::parse_file("struct Error; use self::Error as Alias;").unwrap());
        assert!(defaults.lacks_error_default("Alias"));
        defaults.defined.insert("Alias".into());
        defaults.defaulted.insert("Alias".into());
        assert!(!defaults.lacks_error_default("Alias"));
        defaults.defaulted.remove("Alias");
        defaults.ambiguous.insert("Alias".into());
        assert!(!defaults.lacks_error_default("Alias"));
    }

    #[test]
    fn public_reexports_block_negative_evidence_for_each_introduced_name() {
        let file = syn::parse_file(
            "struct Named; struct Renamed; struct Grouped; pub use external::{Named, Original as Renamed, nested::{Grouped}};",
        )
        .unwrap();
        let index = super::Defaults::of(&file);
        for name in ["Named", "Renamed", "Grouped"] {
            assert!(!index.lacks_default(&syn::parse_str(name).unwrap()), "{name}");
        }
        let file = syn::parse_file("struct Any; pub use external::*;").unwrap();
        assert!(!super::Defaults::of(&file).lacks_default(&syn::parse_quote!(Any)));
        let mut index = super::Defaults::of(&syn::parse_file("struct Missing;").unwrap());
        index.absorb(super::Defaults::default());
        assert!(index.lacks_default(&syn::parse_quote!(Missing)));
    }

    #[test]
    fn source_aliases_preserve_both_nominal_and_resource_evidence_without_poisoning_self_aliases() {
        for report in ["src\\lib.rs", "display\\lib.rs"] {
            let mut defaults = Defaults::of_source_in(
                &parse_file("struct Record; enum Choice { A }").unwrap(),
                &CfgSet::unconditional(),
                "src\\lib.rs",
            );
            defaults.bind_sources(&[("src\\lib.rs".into(), "src\\lib.rs".into(), Vec::new())]);
            defaults.alias_source("src\\lib.rs", report);
            assert_eq!(defaults.source_default(report, &[], &["Record".into()]), Some(false));
            assert!(defaults.resource_namespace_transparent(report, &[]));
            assert!(defaults.source_is_enum(report, &[], &["Choice".into()]));
            assert!(!defaults.source_is_enum(report, &[], &["Record".into()]));
        }
    }

    #[test]
    fn extending_resource_evidence_shares_namespaces_but_not_nominal_type_claims() {
        let cfg = CfgSet::unconditional();
        let mut root = Defaults::of_source_in(&parse_file("mod child;").unwrap(), &cfg, "lib.rs");
        let child = Defaults::of_source_in(&parse_file("struct Record;").unwrap(), &cfg, "child.rs");
        root.extend_resource_evidence(&child);
        root.bind_sources(&[
            ("lib.rs".into(), "lib.rs".into(), Vec::new()),
            ("child.rs".into(), "lib.rs".into(), vec!["child".into()]),
        ]);
        assert!(root.resource_namespace_transparent("child.rs", &[]));
        assert_eq!(root.source_default("child.rs", &[], &["Record".into()]), None);
    }

    #[test]
    fn default_implementations_through_alias_chains_make_every_underlying_name_uncertain() {
        let file = parse_file(
            "struct Real; type Middle = Real; type Outer = Middle;
             impl Default for Outer { fn default() -> Self { Real } }",
        )
        .unwrap();
        let defaults = Defaults::of(&file);
        for name in ["Middle", "Real"] {
            assert!(defaults.generic_default_is_unproven(name), "{name}");
            assert!(!defaults.lacks_error_default(name), "{name}");
        }
        let mut combined = Defaults::of(&parse_file("struct Real; type Middle = Real; type Outer = Middle;").unwrap());
        combined.absorb(Defaults::of(
            &parse_file("impl Default for Outer { fn default() -> Self { Real } }").unwrap(),
        ));
        assert!(combined.generic_default_is_unproven("Real"));
        assert!(!combined.lacks_error_default("Real"));
    }

    #[test]
    fn default_alias_worklist_skips_repeated_names_without_abandoning_pending_siblings() {
        let mut defaults = Defaults {
            defaulted: HashSet::from_iter(["Root".into()]),
            alias_targets: HashMap::from_iter([
                ("Root".into(), HashSet::from_iter(["Deferred".into()])),
                ("Cycle".into(), HashSet::from_iter(["Cycle".into()])),
            ]),
            local_imports: HashMap::from_iter([("Root".into(), Some("Cycle".into()))]),
            ..Defaults::default()
        };
        let expected = ["Root", "Cycle", "Cycle", "Deferred"];
        let mut visits = 0;
        defaults.note_default_aliases_with(|name| {
            assert_eq!(Some(name), expected.get(visits).copied(), "alias visit {visits}");
            visits += 1;
        });
        assert_eq!(visits, expected.len());
        assert_eq!(defaults.uncertain_types, HashSet::from_iter(["Cycle".into(), "Deferred".into()]));
    }

    #[test]
    fn partial_and_merged_indexes_do_not_claim_single_source_or_foreign_path_negative_evidence() {
        let mut defaults = Defaults::of(&parse_file("struct Missing;").unwrap());
        assert!(defaults.has_single_source_evidence());
        defaults.mark_incomplete();
        assert!(!defaults.has_single_source_evidence());
        assert!(!defaults.lacks_local_path(&["crate".into(), "Missing".into()]));
        let mut defaults = Defaults::of(&parse_file("struct Missing;").unwrap());
        defaults.absorb(Defaults::of(&parse_file("").unwrap()));
        assert!(!defaults.has_single_source_evidence());
        assert_eq!(defaults.local_default(&["Missing".into()]), None);
        assert!(defaults.lacks_local_path(&["crate".into(), "Missing".into()]));
        assert!(!defaults.lacks_local_path(&["external".into(), "Missing".into()]));
        assert!(!defaults.lacks_default(&parse_quote!(<Owner>::Missing)));
    }

    #[test]
    fn an_empty_accumulator_adopts_the_first_sources_exact_declaration_identity() {
        let mut defaults = Defaults::default();
        defaults.absorb(Defaults::of(&parse_file("struct Missing;").unwrap()));
        assert!(defaults.has_single_source_evidence());
        assert_eq!(defaults.local_default(&["Missing".into()]), Some(false));
    }

    #[test]
    fn absorbed_conditional_and_generic_impl_uncertainty_remain_independent() {
        let mut defaults = Defaults::of(&parse_file("struct Existing;").unwrap());
        defaults.absorb(Defaults::of(
            &parse_file("#[cfg_attr(feature = \"maybe\", derive(Default))] struct Conditional;").unwrap(),
        ));
        assert!(defaults.generic_default_is_unproven("Conditional"));
        defaults.absorb(Defaults::of(
            &parse_file(
                "struct Generic<T>(core::marker::PhantomData<T>);
             impl<T> Default for Generic<T> { fn default() -> Self { Self(core::marker::PhantomData) } }",
            )
            .unwrap(),
        ));
        assert!(defaults.generic_default_is_unproven("Generic"));
    }

    #[test]
    fn generic_impl_uncertainty_checks_parameters_and_where_clauses_separately() {
        let defaults = Defaults::of(
            &parse_file(
                "struct Plain; impl Default for Plain { fn default() -> Self { Plain } }
             struct Constrained; impl Default for Constrained where u8: Copy { fn default() -> Self { Constrained } }",
            )
            .unwrap(),
        );
        assert!(!defaults.generic_default_is_unproven("Plain"));
        assert!(defaults.generic_default_is_unproven("Constrained"));
    }

    #[test]
    fn local_import_chains_keep_uniform_negative_evidence_and_reject_cycles() {
        let defaults = Defaults::of(
            &parse_file(
                "struct Error; use self::Error as First; use self::First as Second;
             mod one { pub struct Alias; }
             mod two { pub struct Other; use self::Other as Alias; }
             use self::CycleB as CycleA; use self::CycleA as CycleB;",
            )
            .unwrap(),
        );
        assert!(defaults.lacks_error_default("Second"));
        assert!(defaults.lacks_error_default("Alias"));
        assert!(!defaults.lacks_error_default("CycleA"));
        assert!(!defaults.lacks_error_default("Unknown"));
    }

    #[test]
    fn local_import_cycles_stop_on_the_first_revisited_name() {
        let defaults = Defaults::of(&parse_file("use self::Second as First; use self::First as Second;").unwrap());
        let expected = ["First", "Second", "First"];
        let mut visits = 0;
        assert!(!defaults.lacks_error_default_with("First", |name| {
            assert_eq!(Some(name), expected.get(visits).copied(), "import visit {visits}");
            visits += 1;
        }));
        assert_eq!(visits, expected.len());
    }

    #[test]
    fn grouped_self_imports_keep_the_introduced_module_and_alias_names_exact() {
        let defaults = Defaults::of(
            &parse_file(
                "mod errors { pub struct Error; }
             use self::errors::{self, self as local, Error as First, Error as Second};",
            )
            .unwrap(),
        );
        assert_eq!(defaults.local_imports.get("errors"), Some(&Some("errors".into())));
        assert_eq!(defaults.local_imports.get("local"), Some(&Some("errors".into())));
        assert_eq!(defaults.local_imports.get("First"), Some(&Some("Error".into())));
        assert_eq!(defaults.local_imports.get("Second"), Some(&Some("Error".into())));
        assert!(defaults.lacks_error_default("First"));
        assert!(defaults.lacks_error_default("Second"));
    }

    #[test]
    fn absolute_external_imports_and_configured_out_imports_cannot_override_local_type_evidence() {
        let defaults = Defaults::of(
            &parse_file(
                "mod errors { pub struct Error; }
             use ::errors::Error as External;
             use errors::Error as Local;
             mod outer { mod errors { pub struct NestedError; } use errors::NestedError as Nested; }",
            )
            .unwrap(),
        );
        assert!(!defaults.lacks_error_default("External"));
        assert!(defaults.lacks_error_default("Local"));
        assert!(defaults.lacks_error_default("Nested"));
        let defaults = Defaults::of_in(
            &parse_file("struct Error; #[cfg(any())] use external::Other as Error;").unwrap(),
            &CfgSet::parse(""),
        );
        assert!(defaults.lacks_error_default("Error"));
    }

    #[test]
    fn nested_module_imports_resolve_roots_that_do_not_exist_at_the_crate_root() {
        let defaults = Defaults::of(&parse_file("mod outer { mod errors { pub struct Error; } use errors::Error as Nested; }").unwrap());
        assert_eq!(defaults.local_imports.get("Nested"), Some(&Some("Error".into())));
        assert!(defaults.lacks_error_default("Nested"));
    }

    #[test]
    fn enum_declarations_supply_default_evidence_for_both_positive_and_negative_cases() {
        let defaults = Defaults::of(&parse_file("enum Missing { A } #[derive(Default)] enum Ready { #[default] A }").unwrap());
        assert!(defaults.defines("Missing"));
        assert!(defaults.lacks_default(&parse_quote!(Missing)));
        assert!(defaults.declares_default_name("Ready"));
        assert!(!defaults.lacks_default(&parse_quote!(Ready)));
    }

    #[test]
    fn single_source_negative_evidence_requires_the_qualified_declaration() {
        let defaults = Defaults::of(
            &parse_file(
                "struct Value;
                 mod missing { pub struct Value; }
                 mod ready { #[derive(Default)] pub struct Value; }",
            )
            .unwrap(),
        );
        for path in [vec!["crate", "Value"], vec!["crate", "missing", "Value"]] {
            let path = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(defaults.lacks_local_path(&path), "{path:?}");
        }
        for path in [vec!["crate", "ready", "Value"], vec!["crate", "unresolved", "Value"]] {
            let path = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!defaults.lacks_local_path(&path), "{path:?}");
        }
    }

    #[test]
    fn unrelated_import_bindings_preserve_standard_default_and_later_where_bounds() {
        for text in ["use core as root;", "use foreign::Other;", "use core::default as defaults;"] {
            let paths = DefaultPaths::of(&parse_file(text).unwrap());
            assert!(paths.is_standard_trait(&parse_quote!(Default)), "{text}");
            assert!(paths.is_standard_default_callee(&parse_quote!(Default::default)), "{text}");
        }
        let item: syn::ItemFn = parse_quote!(
            fn f<'a, const N: usize, T, U, V>()
            where
                'a: 'static,
                (T,): Clone,
                T::Item: Clone,
                U: Clone + Default,
                V: Default,
            {
            }
        );
        assert_eq!(
            standard_defaulted_parameters(&item.sig.generics, &DefaultPaths::default()),
            ["U", "V"]
        );
        let index = Defaults::of(&parse_file("#[derive(Default)] struct Ready;").unwrap());
        assert!(index.has_default(&parse_quote!(Ready)));
        assert!(!index.has_default(&parse_quote!(<T>::Ready)));
    }

    #[test]
    fn generic_default_declarations_require_generic_unambiguous_repeated_evidence() {
        let repeated = Defaults::of(
            &parse_file(
                "#[derive(Default)] struct Plain; #[derive(Default)] struct Plain; \
             #[derive(Default)] struct Generic<T>(T); #[derive(Default)] struct Generic<T>(T);",
            )
            .unwrap(),
        );
        assert!(repeated.has_default_name("Plain"));
        assert!(!repeated.declares_generic_default_name("Plain"));
        assert!(repeated.declares_generic_default_name("Generic"));
        let conflicting = Defaults::of(&parse_file("#[derive(Default)] struct Generic<T>(T); struct Generic<T>(T);").unwrap());
        assert!(!conflicting.declares_generic_default_name("Generic"));
        let mut combined = repeated;
        combined.absorb(conflicting);
        assert!(!combined.declares_generic_default_name("Generic"));
        assert!(combined.has_default_name("Plain"));
    }

    #[test]
    fn default_path_matrix_separates_prelude_aliases_modules_and_custom_shadows() {
        for (source, trait_path, standard, fallback, derive) in [
            ("", "Default", true, true, true),
            ("trait Default {}", "Default", false, true, true),
            ("trait Other {}", "Default", true, true, true),
            ("use other::Default;", "Default", false, true, false),
            ("use core::default::Default as D;", "D", true, true, true),
            ("use core::default::Default as D; struct Default;", "D", true, false, true),
            ("use core::default as d;", "d::Default", true, true, true),
            ("use core::default::{self as d};", "d::Default", true, true, true),
            ("use other::default as d;", "d::Default", false, false, true),
            ("use core::default as Default;", "Default::Default", true, false, true),
            ("mod child { trait Default{} }", "Default", true, true, true),
            (
                "#[cfg(not(unix))] struct Default; use core::default::Default as D;",
                "D",
                true,
                true,
                true,
            ),
        ] {
            let paths = DefaultPaths::of_in(&parse_file(source).unwrap(), &CfgSet::parse("unix"));
            let path = syn::parse_str::<Path>(trait_path).unwrap();
            assert_eq!(paths.is_standard_trait(&path), standard, "{source} / {trait_path}");
            assert_eq!(paths.is_fallback_trait(&path), fallback, "{source} / {trait_path}");
            assert_eq!(paths.is_standard_derive(&parse_quote!(Default)), derive, "{source}");
            let callee = syn::parse_str::<Path>(&format!("{trait_path}::default")).unwrap();
            assert_eq!(paths.is_standard_default_callee(&callee), standard, "{source}");
            let segments = callee.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
            assert_eq!(paths.is_standard_default_segments(&segments), standard, "{source}");
            assert!(!paths.is_standard_default_callee(&parse_quote!(helper)));
            assert!(!paths.is_standard_default_segments(&["D".into(), "new".into()]));
        }
        let generics: syn::Generics = syn::parse_quote!(<'a, const N: usize, T, U: Default, V: Clone, W: core::default::Default>);
        assert_eq!(standard_defaulted_parameters(&generics, &DefaultPaths::default()), ["U", "W"]);
        let qualified: Type = parse_quote!(<T as Trait>::Output);
        assert_eq!(unqualified_name_of(&qualified), None);
    }

    #[test]
    fn unqualified_names_follow_parenthesized_and_grouped_types() {
        assert_eq!(unqualified_name_of(&parse_quote!((Widget))), Some("Widget".to_owned()));
        let grouped = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            elem: Box::new(parse_quote!(Widget)),
        });
        assert_eq!(unqualified_name_of(&grouped), Some("Widget".to_owned()));
        assert_eq!(unqualified_name_of(&parse_quote!(_)), None);
    }

    #[test]
    fn derive_paths_cfg_modules_and_associated_attributes_preserve_exact_evidence() {
        let defaults = Defaults::of_in(
            &parse_file(
                "use core::default as d;
             #[other(Default)] struct NotDerived;
             #[derive(Clone,Default)] struct Derived;
             #[cfg(not(unix))] mod inactive { #[derive(Default)] struct Disabled; }
             mod nested { struct Bare; impl Default for Bare {} struct Qualified; impl core::default::Default for Qualified {}
                 struct ViaModule; impl d::Default for ViaModule {} }
             struct Root; impl Default for Root {}",
            )
            .unwrap(),
            &CfgSet::parse("unix"),
        );
        assert!(!defaults.has_default(&parse_quote!(NotDerived)));
        assert!(defaults.has_default(&parse_quote!(Derived)));
        assert!(!defaults.defines("Disabled"));
        assert!(!defaults.has_default(&parse_quote!(Bare)));
        assert!(defaults.has_default(&parse_quote!(Qualified)));
        assert!(defaults.has_default(&parse_quote!(ViaModule)));
        assert!(defaults.has_default(&parse_quote!(Root)));
        let implementation: syn::ItemImpl = parse_quote!(impl Example {
            #[cfg(unix)] const VALUE:u8=0;
            #[cfg(unix)] type Associated=u8;
        });
        for item in &implementation.items {
            assert_eq!(impl_item_attrs(item).len(), 1);
            assert!(impl_item_attrs(item)[0].path().is_ident("cfg"));
        }
        let declaration: syn::ItemTrait = parse_quote!(
            trait Example {
                #[cfg(unix)]
                const VALUE: u8;
                #[cfg(unix)]
                type Associated;
            }
        );
        for item in &declaration.items {
            assert_eq!(trait_item_attrs(item).len(), 1);
            assert!(trait_item_attrs(item)[0].path().is_ident("cfg"));
        }
    }

    fn index(sources: &[&str]) -> Defaults {
        let mut defaults = Defaults::default();

        for source in sources {
            defaults.absorb(Defaults::of(&parse_file(source).unwrap()));
        }

        defaults
    }

    #[test]
    fn partial_and_colliding_default_indexes_preserve_unknown_evidence() {
        let mut complete = index(&["struct Missing; #[derive(Default)] struct Present; union Bits { value: u32 }"]);
        assert!(complete.lacks_default(&parse_quote!(Missing)));
        assert!(complete.lacks_default(&parse_quote!(Bits)));
        assert!(complete.declares_default_name("Present"));
        complete.mark_incomplete();
        assert!(!complete.is_complete());
        assert!(!complete.lacks_default(&parse_quote!(Missing)));
        assert!(!complete.declares_default_name("Present"));

        for sources in [
            ["#[derive(Default)] struct Config;", "struct Config;"],
            ["struct Config;", "#[derive(Default)] struct Config;"],
        ] {
            let collided = index(&sources);
            assert!(!collided.declares_default_name("Config"));
            assert!(!collided.has_default(&parse_quote!(Config)));
            assert!(!collided.lacks_default(&parse_quote!(Config)));
            assert!(collided.defines("Config"));
        }
        let generic = index(&["#[derive(Default)] struct Generic<T>(T);"]);
        assert!(!generic.declares_default_name("Generic"));
        assert!(generic.declares_generic_default_name("Generic"));
        let gated = Defaults::of_in(
            &parse_file("#[cfg(not(unix))] union Disabled { field: u32 } union Active { field: u32 }").unwrap(),
            &CfgSet::parse("unix"),
        );
        assert!(!gated.defines("Disabled"));
        assert!(gated.defines("Active"));
    }

    #[test]
    fn same_file_type_name_collisions_are_unknown() {
        let defaults = index(&["mod one { #[derive(Default)] struct Config; } mod two { struct Config; }"]);
        let ty: Type = parse_quote!(Config);

        assert!(!defaults.has_default(&ty));
        assert!(!defaults.lacks_default(&ty));
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
    fn colliding_type_names_supply_neither_positive_nor_negative_default_evidence() {
        let defaults = index(&["pub struct Config;", "#[derive(Default)] pub struct Config;"]);
        let ty: Type = parse_quote!(Config);

        assert!(!defaults.has_default(&ty));
        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn uniformly_defaulted_colliding_names_preserve_positive_evidence() {
        let defaults = index(&[
            "#[derive(Default)] pub struct Config;",
            "pub struct Config; impl Default for Config { fn default() -> Self { Self } }",
        ]);
        let ty: Type = parse_quote!(Config);

        assert!(defaults.has_default(&ty));
        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn repeated_non_default_type_names_still_supply_negative_evidence() {
        let defaults = index(&["pub struct Error;", "pub enum Error { One }"]);
        let ty: Type = parse_quote!(Error);

        assert!(!defaults.has_default(&ty));
        assert!(defaults.lacks_default(&ty));
        assert!(defaults.lacks_error_default("Error"));
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
        let nested_custom = index(&["mod nested {
                pub trait Default { fn default() -> Self; }
                pub struct Error;
                impl Default for Error { fn default() -> Self { Self } }
            }"]);

        assert!(!standard_alias.lacks_default(&ty));
        assert!(!standard_module_alias.lacks_default(&ty));
        assert!(!standard_derive.lacks_default(&ty));
        assert!(custom.lacks_default(&ty));
        assert!(custom_alias.lacks_default(&ty));
        assert!(custom_derive.lacks_default(&ty));
        assert!(nested_custom.lacks_default(&ty));
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
    fn an_incomplete_index_does_not_prove_same_named_default_declarations() {
        let file = syn::parse_file("#[derive(Default)] struct Config;").expect("the defaults fixture parses");
        let defaults = Defaults::optimistic_of_in(&file, &CfgSet::unconditional());

        assert!(defaults.has_default_name("Config"));
        assert!(!defaults.declares_default_name("Config"));
    }

    #[test]
    fn merging_complete_and_partial_indexes_remains_partial_in_both_orders() {
        let partial = || {
            let file = syn::parse_file("struct Config;").expect("the partial defaults fixture parses");
            Defaults::optimistic_of_in(&file, &CfgSet::unconditional())
        };
        let complete = || {
            let file = syn::parse_file("struct Unrelated;").expect("the complete defaults fixture parses");
            Defaults::of(&file)
        };

        let mut partial_first = partial();
        partial_first.absorb(complete());
        let mut complete_first = complete();
        complete_first.absorb(partial());

        for defaults in [&partial_first, &complete_first] {
            assert!(!defaults.is_complete());
            assert!(!defaults.lacks_error_default("Config"));
        }
    }

    #[test]
    fn a_qualified_path_does_not_reuse_negative_evidence_for_its_last_segment() {
        let defaults = index(&["pub struct Config;"]);
        let ty: Type = parse_quote!(dependency::Config);

        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_qualified_path_does_not_reuse_positive_evidence_for_its_last_segment() {
        let defaults = index(&["#[derive(Default)] pub struct Config;"]);
        let ty: Type = parse_quote!(dependency::Config);

        assert!(!defaults.has_default(&ty));
    }

    #[test]
    fn a_generic_path_does_not_reuse_evidence_for_its_type_name() {
        let defaults = index(&["#[derive(Default)] pub struct Config<T>(T);"]);
        let ty: Type = parse_quote!(Config<NoDefault>);

        assert!(!defaults.has_default(&ty));
        assert!(!defaults.lacks_default(&ty));
    }

    #[test]
    fn a_bare_generic_name_with_omitted_defaults_is_not_positive_evidence() {
        let defaults = index(&["#[derive(Default)] pub struct Config<T = NoDefault>(T);"]);

        assert!(!defaults.has_default(&parse_quote!(Config)));
    }

    #[test]
    fn an_absolute_path_does_not_reuse_evidence_for_its_type_name() {
        let defaults = index(&["#[derive(Default)] pub struct Config;"]);
        let ty: Type = parse_quote!(::Config);

        assert!(!defaults.has_default(&ty));
        assert!(!defaults.lacks_default(&ty));
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

        let verbatim = Item::Verbatim(TokenStream::new());

        assert!(
            item_attrs(&verbatim).is_empty(),
            "an unrecognized item kind has no attributes to read"
        );
    }

    #[test]
    fn associated_item_attrs_read_macros_and_fall_back_for_verbatim_tokens() {
        let implementation: ImplItem = parse_quote!(
            #[allow(dead_code)]
            m!();
        );
        let declaration: TraitItem = parse_quote!(
            #[allow(dead_code)]
            m!();
        );

        assert_eq!(impl_item_attrs(&implementation).len(), 1);
        assert_eq!(trait_item_attrs(&declaration).len(), 1);
        assert!(impl_item_attrs(&ImplItem::Verbatim(TokenStream::new())).is_empty());
        assert!(trait_item_attrs(&TraitItem::Verbatim(TokenStream::new())).is_empty());
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

    #[test]
    fn mutation_boundaries_are_observable_across_paths_attributes_and_visitors() {
        for declaration in ["enum Default {}", "type Default = ();", "mod Default {}", "struct Default;"] {
            let source = format!("struct Error; {declaration} impl Default for Error {{ fn default() -> Self {{ Self }} }}");
            assert!(Defaults::of(&parse_file(&source).unwrap()).lacks_default(&parse_quote!(Error)));
        }

        let paths = DefaultPaths::default();
        let bare: Path = parse_quote!(Default);
        let qualified: Path = parse_quote!(core::default::Default);
        let custom: Path = parse_quote!(custom::Default);
        assert!(paths.is_fallback_trait(&bare));
        assert!(paths.is_fallback_trait(&qualified));
        assert!(!paths.is_fallback_trait(&custom));
        assert!(!is_standard_module_path(&["custom".to_owned()]));
        assert!(!is_standard_module_path(&["std".to_owned(), "other".to_owned()]));

        let chained = index(&[
            "struct Error; use std as first; use first as second; impl second::default::Default for Error { fn default() -> Self { Self } }",
        ]);
        assert!(!chained.lacks_default(&parse_quote!(Error)));

        let not_result = index(&["type Alias = Vec<u8, Error>;"]);
        assert_eq!(not_result.aliased_error("Alias"), None);
        let lifetime_alias = index(&["type MyResult<'a, const N: usize> = Result<u8, Error>;"]);
        assert_eq!(lifetime_alias.aliased_error("MyResult"), Some("Error"));

        let item_kinds: Vec<Item> = vec![
            parse_quote!(
                #[allow(dead_code)]
                enum E {}
            ),
            parse_quote!(
                #[allow(dead_code)]
                fn f() {}
            ),
            parse_quote!(
                #[allow(dead_code)]
                impl S {}
            ),
            parse_quote!(
                #[allow(dead_code)]
                static S: usize = 0;
            ),
            parse_quote!(
                #[allow(dead_code)]
                struct S;
            ),
            parse_quote!(
                #[allow(dead_code)]
                type A = usize;
            ),
            parse_quote!(
                #[allow(dead_code)]
                use core::fmt;
            ),
        ];
        assert!(item_kinds.iter().all(|item| item_attrs(item).len() == 1));

        let function: syn::ItemFn = parse_quote!(
            fn f<T>()
            where
                T: Clone + Default,
            {
            }
        );
        assert_eq!(standard_defaulted_parameters(&function.sig.generics, &paths), vec!["T"]);
    }
}
