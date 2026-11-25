// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Syntax facts shared by every node in one schema render.

use core::ops::Range;

use syn::Expr;
use syn::visit::{self, Visit};

use crate::parse::Extent as _;
use crate::{HashMap, HashSet};

#[derive(Default)]
/// Once-per-render syntax facts in absolute byte coordinates of the borrowed original source.
pub(super) struct Syntax<'src> {
    text: &'src str,
    leading: HashSet<Range<usize>>,
    contexts: HashMap<Range<usize>, usize>,
    returns: Vec<Vec<Range<usize>>>,
    scope: usize,
}

impl<'src> Syntax<'src> {
    pub(super) fn of(text: &'src str) -> Self {
        let mut syntax = Self {
            text,
            returns: vec![Vec::new()],
            ..Self::default()
        };
        let parsed = crate::parse::shebang_as_whitespace(text);
        if let Ok(file) = syn::parse_file(&parsed) {
            syntax.visit_file(&file);
        } else if let Ok(expression) = syn::parse_str::<Expr>(text) {
            syntax.visit_expr(&expression);
        }
        syntax
    }

    fn canonical(text: &str, span: &Range<usize>) -> Range<usize> {
        let source = &text[span.clone()];
        let mut start = span.start;
        for token in rustc_lexer::tokenize(source) {
            let raw = &text[start..start + token.len];
            // Syn includes doc comments as attributes in node spans. Retain them so canonical
            // keys still find parsed nodes, including a documented block-leading expression.
            if raw.starts_with("//!")
                || raw.starts_with("/*!")
                || (raw.starts_with("///") && !raw.starts_with("////"))
                || (raw.starts_with("/**") && !raw.starts_with("/***") &&
                    // #[gamma::skip(string.starts_with_to_ends_with, tag = "equivalent", reason = "A terminated block token starts or ends with /**/ only when it is exactly /**/; unterminated doc tokens provide no parsed contexts.")]
                    !raw.starts_with("/**/"))
            {
                break;
            }
            if !matches!(
                token.kind,
                rustc_lexer::TokenKind::Whitespace | rustc_lexer::TokenKind::LineComment | rustc_lexer::TokenKind::BlockComment { .. }
            ) {
                break;
            }
            start += token.len;
        }
        start..span.start + source.trim_end().len()
    }

    pub(super) fn block_leading(&self, span: &Range<usize>) -> bool {
        self.leading.contains(&Self::canonical(self.text, span))
    }

    pub(super) fn returns(&self, span: &Range<usize>) -> Vec<Range<usize>> {
        let span = Self::canonical(self.text, span);
        let Some(scope) = self.contexts.get(&span) else { return Vec::new() };
        let sites = &self.returns[*scope];
        // The visitor records each keyword before its operand, preserving non-overlapping source
        // order within each scope for both partition searches.
        let start = sites.partition_point(|site| site.start < span.start);
        let end = sites.partition_point(|site| site.end <= span.end);
        sites[start..end].to_vec()
    }

    fn nested(&mut self, visit: impl FnOnce(&mut Self)) {
        let scope = self.scope;
        self.scope = self.returns.len();
        self.returns.push(Vec::new());
        visit(self);
        self.scope = scope;
    }
}

pub(super) fn block_leading(expression: &Expr) -> bool {
    fn leading(expression: &Expr) -> bool {
        match expression {
            Expr::Binary(binary) => leading(&binary.left),
            Expr::Cast(cast) => leading(&cast.expr),
            Expr::Block(_)
            | Expr::Unsafe(_)
            | Expr::Const(_)
            | Expr::If(_)
            | Expr::Match(_)
            | Expr::Loop(_)
            | Expr::While(_)
            | Expr::ForLoop(_)
            | Expr::TryBlock(_) => true,
            _ => false,
        }
    }
    matches!(expression, Expr::Binary(_) | Expr::Cast(_)) && leading(expression)
}

