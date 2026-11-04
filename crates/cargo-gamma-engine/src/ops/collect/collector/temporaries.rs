// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guards for temporary-backed borrows belong around their consuming expression.

use core::ops::Range;

use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Expr, ExprMethodCall};

use crate::HashMap;

pub(super) fn consuming_sites(file: &syn::File) -> HashMap<Range<usize>, Range<usize>> {
    #[derive(Default)]
    struct Walk<'a> {
        stack: Vec<&'a Expr>,
        sites: HashMap<Range<usize>, Range<usize>>,
    }
    #[expect(clippy::renamed_function_params, reason = "visitor parameters use descriptive names")]
    impl<'ast> Visit<'ast> for Walk<'ast> {
        fn visit_expr(&mut self, expression: &'ast Expr) {
            let expression_span = expression.span().byte_range();
            let boundary = self
                .stack
                .iter()
                .rposition(|expression| {
                    matches!(
                        expression,
                        Expr::Block(_)
                            | Expr::If(_)
                            | Expr::Closure(_)
                            | Expr::Async(_)
                            | Expr::Loop(_)
                            | Expr::ForLoop(_)
                            | Expr::While(_)
                            | Expr::Return(_)
                    ) || matches!(expression, Expr::Match(matched) if {
                        let scrutinee = matched.expr.span().byte_range();
                        let current = &expression_span;
                        current.start < scrutinee.start || current.end > scrutinee.end
                    }) || matches!(expression, Expr::Binary(binary) if matches!(binary.op, syn::BinOp::And(_) | syn::BinOp::Or(_)))
                })
                .map_or(0, |index| index + 1);
            let ancestors = &self.stack[boundary..];
            let borrowed = borrows_temporary(expression);
            let root = ancestors.first().copied().and_then(|root| {
                if !matches!(root, Expr::Let(_)) {
                    return Some(root);
                }
                self.stack.iter().rev().copied().find(|ancestor| {
                    let condition = match ancestor {
                        Expr::If(conditional) => &conditional.cond,
                        Expr::While(conditional) => &conditional.cond,
                        _ => return false,
                    };
                    let condition = condition.span().byte_range();
                    // #[gamma::skip(logical.and_remove_left, reason = "Candidates are ancestor if/while nodes in the parsed expression stack, so a descendant expression never begins before their condition. When borrowed evidence is published with an Expr::Let root, the borrowed descendant necessarily starts strictly after the condition's leading let token. Removing the first bound or making it strict changes no published site.", tag = "equivalent")]
                    (
                        // #[gamma::skip(relational.le_to_lt, reason = "Candidates are ancestor if/while nodes in the parsed expression stack, so a descendant expression never begins before their condition. When borrowed evidence is published with an Expr::Let root, the borrowed descendant necessarily starts strictly after the condition's leading let token. Removing the first bound or making it strict changes no published site.", tag = "equivalent")]
                        condition.start <= expression_span.start
                    ) && expression_span.end <= condition.end
                })
            });
            if borrowed
                && let Some(root) = root
                && root.span().byte_range() != expression.span().byte_range()
            {
                let _prior = self.sites.insert(expression.span().byte_range(), root.span().byte_range());
            }
            self.stack.push(expression);
            visit::visit_expr(self, expression);
            let _expression = self.stack.pop();
        }
    }
    let mut walk = Walk::default();
    walk.visit_file(file);
    walk.sites
}

fn temporary(expression: &Expr) -> bool {
    match expression {
        Expr::Call(_) | Expr::MethodCall(_) | Expr::Struct(_) | Expr::Array(_) | Expr::Tuple(_) => true,
        Expr::Try(tried) => temporary(&tried.expr),
        Expr::Paren(paren) => temporary(&paren.expr),
        Expr::Group(group) => temporary(&group.expr),
        Expr::Index(index) => temporary(&index.expr),
        Expr::Field(field) => temporary(&field.base),
        _ => false,
    }
}

