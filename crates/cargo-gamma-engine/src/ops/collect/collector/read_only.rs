// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Syntax checks record attribute ranges, temporary-value uses, and possible `format!` shadows.
//!
//! They include cfg-disabled and lexically skipped code without supplying lexical facts or
//! making excluded code eligible for mutation.

use core::any::type_name;
use core::fmt;
use core::ops::Range;

use syn::visit::{self, Visit};
use syn::{Block, Expr, ExprAssign, ItemExternCrate, ItemMacro, Stmt, StmtMacro};

use crate::ops::collect::collector::predicates::expr_attrs;
use crate::ops::collect::collector::temporaries;
use crate::parse::Extent as _;
use crate::{HashMap, HashSet};

/// Whole-file syntax observations, including subtrees skipped by lexical collection.
/// These maps and flags do not establish active-configuration or mutation-eligibility facts.
#[derive(Default, PartialEq, Eq)]
pub(super) struct Evidence {
    pub(super) expressions: HashMap<Range<usize>, Range<usize>>,
    pub(super) statement_blocks: HashSet<Range<usize>>,
    pub(super) temporaries: HashMap<Range<usize>, Range<usize>>,
    pub(super) format_macro_shadowed: bool,
}

impl fmt::Debug for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(type_name::<Self>())
            .field("expressions", &self.expressions.len())
            .field("statement_blocks", &self.statement_blocks.len())
            .field("temporary_sites", &self.temporaries.len())
            .field("format_macro_shadowed", &self.format_macro_shadowed)
            .finish()
    }
}

impl Evidence {
    #[cfg(test)]
    pub(super) fn of(file: &syn::File) -> Self {
        let mut walk = Walk::default();
        walk.visit_file(file);
        walk.finish()
    }
}

#[derive(Default)]
pub(super) struct Walk<'ast> {
    evidence: Evidence,
    temporaries: temporaries::Walk<'ast>,
}

impl<'ast> Walk<'ast> {
    pub(super) fn finish(self) -> Evidence {
        let mut evidence = self.evidence;
        evidence.statement_blocks = evidence
            .statement_blocks
            .into_iter()
            .map(|range| evidence.expressions.get(&range).unwrap_or(&range).clone())
            .collect();
        evidence.temporaries = self.temporaries.sites;
        evidence
    }

    pub(super) fn block(&mut self, block: &Block) {
        for statement in block.stmts.iter().take(block.stmts.len().saturating_sub(1)) {
            if let Stmt::Expr(expression, None) = statement {
                let _inserted = self.evidence.statement_blocks.insert(expression.extent().byte_range());
            }
        }
    }

    pub(super) fn enter(&mut self, expression: &'ast Expr) {
        self.temporaries.enter(expression);
        if let Some(last) = expr_attrs(expression).last() {
            let whole = expression.extent().byte_range();
            let _previous = self
                .evidence
                .expressions
                .insert(whole.clone(), last.extent().byte_range().end..whole.end);
        }
    }

    pub(super) fn leave(&mut self) {
        self.temporaries.leave();
    }

    pub(super) fn assignment(&mut self, assignment: &'ast ExprAssign) {
        self.temporaries.assignment(assignment);
    }

    pub(super) fn item_macro(&mut self, item: &ItemMacro) {
        self.evidence.format_macro_shadowed |= item.ident.as_ref().is_none_or(|name| name == "format");
    }

    pub(super) const fn statement_macro(&mut self) {
        self.evidence.format_macro_shadowed = true;
    }

    pub(super) fn extern_crate(&mut self, item: &ItemExternCrate) {
        self.evidence.format_macro_shadowed |= item.attrs.iter().any(|attribute| attribute.path().is_ident("macro_use"));
    }
}

impl<'ast> Visit<'ast> for Walk<'ast> {
    fn visit_block(&mut self, i: &'ast Block) {
        self.block(i);
        visit::visit_block(self, i);
    }

    fn visit_expr(&mut self, i: &'ast Expr) {
        self.enter(i);
        visit::visit_expr(self, i);
        self.leave();
    }

    fn visit_expr_assign(&mut self, i: &'ast ExprAssign) {
        self.assignment(i);
        visit::visit_expr_assign(self, i);
    }

    fn visit_item_macro(&mut self, i: &'ast ItemMacro) {
        self.item_macro(i);
    }

    fn visit_stmt_macro(&mut self, _: &'ast StmtMacro) {
        self.statement_macro();
    }

    fn visit_item_extern_crate(&mut self, i: &'ast ItemExternCrate) {
        self.extern_crate(i);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn assignment_children_retain_their_temporary_consumer() {
        let source = "fn f(mut values: [u8; 1]) { values[count(bytes().first())] = 1; }";
        let file = syn::parse_file(source).unwrap();
        let borrowed = source.find("bytes().first()").unwrap();
        let assignment = source.find("values[count").unwrap();
        let end = source.find(" = 1").unwrap() + " = 1".len();
        assert_eq!(
            Evidence::of(&file).temporaries.get(&(borrowed..borrowed + "bytes().first()".len())),
            Some(&(assignment..end))
        );
    }

    #[test]
    fn macro_use_is_detected_among_other_attributes_but_not_in_an_empty_list() {
        for (source, expected) in [
            ("extern crate core;", false),
            ("#[allow(unused)] extern crate core;", false),
            ("#[allow(unused)] #[macro_use] extern crate core;", true),
        ] {
            let file = syn::parse_file(source).unwrap();
            assert_eq!(Evidence::of(&file).format_macro_shadowed, expected, "{source}");
        }
    }

    #[test]
    fn read_only_ranges_and_shadows_are_retained_in_nested_modules() {
        let source = r"
            #[cfg(any())] mod hidden { macro_rules! format { () => {} } }
            mod outer { mod inner {
                fn f() {
                    if true {} #[allow(unused)] { value(); }
                    consume(make().as_str());
                    [*left, *right] = values();
                }
            } }
        ";
        let file = syn::parse_file(source).unwrap();
        let fused = Evidence::of(&file);
        assert!(format!("{fused:?}").len() < 512);
        let range = |needle: &str| {
            let start = source.find(needle).unwrap();
            start..start + needle.len()
        };
        let whole = range("#[allow(unused)] { value(); }");
        let content = range("#[allow(unused)]").end..whole.end;
        assert_eq!(fused.expressions, core::iter::once((whole, content.clone())).collect());
        assert_eq!(fused.statement_blocks, [range("if true {}"), content].into_iter().collect());
        assert_eq!(fused.temporaries, temporaries::consuming_sites(&file));
        assert!(fused.format_macro_shadowed);
    }
}
