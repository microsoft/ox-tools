// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type provenance for direct scalar declarations before a root function loop.

#![expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]

use core::ops::Range;

use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Expr, FnArg, Pat, Stmt, Type};

pub(in super::super) struct ScalarSources {
    pub(in super::super) locals: Vec<(String, Expr)>,
    pub(in super::super) nonzero_get: Option<(String, Type)>,
}

struct Shadows<'a> {
    target: usize,
    locals: &'a mut Vec<(String, Expr)>,
    nonzero: &'a mut Option<(String, Type)>,
    budget: usize,
}

impl<'ast> Visit<'ast> for Shadows<'_> {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if self.budget == 0 || expression.span().byte_range().start > self.target {
            return;
        }
        self.budget -= 1;
        visit::visit_expr(self, expression);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        if block.span().byte_range().contains(&self.target) {
            visit::visit_block(self, block);
        }
    }

    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        if pattern.span().byte_range().start > self.target {
            return;
        }
        self.locals.retain(|(name, _)| name != &pattern.ident.to_string());
        if self.nonzero.as_ref().is_some_and(|(name, _)| name == &pattern.ident.to_string()) {
            *self.nonzero = None;
        }
        visit::visit_pat_ident(self, pattern);
    }

    fn visit_macro(&mut self, _: &'ast syn::Macro) {
        self.locals.clear();
        *self.nonzero = None;
    }
}

