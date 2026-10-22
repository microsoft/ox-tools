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
                    condition.start <= expression_span.start && expression_span.end <= condition.end
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

    use super::*;

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
