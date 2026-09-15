// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fusing the stated-value audit and the numeric/import indexes into one syntax-tree walk.
//!
//! [`stated::check`](super::super::stated::check) and [`indexes_in`](super::indexes::indexes_in)
//! each drive their own [`syn::visit::Visit`] over the same file, and the
//! [`Collector`](super::Collector) that follows them drives a third. The two pre-passes visit
//! completely disjoint sets of node kinds — `Audit` reads attributes and function-like items,
//! `Walk` reads structs, `use`s, constants and a handful of numeric-looking expressions — so
//! nothing about combining them into one walk changes what either one sees or in what order it
//! sees it: every visit method below is exactly the local update the corresponding standalone type
//! already made, run in the same recursive descent, just not paying for that descent twice.
//!
//! Both pre-passes also gate that shared descent by the same decision the collector itself reads
//! before it offers a mutant — [`CfgSet::skip_gate`], which excludes code a false predicate strips
//! *and* code confined to the test build. `visit_item`, `visit_impl_item`, and `visit_trait_item`
//! are this walk's dispatch points for every item and associated item, wherever it is nested, and
//! `visit_stmt` and `visit_expr` cover the levels inside a body that no item visitor sees, so
//! refusing to descend there keeps a skipped region from validating an attribute or indexing a
//! declaration the candidates that follow will never mutate.
//!
//! That makes this pass's stated-value audit narrower than
//! [`stated::check`](super::super::stated::check) run on its own, which reads a whole file and
//! knows nothing about configuration. The narrowing is deliberate in both directions: a malformed
//! `#[gamma::value(...)]` cannot fail a campaign that never compiles the code it sits in, and a
//! `#[gamma::value(...)]` inside a `#[cfg(test)]` module is a hint on code this tool does not
//! mutate — `rustc` still rejects a malformed one when the crate's own tests are built.
//!
//! [`super::defaults::DefaultPaths`] deliberately stays out of this fusion. It never was a
//! recursive [`syn::visit::Visit`] walk — [`DefaultPaths::of_in`](super::defaults::DefaultPaths::of_in)
//! is a single pass over `file.items` — so folding it in here would not remove a traversal, only
//! move an already-cheap one.

use syn::visit::{self, Visit};
use syn::{
    Arm, Attribute, Expr, ExprBinary, ExprForLoop, ExprIndex, ExprMethodCall, Field, ImplItem, ImplItemConst, ImplItemFn, Item, ItemConst,
    ItemFn, ItemStatic, ItemStruct, ItemUse, Stmt, TraitItem, TraitItemConst, TraitItemFn,
};

use crate::Result;
use crate::cfg::CfgSet;
use crate::ops::collect::collector::indexes::{Indexes, Walk};
use crate::ops::collect::collector::predicates::{expr_attrs, stmt_attrs};
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::ops::collect::stated::{self, Audit};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

/// Runs the stated-value audit and the numeric/import indexes in the same walk over a file, then
/// reports the audit's fault exactly as [`stated::check`](super::super::stated::check) would.
///
/// `cfg` decides which code either pre-pass is even allowed to learn from: a malformed or misplaced
/// `#[gamma::value(...)]` behind a predicate the selected build does not satisfy must not fail a
/// campaign that predicate keeps out of the build, and a declaration or numeric use behind the same
/// false predicate must not inform a guess made about active code. Test-gated code is held out for
/// the same reason — see [`Collector::skipped`](super::Collector::skipped), whose rule this shares
/// exactly and which the candidates that follow this pre-pass are already held to.
///
/// Returns the indexes only when there is no fault to report, matching the order the two passes
/// already ran in at their one call site: the stated-value check has always run, and had to fail
/// the whole file, before the indexes it built were of any use to a collector that would never run.
pub(in crate::ops::collect) fn run(file: &SourceFile, selection: &Selection, cfg: &CfgSet) -> Result<Indexes> {
    let mut combined = PhaseOne {
        audit: Audit::default(),
        walk: Walk::new(selection, cfg),
        cfg,
    };

    combined.visit_file(&file.ast);

    stated::fault(file, combined.audit)?;

    Ok(combined.walk.into_indexes())
}