pub(in super::super) fn scalar_sources(file: &syn::File, target: &Range<usize>) -> Option<ScalarSources> {
    if file.items.len() > 256 {
        return None;
    }
    let function = file.items.iter().find_map(|item| match item {
        syn::Item::Fn(function) if function.block.span().byte_range().contains(&target.start) => Some(function),
        _ => None,
    })?;
    let mut locals = Vec::new();
    let mut nonzero_get = None;
    for (index, statement) in function.block.stmts.iter().take(64).enumerate() {
        if let Stmt::Expr(expression, _) = statement
            && expression.span().byte_range() == *target
        {
            return Some(ScalarSources { locals, nonzero_get });
        }
        if statement.span().byte_range().end > target.start {
            if statement.span().byte_range().start > target.start {
                return None;
            }
            let mut shadows = Shadows {
                target: target.start,
                locals: &mut locals,
                nonzero: &mut nonzero_get,
                budget: 256,
            };
            shadows.visit_stmt(statement);
            if shadows.budget == 0 {
                return None;
            }
            return Some(ScalarSources { locals, nonzero_get });
        }
        let Stmt::Local(local) = statement else { continue };
        let pattern = match &local.pat {
            Pat::Type(pattern) => &*pattern.pat,
            pattern => pattern,
        };
        let Pat::Ident(pattern) = pattern else {
            locals.clear();
            nonzero_get = None;
            continue;
        };
        let name = pattern.ident.to_string();
        locals.retain(|(prior, _)| prior != &name);
        if nonzero_get.as_ref().is_some_and(|(prior, _)| prior == &name) {
            nonzero_get = None;
        }
        if !local.attrs.is_empty() || pattern.by_ref.is_some() || pattern.subpat.is_some() {
            continue;
        }
        let Some(init) = local.init.as_ref().filter(|init| init.diverge.is_none()) else {
            continue;
        };
        if init.expr.span().byte_range().len() > 1024 {
            continue;
        }
        locals.push((name.clone(), (*init.expr).clone()));
        if index != 0 || pattern.mutability.is_some() {
            continue;
        }
        let Expr::MethodCall(call) = &*init.expr else { continue };
        if !call.attrs.is_empty()
            || call.turbofish.is_some()
            || call.method != "get"
            || !call.args.is_empty()
            || !matches!(&*call.receiver, Expr::Path(path) if path.path.is_ident(&name))
        {
            continue;
        }
        nonzero_get = function.sig.inputs.iter().find_map(|argument| {
            let FnArg::Typed(argument) = argument else { return None };
            let Pat::Ident(parameter) = &*argument.pat else { return None };
            (parameter.ident == pattern.ident && parameter.by_ref.is_none() && parameter.subpat.is_none())
                .then(|| (name.clone(), (*argument.ty).clone()))
        });
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn sources(source: &str) -> ScalarSources {
        struct Find(Option<Range<usize>>);
        impl<'ast> Visit<'ast> for Find {
            fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
                self.0 = Some(node.span().byte_range());
            }
        }
        let file = syn::parse_file(source).unwrap();
        let mut find = Find(None);
        find.visit_file(&file);
        scalar_sources(&file, &find.0.unwrap()).unwrap()
    }

    #[test]
    fn source_search_fails_closed_on_exhausted_or_outside_statement_targets() {
        let mut file = syn::parse_file("fn f(){ let n=1; { while ready{} } }").unwrap();
        let syn::Item::Fn(function) = &mut file.items[0] else {
            unreachable!()
        };
        let start = function.block.span().byte_range().start + 1;
        assert!(scalar_sources(&file, &(start..start)).is_none());
        let syn::Item::Fn(function) = &mut file.items[0] else {
            unreachable!()
        };
        let end = function.block.span().byte_range().end - 1;
        assert!(scalar_sources(&file, &(end..end)).is_none());
        let syn::Item::Fn(function) = &mut file.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Block(block), _) = &mut function.block.stmts[1] else {
            unreachable!()
        };
        let Stmt::Expr(target, _) = &block.block.stmts[0] else {
            unreachable!()
        };
        let target = target.span().byte_range();
        block
            .block
            .stmts
            .splice(0..0, core::iter::repeat_n(syn::parse_quote!(consume(0);), 256));
        assert!(scalar_sources(&file, &target).is_none());
        let item = file.items[0].clone();
        file.items.extend(core::iter::repeat_n(item, 256));
        assert!(scalar_sources(&file, &target).is_none());
    }

    #[test]
    fn scalar_sources_keep_latest_direct_initializer_and_ignore_post_target_bindings() {
        let found = sources("fn f(){ let n=1; let n:u32=2; let mut other=3; while ready{} let later=4; }");
        assert_eq!(
            found.locals,
            vec![("n".into(), syn::parse_quote!(2)), ("other".into(), syn::parse_quote!(3))]
        );
        assert!(found.nonzero_get.is_none());
        for source in [
            "fn f(){ let n=1; let n; while ready{} }",
            "fn f(){ let n=1; let ref n=other; while ready{} }",
            "fn f(){ let n=1; #[cfg(test)] let n=2; while ready{} }",
            "fn f(){ let n=1; let (left,right)=pair; while ready{} }",
            "fn f(){ let n=1; { let n=other; while ready{} } }",
            "fn f(){ let n=1; { opaque!(); while ready{} } }",
        ] {
            assert!(sources(source).locals.is_empty(), "{source}");
        }
        assert_eq!(
            sources("fn f(){ let n=1; { let unrelated=2; while ready{} } }").locals,
            vec![("n".into(), syn::parse_quote!(1))],
        );
    }

    #[test]
    fn nonzero_get_provenance_requires_first_immutable_self_named_parameter_binding() {
        let found = sources("fn f(n:NonZeroU32){ let n=n.get(); while ready{} }");
        assert_eq!(found.nonzero_get, Some(("n".into(), syn::parse_quote!(NonZeroU32))));
        for source in [
            "fn f(n:NonZeroU32){ let mut n=n.get(); while ready{} }",
            "fn f(n:NonZeroU32){ let first=0; let n=n.get(); while ready{} }",
            "fn f(n:NonZeroU32){ let n=other.get(); while ready{} }",
            "fn f(n:NonZeroU32){ let n=n.get(0); while ready{} }",
            "fn f(n:NonZeroU32){ let n=n.get::<u32>(); while ready{} }",
            "fn f(n:NonZeroU32){ let n=n.other(); while ready{} }",
            "fn f(other:NonZeroU32){ let n=n.get(); while ready{} }",
            "fn f(n:NonZeroU32){ let n=n.get(); let n=0; while ready{} }",
            "fn f(n:NonZeroU32){ let n=n.get(); { let n=0; while ready{} } }",
        ] {
            assert!(sources(source).nonzero_get.is_none(), "{source}");
        }
        let found = sources("fn f(n:NonZeroU32){ let n=n.get(); { let other=0; while ready{} } }");
        assert_eq!(found.nonzero_get, Some(("n".into(), syn::parse_quote!(NonZeroU32))));
    }

    #[test]
    fn nested_pattern_bindings_invalidate_only_the_names_visible_at_the_target() {
        for (source, expected) in [
            (
                "fn f(){ let n=1; let keep=2; if let Some(n)=input { while ready{} } }",
                vec![("keep".into(), syn::parse_quote!(2))],
            ),
            (
                "fn f(){ let n=1; let keep=2; match input { Some(n) => { while ready{} }, _ => {} } }",
                vec![("keep".into(), syn::parse_quote!(2))],
            ),
            ("fn f(){ let n=1; let closure=|n| { while ready{} }; }", Vec::new()),
            (
                "fn f(){ let n=1; { let n=2; } { while ready{} let n=3; } }",
                vec![("n".into(), syn::parse_quote!(1))],
            ),
            (
                "fn f(){ let n=1; { unrelated!(); } { while ready{} } }",
                vec![("n".into(), syn::parse_quote!(1))],
            ),
        ] {
            assert_eq!(sources(source).locals, expected, "{source}");
        }
    }

    #[test]
    fn nonzero_parameter_evidence_obeys_nested_patterns_and_macro_boundaries() {
        for (source, expected) in [
            ("fn f(n:NonZeroU32){ let n=n.get(); if let Some(n)=input { while ready{} } }", None),
            ("fn f(n:NonZeroU32){ let n=n.get(); { opaque!(); while ready{} } }", None),
            (
                "fn f(n:NonZeroU32){ let n=n.get(); { let n=0; } { while ready{} let n=0; } }",
                Some(("n".into(), syn::parse_quote!(NonZeroU32))),
            ),
            ("fn f(ref n:NonZeroU32){ let n=n.get(); while ready{} }", None),
            ("fn f(n @ _:NonZeroU32){ let n=n.get(); while ready{} }", None),
        ] {
            assert_eq!(sources(source).nonzero_get, expected, "{source}");
        }
    }

    #[test]
    fn declaration_window_includes_the_last_allowed_loop_but_not_the_next_statement() {
        for (declarations, accepted) in [(63, true), (64, false)] {
            let mut file =
                syn::parse_file("fn f(){ let n=1; while ready{} }").expect("the fixed declaration and target loop are valid Rust syntax");
            let syn::Item::Fn(function) = &mut file.items[0] else {
                panic!("the fixture has exactly one function");
            };
            let declaration = function.block.stmts[0].clone();
            let target = function.block.stmts[1].span().byte_range();
            function
                .block
                .stmts
                .splice(0..0, core::iter::repeat_n(declaration, declarations - 1));
            assert_eq!(
                scalar_sources(&file, &target).map(|found| (found.locals, found.nonzero_get)),
                accepted.then(|| (vec![("n".into(), syn::parse_quote!(1))], None)),
                "{declarations} preceding declarations"
            );
        }
    }
}