fn borrows_temporary(expression: &Expr) -> bool {
    match expression {
        Expr::Reference(reference) => temporary(&reference.expr),
        Expr::MethodCall(ExprMethodCall { receiver, method, .. }) => {
            matches!(
                method.to_string().as_str(),
                "trim"
                    | "trim_start"
                    | "trim_end"
                    | "trim_matches"
                    | "trim_start_matches"
                    | "trim_end_matches"
                    | "first"
                    | "last"
                    | "get"
                    | "get_mut"
                    | "as_str"
                    | "as_path"
                    | "as_slice"
            ) && temporary(receiver)
                || matches!(
                    method.to_string().as_str(),
                    "ok_or" | "ok_or_else" | "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "as_ref" | "as_mut"
                ) && borrows_temporary(receiver)
        }
        Expr::Try(tried) => borrows_temporary(&tried.expr),
        Expr::Paren(paren) => borrows_temporary(&paren.expr),
        Expr::Group(group) => borrows_temporary(&group.expr),
        Expr::Call(call) => {
            matches!(&*call.func, Expr::Path(path)
            if path.path.get_ident().is_some_and(|name| matches!(name.to_string().as_str(), "Some" | "Ok" | "Err")))
                && call.args.iter().any(borrows_temporary)
        }
        Expr::Index(index) => borrows_temporary(&index.expr),
        _ => false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::token::Group;
    use syn::{Item, Stmt, parse_file, parse_str};

    use super::*;

    #[test]
    fn borrow_propagation_requires_recognized_wrappers_and_checks_each_argument_and_index_base() {
        for (source, expected) in [
            ("make().as_slice()[0]", true),
            ("named[0]", false),
            ("make().as_str().unwrap()", true),
            ("make().as_str().unknown()", false),
            ("Some(&make(), named)", true),
            ("Some(named, &make())", true),
            ("Some(named, other)", false),
            ("unrelated(&make())", false),
        ] {
            assert_eq!(borrows_temporary(&parse_str(source).unwrap()), expected, "{source}");
        }
    }

    #[test]
    fn nested_let_condition_borrows_are_guarded_by_the_nearest_consuming_if() {
        let file = parse_file(
            "fn f() { if let Some(outer) = (if let Some(inner) = Some(&make()) { inner } else { return }) { consume(outer); } }",
        )
        .unwrap();
        let Item::Fn(function) = &file.items[0] else { unreachable!() };
        let Stmt::Expr(Expr::If(outer), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Expr::Let(condition) = &*outer.cond else { unreachable!() };
        let Expr::Paren(paren) = &*condition.expr else { unreachable!() };
        let Expr::If(inner) = &*paren.expr else { unreachable!() };
        let sites = consuming_sites(&file);
        assert!(sites.values().any(|site| *site == inner.span().byte_range()));
        assert!(!sites.values().any(|site| *site == outer.span().byte_range()));
    }

    #[test]
    fn match_guard_let_borrows_do_not_consume_an_enclosing_ifs_unrelated_condition() {
        let file =
            parse_file("fn f() { if ready { match value { _ if let Some(item) = Some(&make()) => consume(item), _ => () } } }").unwrap();
        assert!(consuming_sites(&file).is_empty());
    }

    #[test]
    fn grouped_and_parenthesized_temporary_borrows_keep_their_lifetime_requirement() {
        let call: Expr = syn::parse_quote!(make());
        let paren: Expr = syn::parse_quote!((make()));
        let group = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: Group::default(),
            expr: Box::new(call),
        });
        assert!(temporary(&paren));
        assert!(temporary(&group));
        let borrow: Expr = syn::parse_quote!(&make());
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: Group::default(),
            expr: Box::new(borrow),
        });
        assert!(borrows_temporary(&grouped));
        assert!(!borrows_temporary(&syn::parse_quote!(&named)));
    }
}
