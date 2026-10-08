// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The per-file pre-pass the collector reads before it can judge an expression.

use syn::visit::{self, Visit};
use syn::{
    BinOp, Block, Expr, ExprBinary, ExprForLoop, ExprIndex, ExprMethodCall, File, FnArg, ImplItem, ImplItemConst, ImplItemFn, Item,
    ItemConst, ItemFn, ItemMod, ItemStatic, ItemStruct, ItemType, ItemUse, Member, Pat, ReturnType, Signature, Stmt, TraitItem,
    TraitItemConst, TraitItemFn, Type, UseTree,
};

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
    block_scopes: HashSet<ScopePath>,

    /// Names the file uses somewhere in a way only a number can be used.
    pub(super) numeric_uses: NumericUses,

    /// Whether each constant and static declared anywhere in this file holds a number.
    pub(super) constants: HashMap<String, bool>,

    /// Unambiguous source-written types for fields and constants.
    pub(super) declared_types: HashMap<String, Option<Type>>,

    /// Return types of locally visible functions.
    pub(super) returns: HashMap<String, Option<Type>>,
    pub(super) parameters: HashMap<String, Option<Vec<Type>>>,

    /// Locally declared type aliases and their targets.
    pub(super) aliases: HashMap<String, Option<Alias>>,
}

impl Indexes {
    fn canonicalize_imports(&mut self) {
        let imports = self.imports.clone();
        for (name, path) in &mut self.imports {
            if path.is_some() {
                *path = canonical_import(name, &imports);
            }
        }
        let root_imports = self.root_imports.clone();
        for (name, path) in &mut self.root_imports {
            if path.is_some() {
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
                    .map(|(name, path)| (name.clone(), path.clone().map(protect_import)))
                    .collect::<HashMap<_, _>>()
            });
            let mut visible = parent.clone().unwrap_or_default();
            visible.extend(local.clone());
            for (name, path) in &mut local {
                if path.is_some() {
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
    }
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
        if first == ABSOLUTE_ROOT {
            return Some(path);
        }
        let Some(imported) = imports.get(first) else {
            return Some(path);
        };
        let prefix = imported.as_ref()?;
        if prefix.len() == 1 && prefix.first() == Some(first) {
            return Some(path);
        }
        // #[gamma::skip(cond.always_false, reason = "disabling cycle detection makes cyclic import aliases alternate forever")]
        if !expanded.insert(first.clone()) {
            return None;
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
    /// Notes a name, when the expression is the bare identifier that proves it.
    pub(super) fn note(&mut self, expression: &Expr) {
        match expression {
            Expr::Path(path) if path.qself.is_none() => {
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
            Expr::Path(path) if path.qself.is_none() => {
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
        merge_type(&mut self.indexes.returns, name, ty);
    }

    pub(super) fn signature(&mut self, signature: &Signature) {
        if !self.type_evidence {
            return;
        }

        self.returned(&signature.ident.to_string(), &signature.output);
        let parameters = signature
            .inputs
            .iter()
            .filter_map(|input| match input {
                FnArg::Typed(typed) if !self.cfg.skip_gate(&typed.attrs) => Some((*typed.ty).clone()),
                FnArg::Receiver(_) | FnArg::Typed(_) => None,
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
        for field in &node.fields {
            if self.cfg.skip_gate(&field.attrs) {
                continue;
            }

            let Some(name) = field.ident.as_ref() else {
                continue;
            };

            let name = name.to_string();
            if self.type_evidence {
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
            visit::visit_stmt(self, node);
        }
    }

    /// Gates every expression, covering the positions `rustc` admits an attribute in today and the
    /// ones it does not admit yet, exactly as the collector's own `visit_expr` does.
    fn visit_expr(&mut self, node: &'ast Expr) {
        if !self.cfg.skip_gate(expr_attrs(node)) {
            visit::visit_expr(self, node);
        }
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        self.on_item_struct(node);

        visit::visit_item_struct(self, node);
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
        self.signature(&node.sig);
        visit::visit_item_fn(self, node);
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
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_item_static(self, node);
    }

    fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
        self.declared(&node.ident.to_string(), &node.ty);

        visit::visit_impl_item_const(self, node);
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

    if !walk.numeric && !walk.type_evidence {
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
                aliases: HashMap::default(),
            },
            numeric: selection.contains("expr.increment")
                || selection.contains("expr.decrement")
                || selection.contains("literal.int_decrement"),

            // Several families need source-visible standard-library identities: value synthesis
            // and result mutation use them for `Default`, while integer decrement uses them to
            // recognize fixed unsigned constructor arguments.
            type_evidence: selection.any_in_family("fn_value")
                || selection.contains("result.ok_to_err")
                || selection.contains("result.err_to_ok")
                || selection.contains("option.none_to_some")
                || selection.contains("assign_value.default")
                || selection.any_in_family("call")
                || selection.any_in_family("call_result")
                || selection.any_in_family("parameter")
                || selection.any_in_family("return_value")
                || selection.any_in_family("bool_expr")
                || selection.any_in_family("arith")
                || selection.contains("iter.last_to_first")
                || selection.contains("iter.remove_filter")
                || selection.contains("expr.increment")
                || selection.contains("expr.decrement")
                || selection.contains("literal.int_decrement"),
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
    use syn::{ExprGroup, parse_quote, parse_str, token};

    use super::*;

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
                aliases: HashMap::default(),
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
    fn signatures_record_implicit_unit_and_only_free_functions() {
        let file =
            syn::parse_file("fn free() {} struct Service; impl Service { fn free() -> usize { 1 } } trait Contract { fn free() -> bool; }")
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
        let file = syn::parse_file("trait Iterator {} mod nested { trait Future {} }").expect("the trait fixture parses");
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
        let file = syn::parse_file(
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
        let file = syn::parse_file(
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
    fn selection_enables_only_the_indexes_its_exact_families_consume() {
        let cfg = CfgSet::unconditional();

        let numeric = Walk::new(&Selection::parse("expr.decrement").expect("selector resolves"), &cfg);
        assert!(numeric.numeric);
        assert!(numeric.type_evidence);

        let result = Walk::new(&Selection::parse("result.ok_to_err").expect("selector resolves"), &cfg);
        assert!(!result.numeric);
        assert!(result.type_evidence);

        let unrelated = Walk::new(&Selection::parse("literal.bool_flip").expect("selector resolves"), &cfg);
        assert!(!unrelated.numeric);
        assert!(!unrelated.type_evidence);

        let file = syn::parse_file("const LIMIT: usize = left - right; use crate::Thing;").expect("the file parses");
        let indexes = indexes_in(&file, &Selection::parse("literal.bool_flip").expect("selector resolves"), &cfg);
        assert!(indexes.constants.is_empty());
        assert!(indexes.imports.is_empty());
        assert!(indexes.numeric_uses.names.is_empty());
    }

    /// A field or a constant behind a predicate the active build does not satisfy must not
    /// inform the numeric guess for an active same-named field or constant elsewhere in the file.
    ///
    /// The set has to be an enforced one: [`CfgSet::unconditional`] answers every predicate `true`
    /// by construction, so nothing is stripped under it and the fixture would prove nothing.
    #[test]
    fn inactive_fields_and_constants_do_not_pollute_the_index() {
        let file = syn::parse_file(
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
        let file = syn::parse_file(
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
        let file = syn::parse_file(
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
        let file = syn::parse_file(
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
        let file = syn::parse_file(
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
