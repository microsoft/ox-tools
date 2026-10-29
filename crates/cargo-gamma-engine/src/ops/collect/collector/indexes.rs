// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The per-file pre-pass the collector reads before it can judge an expression.

use syn::visit::{self, Visit};
use syn::{
    Attribute, BinOp, Block, Expr, ExprBinary, ExprForLoop, ExprIndex, ExprMethodCall, File, FnArg, ImplItem, ImplItemConst, ImplItemFn,
    Item, ItemConst, ItemFn, ItemMod, ItemStatic, ItemStruct, ItemType, ItemUse, Member, Pat, ReturnType, Signature, Stmt, TraitItem,
    TraitItemConst, TraitItemFn, Type, UseTree,
};

use super::noop::LocalCalls;
use crate::cfg::CfgSet;
use crate::ops::collect::collector::predicates::{expr_attrs, is_int_literal, is_numeric_binding, is_numeric_receiver, stmt_attrs};
use crate::ops::collect::collector::types::Alias;
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::ops::registry::Selection;
use crate::{HashMap, HashSet};

pub(super) const ABSOLUTE_ROOT: &str = "<absolute>";
pub(super) type ScopePath = Vec<(usize, usize)>;

pub(super) fn module_id(ident: &syn::Ident) -> (usize, usize) {
    let start = ident.span().start();
    (start.line, start.column)
}

pub(super) fn block_id(block: &Block) -> (usize, usize) {
    let start = block.brace_token.span.open().start();
    (start.line, start.column)
}

fn merge_type(index: &mut HashMap<String, Option<Type>>, name: &str, ty: &Type) {
    let _known = index
        .entry(name.to_owned())
        .and_modify(|known| {
            if known.as_ref() != Some(ty) {
                *known = None;
            }
        })
        .or_insert_with(|| Some(ty.clone()));
}

/// The names a file uses in a way only a number can be used.
#[derive(Default)]
pub(super) struct NumericUses {
    /// Bare identifiers: locals, parameters, loop indices.
    pub(super) names: HashSet<String>,

    /// Bare identifiers used where Rust requires an unsigned index or slice bound.
    pub(super) unsigned_names: HashSet<String>,

    /// Field names, which stand in for the declarations the per-file pre-pass cannot reach.
    pub(super) fields: HashSet<String>,
}

/// The per-file indexes the collector reads before it can judge an expression.
///
/// Four questions, one descent. Each is answered by looking at a different kind of item — a
/// `struct`'s fields, a `use`, a constant's declared type, an expression that only a number could
/// appear in — so none of them depends on another's answer, and building them separately was four
/// walks of a syntax tree to learn things one walk can learn at once.
///
/// Read before traversal rather than during it, because every one of these is used above the item
/// that establishes it at least as often as below: a field is read before the `struct` is declared,
/// a type before its `use`, a constant before its `const`.
pub(in crate::ops::collect) struct Indexes {
    /// Whether each field name declared anywhere in this file holds a number.
    pub(super) fields: HashMap<String, bool>,

    /// The module path each name this file imports was brought in from.
    ///
    /// `None` marks a name two `use` items disagree about, which is as unknown as never having
    /// been imported.
    pub(super) imports: HashMap<String, Option<Vec<String>>>,

    /// Imports written directly in the file's root module.
    pub(super) root_imports: HashMap<String, Option<Vec<String>>>,

    /// Imports and shadows visible in each nested lexical scope.
    pub(super) scope_imports: HashMap<ScopePath, HashMap<String, Option<Vec<String>>>>,

    /// Scope paths that inherit bindings from their lexical parent.
    pub(super) block_scopes: HashSet<ScopePath>,

    /// Names the file uses somewhere in a way only a number can be used.
    pub(super) numeric_uses: NumericUses,

    /// Whether each constant and static declared anywhere in this file holds a number.
    pub(super) constants: HashMap<String, bool>,

    /// Unambiguous source-written types for fields and constants.
    pub(super) declared_types: HashMap<String, Option<Type>>,

    /// Return types of locally visible functions.
    pub(super) returns: HashMap<String, Option<Type>>,
    pub(super) parameters: HashMap<String, Option<Vec<Type>>>,
    pub(super) local_calls: LocalCalls,

    /// Locally declared type aliases and their targets.
    pub(super) aliases: HashMap<String, Option<Alias>>,
    /// Aliases and nominal shadows in their declaring lexical scope.
    pub(super) scope_aliases: HashMap<ScopePath, HashMap<String, Option<Alias>>>,
}

impl Indexes {
    fn canonicalize_imports(&mut self) {
        let imports = self.imports.clone();
        for (name, path) in &mut self.imports {
            if
            // #[gamma::skip(cond.always_true, reason = "When the current import entry is None, the immutable import snapshot (or visible map overlaid with that local None) also contains None for the same name. canonical_import then immediately returns None; calling it instead of skipping changes nothing.", tag = "equivalent")]
            path.is_some() {
                *path = canonical_import(name, &imports);
            }
        }
        let root_imports = self.root_imports.clone();
        for (name, path) in &mut self.root_imports {
            if
            // #[gamma::skip(cond.always_true, reason = "When the current import entry is None, the immutable import snapshot (or visible map overlaid with that local None) also contains None for the same name. canonical_import then immediately returns None; calling it instead of skipping changes nothing.", tag = "equivalent")]
            path.is_some() {
                *path = canonical_import(name, &root_imports);
            }
        }

        let mut scopes = self.scope_imports.keys().cloned().collect::<Vec<_>>();
        scopes.sort_by_key(Vec::len);
        for scope in scopes {
            let mut local = self
                .scope_imports
                .get(&scope)
                .expect("every recorded lexical scope has an import index")
                .clone();
            let parent = self.block_scopes.contains(&scope).then(|| {
                let imports = if scope.len() == 1 {
                    &self.root_imports
                } else {
                    self.scope_imports
                        .get(&scope[..scope.len() - 1])
                        .expect("a block's enclosing lexical scope has an import index")
                };
                imports
                    .iter()
                    .filter(|(name, _)| !self.scope_aliases.get(&scope).is_some_and(|aliases| aliases.contains_key(*name)))
                    .map(|(name, path)| (name.clone(), path.clone().map(protect_import)))
                    .collect::<HashMap<_, _>>()
            });
            let mut visible = parent.clone().unwrap_or_default();
            visible.extend(local.clone());
            for (name, path) in &mut local {
                if
                // #[gamma::skip(cond.always_true, reason = "When the current import entry is None, the immutable import snapshot (or visible map overlaid with that local None) also contains None for the same name. canonical_import then immediately returns None; calling it instead of skipping changes nothing.", tag = "equivalent")]
                path.is_some() {
                    *path = canonical_import(name, &visible);
                }
            }
            if let Some(parent) = parent {
                for (name, path) in parent {
                    let _inherited = local.entry(name).or_insert(path);
                }
            }
            let _previous = self.scope_imports.insert(scope, local);
        }
        self.canonicalize_aliases();
    }

    fn canonicalize_aliases(&mut self) {
        let defaults = super::super::Defaults::default();
        let declarations = self
            .scope_aliases
            .iter()
            .map(|(scope, aliases)| (scope.clone(), aliases.keys().cloned().collect::<HashSet<_>>()))
            .collect::<HashMap<_, _>>();
        let mut scopes = self.scope_imports.keys().cloned().collect::<Vec<_>>();
        scopes.push(Vec::new());
        scopes.sort_by_key(Vec::len);
        // #[gamma::skip(iter.remove_dedup, reason = "scope_imports keys are unique and nonempty: only enter_module/enter_block or scoped import insertion creates them. Root imports have their separate map. Appending the empty root scope therefore introduces no duplicate and dedup removes nothing.", tag = "equivalent")]
        scopes.dedup();
        for scope in scopes {
            let mut visible = if self.block_scopes.contains(&scope) {
                self.scope_aliases.get(&scope[..scope.len() - 1]).cloned().unwrap_or_default()
            } else {
                HashMap::default()
            };
            let local = self.scope_aliases.get(&scope).cloned().unwrap_or_default();
            let local_names = local.keys().cloned().collect::<Vec<_>>();
            visible.extend(local);
            let mut imports = self.scope_imports.get(&scope).unwrap_or(&self.root_imports).clone();
            for (name, definition) in &visible {
                if definition.is_none() && !imports.contains_key(name) {
                    let block_local = (
                        // #[gamma::skip(literal.int_increment, reason = "Skipping the empty-root declaration can only turn a nearest-root match into None. The empty root is never in block_scopes, so block_local is false in both cases; any nearer nonempty declaration is still found before that omitted root.", tag = "equivalent")]
                        0..=scope.len()
                    )
                        .rev()
                        .find(|&length| declarations.get(&scope[..length]).is_some_and(|names| names.contains(name)))
                        .is_some_and(|length| self.block_scopes.contains(&scope[..length]));
                    let _old = imports.insert(name.clone(), (!block_local).then(|| vec!["self".to_owned(), name.clone()]));
                }
            }
            // Freeze each declaration's names before expanding it under another alias's parameters.
            for expand_aliases in [
                // #[gamma::skip(literal.bool_flip, reason = "Changing [false,true] to [true,true] retains the final expansion pass and also switches the first pass to alias_order: every dependency is frozen under its own restored parameter/import context before its dependents expand. Thus it cannot capture another alias's free names through parameters. Already expanded targets are idempotent under the final normalization; failures are absorbing whole-type _ values. Unlike the reversed-pass mutant, the final frozen-path reconstruction budget is still enforced.", tag = "equivalent")]
                false, true,
            ] {
                let (names, unresolved) = if
                // #[gamma::skip(cond.always_true, reason = "This replacement changes only name ordering in the nonexpanding declaration pass, not whether declaration_type or canonical_type is called. Topological rather than hash-map order freezes the same independent declaration targets in their own restored generic/import environments. Cyclic aliases and dependents become _ earlier, but the unchanged final expansion pass would replace the identical component with _ anyway.", tag = "equivalent")]
                expand_aliases {
                    alias_order(&visible, &local_names, &imports)
                } else {
                    (local_names.clone(), HashSet::default())
                };
                for name in unresolved {
                    if let Some(Some(alias)) = visible.get_mut(&name) {
                        alias.target = syn::parse_quote!(_);
                    }
                }
                for name in &names {
                    let Some(mut alias) = visible.get(name).cloned().flatten() else {
                        continue;
                    };
                    let shadowed = alias
                        .parameters
                        .iter()
                        .map(|parameter| (parameter, visible.insert(parameter.clone(), None), imports.remove(parameter)))
                        .collect::<Vec<_>>();
                    let types = super::types::Types {
                        abstracts: &[],
                        defaulted: &[],
                        imports: &imports,
                        defaults: &defaults,
                        aliases: Some(&visible),
                        self_type: None,
                        self_associated: None,
                    };
                    alias.target = complete_type(if expand_aliases {
                        types.canonical_type(&alias.target)
                    } else {
                        types.declaration_type(&alias.target)
                    });
                    for (parameter, previous_alias, previous_import) in shadowed.into_iter().rev() {
                        if let Some(previous) = previous_alias {
                            let _old = visible.insert(parameter.clone(), previous);
                        } else {
                            let _old = visible.remove(parameter);
                        }
                        if let Some(previous) = previous_import {
                            let _old = imports.insert(parameter.clone(), previous);
                        }
                    }
                    let _old = visible.insert(name.clone(), Some(alias));
                }
            }
            let _old = self.scope_aliases.insert(scope, visible);
        }
    }
}

pub(super) fn complete_type(ty: Type) -> Type {
    struct Inferred(bool);
    impl<'ast> Visit<'ast> for Inferred {
        fn visit_type_infer(&mut self, _i: &'ast syn::TypeInfer) {
            self.0 = true;
        }
    }
    let mut inferred = Inferred(false);
    inferred.visit_type(&ty);
    if inferred.0 { syn::parse_quote!(_) } else { ty }
}

