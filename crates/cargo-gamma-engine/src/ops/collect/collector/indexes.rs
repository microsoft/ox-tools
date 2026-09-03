// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The per-file pre-pass the collector reads before it can judge an expression.

use syn::visit::{self, Visit};
use syn::{
    BinOp, Expr, ExprBinary, ExprForLoop, ExprIndex, ExprMethodCall, File, ImplItem, ImplItemConst, Item, ItemConst, ItemStatic,
    ItemStruct, ItemUse, Member, Pat, Stmt, TraitItem, TraitItemConst, Type, UseTree,
};

use crate::cfg::CfgSet;
use crate::ops::collect::collector::predicates::{expr_attrs, is_int_literal, is_numeric_binding, is_numeric_receiver, stmt_attrs};
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::ops::registry::Selection;
use crate::{HashMap, HashSet};

/// The names a file uses in a way only a number can be used.
#[derive(Default)]
pub(super) struct NumericUses {
    /// Bare identifiers: locals, parameters, loop indices.
    pub(super) names: HashSet<String>,

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

    /// Names the file uses somewhere in a way only a number can be used.
    pub(super) numeric_uses: NumericUses,

    /// Whether each constant and static declared anywhere in this file holds a number.
    pub(super) constants: HashMap<String, bool>,
}

/// Fills whichever indexes were asked for, ignoring scope.
pub(super) struct Walk<'cfg> {
    indexes: Indexes,

    /// Whether the numeric evidence — fields, constants, uses — is wanted.
    numeric: bool,

    /// Whether the import paths are wanted.
    imports: bool,

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

    /// Records one constant's declaration, demoting a name two declarations disagree about.
    pub(super) fn declared(&mut self, name: &str, ty: &Type) {
        if !self.numeric {
            return;
        }

        let numeric = is_numeric_binding(ty);

        let _known = self
            .indexes
            .constants
            .entry(name.to_owned())
            .and_modify(|known| *known = *known && numeric)
            .or_insert(numeric);
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
                if let Some((binding, parent)) = prefix.split_last() {
                    self.imported(binding.clone(), parent);
                }
            }

            UseTree::Name(name) => self.imported(name.ident.to_string(), prefix),

            UseTree::Rename(rename) => self.imported(rename.rename.to_string(), prefix),

            UseTree::Group(group) => {
                for item in &group.items {
                    self.descend(prefix, item);
                }
            }

            UseTree::Glob(_) => {}
        }
    }

    /// Records where one imported name came from, demoting a name two `use` items disagree about.
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
        let _known = self
            .indexes
            .imports
            .entry(name)
            .and_modify(|known| {
                if known.as_deref() != Some(prefix) {
                    *known = None;
                }
            })
            .or_insert_with(|| Some(prefix.to_vec()));
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
        if !self.numeric {
            return;
        }

        for field in &node.fields {
            if self.cfg.skip_gate(&field.attrs) {
                continue;
            }

            let Some(name) = field.ident.as_ref() else {
                continue;
            };

            let numeric = is_numeric_binding(&field.ty);

            // Two structs disagreeing about a name means neither answer can be trusted for
            // a bare `x.count`, so the name is demoted to unknown rather than won by
            // whichever was seen last.
            let _known = self
                .indexes
                .fields
                .entry(name.to_string())
                .and_modify(|known| *known = *known && numeric)
                .or_insert(numeric);
        }
    }

    /// The local update `visit_item_use` makes, without its recursive continuation.
    pub(super) fn on_item_use(&mut self, node: &ItemUse) {
        if self.imports {
            self.descend(&mut Vec::new(), &node.tree);
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
            self.note(&node.index);
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

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        self.on_item_use(node);

        visit::visit_item_use(self, node);
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

        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        if self.numeric {
            self.note(&node.index);
        }

        visit::visit_expr_index(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if self.numeric && is_numeric_receiver(&node.method.to_string()) {
            self.note(&node.receiver);
        }

        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        if self.numeric
            && matches!(&*node.expr, Expr::Range(_))
            && let Pat::Ident(ident) = &*node.pat
        {
            let _added = self.indexes.numeric_uses.names.insert(ident.ident.to_string());
        }

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

    if !walk.numeric && !walk.imports {
        return walk.indexes;
    }

    walk.visit_file(file);
    walk.indexes
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
                numeric_uses: NumericUses::default(),
                constants: HashMap::default(),
            },
            numeric: selection.contains("expr.increment") || selection.contains("expr.decrement"),

            // Two families read this one, not one: `fn_value` to know whether a return type has a
            // `Default` to reach for, and `result.ok_to_err` to know whether the `Err` it would
            // write could be built at all. Missing the second cost five mutants when this gate was
            // first written, which is what a gate on an index has to be checked against.
            imports: selection.any_in_family("fn_value") || selection.contains("result.ok_to_err"),
            cfg,
        }
    }

    /// Consumes the walk, returning what it found.
    pub(super) fn into_indexes(self) -> Indexes {
        self.indexes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(numeric: bool, imports: bool, cfg: &CfgSet) -> Walk<'_> {
        Walk {
            indexes: Indexes {
                fields: HashMap::default(),
                imports: HashMap::default(),
                numeric_uses: NumericUses::default(),
                constants: HashMap::default(),
            },
            numeric,
            imports,
            cfg,
        }
    }

    #[test]
    fn note_tracks_named_fields_behind_references() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(true, false, &cfg);
        let expression = syn::parse_str::<Expr>("&record.count").expect("the field expression parses");

        walk.note(&expression);

        assert!(walk.indexes.numeric_uses.fields.contains("count"));
    }

    #[test]
    fn declared_ignores_constants_when_numeric_index_is_disabled() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, false, &cfg);
        let ty = syn::parse_str::<Type>("usize").expect("the numeric type parses");

        walk.declared("COUNT", &ty);

        assert!(walk.indexes.constants.is_empty());
    }

    #[test]
    fn descend_handles_groups_renames_and_globs() {
        let cfg = CfgSet::unconditional();
        let mut walk = walk(false, true, &cfg);
        let item = syn::parse_str::<ItemUse>("use crate::{Thing as Alias, inner::Item, *};").expect("the use item parses");

        walk.descend(&mut Vec::new(), &item.tree);

        assert_eq!(walk.indexes.imports.get("Alias"), Some(&Some(vec!["crate".to_owned()])));
        assert_eq!(
            walk.indexes.imports.get("Item"),
            Some(&Some(vec!["crate".to_owned(), "inner".to_owned()]))
        );
        assert_eq!(walk.indexes.imports.len(), 2);
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
