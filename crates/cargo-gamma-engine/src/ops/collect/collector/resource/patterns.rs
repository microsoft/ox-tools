// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;

use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{Expr, PatIdent};

use crate::HashSet;

pub(in super::super) fn supported_match(expression: &syn::ExprMatch) -> bool {
    expression.arms.iter().any(|arm| matches!(&arm.pat, syn::Pat::Lit(pattern) if matches!(pattern.lit, syn::Lit::Char(_))))
        && expression.arms.iter().all(|arm| {
            matches!(&arm.pat, syn::Pat::Lit(pattern) if pattern.attrs.is_empty() && matches!(pattern.lit, syn::Lit::Char(_)))
                || matches!(&arm.pat, syn::Pat::Ident(pattern) if pattern.attrs.is_empty() && pattern.subpat.is_none() && pattern.by_ref.is_none())
                || matches!(&arm.pat, syn::Pat::Wild(pattern) if pattern.attrs.is_empty())
        })
}

struct Enclosing<'a> {
    target: &'a Range<usize>,
    names: HashSet<String>,
    remaining: usize,
    collecting: bool,
}

#[expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]
impl<'ast> Visit<'ast> for Enclosing<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        if !block.span().byte_range().contains(&self.target.start) {
            return;
        }
        if block.stmts.iter().any(|statement| {
            let syn::Stmt::Item(item) = statement else { return false };
            let (name, attributes) = match item {
                syn::Item::Struct(item) => (&item.ident, &item.attrs),
                syn::Item::Enum(item) => (&item.ident, &item.attrs),
                syn::Item::Type(item) => (&item.ident, &item.attrs),
                _ => return true,
            };
            let _new = self.names.insert(name.to_string());
            matches!(
                name.to_string().as_str(),
                "str" | "String" | "char" | "usize" | "bool" | "Some" | "None" | "Option"
            ) || !crate::ops::collect::resource_namespaces::declaration_attributes(attributes)
        }) {
            self.remaining = 0;
            return;
        }
        visit::visit_block(self, block);
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        let span = expression.span().byte_range();
        if !span.contains(&self.target.start) || span == *self.target || self.remaining == 0 {
            return;
        }
        self.remaining -= 1;
        if let Expr::Match(expression) = expression {
            self.visit_expr(&expression.expr);
            for arm in &expression.arms {
                if arm.body.span().byte_range().contains(&self.target.start) || arm.pat.span().byte_range().contains(&self.target.start) {
                    let collecting = self.collecting;
                    self.collecting = true;
                    self.visit_pat(&arm.pat);
                    self.collecting = collecting;
                    self.visit_expr(&arm.body);
                }
            }
        } else {
            visit::visit_expr(self, expression);
        }
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function.block.span().byte_range().contains(&self.target.start) {
            self.visit_block(&function.block);
        }
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        if function.block.span().byte_range().contains(&self.target.start) {
            self.visit_block(&function.block);
        }
    }

    fn visit_pat_ident(&mut self, pattern: &'ast PatIdent) {
        if self.remaining == 0 || !self.collecting {
            return;
        }
        self.remaining -= 1;
        let _new = self.names.insert(pattern.ident.to_string());
        visit::visit_pat_ident(self, pattern);
    }
}

pub(in super::super) fn enclosing_pattern_shadows(file: &syn::File, target: &Range<usize>) -> Option<HashSet<String>> {
    let mut enclosing = Enclosing {
        target,
        names: HashSet::default(),
        remaining: 256,
        collecting: false,
    };
    enclosing.visit_file(file);
    (enclosing.remaining != 0).then_some(enclosing.names)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn match_support_requires_a_character_arm_and_no_opaque_pattern_shapes() {
        for (source, expected) in [
            ("match value { 'a' => {}, _ => {} }", true),
            ("match value { 'a' => {}, rest => {} }", true),
            ("match value { 'a' => {}, mut rest => {} }", true),
            ("match value { rest => {} }", false),
            ("match value { _ => {} }", false),
            ("match value { 1 => {}, _ => {} }", false),
            ("match value { 'a' => {}, 1 => {} }", false),
            ("match value { 'a' | 'b' => {}, _ => {} }", false),
            ("match value { 'a' => {}, ref rest => {} }", false),
            ("match value { 'a' => {}, rest @ _ => {} }", false),
            ("match value { 'a' => {}, Some(rest) => {} }", false),
        ] {
            assert_eq!(supported_match(&syn::parse_str(source).unwrap()), expected, "{source}");
        }
    }

    #[test]
    fn enclosing_patterns_collect_only_bindings_on_the_target_arm_path() {
        struct Target(Option<Range<usize>>);
        impl<'ast> Visit<'ast> for Target {
            fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
                if matches!(&*i.func, Expr::Path(path) if path.path.is_ident("target")) {
                    self.0 = Some(i.span().byte_range());
                }
                visit::visit_expr_call(self, i);
            }
        }
        let file = syn::parse_file(
            "fn f(){ match source { Some((outer, other)) => { match nested { inner => { target(); }, sibling => { ignored(); } } }, unrelated => { outside(); } } }"
        ).unwrap();
        let mut target = Target(None);
        target.visit_file(&file);
        let names = enclosing_pattern_shadows(&file, &target.0.unwrap()).unwrap();
        assert_eq!(names, ["outer", "other", "inner"].into_iter().map(str::to_owned).collect());
    }

    #[test]
    fn local_declarations_preserve_ordinary_names_but_opaque_or_primitive_shadows_abort() {
        for (source, expected) in [
            ("fn f(){ struct Local; target(); }", Some(vec!["Local"])),
            ("fn f(){ enum Local { V } target(); }", Some(vec!["Local"])),
            ("fn f(){ type Local = char; target(); }", Some(vec!["Local"])),
            ("fn f(){ struct String; target(); }", None),
            ("fn f(){ enum Some { V } target(); }", None),
            ("fn f(){ type bool = char; target(); }", None),
            ("fn f(){ fn nested(){} target(); }", None),
            ("fn f(){ #[cfg(any())] struct Local; target(); }", Some(vec!["Local"])),
            ("fn f(){ #[custom] struct Local; target(); }", None),
        ] {
            let file = syn::parse_file(source).unwrap();
            let syn::Item::Fn(function) = &file.items[0] else { unreachable!() };
            let syn::Stmt::Expr(target, _) = function.block.stmts.last().unwrap() else {
                unreachable!()
            };
            assert_eq!(
                enclosing_pattern_shadows(&file, &target.span().byte_range()),
                expected.map(|names| names.into_iter().map(str::to_owned).collect()),
                "{source}",
            );
        }
    }
}