/// Orders alias dependencies with [Kahn's algorithm].
///
/// The residual set includes cycles and every alias blocked by them, not just cycle members.
/// None has a fully normalized definition, so all must remain unknown.
///
/// [Kahn's algorithm]: https://en.wikipedia.org/wiki/Topological_sorting#Kahn's_algorithm
fn alias_order(
    aliases: &HashMap<String, Option<Alias>>,
    local: &[String],
    imports: &HashMap<String, Option<Vec<String>>>,
) -> (Vec<String>, HashSet<String>) {
    struct Dependencies<'a> {
        local: &'a HashSet<String>,
        parameters: &'a [String],
        imports: &'a HashMap<String, Option<Vec<String>>>,
        found: HashSet<String>,
    }

    impl<'ast> Visit<'ast> for Dependencies<'_> {
        fn visit_type_path(&mut self, i: &'ast syn::TypePath) {
            if (
                // #[gamma::skip(logical.and_remove_left, reason = "Dependencies independently requires leading_colon=None AND exactly one segment. A parsed qualified path without as has leading_colon=Some; a qualified as path has at least two segments. Deleting qself cannot admit either. The previous <Owner>::Alias fixture failed the untouched leading-colon check and never created the alleged self-dependency.", tag = "equivalent")]
                i.qself.is_none() && i.path.leading_colon.is_none()
            ) && i.path.segments.len() == 1
                && let Some(segment) =
                    // #[gamma::skip(iter.first_to_last, reason = "This first/last access is dominated by path.segments.len()==1, hence selects the same segment.", tag = "equivalent")]
                    i.path.segments.first()
            {
                let name = segment.ident.to_string();
                if self.local.contains(&name) && !self.parameters.contains(&name) && !self.imports.contains_key(&name) {
                    let _new = self.found.insert(name);
                }
            }
            visit::visit_type_path(self, i);
        }
    }

    let local: HashSet<_> = local
        .iter()
        .filter(|name| aliases.get(*name).is_some_and(Option::is_some))
        .cloned()
        .collect();
    let mut pending = HashMap::default();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::default();
    for name in &local {
        let alias = aliases[name].as_ref().expect("local contains only names with an alias definition");
        let mut dependencies = Dependencies {
            local: &local,
            parameters: &alias.parameters,
            imports,
            found: HashSet::default(),
        };
        dependencies.visit_type(&alias.target);
        let _old = pending.insert(name.clone(), dependencies.found.len());
        for dependency in dependencies.found {
            dependents.entry(dependency).or_default().push(name.clone());
        }
    }

    let mut ready = pending
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let mut ordered = Vec::with_capacity(local.len());
    while let Some(name) = ready.pop() {
        let _completed = pending.remove(&name);
        for dependent in dependents.get(&name).into_iter().flatten() {
            let count = pending
                .get_mut(dependent)
                .expect("each dependent remains pending until all its recorded dependencies complete");
            *count -= 1;
            if *count == 0 {
                ready.push(dependent.clone());
            }
        }
        ordered.push(name);
    }
    (ordered, pending.into_keys().collect())
}

fn protect_import(mut path: Vec<String>) -> Vec<String> {
    if path.first().is_none_or(|first| first != ABSOLUTE_ROOT) {
        path.insert(0, ABSOLUTE_ROOT.to_owned());
    }
    path
}

fn canonical_import(name: &str, imports: &HashMap<String, Option<Vec<String>>>) -> Option<Vec<String>> {
    let mut path = imports.get(name)?.clone()?;
    let mut expanded = HashSet::default();

    loop {
        let first = path.first()?;
        if
        // #[gamma::skip(cond.always_false, reason = "ABSOLUTE_ROOT is the synthetic string <absolute>, which cannot be a Rust imported identifier or the wildcard key *. If its early return is skipped, imports.get(first) is necessarily None and the immediately following branch returns the same protected path.", tag = "equivalent")]
        first == ABSOLUTE_ROOT {
            return Some(path);
        }
        let Some(imported) = imports.get(first) else {
            return Some(path);
        };
        let prefix = imported.as_ref()?;
        // #[gamma::skip(iter.first_to_last, reason = "this comparison is evaluated only when prefix contains exactly one element, so first and last select the identical element")]
        if prefix.len() == 1 && prefix.first() == Some(first) {
            return Some(path);
        }
        // #[gamma::skip(cond.always_false, reason = "disabling cycle detection makes cyclic import aliases alternate forever")]
        if !expanded.insert(first.clone()) {
            return
            // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
            None;
        }
        let _replaced = path.splice(..1, prefix.iter().cloned());
    }
}

/// Fills whichever indexes were asked for, ignoring scope.
///
/// Signature maps are deliberately file-wide and keyed by bare names because their consumers
/// classify unqualified calls. Collisions merge to unknown rather than depending on visit order;
/// receiver methods and associated functions therefore cannot supply bare-function evidence.
pub(super) struct Walk<'cfg> {
    indexes: Indexes,
    scope_path: ScopePath,

    /// Whether the numeric evidence — fields, constants, uses — is wanted.
    numeric: bool,

    /// Whether source-visible type evidence is wanted.
    type_evidence: bool,

    /// The configuration predicates that hold for the build this file will be part of.
    ///
    /// A field, constant, `use`, or numeric use drawn from code the collector will not mutate —
    /// because a predicate strips it, or because it is test code — would misinform every active
    /// site that later consults the index it feeds. The gates below therefore ask
    /// [`CfgSet::skip_gate`], the one question [`Collector::skipped`](super::Collector::skipped)
    /// asks, at every place this walk can be entered: items, associated items, struct fields,
    /// statements, and expressions.
    cfg: &'cfg CfgSet,
}