/// The combined visitor: one [`Audit`] and one [`Walk`], driven by a single recursive descent.
///
/// Holds both sub-visitors' state rather than merging their fields into one type, so each keeps
/// exactly the fields, invariants and standalone tests it already had; only the traversal itself is
/// shared. `cfg` is held here rather than in either sub-visitor because it gates the shared
/// traversal itself — see [`Self::visit_item`] — not either sub-visitor's own state.
struct PhaseOne<'cfg> {
    audit: Audit,
    walk: Walk<'cfg>,
    cfg: &'cfg CfgSet,
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for PhaseOne<'_> {
    fn visit_attribute(&mut self, node: &'ast Attribute) {
        self.audit.on_attribute(node);
        // #[gamma::skip(stmt.delete_call, reason = "syn attributes contain an opaque token stream and no recursively visitable Rust nodes, so this continuation has no observable work")]
        visit::visit_attribute(self, node);
    }

    /// Gates every item this walk might otherwise audit or index by the same decision the collector
    /// reads before it ever offers a mutant.
    ///
    /// [`CfgSet::skip_gate`] rather than [`CfgSet::holds_for`], because the collector excludes test
    /// code as well as configured-out code, and either kind is code no candidate will be offered
    /// in — so neither may fail a run over a stated value nor inform a guess about the code that
    /// remains.
    ///
    /// `visit_item` is the single dispatch point every top-level and nested item passes through —
    /// including a local item declared inside a function body — so gating here, rather than
    /// separately in each of the item-kind methods below, keeps one skipped item from reaching
    /// any of them.
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

    /// Gates fields before either their attributes or their types reach a pre-pass.
    fn visit_field(&mut self, node: &'ast Field) {
        if !self.cfg.skip_gate(&node.attrs) {
            visit::visit_field(self, node);
        }
    }

    /// Gates match arms, whose attributes belong to the arm rather than its body expression.
    fn visit_arm(&mut self, node: &'ast Arm) {
        if !self.cfg.skip_gate(&node.attrs) {
            visit::visit_arm(self, node);
        }
    }

    /// Gates every statement, which is the level conditional compilation is written at inside a
    /// body and which no item visitor above ever sees.
    ///
    /// The collector descends a block statement by statement for exactly this reason, so without
    /// this the pre-pass would index a `#[cfg(windows)] let n: usize = 0;` that a Unix build
    /// discards, and that evidence would then answer for the `n` the build actually has.
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

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.audit.on_item_fn(node);
        visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        self.audit.on_impl_item_fn(node);
        visit::visit_impl_item_fn(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        self.audit.on_trait_item_fn(node);
        visit::visit_trait_item_fn(self, node);
    }

    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        self.walk.on_item_struct(node);
        visit::visit_item_struct(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        self.walk.on_item_use(node);
        // #[gamma::skip(stmt.delete_call, reason = "a use tree contains no expressions, attributes, function items, or declarations consumed by either fused visitor")]
        visit::visit_item_use(self, node);
    }

    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        self.walk.declared(&node.ident.to_string(), &node.ty);
        visit::visit_item_const(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.walk.declared(&node.ident.to_string(), &node.ty);
        visit::visit_item_static(self, node);
    }

    fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
        self.walk.declared(&node.ident.to_string(), &node.ty);
        visit::visit_impl_item_const(self, node);
    }

    fn visit_trait_item_const(&mut self, node: &'ast TraitItemConst) {
        self.walk.declared(&node.ident.to_string(), &node.ty);
        visit::visit_trait_item_const(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        self.walk.on_expr_binary(node);
        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        self.walk.on_expr_index(node);
        visit::visit_expr_index(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        self.walk.on_expr_method_call(node);
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.walk.on_expr_for_loop(node);
        visit::visit_expr_for_loop(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_source(source: &str, selection: &str, cfg: &CfgSet) -> Result<Indexes> {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).expect("the fixture parses");
        let selection = Selection::parse(selection).expect("the selectors resolve");

        run(&file, &selection, cfg)
    }

    #[test]
    fn the_fused_walk_visits_every_indexed_and_audited_item_kind() {
        let indexes = run_source(
            r"
                use std::vec::Vec;

                struct Record {
                    count: usize,
                }

                const LIMIT: usize = 1;
                static FLOOR: usize = 0;

                trait Measure {
                    const STEP: usize;
                    fn read(&self) -> usize;
                }

                impl Record {
                    const CAPACITY: usize = 2;
                    fn value(&self) -> usize {
                        self.count + LIMIT
                    }
                }

                fn collect(record: &Record) -> usize {
                    for index in 0..record.count {
                        let _ = Vec::<usize>::new()[index + record.value()];
                    }
                    let _width = record.count + LIMIT;
                    record.value().saturating_add(FLOOR)
                }
            ",
            "expr.increment,arith.add_to_sub,fn_value.default",
            &CfgSet::unconditional(),
        )
        .expect("the fixture has no stated-value fault");

        assert_eq!(indexes.constants.get("LIMIT"), Some(&true));
        assert_eq!(indexes.constants.get("FLOOR"), Some(&true));
        assert_eq!(indexes.constants.get("STEP"), Some(&true));
        assert_eq!(indexes.constants.get("CAPACITY"), Some(&true));
        assert_eq!(indexes.fields.get("count"), Some(&true));
        assert_eq!(indexes.imports.get("Vec"), Some(&Some(vec!["std".to_owned(), "vec".to_owned()])));
        assert!(indexes.numeric_uses.names.contains("index"));
    }

    #[test]
    fn every_gate_excludes_invalid_stated_values_in_inactive_code() {
        let source = r"
            #[cfg(any())]
            #[gamma::value(0, 1)]
            fn item() {}

            struct Record {
                #[cfg(any())]
                #[gamma::value(0, 1)]
                field: usize,
            }

            impl Record {
                #[cfg(any())]
                #[gamma::value(0, 1)]
                fn implementation(&self) {}
            }

            trait Measure {
                #[cfg(any())]
                #[gamma::value(0, 1)]
                fn declaration(&self) {}
            }

            fn body(value: Option<usize>) {
                #[cfg(any())]
                #[gamma::value(0, 1)]
                let skipped = 1;

                match value {
                    #[cfg(any())]
                    #[gamma::value(0, 1)]
                    Some(number) => number,
                    _ => 0,
                };
            }
        ";

        run_source(source, "all", &CfgSet::parse("unix")).expect("attributes on every inactive syntax level are ignored");
    }

    #[test]
    fn active_attributes_and_nested_functions_are_audited() {
        assert!(
            run_source(
                "fn outer() { #[gamma::value(0, 1)] fn inner() -> usize { 2 } }",
                "all",
                &CfgSet::unconditional(),
            )
            .is_err(),
            "the recursive descent must reach and audit a nested function"
        );
        assert!(
            run_source(
                "struct S; impl S { #[gamma::value(0, 1)] fn f(&self) -> usize { 2 } }",
                "all",
                &CfgSet::unconditional(),
            )
            .is_err(),
            "implementation methods are audited"
        );
        assert!(
            run_source(
                "trait T { #[gamma::value(0, 1)] fn f(&self) -> usize { 2 } }",
                "all",
                &CfgSet::unconditional(),
            )
            .is_err(),
            "trait methods are audited"
        );

        for source in [
            "struct S { #[gamma::value(0, 1)] field: usize }",
            "fn f(value: Option<usize>) { match value { #[gamma::value(0, 1)] Some(n) => n, _ => 0 }; }",
            "fn f() { let _ = #[gamma::value(0, 1)] 1; }",
        ] {
            assert!(
                run_source(source, "all", &CfgSet::unconditional()).is_err(),
                "the active nested attribute must be audited: {source}"
            );
        }
    }

    #[test]
    fn recursive_visits_reach_expressions_inside_every_declaration_kind() {
        let indexes = run_source(
            r"
            struct Buffer([u8; width - 1]);
            const LIMIT: usize = left - right;
            static FLOOR: usize = low - high;
            trait T {
                const STEP: usize = trait_left - trait_right;
            }
            impl T for Buffer {
                const STEP: usize = impl_left - impl_right;
            }
            ",
            "expr.increment",
            &CfgSet::unconditional(),
        )
        .expect("the fixture has no stated-value fault");

        for expected in [
            "width",
            "left",
            "right",
            "low",
            "high",
            "trait_left",
            "trait_right",
            "impl_left",
            "impl_right",
        ] {
            assert!(indexes.numeric_uses.names.contains(expected), "{expected}");
        }
    }
}