impl<'ast> Visit<'ast> for Syntax<'_> {
    fn visit_expr(&mut self, i: &'ast Expr) {
        let span = i.extent().byte_range();
        let _previous = self.contexts.insert(span.clone(), self.scope);
        if block_leading(i) {
            let _inserted = self.leading.insert(span);
        }
        visit::visit_expr(self, i);
    }

    fn visit_block(&mut self, i: &'ast syn::Block) {
        let _previous = self.contexts.insert(i.brace_token.span.join().byte_range(), self.scope);
        visit::visit_block(self, i);
    }

    fn visit_expr_return(&mut self, i: &'ast syn::ExprReturn) {
        // Record the keyword before its operand so each scope's ranges remain in source order.
        self.returns[self.scope].push(i.return_token.span.byte_range());
        visit::visit_expr_return(self, i);
    }

    fn visit_expr_closure(&mut self, i: &'ast syn::ExprClosure) {
        self.nested(|syntax| visit::visit_expr_closure(syntax, i));
    }

    fn visit_expr_async(&mut self, i: &'ast syn::ExprAsync) {
        self.nested(|syntax| visit::visit_expr_async(syntax, i));
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.nested(|syntax| visit::visit_item_fn(syntax, i));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.nested(|syntax| visit::visit_impl_item_fn(syntax, i));
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.nested(|syntax| visit::visit_trait_item_fn(syntax, i));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn canonical_spans_preserve_doc_comments_but_skip_ordinary_trivia() {
        for (comment, documentation) in [
            ("//! inner\n", true),
            ("/*! inner */", true),
            ("/// outer\n", true),
            ("/** outer */", true),
            ("// ordinary\n", false),
            ("//// ordinary\n", false),
            ("// ordinary //!\n", false),
            ("// ordinary ///\n", false),
            ("// ordinary ////\n", false),
            ("/* ordinary */", false),
            ("/*** ordinary */", false),
            ("/**/", false),
        ] {
            let text = format!("xx  {comment}  {{ 1 }} + 2 \n");
            let span = 2..text.len();
            let canonical = Syntax::canonical(&text, &span);
            let expected = if documentation { 4 } else { text.find("{ 1 }").unwrap() };
            assert_eq!(canonical, expected..text.find(" \n").unwrap(), "{comment:?}");
        }
        let text = " \t/* first */ // second\n { 1 } + 2 ";
        assert_eq!(Syntax::canonical(text, &(0..text.len())), 25..34);
    }

    #[test]
    fn canonical_trivia_queries_find_the_original_block_leading_expression() {
        for prefix in ["  ", "/* ordinary */ ", "//// ordinary\n", "/// outer\n", "/** outer */ "] {
            let text = format!("{prefix}{{ 1 }} + 2  ");
            let syntax = Syntax::of(&text);
            assert!(syntax.block_leading(&(0..text.len())), "{text:?}");
        }
    }

    #[test]
    fn return_queries_include_tokens_at_both_exact_span_boundaries() {
        let text = "return";
        let syntax = Syntax::of(text);
        assert_eq!(syntax.returns(&(0..6)), vec![0..6]);
        let text = "{ return return 1; }";
        let syntax = Syntax::of(text);
        assert_eq!(syntax.returns(&(0..text.len())), vec![2..8, 9..15]);
        assert_eq!(syntax.returns(&(2..17)), vec![2..8, 9..15]);
        assert_eq!(syntax.returns(&(9..17)), vec![9..15]);
    }

    #[test]
    fn nested_return_scopes_are_indexed_without_leaking_into_the_enclosing_block() {
        let text = "{ let closure = || { return 1; }; let future = async { return 2; }; struct S; impl S { fn method() { return 3; } } trait T { fn defaulted() { return 4; } } return 5; }";
        let syntax = Syntax::of(text);
        for value in 1..=4 {
            let body = format!("{{ return {value}; }}");
            let start = text.find(&body).unwrap();
            assert_eq!(syntax.returns(&(start..start + body.len())), vec![start + 2..start + 8], "{body}");
            assert_eq!(syntax.returns(&(start + 2..start + 10)), vec![start + 2..start + 8], "{body}");
        }
        let outer = text.find("return 5").unwrap();
        assert_eq!(syntax.returns(&(0..text.len())), vec![outer..outer + 6]);
    }
}