impl Walk<'_> {
    pub(super) fn on_item_fn(&mut self, node: &ItemFn) {
        if self.type_evidence {
            self.indexes.local_calls.function(node, &self.scope_path);
        }
    }

    pub(super) fn on_pat_ident(&mut self, node: &syn::PatIdent) {
        if self.type_evidence {
            self.indexes.local_calls.binding(node);
        }
    }

    pub(super) fn on_value_name(&mut self, name: &syn::Ident) {
        if self.type_evidence {
            self.indexes.local_calls.value_name(name);
        }
    }

    pub(super) fn on_opaque_macro(&mut self) {
        self.indexes.local_calls.opaque(&self.scope_path);
    }

    pub(super) fn on_local_attributes(&mut self, attributes: &[syn::Attribute]) {
        if self.indexes.block_scopes.contains(&self.scope_path) && !LocalCalls::nontransforming_attributes(attributes) {
            self.on_opaque_macro();
        }
    }

    pub(super) fn on_item_extern_crate(&mut self, node: &syn::ItemExternCrate) {
        if self.type_evidence {
            let name = node.rename.as_ref().map_or(&node.ident, |(_, name)| name);
            self.imported(name.to_string(), &[node.ident.to_string()]);
        }
    }

    /// Notes a name, when the expression is the bare identifier that proves it.
    pub(super) fn note(&mut self, expression: &Expr) {
        match expression {
            Expr::Path(path) if
            // #[gamma::skip(match_guard.always_true, reason = "These sites independently require Path::get_ident/is_ident after the qself guard. On parsed syn paths, <Owner>::Name has a leading_colon, so get_ident/is_ident rejects it independently; <Owner as Trait>::Name contains at least two segments and is also rejected independently. The prior simple qualified-name negative assertions therefore never distinguished this guard. The proof is for parsed production inputs, not artificially inconsistent manually constructed paths.", tag = "equivalent")]
            path.qself.is_none() => {
                if let Some(ident) = path.path.get_ident() {
                    let _added = self.indexes.numeric_uses.names.insert(ident.to_string());
                }
            }

            // A field's own `struct` is very often in another file, which the pre-pass cannot
            // see. How the field is used here is the only evidence available for those.
            Expr::Field(field) => {
                if let Member::Named(name) = &field.member {
                    let _added = self.indexes.numeric_uses.fields.insert(name.to_string());
                }
            }

            Expr::Paren(paren) => self.note(&paren.expr),
            Expr::Reference(reference) => self.note(&reference.expr),
            _ => {}
        }
    }

    /// Notes every bare name whose value contributes to an index or slice bound.
    pub(super) fn note_unsigned(&mut self, expression: &Expr) {
        match expression {
            Expr::Path(path) if
            // #[gamma::skip(match_guard.always_true, reason = "These sites independently require Path::get_ident/is_ident after the qself guard. On parsed syn paths, <Owner>::Name has a leading_colon, so get_ident/is_ident rejects it independently; <Owner as Trait>::Name contains at least two segments and is also rejected independently. The prior simple qualified-name negative assertions therefore never distinguished this guard. The proof is for parsed production inputs, not artificially inconsistent manually constructed paths.", tag = "equivalent")]
            path.qself.is_none() => {
                if let Some(ident) = path.path.get_ident() {
                    let name = ident.to_string();
                    let _added = self.indexes.numeric_uses.names.insert(name.clone());
                    let _added = self.indexes.numeric_uses.unsigned_names.insert(name);
                }
            }
            Expr::Range(range) => {
                if let Some(start) = &range.start {
                    self.note_unsigned(start);
                }
                if let Some(end) = &range.end {
                    self.note_unsigned(end);
                }
            }
            Expr::Paren(paren) => self.note_unsigned(&paren.expr),
            Expr::Group(group) => self.note_unsigned(&group.expr),
            Expr::Reference(reference) => self.note_unsigned(&reference.expr),
            _ => {}
        }
    }

    /// Records one constant's declaration, demoting a name two declarations disagree about.
    pub(super) fn declared(&mut self, name: &str, ty: &Type) {
        if self.type_evidence {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
            merge_type(&mut self.indexes.declared_types, name, ty);
        }

        if self.numeric {
            let numeric = is_numeric_binding(ty);

            let _known = self
                .indexes
                .constants
                .entry(name.to_owned())
                .and_modify(|known| *known = *known && numeric)
                .or_insert(numeric);
        }
    }

    pub(super) fn returned(&mut self, name: &str, output: &ReturnType) {
        if !self.type_evidence {
            return;
        }

        let unit;
        let ty = match output {
            ReturnType::Default => {
                unit = syn::parse_quote!(());
                &unit
            }
            ReturnType::Type(_, ty) => ty,
        };
        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
        merge_type(&mut self.indexes.returns, name, ty);
    }

    pub(super) fn signature(&mut self, signature: &Signature) {
        if !self.type_evidence {
            return;
        }

        if signature.asyncness.is_some() {
            let _previous = self.indexes.returns.insert(signature.ident.to_string(), None);
        } else if
        // #[gamma::skip(cond.always_true, reason = "A return entry that is already None remains None under merge_type: its sticky conflict merge never restores Some. Calling returned after an unknown entry instead of skipping therefore preserves the same unknown return evidence.", tag = "equivalent")]
        !self.indexes.returns.get(&signature.ident.to_string()).is_some_and(Option::is_none) {
            self.returned(&signature.ident.to_string(), &signature.output);
        }
        let parameters = signature
            .inputs
            .iter()
            .filter_map(|input| match input {
                FnArg::Typed(typed) if !self.cfg.skip_gate(&typed.attrs) => Some((*typed.ty).clone()),
                FnArg::Receiver(receiver) if !self.cfg.skip_gate(&receiver.attrs) => Some(syn::parse_quote!(Self)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let name = signature.ident.to_string();
        let _known = self
            .indexes
            .parameters
            .entry(name)
            .and_modify(|known| {
                if known.as_ref() != Some(&parameters) {
                    *known = None;
                }
            })
            .or_insert(Some(parameters));
    }

    pub(super) fn alias(&mut self, name: &str, generics: &syn::Generics, ty: &Type) {
        if self.type_evidence {
            let alias = Alias {
                parameters: generics.type_params().map(|parameter| parameter.ident.to_string()).collect(),
                target: ty.clone(),
            };
            let _known = self
                .indexes
                .aliases
                .entry(name.to_owned())
                .and_modify(|known| {
                    if known.as_ref() != Some(&alias) {
                        *known = None;
                    }
                })
                .or_insert(Some(alias.clone()));
            let _known = self
                .indexes
                .scope_aliases
                .entry(self.scope_path.clone())
                .or_default()
                .entry(name.to_owned())
                .and_modify(|known| {
                    if known.as_ref() != Some(&alias) {
                        *known = None;
                    }
                })
                .or_insert(Some(alias));
        }
    }

    /// Records every name one `use` tree brings into scope, and where each came from.
    pub(super) fn descend(&mut self, prefix: &mut Vec<String>, tree: &UseTree) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.descend(prefix, &path.tree);
                let _popped = prefix.pop();
            }

            UseTree::Name(name) if name.ident == "self" => {
                if let Some(binding) = prefix.last() {
                    self.imported(binding.clone(), prefix);
                }
            }

            UseTree::Name(name) => {
                let mut source = prefix.clone();
                source.push(name.ident.to_string());
                self.imported(name.ident.to_string(), &source);
            }

            UseTree::Rename(rename) => {
                let mut source = prefix.clone();
                if rename.ident != "self" {
                    source.push(rename.ident.to_string());
                }
                self.imported(rename.rename.to_string(), &source);
            }

            UseTree::Group(group) => {
                for item in &group.items {
                    self.descend(prefix, item);
                }
            }

            UseTree::Glob(_) => {
                let _previous = self.indexes.imports.insert("*".to_owned(), None);
                if self.scope_path.is_empty() {
                    let _previous = self.indexes.root_imports.insert("*".to_owned(), None);
                } else {
                    let _previous = self
                        .indexes
                        .scope_imports
                        .entry(self.scope_path.clone())
                        .or_default()
                        .insert("*".to_owned(), None);
                }
            }
        }
    }

    /// Records the complete source path of one imported name, demoting a name two `use` items
    /// disagree about.
    ///
    /// The index is keyed by the bare name and spans the whole file, but a file may hold several
    /// modules, and `use crate::Error` in one says nothing about `use std::io::Error` in another.
    /// Letting the later `use` win made one module's answer depend on another's: a bare `Error`
    /// resolved to whichever happened to be written last, so the wrong module either emitted
    /// `Err(Default::default())` mutants that cannot compile or silently withheld valid ones.
    ///
    /// Demoting to `None` rather than picking a winner leaves the name exactly as unknown as one
    /// that was never imported. That is a weaker answer than module scoping would give, and a
    /// deliberately cheap one: the resulting guess is re-checked by the compiler, so being wrong
    /// costs a withdrawn mutant rather than a wrong score. Importing the same path twice is not a
    /// disagreement and does not demote.
    fn imported(&mut self, name: String, prefix: &[String]) {
        self.indexes.local_calls.imported(&name);
        Self::merge_import(&mut self.indexes.imports, name.clone(), prefix);
        if self.scope_path.is_empty() {
            Self::merge_import(&mut self.indexes.root_imports, name, prefix);
        } else {
            Self::merge_import(self.indexes.scope_imports.entry(self.scope_path.clone()).or_default(), name, prefix);
        }
    }

    fn merge_import(imports: &mut HashMap<String, Option<Vec<String>>>, name: String, prefix: &[String]) {
        let _known = imports
            .entry(name)
            .and_modify(|known| {
                if known.as_deref() != Some(prefix) {
                    *known = None;
                }
            })
            .or_insert_with(|| Some(prefix.to_vec()));
    }

    pub(super) fn enter_module(&mut self, name: &syn::Ident) {
        self.scope_path.push(module_id(name));
        self.indexes.local_calls.module(&self.scope_path);
        let _imports = self.indexes.scope_imports.entry(self.scope_path.clone()).or_default();
    }

    pub(super) fn enter_block(&mut self, block: &Block) {
        self.scope_path.push(block_id(block));
        let _scope = self.indexes.block_scopes.insert(self.scope_path.clone());
        let _imports = self.indexes.scope_imports.entry(self.scope_path.clone()).or_default();
    }

    pub(super) fn exit_scope(&mut self) {
        let _scope = self.scope_path.pop();
    }

    /// The local update `visit_item_struct` makes, without its recursive continuation.
    ///
    /// Exposed with no continuation of its own so the fused phase-one pass (see
    /// `collector::phase_one`) can drive this exact per-node logic from its own single traversal.
    ///
    /// The struct itself is assumed already active — every caller reaches this only through the
    /// `visit_item` gate below, which excludes a struct the selected build strips before its
    /// fields are ever read. A field can still carry its own `#[cfg(...)]` distinct from the
    /// struct's, so each field is checked again here: a field the build does not compile must not
    /// inform the numeric guess for a same-named field elsewhere that the build does compile.
    pub(super) fn on_item_struct(&mut self, node: &ItemStruct) {
        self.shadow_type(&node.ident);
        self.on_value_name(&node.ident);
        for field in &node.fields {
            if self.cfg.skip_gate(&field.attrs) {
                continue;
            }

            let Some(name) = field.ident.as_ref() else {
                // #[gamma::skip(loop.continue_to_break, reason = "The exact continue is field.ident=None in on_item_struct, not cfg-filtered field descent. For parsed syn::Fields::Unnamed every field lacks an identifier, and this callback records nothing for any of them; for Fields::Named every field has an identifier. Breaking on the first unnamed field therefore skips only ident-less no-ops. The old recursive declaration fixture exercised a different continuation.", tag = "equivalent")]
                continue;
            };

            let name = name.to_string();
            if self.type_evidence {
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                merge_type(&mut self.indexes.declared_types, &name, &field.ty);
            }

            if !self.numeric {
                continue;
            }

            let numeric = is_numeric_binding(&field.ty);
            // Two structs disagreeing about a name means neither answer can be trusted for
            // a bare `x.count`, so the name is demoted to unknown rather than won by
            // whichever was seen last.
            let _known = self
                .indexes
                .fields
                .entry(name)
                .and_modify(|known| *known = *known && numeric)
                .or_insert(numeric);
        }
    }

    pub(super) fn shadow_type(&mut self, name: &syn::Ident) {
        let _previous = self
            .indexes
            .scope_aliases
            .entry(self.scope_path.clone())
            .or_default()
            .insert(name.to_string(), None);
    }

    pub(super) fn on_item_mod(&mut self, node: &ItemMod) {
        let name = node.ident.to_string();
        let source = vec!["self".to_owned(), name.clone()];

        self.imported(name, &source);
    }

    pub(super) fn on_item_trait(&mut self, node: &syn::ItemTrait) {
        let name = node.ident.to_string();
        let _previous = self.indexes.imports.insert(name.clone(), None);
        if self.scope_path.is_empty() {
            let _previous = self.indexes.root_imports.insert(name, None);
        } else {
            let _previous = self
                .indexes
                .scope_imports
                .entry(self.scope_path.clone())
                .or_default()
                .insert(name, None);
        }
    }

    /// The local update `visit_item_use` makes, without its recursive continuation.
    pub(super) fn on_item_use(&mut self, node: &ItemUse) {
        if self.type_evidence {
            let mut prefix = if node.leading_colon.is_some() {
                vec![ABSOLUTE_ROOT.to_owned()]
            } else {
                Vec::new()
            };
            self.descend(&mut prefix, &node.tree);
        }
    }

    /// The local update `visit_expr_binary` makes, without its recursive continuation.
    pub(super) fn on_expr_binary(&mut self, node: &ExprBinary) {
        if self.numeric {
            match node.op {
                // Nothing else in wide use subtracts, multiplies, divides or takes a remainder.
                BinOp::Sub(_)
                | BinOp::Mul(_)
                | BinOp::Div(_)
                | BinOp::Rem(_)
                | BinOp::SubAssign(_)
                | BinOp::MulAssign(_)
                | BinOp::DivAssign(_)
                | BinOp::RemAssign(_) => {
                    self.note(&node.left);
                    self.note(&node.right);
                }

                // `String + &str` and `Ordering` comparisons make these two ambiguous on their
                // own, so they count only against an integer literal, which fixes both sides.
                BinOp::Add(_) | BinOp::AddAssign(_) | BinOp::Lt(_) | BinOp::Gt(_) | BinOp::Le(_) | BinOp::Ge(_) => {
                    if is_int_literal(&node.right) {
                        self.note(&node.left);
                    }

                    if is_int_literal(&node.left) {
                        self.note(&node.right);
                    }
                }

                _ => {}
            }
        }
    }

    /// The local update `visit_expr_index` makes, without its recursive continuation.
    pub(super) fn on_expr_index(&mut self, node: &ExprIndex) {
        if self.numeric {
            self.note_unsigned(&node.index);
        }
    }

    /// The local update `visit_expr_method_call` makes, without its recursive continuation.
    pub(super) fn on_expr_method_call(&mut self, node: &ExprMethodCall) {
        if self.numeric && is_numeric_receiver(&node.method.to_string()) {
            self.note(&node.receiver);
        }
    }

    /// The local update `visit_expr_for_loop` makes, without its recursive continuation.
    pub(super) fn on_expr_for_loop(&mut self, node: &ExprForLoop) {
        if self.numeric
            && matches!(&*node.expr, Expr::Range(_))
            && let Pat::Ident(ident) = &*node.pat
        {
            let _added = self.indexes.numeric_uses.names.insert(ident.ident.to_string());
        }
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_attribute(&mut self, _node: &'ast Attribute) {}

    /// Gates every item this walk might otherwise index by the same decision the collector reads
    /// before it ever offers a mutant.
    ///
    /// [`CfgSet::skip_gate`] rather than [`CfgSet::holds_for`], because the collector excludes test
    /// code as well as configured-out code, and an index built from a `#[cfg(test)]` helper informs
    /// guesses about production code the helper is not part of — a field named there can make an
    /// active `x.count` look numeric on evidence the measured build never compiles.
    ///
    /// `visit_item` is the single dispatch point every top-level and nested item passes through —
    /// including a local item declared inside a function body — so gating here, rather than
    /// separately in each of `visit_item_struct`, `visit_item_use`, `visit_item_const`, and
    /// `visit_item_static`, keeps one skipped item from reaching any of them.
    fn visit_item(&mut self, node: &'ast Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            self.on_local_attributes(item_attrs(node));
            if let Item::Mod(module) = node {
                self.on_item_mod(module);
            }
            if let Item::Trait(declaration) = node {
                self.on_item_trait(declaration);
            }
            visit::visit_item(self, node);
        }
    }

    /// Gates every associated item inside an `impl` block, for the same reason [`Self::visit_item`]
    /// gates top-level items.
    fn visit_impl_item(&mut self, node: &'ast ImplItem) {
        if !self.cfg.skip_gate(impl_item_attrs(node)) {
            visit::visit_impl_item(self, node);
        }
    }

    /// Gates every associated item inside a `trait` block, for the same reason [`Self::visit_item`]
    /// gates top-level items.
    fn visit_trait_item(&mut self, node: &'ast TraitItem) {
        if !self.cfg.skip_gate(trait_item_attrs(node)) {
            visit::visit_trait_item(self, node);
        }
    }

    /// Gates every statement, which no item visitor above ever sees.
    ///
    /// A `#[cfg(windows)] let n: usize = 0;` inside an active function is discarded by the compiler
    /// on a Unix build, and the collector skips it for that reason — but the numeric evidence it
    /// carries would otherwise still be indexed, and would then answer for the `n` the build
    /// actually has. Statements are the level at which conditional compilation is written inside a
    /// body, so this is where that evidence has to be refused.
    fn visit_stmt(&mut self, node: &'ast Stmt) {
        if !self.cfg.skip_gate(stmt_attrs(node)) {
            self.on_local_attributes(stmt_attrs(node));
            if matches!(node, Stmt::Expr(Expr::Macro(_), _)) {
                self.on_opaque_macro();
            }
            visit::visit_stmt(self, node);
        }
    }

    /// Gates every expression, covering the positions `rustc` admits an attribute in today and the
    /// ones it does not admit yet, exactly as the collector's own `visit_expr` does.
    fn visit_expr(&mut self, node: &'ast Expr) {
        if !self.cfg.skip_gate(expr_attrs(node)) {
            self.on_local_attributes(expr_attrs(node));
            visit::visit_expr(self, node);
        }
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        self.on_item_struct(node);

        visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        self.shadow_type(&node.ident);
        visit::visit_item_enum(self, node);
    }

    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        self.shadow_type(&node.ident);
        visit::visit_item_union(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        self.enter_module(&node.ident);
        visit::visit_item_mod(self, node);
        self.exit_scope();
    }

    fn visit_block(&mut self, node: &'ast Block) {
        self.enter_block(node);
        visit::visit_block(self, node);
        self.exit_scope();
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        self.on_item_use(node);

        // #[gamma::skip(stmt.delete_call, reason = "a use tree contains no expressions, declarations, or nested items consumed by this visitor, so syn's recursive continuation cannot update an index")]
        visit::visit_item_use(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.on_item_fn(node);
        self.signature(&node.sig);
        visit::visit_item_fn(self, node);
    }

    fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
        self.on_pat_ident(node);
        visit::visit_pat_ident(self, node);
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        self.on_opaque_macro();
        // #[gamma::skip(stmt.delete_call, reason = "Walk::visit_attribute ignores metadata. The default continuation visits only attributes, paths/identifiers, visibility, and opaque macro tokens; none supplies numeric declarations or uses. Value-name registration is outside the deleted continuation and remains unchanged.", tag = "equivalent")]
        visit::visit_item_macro(self, node);
    }

    fn visit_item_extern_crate(&mut self, node: &'ast syn::ItemExternCrate) {
        self.on_item_extern_crate(node);
        // #[gamma::skip(stmt.delete_call, reason = "Walk::visit_attribute ignores metadata. The default continuation visits only attributes, paths/identifiers, visibility, and opaque macro tokens; none supplies numeric declarations or uses. Value-name registration is outside the deleted continuation and remains unchanged.", tag = "equivalent")]
        visit::visit_item_extern_crate(self, node);
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        self.on_opaque_macro();
        // #[gamma::skip(stmt.delete_call, reason = "Walk::visit_attribute ignores metadata. The default continuation visits only attributes, paths/identifiers, visibility, and opaque macro tokens; none supplies numeric declarations or uses. Value-name registration is outside the deleted continuation and remains unchanged.", tag = "equivalent")]
        visit::visit_stmt_macro(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        visit::visit_trait_item_fn(self, node);
    }

    fn visit_item_type(&mut self, node: &'ast ItemType) {
        self.alias(&node.ident.to_string(), &node.generics, &node.ty);
        visit::visit_item_type(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        self.on_value_name(&node.ident);
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.on_value_name(&node.ident);
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_item_static(self, node);
    }

    fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_impl_item_const(self, node);
    }

    fn visit_foreign_item_fn(&mut self, node: &'ast syn::ForeignItemFn) {
        self.on_value_name(&node.sig.ident);
        visit::visit_foreign_item_fn(self, node);
    }

    fn visit_foreign_item_static(&mut self, node: &'ast syn::ForeignItemStatic) {
        self.on_value_name(&node.ident);
        visit::visit_foreign_item_static(self, node);
    }

    fn visit_trait_item_const(&mut self, node: &'ast TraitItemConst) {
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_trait_item_const(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        self.on_expr_binary(node);

        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        self.on_expr_index(node);

        visit::visit_expr_index(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        self.on_expr_method_call(node);

        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.on_expr_for_loop(node);

        visit::visit_expr_for_loop(self, node);
    }
}

/// Builds the indexes a selection actually consults under a build's active configuration, and
/// skips the walk entirely when the selection consults none of them.
///
/// Three of the four exist only to decide whether an expression is a number, which only the
/// perturbation family asks; the fourth exists only to recognise a type that has no `Default`,
/// which only the `fn_value` family asks. A run narrowed to, say, the relational mutators asks
/// neither, and paying for the answers anyway is the whole of this cost.
///
/// `cfg` decides which conditionally compiled code the walk is even allowed to learn from: a
/// field, constant, `use`, or numeric use the selected build strips must not inform the guesses
/// made about code the build keeps, any more than the collector would offer a mutant there. Pass
/// [`CfgSet::unconditional`] where the build's configuration is not known, matching every other
/// unconditional entry point in this module.
pub(super) fn indexes_in(file: &File, selection: &Selection, cfg: &CfgSet) -> Indexes {
    let mut walk = Walk::new(selection, cfg);

    if
    // #[gamma::skip(cond.always_false, logical.and_remove_left, reason = "indexes_in constructs its Walk only with Walk::new, whose type_evidence is unconditionally true. The original early-exit condition and all three replacements remain false, independent of the numeric selection.", tag = "equivalent")]
    (
        // #[gamma::skip(unary.remove_not, reason = "indexes_in constructs its Walk only with Walk::new, whose type_evidence is unconditionally true. The original early-exit condition and all three replacements remain false, independent of the numeric selection.", tag = "equivalent")]
        !walk.numeric
    ) && !walk.type_evidence
    {
        return walk.indexes;
    }

    walk.visit_file(file);
    walk.into_indexes()
}

impl<'cfg> Walk<'cfg> {
    /// Builds an empty index set, gated exactly as [`indexes_in`] gates its own walk.
    ///
    /// Exposed so the fused phase-one pass (`collector::phase_one`) can build the same starting
    /// state `indexes_in` would, drive it through one combined traversal instead of
    /// `indexes_in`'s own, and read the result back out with [`Walk::into_indexes`].
    pub(super) fn new(selection: &Selection, cfg: &'cfg CfgSet) -> Self {
        Self {
            indexes: Indexes {
                fields: HashMap::default(),
                imports: HashMap::default(),
                root_imports: HashMap::default(),
                scope_imports: HashMap::default(),
                block_scopes: HashSet::default(),
                numeric_uses: NumericUses::default(),
                constants: HashMap::default(),
                declared_types: HashMap::default(),
                returns: HashMap::default(),
                parameters: HashMap::default(),
                local_calls: LocalCalls::default(),
                aliases: HashMap::default(),
                scope_aliases: HashMap::default(),
            },
            numeric: selection.contains("expr.increment")
                || selection.contains("expr.decrement")
                || selection.contains("literal.int_decrement"),

            // Resource admission needs trustworthy names even for narrow mutator selections.
            type_evidence: true,
            scope_path: Vec::new(),
            cfg,
        }
    }

    /// Consumes the walk, returning what it found.
    pub(super) fn into_indexes(mut self) -> Indexes {
        self.indexes.canonicalize_imports();
        self.indexes
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{ExprGroup, parse_file, parse_quote, parse_str, token};

    use super::*;

    #[test]
    fn disabled_type_evidence_does_not_record_declarations_or_change_local_call_bindings() {
        let cfg = CfgSet::unconditional();
        for enabled in [false, true] {
            let mut found = walk(false, enabled, &cfg);
            found.on_item_fn(&parse_quote!(
                fn helper() {}
            ));
            assert_eq!(
                found
                    .indexes
                    .local_calls
                    .returns_unit(&parse_quote!(helper), &Vec::new(), &HashMap::default()),
                enabled
            );
            found.declared("VALUE", &parse_quote!(u8));
            found.alias("Alias", &parse_quote!(<T>), &parse_quote!(Vec<T>));
            found.on_item_extern_crate(&parse_quote!(
                extern crate external as renamed;
            ));
            found.signature(&parse_quote!(fn selected(value: u8) -> bool));
            assert_eq!(found.indexes.declared_types.contains_key("VALUE"), enabled);
            assert_eq!(found.indexes.aliases.contains_key("Alias"), enabled);
            assert_eq!(found.indexes.root_imports.contains_key("renamed"), enabled);
            assert_eq!(found.indexes.parameters.contains_key("selected"), enabled);
            assert_eq!(found.indexes.returns.contains_key("selected"), enabled);
            for binding in ["pattern", "value"] {
                let mut found = walk(false, enabled, &cfg);
                found.indexes.local_calls.function(
                    &parse_quote!(
                        fn helper() {}
                    ),
                    &Vec::new(),
                );
                if binding == "pattern" {
                    let Pat::Ident(binding) = parse_quote!(helper) else {
                        unreachable!()
                    };
                    found.on_pat_ident(&binding);
                } else {
                    found.on_value_name(&parse_quote!(helper));
                }
                assert_eq!(
                    found
                        .indexes
                        .local_calls
                        .returns_unit(&parse_quote!(helper), &Vec::new(), &HashMap::default()),
                    !enabled,
                    "{binding}: {enabled}"
                );
            }
        }
    }

    #[test]
    fn signatures_keep_active_receivers_but_skip_only_configured_out_receivers() {
        let cfg = CfgSet::parse("");
        let mut found = walk(false, true, &cfg);
        found.signature(&parse_quote!(fn active(&self, amount: u8) -> bool));
        found.signature(&parse_quote!(fn excluded(#[cfg(any())] &self, amount: u8) -> bool));
        found.signature(&parse_quote!(async fn asynchronous() -> bool));
        assert_eq!(found.indexes.parameters["active"], Some(vec![parse_quote!(Self), parse_quote!(u8)]));
        assert_eq!(found.indexes.parameters["excluded"], Some(vec![parse_quote!(u8)]));
        assert_eq!(found.indexes.returns["asynchronous"], None);
    }

    #[test]
    fn attribute_metadata_does_not_supply_runtime_numeric_binding_evidence() {
        let file = parse_file(
            "#[tool::metadata = macro_left - macro_right]
             macro_rules! opaque { () => { hidden_left - hidden_right } }
             #[tool::metadata = external_left - external_right]
             extern crate dependency;
             fn f() {
                 #[tool::metadata = statement_left - statement_right]
                 opaque!();
                 runtime_left - runtime_right;
             }",
        )
        .unwrap();
        let found = indexes_in(
            &file,
            &crate::ops::registry::Selection::everything(),
            &crate::cfg::CfgSet::parse(""),
        );
        assert_eq!(
            found.numeric_uses.names,
            HashSet::from_iter(["runtime_left".into(), "runtime_right".into()])
        );
    }

    #[test]
    fn alias_dependency_order_only_follows_bare_nonparameter_nonimported_names() {
        for target in [parse_quote!(<Owner>::Alias), parse_quote!(::Alias), parse_quote!(Alias::Associated)] {
            let aliases = HashMap::from_iter([
                (
                    "Alias".into(),
                    Some(Alias {
                        parameters: vec!["Owner".into()],
                        target,
                    }),
                ),
                (
                    "Dependent".into(),
                    Some(Alias {
                        parameters: Vec::new(),
                        target: parse_quote!(Alias),
                    }),
                ),
            ]);
            let (ordered, unresolved) = alias_order(&aliases, &["Alias".into(), "Dependent".into()], &HashMap::default());
            assert!(unresolved.is_empty());
            assert_eq!(ordered, ["Alias", "Dependent"]);
        }
        let aliases = HashMap::from_iter([
            (
                "Alias".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(T),
                }),
            ),
            (
                "T".into(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(Alias<bool>),
                }),
            ),
        ]);
        let (ordered, unresolved) = alias_order(&aliases, &["Alias".into(), "T".into()], &HashMap::default());
        assert!(unresolved.is_empty());
        assert_eq!(ordered, ["Alias", "T"]);
        let aliases = HashMap::from_iter([
            (
                "Alias".into(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(Option<T>),
                }),
            ),
            (
                "T".into(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(Alias),
                }),
            ),
        ]);
        let (ordered, unresolved) = alias_order(&aliases, &["Alias".into(), "T".into()], &HashMap::default());
        assert!(ordered.is_empty());
        assert_eq!(unresolved, HashSet::from_iter(["Alias".into(), "T".into()]));
        let imports = HashMap::from_iter([("T".into(), Some(vec!["external".into(), "T".into()]))]);
        let (ordered, unresolved) = alias_order(&aliases, &["Alias".into(), "T".into()], &imports);
        assert!(unresolved.is_empty());
        assert_eq!(ordered, ["Alias", "T"]);
    }

    #[test]
    fn nearest_block_nominals_do_not_inherit_an_outer_nominals_global_identity() {
        let file = parse_file(
            "struct Number; fn f() {
                struct Number; type Local = Number;
                { struct Number; type Nested = Number; }
            }",
        )
        .unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        let mut seen = 0;
        for aliases in indexes.scope_aliases.values() {
            for name in ["Local", "Nested"] {
                if let Some(Some(alias)) = aliases.get(name) {
                    assert_eq!(alias.target, parse_quote!(_), "{name}");
                    seen += 1;
                }
            }
        }
        assert!(seen >= 2);
    }

    #[test]
    fn repeated_conditional_generic_names_restore_the_original_alias_after_each_overlay() {
        let file = parse_file(
            "type T = u8;
             type Conditional<#[cfg(unix)] T, #[cfg(not(unix))] T> = T;
             type After = T;",
        )
        .unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        assert_eq!(
            indexes.scope_aliases[&Vec::new()]["T"].as_ref().unwrap().target,
            parse_quote!(::core::primitive::u8)
        );
        assert_eq!(
            indexes.scope_aliases[&Vec::new()]["After"].as_ref().unwrap().target,
            parse_quote!(::core::primitive::u8)
        );
    }

    #[test]
    fn recursive_declaration_continuations_visit_nested_numeric_array_lengths_and_pattern_bindings() {
        let file = parse_file(
            "fn bound() {}
             struct Record { #[cfg(any())] absent: u8, visible: [u8; struct_left - struct_right] }
             enum Choice { A([u8; enum_left - enum_right]) }
             union Shared { field: [u8; union_left - union_right] }
             type Alias = [u8; alias_left - alias_right];
             unsafe extern \"C\" {
                 fn external(arg: [u8; foreign_left - foreign_right]);
                 static FOREIGN: [u8; static_left - static_right];
             }
             fn caller(arg: [u8; parameter_left - parameter_right]) {
                 let outer @ Some(bound) = unknown;
                 for item in values[range_start..range_end] {}
                 values[index];
                 value.saturating_add(method_argument - other_argument);
                 let _ = (nested_left - nested_right) * outer;
             }",
        )
        .unwrap();
        let found = indexes_in(&file, &Selection::everything(), &CfgSet::parse(""));
        for name in [
            "struct_left",
            "struct_right",
            "alias_left",
            "alias_right",
            "foreign_left",
            "foreign_right",
            "static_left",
            "static_right",
            "parameter_left",
            "parameter_right",
            "method_argument",
            "other_argument",
            "enum_left",
            "enum_right",
            "union_left",
            "union_right",
            "nested_left",
            "nested_right",
        ] {
            assert!(found.numeric_uses.names.contains(name), "{name}");
        }
        for name in ["range_start", "range_end", "index"] {
            assert!(found.numeric_uses.unsigned_names.contains(name), "{name}");
        }
        assert!(found.declared_types.contains_key("visible"));
        assert!(!found.declared_types.contains_key("absent"));
        assert!(
            !found
                .local_calls
                .returns_unit(&parse_quote!(bound), &Vec::new(), &HashMap::default())
        );
    }

    #[test]
    fn direct_numeric_notes_preserve_parentheses_and_reject_self_qualified_associated_names() {
        let cfg = CfgSet::unconditional();
        let mut found = walk(true, false, &cfg);
        found.note(&parse_quote!((ordinary)));
        found.note(&parse_quote!(<Owner>::VALUE));
        found.note_unsigned(&parse_quote!(<Owner>::INDEX));
        assert_eq!(found.indexes.numeric_uses.names, HashSet::from_iter(["ordinary".into()]));
        assert!(found.indexes.numeric_uses.unsigned_names.is_empty());
    }

    #[test]
    fn an_expression_transform_invalidates_the_current_block_not_just_its_nested_children() {
        let cfg = CfgSet::unconditional();
        let mut found = walk(false, true, &cfg);
        found.enter_block(&parse_quote!({ 0 }));
        let scope = found.scope_path.clone();
        let expression: Expr = parse_quote!(
            #[transform]
            0
        );
        assert!(found.indexes.local_calls.scope_is_transparent(&scope));
        found.visit_expr(&expression);
        assert!(!found.indexes.local_calls.scope_is_transparent(&scope));
    }

    #[test]
    fn alias_cycles_and_aliases_blocked_by_cycles_retain_only_an_unknown_target() {
        let file = parse_file("type A = B; type B = A; type Blocked = Option<A>;").unwrap();
        let found = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        for name in ["A", "B", "Blocked"] {
            assert_eq!(
                found.scope_aliases[&Vec::new()][name].as_ref().unwrap().target,
                parse_quote!(_),
                "{name}"
            );
        }
    }

    #[test]
    fn qualified_associated_values_do_not_become_bare_numeric_names() {
        let file = parse_file("fn f<T: Associated>(values: &[u8]) { let _ = <T>::VALUE + 1; let _ = values[<T>::INDEX]; }").unwrap();
        let found = indexes_in(
            &file,
            &crate::ops::registry::Selection::everything(),
            &crate::cfg::CfgSet::unconditional(),
        );
        assert!(!found.numeric_uses.names.contains("VALUE"));
        assert!(!found.numeric_uses.names.contains("INDEX"));
        assert!(!found.numeric_uses.unsigned_names.contains("INDEX"));
    }

    #[test]
    fn imported_foreign_and_parent_module_names_cannot_prove_a_local_unit_call() {
        let file = parse_file(
            "fn drop() {} fn foreign_value() {} fn unit() {} \
             mod names {use core::mem::drop; unsafe extern \"C\" {static foreign_value:u8;}} \
             mod nested { fn caller() {} }",
        )
        .unwrap();
        let found = indexes_in(
            &file,
            &crate::ops::registry::Selection::everything(),
            &crate::cfg::CfgSet::unconditional(),
        );
        let imports = crate::HashMap::default();
        for name in ["drop", "foreign_value"] {
            assert!(
                !found
                    .local_calls
                    .returns_unit(&syn::parse_str(name).unwrap(), &Vec::new(), &imports),
                "{name}"
            );
        }
        assert!(found.local_calls.returns_unit(&syn::parse_quote!(unit), &Vec::new(), &imports));
        let syn::Item::Mod(module) = file.items.last().unwrap() else {
            unreachable!()
        };
        let scope = vec![super::module_id(&module.ident)];
        assert!(!found.local_calls.returns_unit(&syn::parse_quote!(unit), &scope, &imports));
    }

    #[test]
    fn canonical_root_imports_preserve_agreement_and_demote_independent_conflicts_and_cycles() {
        let file = parse_file(
            "use std::time as clock; use clock::Duration as Span; \
             use std::time::Instant as Same; use std::time::Instant as Same; \
             use std::time::Instant as Conflict; use foreign::Instant as Conflict; \
             use cycle::nested as cycle; fn f() {}",
        )
        .unwrap();
        let cfg = crate::cfg::CfgSet::unconditional();
        let found = indexes_in(&file, &crate::ops::registry::Selection::everything(), &cfg);
        assert_eq!(
            found.root_imports["Span"],
            Some(vec!["std".into(), "time".into(), "Duration".into()])
        );
        assert_eq!(
            found.root_imports["Same"],
            Some(vec!["std".into(), "time".into(), "Instant".into()])
        );
        assert_eq!(found.root_imports["Conflict"], None);
        assert_eq!(found.root_imports["cycle"], None);
        assert_eq!(found.imports["Conflict"], None);
        assert_eq!(found.imports["cycle"], None);
    }

    #[test]
    fn recursive_declaration_visits_supply_evidence_beyond_their_outer_signature() {
        let file = parse_file(
            r#"
            fn unit() {}
            fn occupied() {}
            fn external() {}
            type Width = [u8; alias_left - alias_right];
            extern "C" {
                fn imported(external: [u8; foreign_left - foreign_right]);
                static VALUE: [u8; static_left - static_right];
            }
            struct Fields { #[cfg(any())] hidden: u8, visible: u16 }
            const CLEAN: u8 = #[transform] 1;
            fn caller() { struct occupied; use dependency::*; unit(); }
        "#,
        )
        .unwrap();
        let cfg = crate::cfg::CfgSet::parse("");
        let found = indexes_in(&file, &crate::ops::registry::Selection::everything(), &cfg);
        for name in [
            "alias_left",
            "alias_right",
            "foreign_left",
            "foreign_right",
            "static_left",
            "static_right",
        ] {
            assert!(found.numeric_uses.names.contains(name), "{name}");
        }
        assert_eq!(found.fields.get("hidden"), None);
        assert_eq!(found.fields.get("visible"), Some(&true));
        let imports = crate::HashMap::default();
        for name in ["occupied", "external", "unit"] {
            let expression = syn::parse_str(name).unwrap();
            let expected = name == "unit";
            assert_eq!(
                found.local_calls.returns_unit(&expression, &Vec::new(), &imports),
                expected,
                "{name}"
            );
        }
        let caller = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(item) if item.sig.ident == "caller" => Some(item),
                _ => None,
            })
            .unwrap();
        let scope = vec![super::block_id(&caller.block)];
        assert!(found.scope_imports[&scope].contains_key("*"));
        assert!(
            !found
                .local_calls
                .returns_unit(&syn::parse_quote!(unit), &scope, &found.scope_imports[&scope])
        );
    }

    #[test]
    fn local_index_callbacks_obey_independent_evidence_gates() {
        let cfg = CfgSet::unconditional();
        for numeric in [false, true] {
            for type_evidence in [false, true] {
                let mut walk = Walk::new(&Selection::everything(), &cfg);
                walk.numeric = numeric;
                walk.type_evidence = type_evidence;
                walk.on_item_struct(&parse_quote!(
                    struct Fields {
                        first: u8,
                        second: u16,
                    }
                ));
                walk.on_item_use(&parse_quote!(
                    use std::time::Duration;
                ));
                walk.on_expr_binary(&parse_quote!(left - right));
                walk.on_expr_index(&parse_quote!(items[position]));
                walk.on_expr_method_call(&parse_quote!(receiver.abs()));
                walk.on_expr_for_loop(&parse_quote!(for bound in 0..4 {}));
                assert_eq!(walk.indexes.declared_types.len(), if type_evidence { 2 } else { 0 });
                assert_eq!(walk.indexes.fields.len(), if numeric { 2 } else { 0 });
                assert_eq!(walk.indexes.root_imports.contains_key("Duration"), type_evidence);
                for name in ["left", "right", "position", "receiver", "bound"] {
                    assert_eq!(walk.indexes.numeric_uses.names.contains(name), numeric, "{name}");
                }
                assert_eq!(walk.indexes.numeric_uses.unsigned_names.contains("position"), numeric);
            }
        }
        let file = parse_file("mod retained {} struct Fields { value:u8 }").unwrap();
        let empty = indexes_in(&file, &Selection::parse("relational").unwrap(), &cfg);
        assert_eq!(
            empty.root_imports,
            HashMap::from_iter([("retained".to_owned(), Some(vec!["self".to_owned(), "retained".to_owned()]))])
        );
        assert_eq!(empty.scope_imports.len(), 1);
        assert!(empty.scope_imports.values().all(HashMap::is_empty));
        assert!(empty.fields.is_empty());
    }

    #[test]
    fn block_identity_uses_exact_opening_line_and_column() {
        let file = parse_file("\n fn first() { }\n\n    fn second() { }\n").unwrap();
        let identities = file
            .items
            .iter()
            .map(|item| {
                let syn::Item::Fn(function) = item else {
                    panic!("the fixture contains only functions");
                };
                block_id(&function.block)
            })
            .collect::<Vec<_>>();
        assert_eq!(identities, [(2, 12), (4, 16)]);
    }

    #[test]
    fn qualified_self_paths_are_not_bare_numeric_binding_evidence() {
        let cfg = CfgSet::unconditional();
        let mut walk = Walk::new(&Selection::everything(), &cfg);
        walk.note(&parse_quote!(<T>::value));
        walk.note_unsigned(&parse_quote!(<T>::index));
        assert!(walk.indexes.numeric_uses.names.is_empty());
        assert!(walk.indexes.numeric_uses.unsigned_names.is_empty());
        walk.note(&parse_quote!(value));
        walk.note_unsigned(&parse_quote!(index));
        assert!(walk.indexes.numeric_uses.names.contains("value"));
        assert!(walk.indexes.numeric_uses.unsigned_names.contains("index"));
    }

    #[test]
    fn canonical_import_matrix_preserves_absolute_roots_and_rejects_unknown_cycles() {
        let imports = HashMap::from_iter([
            ("Vec".into(), Some(vec!["std".into(), "vec".into(), "Vec".into()])),
            ("Alias".into(), Some(vec!["Vec".into()])),
            ("Nested".into(), Some(vec!["Alias".into(), "Item".into()])),
            ("Unknown".into(), None),
            ("Empty".into(), Some(vec![])),
            ("SelfName".into(), Some(vec!["SelfName".into()])),
            ("CycleA".into(), Some(vec!["CycleB".into()])),
            ("CycleB".into(), Some(vec!["CycleA".into()])),
            ("Absolute".into(), Some(vec![ABSOLUTE_ROOT.into(), "Vec".into()])),
        ]);
        for (name, expected) in [
            ("Vec", Some(vec!["std", "vec", "Vec"])),
            ("Alias", Some(vec!["std", "vec", "Vec"])),
            ("Nested", Some(vec!["std", "vec", "Vec", "Item"])),
            ("Unknown", None),
            ("Empty", None),
            ("Missing", None),
            ("SelfName", Some(vec!["SelfName"])),
            ("CycleA", None),
            ("CycleB", None),
            ("Absolute", Some(vec![ABSOLUTE_ROOT, "Vec"])),
        ] {
            assert_eq!(
                canonical_import(name, &imports),
                expected.map(|path| path.into_iter().map(str::to_owned).collect()),
                "{name}"
            );
        }
        for path in [vec![], vec!["std".into()], vec![ABSOLUTE_ROOT.into(), "std".into()]] {
            let protected = protect_import(path.clone());
            assert_eq!(protected.first().map(String::as_str), Some(ABSOLUTE_ROOT));
            assert_eq!(protect_import(protected.clone()), protected);
            if path.first().is_none_or(|first| first != ABSOLUTE_ROOT) {
                assert_eq!(&protected[1..], path);
            } else {
                assert_eq!(protected, path);
            }
        }
        let mut types = HashMap::default();
        merge_type(&mut types, "same", &parse_quote!(u8));
        merge_type(&mut types, "same", &parse_quote!(u8));
        assert_eq!(types["same"], Some(parse_quote!(u8)));
        merge_type(&mut types, "same", &parse_quote!(bool));
        assert_eq!(types["same"], None);
        merge_type(&mut types, "same", &parse_quote!(u8));
        assert_eq!(types["same"], None);
    }

    #[test]
    fn lexical_import_scopes_keep_local_shadowing_and_parent_alias_resolution() {
        let file = parse_file(
            "use std::vec::Vec as Root; fn f() {
                use Root as First; { use First as Second; let _ = 1; }
                { use other::Vec as Root; use Root as Foreign; }
             } mod m { use alloc::vec::Vec as Local; fn g() { use Local as Alias; } }",
        )
        .unwrap();
        let indexes = indexes_in(&file, &Selection::parse("fn_value").unwrap(), &CfgSet::unconditional());
        assert_eq!(indexes.root_imports["Root"], Some(vec!["std".into(), "vec".into(), "Vec".into()]));
        assert_eq!(indexes.root_imports["m"], Some(vec!["self".into(), "m".into()]));
        let inherited = Some(vec![ABSOLUTE_ROOT.into(), "std".into(), "vec".into(), "Vec".into()]);
        let mut second = 0;
        let mut foreign = 0;
        let mut alias = 0;
        for scope in indexes.scope_imports.values() {
            if scope.contains_key("Second") {
                second += 1;
                assert_eq!(scope["Second"], inherited);
                assert_eq!(scope["Root"], inherited);
            }
            if scope.contains_key("Foreign") {
                foreign += 1;
                assert_eq!(scope["Root"], Some(vec!["other".into(), "Vec".into()]));
                assert_eq!(scope["Foreign"], Some(vec!["other".into(), "Vec".into()]));
            }
            if scope.contains_key("Alias") {
                alias += 1;
                assert_eq!(
                    scope["Alias"],
                    Some(vec![ABSOLUTE_ROOT.into(), "alloc".into(), "vec".into(), "Vec".into()])
                );
                assert!(!scope.contains_key("Root"));
            }
        }
        assert_eq!((second, foreign, alias), (1, 1, 1));
    }

    #[test]
    fn index_selection_matrix_populates_only_the_requested_evidence() {
        let file = parse_file(
            "use std::vec::Vec; type Count = usize; struct Record { size: usize }
             const LIMIT: usize = 4; fn helper(value: usize) -> bool { value > 0 }
             fn uses(values: &[u8], offset: usize) { let _ = values[offset]; }",
        )
        .unwrap();
        let cfg = CfgSet::unconditional();
        for (selector, numeric, type_evidence) in [
            ("relational.eq_to_ne", false, true),
            ("fn_value.bool_true", false, true),
            ("result.ok_to_err", false, true),
            ("result.err_to_ok", false, true),
            ("option.none_to_some", false, true),
            ("assign_value.default", false, true),
            ("call.replace_with_default", false, true),
            ("call_result.default", false, true),
            ("parameter.default_shadow", false, true),
            ("return_value.default", false, true),
            ("bool_expr.negate", false, true),
            ("arith.add_to_sub", false, true),
            ("iter.last_to_first", false, true),
            ("iter.remove_filter", false, true),
            ("expr.increment", true, true),
            ("expr.decrement", true, true),
            ("literal.int_decrement", true, true),
        ] {
            let selection = Selection::parse(selector).unwrap();
            let indexes = indexes_in(&file, &selection, &cfg);
            assert_eq!(indexes.constants.get("LIMIT").copied(), numeric.then_some(true), "{selector}");
            assert_eq!(indexes.fields.get("size").copied(), numeric.then_some(true), "{selector}");
            assert_eq!(indexes.numeric_uses.names.contains("offset"), numeric, "{selector}");
            assert_eq!(indexes.numeric_uses.unsigned_names.contains("offset"), numeric, "{selector}");
            assert_eq!(
                indexes.returns.get("helper"),
                type_evidence.then_some(&Some(parse_quote!(bool))),
                "{selector}"
            );
            assert_eq!(
                indexes.parameters.get("helper"),
                type_evidence.then_some(&Some(vec![parse_quote!(usize)])),
                "{selector}"
            );
            assert_eq!(
                indexes.declared_types.get("LIMIT"),
                type_evidence.then_some(&Some(parse_quote!(usize))),
                "{selector}"
            );
            assert_eq!(indexes.root_imports.contains_key("Vec"), type_evidence, "{selector}");
            assert_eq!(indexes.aliases.contains_key("Count"), type_evidence, "{selector}");
        }
    }

    #[test]
    fn repeated_declarations_preserve_agreement_and_permanently_demote_collisions() {
        let cfg = CfgSet::unconditional();
        for selector in ["relational", "fn_value", "expr.increment"] {
            let selection = Selection::parse(selector).unwrap();
            let mut walk = Walk::new(&selection, &cfg);
            for (name, text) in [("same", "u8"), ("same", "u8"), ("mixed", "u8"), ("mixed", "bool"), ("mixed", "u8")] {
                walk.declared(name, &parse_str(text).unwrap());
            }
            let first: Signature = parse_quote!(fn helper(value:u8)->u8);
            walk.signature(&first);
            walk.signature(&first);
            assert_eq!(
                walk.indexes.parameters.get("helper"),
                walk.type_evidence.then_some(&Some(vec![parse_quote!(u8)]))
            );
            walk.signature(&parse_quote!(fn helper(value:bool)->u8));
            walk.signature(&first);
            assert_eq!(walk.indexes.parameters.get("helper"), walk.type_evidence.then_some(&None));
            let generic: syn::Generics = parse_quote!(<T>);
            walk.alias("Alias", &generic, &parse_quote!(Vec<T>));
            walk.alias("Alias", &generic, &parse_quote!(Vec<T>));
            assert_eq!(
                walk.indexes.aliases.get("Alias").map(Option::is_some),
                walk.type_evidence.then_some(true)
            );
            walk.alias("Alias", &generic, &parse_quote!(Option<T>));
            walk.alias("Alias", &generic, &parse_quote!(Vec<T>));
            assert_eq!(
                walk.indexes.aliases.get("Alias").map(Option::is_none),
                walk.type_evidence.then_some(true)
            );
            assert_eq!(
                walk.indexes.declared_types.get("same"),
                walk.type_evidence.then_some(&Some(parse_quote!(u8)))
            );
            assert_eq!(walk.indexes.declared_types.get("mixed"), walk.type_evidence.then_some(&None));
            assert_eq!(walk.indexes.constants.get("same"), walk.numeric.then_some(&true));
            assert_eq!(walk.indexes.constants.get("mixed"), walk.numeric.then_some(&false));
        }
        let source = "
            struct A { size:u8 } struct B { size:bool }
            trait T { #[cfg(not(unix))] fn absent()->u8 { 1 } fn active()->u8 { 1 } }
            struct S; impl S { #[cfg(not(unix))] fn hidden()->u8 { 1 } fn visible()->u8 { 1 } }
            #[cfg(not(unix))] fn free_hidden()->u8 { 1 }
            fn free_active()->u8 { 1 }
            fn f(values:&[u8], index:usize) {
                #[cfg(not(unix))] let _ = values[excluded];
                let _ = values[index]; { use a::*; }
            }";
        let cfg = CfgSet::parse("unix");
        let indexes = indexes_in(&parse_file(source).unwrap(), &Selection::everything(), &cfg);
        assert!(!indexes.fields["size"]);
        assert!(!indexes.returns.contains_key("active"));
        assert!(!indexes.returns.contains_key("visible"));
        assert!(!indexes.returns.contains_key("absent"));
        assert!(!indexes.returns.contains_key("hidden"));
        assert_eq!(indexes.returns.get("free_active"), Some(&Some(parse_quote!(u8))));
        assert!(!indexes.returns.contains_key("free_hidden"));
        assert!(indexes.numeric_uses.unsigned_names.contains("index"));
        assert!(!indexes.numeric_uses.names.contains("excluded"));
        assert_eq!(indexes.imports.get("*"), Some(&None));
        assert!(!indexes.root_imports.contains_key("*"));
    }

    fn walk(numeric: bool, type_evidence: bool, cfg: &CfgSet) -> Walk<'_> {
        Walk {
            indexes: Indexes {
                fields: HashMap::default(),
                imports: HashMap::default(),
                root_imports: HashMap::default(),
                scope_imports: HashMap::default(),
                block_scopes: HashSet::default(),
                numeric_uses: NumericUses::default(),
                constants: HashMap::default(),
                declared_types: HashMap::default(),
                returns: HashMap::default(),
                parameters: HashMap::default(),
                local_calls: LocalCalls::default(),
                aliases: HashMap::default(),
                scope_aliases: HashMap::default(),
            },
            numeric,
            type_evidence,
            scope_path: Vec::new(),
            cfg,
        }
    }

    #[test]
    fn note_tracks_named_fields_behind_references() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);
        let expression = parse_str::<Expr>("&record.count").expect("the field expression parses");

        walk.note(&expression);

        assert!(walk.indexes.numeric_uses.fields.contains("count"));
    }

    #[test]
    fn note_accepts_only_unqualified_bare_paths_and_named_fields() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);

        walk.note(&parse_str::<Expr>("offset").expect("the bare path parses"));
        walk.note(&parse_str::<Expr>("<T as Trait>::VALUE").expect("the qualified path parses"));
        walk.note(&parse_str::<Expr>("tuple.0").expect("the tuple field parses"));

        assert_eq!(walk.indexes.numeric_uses.names.len(), 1);
        assert!(walk.indexes.numeric_uses.names.contains("offset"));
        assert!(walk.indexes.numeric_uses.fields.is_empty());
    }

    #[test]
    fn unsigned_notes_descend_through_ranges_and_transparent_wrappers() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);
        let expression = parse_str::<Expr>("&(start..(end))").expect("the wrapped range parses");

        walk.note_unsigned(&expression);

        assert!(walk.indexes.numeric_uses.unsigned_names.contains("start"));
        assert!(walk.indexes.numeric_uses.unsigned_names.contains("end"));

        walk.note_unsigned(&Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: token::Group::default(),
            expr: Box::new(parse_quote!(grouped)),
        }));
        assert!(walk.indexes.numeric_uses.unsigned_names.contains("grouped"));
    }

    #[test]
    fn numeric_index_and_receiver_uses_are_recorded() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);
        let index: syn::ExprIndex = parse_quote!(values[offset]);
        let method: syn::ExprMethodCall = parse_quote!(capacity.saturating_add(1));
        let left_literal: syn::ExprBinary = parse_quote!(1 + width);

        walk.on_expr_index(&index);
        walk.on_expr_method_call(&method);
        walk.on_expr_binary(&left_literal);

        assert!(walk.indexes.numeric_uses.names.contains("offset"));
        assert!(walk.indexes.numeric_uses.names.contains("capacity"));
        assert!(walk.indexes.numeric_uses.names.contains("width"));
    }

    #[test]
    fn numeric_evidence_requires_the_exact_operator_and_receiver_shapes() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);

        for expression in ["left - right", "left + 1", "1 + right", "left[at]", "index.saturating_add(1)"] {
            let expression = parse_str::<Expr>(expression).expect("the numeric expression parses");
            visit::visit_expr(&mut walk, &expression);
        }
        for expression in [
            "text + suffix",
            "value == other",
            "text.max(other)",
            "for item in values { use_item(item); }",
        ] {
            let expression = parse_str::<Expr>(expression).expect("the non-evidence expression parses");
            visit::visit_expr(&mut walk, &expression);
        }

        for expected in ["left", "right", "at", "index"] {
            assert!(walk.indexes.numeric_uses.names.contains(expected), "{expected}");
        }
        for absent in ["text", "suffix", "value", "other", "item"] {
            assert!(!walk.indexes.numeric_uses.names.contains(absent), "{absent}");
        }
    }

    #[test]
    fn declared_ignores_constants_when_numeric_index_is_disabled() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, false, &cfg);
        let ty = parse_str::<Type>("usize").expect("the numeric type parses");

        walk.declared("COUNT", &ty);

        assert!(walk.indexes.constants.is_empty());
    }

    #[test]
    fn conflicting_declarations_and_fields_are_demoted_to_non_numeric() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);
        walk.declared("LIMIT", &parse_quote!(usize));
        walk.declared("LIMIT", &parse_quote!(&str));
        walk.on_item_struct(&parse_quote!(
            struct Numeric {
                count: usize,
            }
        ));
        walk.on_item_struct(&parse_quote!(
            struct Text {
                count: String,
            }
        ));

        assert_eq!(walk.indexes.constants.get("LIMIT"), Some(&false));
        assert_eq!(walk.indexes.fields.get("count"), Some(&false));
    }

    #[test]
    fn conflicting_function_signatures_are_demoted_to_unknown() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, true, &cfg);

        walk.signature(&parse_quote!(fn convert(value: usize) -> usize));
        walk.signature(&parse_quote!(fn convert(value: String) -> usize));

        assert_eq!(walk.indexes.parameters.get("convert"), Some(&None));
    }

    #[test]
    fn conflicting_aliases_are_demoted_and_disabled_type_evidence_stays_empty() {
        let cfg = CfgSet::unconditional();
        let mut enabled = walk(false, true, &cfg);
        let generics: syn::Generics = parse_quote!(<T>);

        enabled.alias("Value", &generics, &parse_quote!(Option<T>));
        enabled.alias("Value", &generics, &parse_quote!(Vec<T>));
        assert!(matches!(enabled.indexes.aliases.get("Value"), Some(None)));
        enabled.on_item_use(&parse_quote!(
            use ::std::vec::Vec;
        ));
        assert_eq!(
            enabled.indexes.imports.get("Vec"),
            Some(&Some(vec![
                ABSOLUTE_ROOT.to_owned(),
                "std".to_owned(),
                "vec".to_owned(),
                "Vec".to_owned()
            ]))
        );

        let mut disabled = walk(false, false, &cfg);
        disabled.returned("f", &parse_quote!(-> usize));
        assert!(disabled.indexes.returns.is_empty());
    }

    #[test]
    fn scoped_aliases_preserve_agreement_and_reject_conflicting_targets_and_parameters() {
        let cfg = CfgSet::unconditional();
        for (second_generics, second_target) in [
            (parse_quote!(<T>), parse_quote!(Option<T>)),
            (parse_quote!(<T>), parse_quote!(Vec<T>)),
            (parse_quote!(<U>), parse_quote!(Option<T>)),
        ] {
            let mut walk = walk(false, true, &cfg);
            let scope = vec![(7, 3)];
            walk.scope_path.clone_from(&scope);
            let generics = parse_quote!(<T>);
            let target = parse_quote!(Option<T>);
            walk.alias("Shared", &generics, &target);
            walk.alias("Shared", &second_generics, &second_target);
            let agrees = second_generics == generics && second_target == target;
            let expected = agrees.then(|| Alias {
                parameters: vec!["T".into()],
                target: target.clone(),
            });
            assert!(walk.indexes.scope_aliases[&scope]["Shared"] == expected);
            assert!(walk.indexes.aliases["Shared"] == expected);
            if !agrees {
                walk.alias("Shared", &generics, &target);
                assert!(walk.indexes.scope_aliases[&scope]["Shared"].is_none());
                assert!(walk.indexes.aliases["Shared"].is_none());
            }
        }
    }

    #[test]
    fn block_imports_shadow_inherited_nominal_markers_when_freezing_local_aliases() {
        let file = parse_file(
            "struct Token;
             fn f() {
                 use core::primitive::u8 as Token;
                 type Value = Token;
             }",
        )
        .unwrap();
        let Item::Fn(function) = &file.items[1] else { unreachable!() };
        let scope = vec![block_id(&function.block)];
        let indexes = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        assert!(indexes.scope_aliases[&scope]["Token"].is_none());
        assert_eq!(
            indexes.scope_aliases[&scope]["Value"].as_ref().unwrap().target,
            parse_quote!(::core::primitive::u8)
        );
    }

    #[test]
    fn canonical_alias_expansion_checks_the_frozen_import_path_against_its_work_budget() {
        let file = parse_file(
            "struct First; struct Second; struct Third; struct Fourth;
             use dependency::Foreign as Short; type Alias = Short;",
        )
        .unwrap();
        let cfg = CfgSet::unconditional();
        // A frozen path costs one type visit plus one per segment. The canonical pass reserves
        // that cost both on entry and for alias instantiation, even for an already qualified path.
        // These adjacent lengths therefore exactly fill, then exceed, TYPE_EXPANSION_LIMIT.
        for segments in [2047, 2048] {
            let mut walk = walk(false, true, &cfg);
            walk.visit_file(&file);
            let aliases = &walk.indexes.scope_aliases[&Vec::new()];
            assert!(
                aliases
                    .keys()
                    .take_while(|name| name.as_str() != "Alias")
                    .any(|name| aliases[name].is_none()),
                "the fixture must visit a nominal marker before the alias to exercise continue"
            );
            let mut path = vec!["segment".to_owned(); segments];
            path[0] = "dependency".into();
            path[segments - 1] = "Foreign".into();
            let _previous = walk.indexes.root_imports.insert("Short".into(), Some(path.clone()));
            walk.indexes.canonicalize_aliases();
            let target = &walk.indexes.scope_aliases[&Vec::new()]["Alias"].as_ref().unwrap().target;
            if segments == 2047 {
                let Type::Path(target) = target else {
                    panic!("the exact work-budget boundary must retain the imported path")
                };
                assert_eq!(
                    target
                        .path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>(),
                    path
                );
            } else {
                assert_eq!(*target, parse_quote!(_));
            }
        }
    }

    #[test]
    fn alias_parameter_overlays_restore_shared_aliases_imports_and_absent_names() {
        let mut file = parse_file(
            "use std::vec::Vec;
             type Existing = usize;
             type AliasParameter<Existing> = (Existing, bool);
             type ImportParameter<Vec> = (Vec, bool);
             type AbsentParameter<Missing> = (Missing, bool);
             type Both<Existing, Vec> = (Existing, Vec);
             type Concrete = Existing;
             type Imported = Vec<Existing>;",
        )
        .unwrap();
        for _ in 0..2 {
            let indexes = indexes_in(&file, &Selection::parse("fn_value").unwrap(), &CfgSet::unconditional());
            let aliases = &indexes.scope_aliases[&Vec::new()];
            for (name, expected) in [
                ("Existing", parse_quote!(::core::primitive::usize)),
                ("AliasParameter", parse_quote!((Existing, ::core::primitive::bool))),
                ("ImportParameter", parse_quote!((Vec, ::core::primitive::bool))),
                ("AbsentParameter", parse_quote!((Missing, ::core::primitive::bool))),
                ("Both", parse_quote!((Existing, Vec))),
                ("Concrete", parse_quote!(::core::primitive::usize)),
                ("Imported", parse_quote!(std::vec::Vec<::core::primitive::usize>)),
            ] {
                assert_eq!(aliases[name].as_ref().unwrap().target, expected, "{name}");
            }
            assert_eq!(aliases["Both"].as_ref().unwrap().parameters, ["Existing", "Vec"]);
            file.items.reverse();
        }
    }

    #[test]
    fn review_forwarded_io_aliases_preserve_nested_payloads() {
        let file = parse_file(
            "type Wrapped<T> = std::io::Result<Vec<T>>;
             type Forward<T> = Wrapped<T>;
             type Again<T> = Forward<T>;",
        )
        .unwrap();
        let cfg = CfgSet::unconditional();
        let mut raw = walk(false, true, &cfg);
        raw.visit_file(&file);
        let defaults = crate::ops::collect::Defaults::of(&file);
        let types = super::super::types::Types {
            abstracts: &[],
            defaulted: &[],
            imports: &raw.indexes.root_imports,
            defaults: &defaults,
            aliases: Some(&raw.indexes.scope_aliases[&Vec::new()]),
            self_type: None,
            self_associated: None,
        };
        assert_eq!(types.declaration_type(&parse_quote!(Wrapped<T>)), parse_quote!(Wrapped<T>));
        let indexes = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        let aliases = &indexes.scope_aliases[&Vec::new()];
        for name in ["Wrapped", "Forward", "Again"] {
            assert_eq!(
                aliases[name].as_ref().unwrap().target,
                parse_quote!(std::result::Result<Vec<T>, std::io::Error>),
                "{name}"
            );
        }
    }

    #[test]
    fn review_block_types_shadow_inherited_imports_without_rebinding_parent_aliases() {
        let file = parse_file(
            "use core::primitive::u8 as Number;
             type Parent = Number;
             fn f() {
                 type Number = i16;
                 type Local = Number;
                 type Retained = Parent;
             }
             fn g() {
                 struct Number;
                 type Local = Number;
             }",
        )
        .unwrap();
        let indexes = indexes_in(&file, &Selection::everything(), &CfgSet::unconditional());
        let mut found = 0;
        for (scope, aliases) in &indexes.scope_aliases {
            if !indexes.block_scopes.contains(scope) || !aliases.contains_key("Local") {
                continue;
            }
            found += 1;
            let local = &aliases["Local"].as_ref().unwrap().target;
            if aliases.contains_key("Retained") {
                assert_eq!(*local, parse_quote!(::core::primitive::i16));
                assert_eq!(aliases["Retained"].as_ref().unwrap().target, parse_quote!(::core::primitive::u8));
            } else {
                assert_eq!(*local, parse_quote!(_));
            }
        }
        assert_eq!(found, 2);
    }

    #[test]
    fn alias_parameters_do_not_capture_sibling_nominal_or_imported_targets() {
        for (source, fixed) in [
            (
                "struct T; type Fixed = T; type Pair<T> = (Fixed, T); type Chained<T> = Pair<T>;
                 type Slice = [T]; type Pointer = *const T; type Nested<T> = (&'static Slice, Pointer, [Fixed; 1], T);",
                parse_quote!(self::T),
            ),
            (
                "use std::string::String as T; type Fixed = T; type Pair<T> = (Fixed, T); type Chained<T> = Pair<T>;
                 type Slice = [T]; type Pointer = *const T; type Nested<T> = (&'static Slice, Pointer, [Fixed; 1], T);",
                parse_quote!(std::string::String),
            ),
            (
                "type T = u16; type Fixed = T; type Pair<T> = (Fixed, T); type Chained<T> = Pair<T>;
                 type Slice = [T]; type Pointer = *const T; type Nested<T> = (&'static Slice, Pointer, [Fixed; 1], T);",
                parse_quote!(::core::primitive::u16),
            ),
        ] {
            let mut file = parse_file(source).unwrap();
            for _ in 0..2 {
                let indexes = indexes_in(&file, &Selection::parse("fn_value").unwrap(), &CfgSet::unconditional());
                let aliases = &indexes.scope_aliases[&Vec::new()];
                assert_eq!(aliases["Fixed"].as_ref().unwrap().target, fixed);
                assert_eq!(
                    aliases["Nested"].as_ref().unwrap().target,
                    parse_quote!((&'static [#fixed], *const #fixed, [#fixed; 1], T))
                );
                let defaults = crate::ops::collect::Defaults::of(&file);
                let types = super::super::types::Types {
                    abstracts: &[],
                    defaulted: &[],
                    imports: &indexes.root_imports,
                    defaults: &defaults,
                    aliases: Some(aliases),
                    self_type: None,
                    self_associated: None,
                };
                for (name, usage) in [("Pair", parse_quote!(Pair<u8>)), ("Chained", parse_quote!(Chained<u8>))] {
                    assert_eq!(aliases[name].as_ref().unwrap().target, parse_quote!((#fixed, T)), "{name}");
                    assert_eq!(types.instantiate_alias(&usage), Some(parse_quote!((#fixed, u8))), "{name}");
                    let values = super::super::values::values_for(&usage, 4, &types);
                    assert_eq!(values.is_empty(), source.starts_with("struct T"), "{name}");
                }
                file.items.reverse();
            }
        }
    }

    #[test]
    fn signatures_record_implicit_unit_and_only_free_functions() {
        let file =
            parse_file("fn free() {} struct Service; impl Service { fn free() -> usize { 1 } } trait Contract { fn free() -> bool; }")
                .expect("the signature fixture parses");
        let selection = Selection::parse("call").expect("the family resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::unconditional());

        assert_eq!(indexes.returns.get("free"), Some(&Some(parse_quote!(()))));
        assert_eq!(indexes.parameters.get("free"), Some(&Some(Vec::new())));
    }

    #[test]
    fn signature_parameters_follow_the_active_configuration() {
        let cfg = CfgSet::parse("unix\n");
        let mut walk = walk(false, true, &cfg);

        walk.signature(&parse_quote!(
            fn configured(#[cfg(windows)] removed: String, kept: usize) -> usize
        ));

        assert_eq!(walk.indexes.parameters.get("configured"), Some(&Some(vec![parse_quote!(usize)])));
    }

    #[test]
    fn descend_handles_groups_renames_and_globs() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, true, &cfg);
        let item = parse_str::<ItemUse>("use crate::{Thing as Alias, inner::Item, *};").expect("the use item parses");

        walk.descend(&mut Vec::new(), &item.tree);

        assert_eq!(
            walk.indexes.imports.get("Alias"),
            Some(&Some(vec!["crate".to_owned(), "Thing".to_owned()]))
        );
        assert_eq!(
            walk.indexes.imports.get("Item"),
            Some(&Some(vec!["crate".to_owned(), "inner".to_owned(), "Item".to_owned()]))
        );
        assert_eq!(walk.indexes.imports.get("*"), Some(&None));
        assert_eq!(walk.indexes.imports.len(), 3);
        assert_eq!(walk.indexes.root_imports, walk.indexes.imports);
    }

    #[test]
    fn root_imports_include_only_root_trait_shadows() {
        let file = parse_file("trait Iterator {} mod nested { trait Future {} }").expect("the trait fixture parses");
        let selection = Selection::parse("iter").expect("the family resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::unconditional());

        assert_eq!(indexes.imports.get("Iterator"), Some(&None));
        assert_eq!(indexes.imports.get("Future"), Some(&None));
        assert_eq!(indexes.root_imports.get("Iterator"), Some(&None));
        assert!(!indexes.root_imports.contains_key("Future"));
    }

    #[test]
    fn conflicting_imports_are_unknown_while_repeated_imports_stay_known() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, true, &cfg);

        for item in [
            "use first::Thing;",
            "use first::Thing;",
            "use second::Thing;",
            "use crate::module::{self, Item};",
        ] {
            let item = parse_str::<ItemUse>(item).expect("the use item parses");
            walk.descend(&mut Vec::new(), &item.tree);
        }

        assert_eq!(walk.indexes.imports.get("Thing"), Some(&None));
        assert_eq!(
            walk.indexes.imports.get("module"),
            Some(&Some(vec!["crate".to_owned(), "module".to_owned()]))
        );
        assert_eq!(
            walk.indexes.imports.get("Item"),
            Some(&Some(vec!["crate".to_owned(), "module".to_owned(), "Item".to_owned()]))
        );
    }

    #[test]
    fn import_paths_are_canonicalized_once_and_cycles_are_unknown() {
        let imports = HashMap::from_iter([
            ("Base".to_owned(), Some(vec!["external".to_owned(), "Item".to_owned()])),
            ("Alias".to_owned(), Some(vec!["Base".to_owned()])),
            ("Left".to_owned(), Some(vec!["Right".to_owned()])),
            ("Right".to_owned(), Some(vec!["Left".to_owned()])),
            ("local".to_owned(), Some(vec!["local".to_owned()])),
            ("Ambiguous".to_owned(), None),
        ]);
        let cfg = CfgSet::unconditional();
        let mut indexes = walk(false, true, &cfg).into_indexes();
        indexes.imports = imports;
        indexes.canonicalize_imports();

        assert_eq!(
            indexes.imports.get("Alias"),
            Some(&Some(vec!["external".to_owned(), "Item".to_owned()]))
        );
        assert_eq!(indexes.imports.get("Left"), Some(&None));
        assert_eq!(indexes.imports.get("Right"), Some(&None));
        assert_eq!(indexes.imports.get("local"), Some(&Some(vec!["local".to_owned()])));
        assert_eq!(indexes.imports.get("Ambiguous"), Some(&None));
    }

    #[test]
    fn indexes_collect_static_and_trait_constants_without_named_tuple_fields() {
        let file = parse_file(
            r"
            trait Limits {
                const TRAIT_LIMIT: usize;
            }

            static STATIC_LIMIT: usize = 1;

            struct Pair(usize, usize);

            fn note(limit: usize) {
                let _ = 1 < limit;
            }
            ",
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::unconditional());

        assert_eq!(indexes.constants.get("STATIC_LIMIT"), Some(&true));
        assert_eq!(indexes.constants.get("TRAIT_LIMIT"), Some(&true));
        assert!(indexes.fields.is_empty());
        assert!(indexes.numeric_uses.names.contains("limit"));
    }

    #[test]
    fn recursive_constant_and_struct_visits_collect_initializer_and_type_expressions() {
        let file = parse_file(
            r"
            const LIMIT: usize = left - right;
            static FLOOR: usize = low - high;
            struct Buffer([u8; width - 1]);
            trait T { const STEP: usize = trait_left - trait_right; }
            impl T for Buffer { const STEP: usize = impl_left - impl_right; }
            ",
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::unconditional());

        for expected in [
            "left",
            "right",
            "low",
            "high",
            "width",
            "trait_left",
            "trait_right",
            "impl_left",
            "impl_right",
        ] {
            assert!(indexes.numeric_uses.names.contains(expected), "{expected}");
        }
    }

    #[test]
    fn selection_always_enables_type_identity_but_gates_numeric_indexes() {
        let cfg = CfgSet::unconditional();

        let numeric = Walk::new(&Selection::parse("expr.decrement").expect("selector resolves"), &cfg);
        assert!(numeric.numeric);
        assert!(numeric.type_evidence);

        let result = Walk::new(&Selection::parse("result.ok_to_err").expect("selector resolves"), &cfg);
        assert!(!result.numeric);
        assert!(result.type_evidence);

        let unrelated = Walk::new(&Selection::parse("literal.bool_flip").expect("selector resolves"), &cfg);
        assert!(!unrelated.numeric);
        assert!(unrelated.type_evidence);

        let file = parse_file("const LIMIT: usize = left - right; use crate::Thing;").expect("the file parses");
        let indexes = indexes_in(&file, &Selection::parse("literal.bool_flip").expect("selector resolves"), &cfg);
        assert!(indexes.constants.is_empty());
        assert_eq!(
            indexes.imports.get("Thing"),
            Some(&Some(vec!["crate".to_owned(), "Thing".to_owned()]))
        );
        assert!(indexes.numeric_uses.names.is_empty());
    }

    /// A field or a constant behind a predicate the active build does not satisfy must not
    /// inform the numeric guess for an active same-named field or constant elsewhere in the file.
    ///
    /// The set has to be an enforced one: [`CfgSet::unconditional`] answers every predicate `true`
    /// by construction, so nothing is stripped under it and the fixture would prove nothing.
    #[test]
    fn inactive_fields_and_constants_do_not_pollute_the_index() {
        let file = parse_file(
            r#"
            struct Active {
                count: u32,
            }

            struct Inactive {
                #[cfg(windows)]
                count: String,
            }

            #[cfg(windows)]
            const COUNT: &str = "not built";

            const COUNT: u32 = 1;
            "#,
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::parse("unix\n"));

        assert_eq!(indexes.fields.get("count"), Some(&true));
        assert_eq!(indexes.constants.get("COUNT"), Some(&true));
    }

    #[test]
    fn a_skipped_or_unnamed_field_does_not_end_the_struct_scan() {
        let file = parse_file(
            r"
            struct Record(
                #[cfg(windows)] String,
                usize,
            );

            struct Named {
                #[cfg(windows)]
                ignored: String,
                count: usize,
            }
            ",
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::parse("unix\n"));

        assert_eq!(indexes.fields.get("count"), Some(&true));
    }

    /// A misplaced or malformed item nested inside an inactive module must not reach this index
    /// at all — the module itself never compiles, so nothing inside it should be able to shadow
    /// or demote evidence the active build actually relies on.
    #[test]
    fn items_nested_in_an_inactive_module_are_not_indexed() {
        let file = parse_file(
            r#"
            #[cfg(windows)]
            mod inactive {
                struct S {
                    count: String,
                }

                const COUNT: &str = "not built";
            }

            struct S {
                count: u32,
            }

            const COUNT: u32 = 1;
            "#,
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::parse("unix\n"));

        assert_eq!(indexes.fields.get("count"), Some(&true));
        assert_eq!(indexes.constants.get("COUNT"), Some(&true));
    }

    /// The collector never offers a mutant in test code, so evidence drawn from test code answers
    /// questions about production code it is not part of. The gate is [`CfgSet::skip_gate`], not
    /// [`CfgSet::holds_for`], for exactly this: a `#[cfg(test)]` predicate *holds* for the
    /// instrumented build, and reading `holds_for` alone let the helper below demote the active
    /// `count` and `COUNT` to unknown.
    ///
    /// Unlike the two fixtures above this needs no enforced set, because the test gate is decided
    /// without consulting whether predicates are enforced at all.
    #[test]
    fn test_gated_fields_and_constants_do_not_pollute_the_index() {
        let file = parse_file(
            r#"
            struct Active {
                count: u32,
            }

            #[cfg(test)]
            mod tests {
                struct Helper {
                    count: String,
                }

                const COUNT: &str = "fixture";
            }

            const COUNT: u32 = 1;
            "#,
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::unconditional());

        assert_eq!(indexes.fields.get("count"), Some(&true));
        assert_eq!(indexes.constants.get("COUNT"), Some(&true));
    }

    /// Conditional compilation inside a body is written on statements, which no item visitor ever
    /// sees. An inactive `let` says nothing about the binding the build actually has, and a
    /// numeric use inside an inactive statement is evidence about code that is not there.
    #[test]
    fn statements_the_build_discards_are_not_indexed() {
        let file = parse_file(
            r"
            fn f(limit: usize) {
                #[cfg(windows)]
                let _ = 1 < only_on_windows;

                let _ = limit;
            }
            ",
        )
        .expect("the file parses");
        let selection = Selection::parse("expr.increment").expect("the numeric selector resolves");
        let indexes = indexes_in(&file, &selection, &CfgSet::parse("unix\n"));

        assert!(
            !indexes.numeric_uses.names.contains("only_on_windows"),
            "a discarded statement must not leave numeric evidence behind"
        );
    }
}
