// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Closed, typed backedge evidence for a stationary frame cursor.

#![expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]

use quote::quote;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, FnArg, Item, Pat, Stmt, Type};

use crate::ops::collect::resource_namespaces::{Namespaces, declaration_attributes};
use crate::{HashMap, HashSet};

mod diagonal;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod regression_tests;
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

pub(super) struct Cursor {
    types: HashMap<String, Type>,
    records: HashMap<String, HashMap<String, Type>>,
    closures: HashSet<String>,
    entry: Option<Stmt>,
}

fn name(expression: &Expr) -> Option<String> {
    super::bare_name(expression)
}

fn local(statement: &Stmt) -> Option<(String, &Expr)> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Stmt::Local(local) = statement else { return None };
    if !local.attrs.is_empty() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let pat = match &local.pat {
        Pat::Type(pat) => &*pat.pat,
        pat => pat,
    };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Pat::Ident(pat) = pat else { return None };
    if !pat.attrs.is_empty() || pat.by_ref.is_some() || pat.subpat.is_some() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let init = local.init.as_ref().filter(|init| init.diverge.is_none())?;
    Some((pat.ident.to_string(), &init.expr))
}

fn method<'a>(expression: &'a Expr, expected: &str, arity: usize) -> Option<&'a syn::ExprMethodCall> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::MethodCall(call) = expression else { return None };
    (call.attrs.is_empty() && call.turbofish.is_none() && call.method == expected && call.args.len() == arity).then_some(call)
}

fn argument<'a>(ty: &'a Type, expected: &str) -> Option<&'a Type> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Path(path) = ty else { return None };
    if !path.attrs.is_empty() || path.qself.is_some() || path.path.segments.len() != 1 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact singleton path check guarantees this sole segment exists.", tag = "equivalent")]
    let segment = path.path.segments.first()?;
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    if segment.ident != expected || arguments.args.len() != 1 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees this sole argument exists.", tag = "equivalent")]
    let syn::GenericArgument::Type(ty) = arguments.args.first()? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    Some(ty)
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.attrs.is_empty() && path.qself.is_none()
        && path.path.get_ident().is_some_and(|ident| matches!(ident.to_string().as_str(),
            "bool" | "usize" | "u8" | "u16" | "u32" | "u64" | "u128")))
}

impl Cursor {
    // Conservative envelopes bound syntax cloning, record fields, prefix visits and retained
    // bindings separately. They are not measured capacities; exhaustion keeps the candidate.
    #[expect(
        clippy::too_many_lines,
        reason = "one bounded visitor retains the dominating typed bindings and captures used by the closed backedge grammar"
    )]
    pub(super) fn new(file: &syn::File, expression: &Expr, sources: &Namespaces, source: &str, inline: &[String]) -> Option<Self> {
        struct Find {
            target: core::ops::Range<usize>,
            context: Cursor,
            remaining: usize,
            expressions: usize,
            valid: bool,
            reached: bool,
        }
        impl<'ast> Visit<'ast> for Find {
            fn visit_file(&mut self, node: &'ast syn::File) {
                for item in &node.items {
                    if let Item::Fn(function) = item
                        && function.span().byte_range().contains(&self.target.start)
                    {
                        self.visit_item_fn(function);
                        // #[gamma::skip(loop.break_to_continue, loop.delete_break, reason = "Parsed top-level function spans are disjoint, so no later function can contain this target.", tag = "equivalent")]
                        break;
                    }
                }
            }

            fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
                // #[gamma::skip(cond.always_false, reason = "The sole caller has just checked the identical immutable function span contains this target.", tag = "equivalent")]
                if !node.span().byte_range().contains(&self.target.start) {
                    return;
                }
                // #[gamma::skip(logical.or_remove_left, reason = "Required closed-method namespace evidence independently rejects active functions with unknown declaration attributes.", tag = "equivalent")]
                if !declaration_attributes(&node.attrs)
                    || node
                        .sig
                        .generics
                        .params
                        .iter()
                        .any(|parameter| !matches!(parameter, syn::GenericParam::Lifetime(_)))
                {
                    // #[gamma::skip(stmt.delete_assign, literal.bool_flip, reason = "This branch returns before visiting the target; reached stays false and independently rejects the context.", tag = "equivalent")]
                    self.valid = false;
                    return;
                }
                for argument in &node.sig.inputs {
                    if let FnArg::Typed(argument) = argument
                        && let Pat::Ident(pat) = &*argument.pat
                        && argument.ty.span().byte_range().len() <= 256
                        && argument.attrs.is_empty()
                        && pat.attrs.is_empty()
                        && pat.by_ref.is_none()
                        && pat.subpat.is_none()
                    {
                        let _old = self.context.types.insert(pat.ident.to_string(), (*argument.ty).clone());
                    }
                }
                self.visit_block(&node.block);
            }

            fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
                let _shadowed = self.context.types.remove(&node.ident.to_string());
                let _shadowed = self.context.closures.remove(&node.ident.to_string());
            }

            fn visit_expr(&mut self, node: &'ast Expr) {
                if self.expressions == 0 || !declaration_attributes(super::super::predicates::expr_attrs(node)) {
                    self.valid = false;
                    return;
                }
                self.expressions -= 1;
                visit::visit_expr(self, node);
            }

            fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
                let ty = match &*node.expr {
                    Expr::Range(range) => range.end.as_ref().and_then(|end| self.context.ty(end, &self.context.types)),
                    _ => None,
                };
                self.visit_pat(&node.pat);
                if let Pat::Ident(pat) = &*node.pat
                    && pat.attrs.is_empty()
                    && pat.by_ref.is_none()
                    && pat.subpat.is_none()
                {
                    let name = pat.ident.to_string();
                    if let Some(ty) = ty {
                        let _old = self.context.types.insert(name, ty);
                    }
                }
                self.visit_block(&node.body);
            }

            fn visit_block(&mut self, block: &'ast syn::Block) {
                // #[gamma::skip(literal.int_increment, reason = "Visiting the last allowance decrements it to zero, which final admission independently rejects.", tag = "equivalent")]
                if self.remaining == 0 || !block.span().byte_range().contains(&self.target.start) {
                    return;
                }
                self.remaining -= 1;
                if block.stmts.len() > 64
                    || block.stmts.iter().any(|statement| {
                        matches!(statement, Stmt::Item(_) | Stmt::Macro(_))
                            || !declaration_attributes(super::super::predicates::stmt_attrs(statement))
                    })
                {
                    // #[gamma::skip(stmt.delete_assign, literal.bool_flip, reason = "This branch returns before scanning this target-containing block, leaving reached false.", tag = "equivalent")]
                    self.valid = false;
                    return;
                }
                for (at, statement) in block
                    .stmts
                    .iter()
                    .take(
                        // #[gamma::skip(literal.int_increment, reason = "The preceding statement-count guard already limits this iterator to at most 64 statements.", tag = "equivalent")]
                        64,
                    )
                    .enumerate()
                {
                    if statement.span().byte_range().start >= self.target.start {
                        self.reached = statement.span().byte_range().start == self.target.start;
                        self.context.entry = at.checked_sub(1).and_then(|previous| {
                            let statement = &block.stmts[previous];
                            // #[gamma::skip(logical.and_remove_left, reason = "The sole entry consumer diagonal independently calls local on this retained statement.", tag = "equivalent")]
                            (local(statement).is_some() && statement.span().byte_range().len() <= 1024).then(|| statement.clone())
                        });
                        break;
                    }
                    if let Stmt::Local(local) = statement {
                        if local.init.as_ref().is_some_and(|init| init.expr.span().byte_range().len() > 1024)
                            || matches!(&local.pat, Pat::Type(pattern) if pattern.ty.span().byte_range().len() > 256)
                        {
                            // #[gamma::skip(stmt.delete_assign, literal.bool_flip, reason = "This branch returns before the target statement; the enclosing scan breaks and reached remains false.", tag = "equivalent")]
                            self.valid = false;
                            return;
                        }
                        if !matches!(local.pat, Pat::Ident(_) | Pat::Type(_)) {
                            self.visit_pat(&local.pat);
                        }
                        if let Pat::Tuple(tuple) = &local.pat
                            && let Some(init) = &local.init
                            && init.diverge.is_none()
                            && let Expr::Tuple(values) = &*init.expr
                            && tuple.elems.len() == values.elems.len()
                            && local.attrs.is_empty()
                        {
                            for (pattern, value) in tuple.elems.iter().zip(&values.elems) {
                                if let Pat::Ident(pat) = pattern {
                                    let ty = self.context.ty(value, &self.context.types);
                                    let _old = self.context.types.remove(&pat.ident.to_string());
                                    if let Some(ty) = ty {
                                        let _old = self.context.types.insert(pat.ident.to_string(), ty);
                                    }
                                }
                            }
                        }
                        let pattern = match &local.pat {
                            Pat::Type(pattern) => Some((&*pattern.pat, Some(&*pattern.ty))),
                            Pat::Ident(_) => Some((&local.pat, None)),
                            _ => None,
                        };
                        if let Some((Pat::Ident(pat), ty)) = pattern {
                            let name = pat.ident.to_string();
                            let inferred = ty.cloned().or_else(|| {
                                let init = local.init.as_ref().filter(|init| init.diverge.is_none())?;
                                self.context.ty(&init.expr, &self.context.types)
                            });
                            let _old = self.context.types.remove(&name);
                            let _old = self.context.closures.remove(&name);
                            if local.attrs.is_empty() && pat.attrs.is_empty() && pat.by_ref.is_none() && pat.subpat.is_none() {
                                if let Some(ty) = inferred {
                                    let _old = self.context.types.insert(name.clone(), ty);
                                }
                                if let Some(init) = &local.init
                                    && init.diverge.is_none()
                                    && let Expr::Closure(closure) = &*init.expr
                                    && pat.mutability.is_none()
                                    && self.context.shift(closure)
                                {
                                    let _new = self.context.closures.insert(name);
                                }
                            }
                        }
                    }
                    if statement.span().byte_range().contains(&self.target.start) {
                        visit::visit_stmt(self, statement);
                        break;
                    }
                }
            }
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::While(outer) = expression else { return None };
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "Final applicable validation repeats the closed loop shape; inline-module targets cannot be reached by the top-level-function-only visitor.", tag = "equivalent")]
            expression.span().byte_range().len() > 4096
                || !matches!((&*outer.cond, outer.body.stmts.len()), (Expr::Let(_), 2) | (Expr::Binary(_), 6 | 7))
                || !inline.is_empty()
        ) || !sources.numeric_scope_for(source, inline)
            || !sources.closed_methods_for(source, inline)
            || file.items.len() > 256
            || !declaration_attributes(&file.attrs)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut records = HashMap::default();
        for item in &file.items {
            let Item::Struct(record) = item else { continue };
            if !record.attrs.is_empty() || !record.generics.params.is_empty() || record.fields.len() > 16 {
                continue;
            }
            let fields = record
                .fields
                .iter()
                .map(|field| {
                    // #[gamma::skip(logical.or_remove_right, relational.gt_to_ge, literal.int_decrement, literal.int_increment, reason = "The only consumer requires every frame field to be a bare primitive identifier, whose type span is far below this limit.", tag = "equivalent")]
                    if !field.attrs.is_empty() || field.ty.span().byte_range().len() > 256 {
                        return None;
                    }
                    Some((field.ident.as_ref()?.to_string(), field.ty.clone()))
                })
                .collect::<Option<HashMap<_, _>>>();
            if let Some(fields) = fields
                && sources.local_record_for(source, inline, &record.ident.to_string())
            {
                let _old = records.insert(record.ident.to_string(), fields);
            }
        }
        let mut find = Find {
            target: expression.span().byte_range(),
            context: Self {
                types: HashMap::default(),
                records,
                closures: HashSet::default(),
                entry: None,
            },
            remaining: 16,
            expressions: 512,
            valid: true,
            reached: false,
        };
        find.visit_file(file);
        (find.valid && find.reached && find.remaining != 0 && find.context.types.len() <= 64 && find.context.applicable(expression))
            .then_some(find.context)
    }

    fn applicable(&self, expression: &Expr) -> bool {
        let mut probe = expression.clone();
        let Expr::While(outer) = &mut probe else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false; // #[gamma::skip(literal.bool_flip, reason = "The sole caller already established this same expression is a While.", tag = "equivalent")]
        };
        if let [_, Stmt::Expr(Expr::If(edge), _)] = outer.body.stmts.as_mut_slice()
            && let [_, Stmt::Expr(Expr::Binary(update), _), _] = edge.then_branch.stmts.as_mut_slice()
            && matches!(update.op, BinOp::AddAssign(_))
        {
            *update.right = syn::parse_quote!(0);
        } else if let Some(Stmt::Expr(Expr::Binary(update), _)) = outer.body.stmts.last_mut()
            && matches!(update.op, BinOp::AddAssign(_))
        {
            *update.right = syn::parse_quote!(0);
        }
        self.branch(&probe).is_some() || self.diagonal(&probe).is_some()
    }

    fn ty(&self, expression: &Expr, types: &HashMap<String, Type>) -> Option<Type> {
        match expression {
            Expr::Path(_) => types.get(&name(expression)?).cloned(),
            Expr::Field(field) if field.attrs.is_empty() => {
                let Type::Path(record) = self.ty(&field.base, types)? else {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                };
                let record = record.path.get_ident()?.to_string();
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                let syn::Member::Named(member) = &field.member else { return None };
                self.records.get(&record)?.get(&member.to_string()).cloned()
            }
            Expr::Index(index) if index.attrs.is_empty() => {
                if self.ty(&index.index, types)? != syn::parse_quote!(usize) {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                let ty = self.ty(&index.expr, types)?;
                match &ty {
                    Type::Reference(reference) if reference.mutability.is_none() => {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        let Type::Slice(slice) = &*reference.elem else { return None };
                        Some((*slice.elem).clone())
                    }
                    _ => argument(&ty, "Vec").cloned(),
                }
            }
            Expr::Unary(unary) if unary.attrs.is_empty() && matches!(unary.op, syn::UnOp::Neg(_)) => {
                let ty = self.ty(&unary.expr, types)?;
                (ty == syn::parse_quote!(isize)).then_some(ty)
            }
            Expr::Binary(binary) if binary.attrs.is_empty() && matches!(binary.op, BinOp::Add(_) | BinOp::Sub(_)) => {
                let ty = self.ty(&binary.left, types)?;
                (matches!(&ty, Type::Path(path) if path.path.is_ident("usize") || path.path.is_ident("isize"))
                    && self.ty(&binary.right, types)? == ty)
                    .then_some(ty)
            }
            Expr::MethodCall(call) if call.attrs.is_empty() && call.turbofish.is_none() => {
                if call.method == "len" && call.args.is_empty() {
                    let ty = self.ty(&call.receiver, types)?;
                    return (argument(&ty, "Vec").is_some()
                        || matches!(ty, Type::Reference(reference) if reference.mutability.is_none() && matches!(*reference.elem, Type::Slice(_))))
                        .then(|| syn::parse_quote!(usize));
                }
                if call.method == "unwrap_or"
                    && call.args.len() == 1
                    && let Expr::Call(convert) = &*call.receiver
                    && convert.attrs.is_empty()
                    && convert.args.len() == 1
                    && let Expr::Path(path) = &*convert.func
                    && path.path == syn::parse_quote!(isize::try_from)
                    && self.ty(&convert.args[0], types)? == syn::parse_quote!(usize)
                    && call.args[0] == syn::parse_quote!(isize::MAX)
                {
                    return Some(syn::parse_quote!(isize));
                }
                None
            }
            Expr::Macro(node) if node.attrs.is_empty() && node.mac.path.is_ident("vec") => {
                let tokens = &node.mac.tokens;
                let repeat = syn::parse2::<syn::ExprRepeat>(quote!([#tokens])).ok()?;
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                let Expr::Lit(literal) = &*repeat.expr else { return None };
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                let syn::Lit::Int(literal) = &literal.lit else { return None };
                (literal.suffix() == "isize").then(|| syn::parse_quote!(Vec<isize>))
            }
            _ => None,
        }
    }

    /// Certifies one stable visited-edge branch; other branches are not assumed effect-free.
    pub(super) fn proves(&self, before: &Expr, after: &Expr) -> bool {
        (self.branch(after).is_some() && self.branch(before).is_none())
            || (self.diagonal(after).is_some() && self.diagonal(before).is_none())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the closed backedge grammar checks every executed statement and binding relationship together"
    )]
    fn branch(&self, expression: &Expr) -> Option<()> {
        struct Reads<'a> {
            written: &'a str,
            found: bool,
        }
        impl<'ast> Visit<'ast> for Reads<'_> {
            fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
                self.found |= node.path.is_ident(self.written);
            }
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::While(loop_) = expression else { return None };
        if !loop_.attrs.is_empty() || loop_.label.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Let(binding) = &*loop_.cond else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::TupleStruct(some) = &*binding.pat else { return None };
        if !binding.attrs.is_empty() || !some.attrs.is_empty() || !some.path.is_ident("Some") || some.elems.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::Ident(frame) = &some.elems[0] else { return None };
        if !frame.attrs.is_empty() || frame.by_ref.is_some() || frame.subpat.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let last = method(&binding.expr, "last_mut", 0)?;
        let stack = name(&last.receiver)?;
        let frame_type = argument(self.types.get(&stack)?, "Vec")?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Path(record) = frame_type else { return None };
        if !self.records.get(&record.path.get_ident()?.to_string())?.values().all(scalar) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let [node, Stmt::Expr(Expr::If(edge), _)] = loop_.body.stmts.as_slice() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let (node_name, node_value) = local(node)?;
        let mut types = self.types.clone();
        let _old = types.insert(frame.ident.to_string(), frame_type.clone());
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "The frame identifier was just inserted into types, so equality with it already implies contains_key.", tag = "equivalent")]
            types.contains_key(&node_name) || frame.ident == node_name
        ) || self.ty(node_value, &types)? != syn::parse_quote!(usize)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(node_field) = node_value else { return None };
        if name(&node_field.base).as_deref() != Some(&frame.ident.to_string()) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let _old = types.insert(node_name, syn::parse_quote!(usize));
        if !edge.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Binary(bound) = &*edge.cond else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(cursor) = &*bound.left else { return None };
        if !bound.attrs.is_empty()
            || !matches!(bound.op, BinOp::Lt(_))
            || cursor.member == node_field.member
            || name(&cursor.base).as_deref() != Some(&frame.ident.to_string())
            || self.ty(&bound.left, &types)? != syn::parse_quote!(usize)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let length = method(&bound.right, "len", 0)?;
        let row_type = self.ty(&length.receiver, &types)?;
        if argument(&row_type, "Vec") != Some(&syn::parse_quote!(usize)) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (neighbor, lookup) = local(edge.then_branch.stmts.first()?)?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Index(lookup) = lookup else { return None };
        if !lookup.attrs.is_empty() || lookup.expr != length.receiver || lookup.index != bound.left || types.contains_key(&neighbor) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let _old = types.insert(neighbor, syn::parse_quote!(usize));
        let (middle, tail) = edge.then_branch.stmts[1..].split_last()?;
        if tail.len() > 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        for statement in tail {
            let Stmt::Expr(Expr::Binary(update), _) = statement else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            };
            if !update.attrs.is_empty()
                || !matches!(update.op, BinOp::AddAssign(_))
                || update.left != bound.left
                || !super::integer_zero(&update.right)
            {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Stmt::Expr(Expr::If(visited), _) = middle else { return None };
        if !visited.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Let(known) = &*visited.cond else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::TupleStruct(some) = &*known.pat else { return None };
        if !known.attrs.is_empty() || !some.attrs.is_empty() || !some.path.is_ident("Some") || some.elems.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::Ident(index) = &some.elems[0] else { return None };
        if !index.attrs.is_empty() || index.by_ref.is_some() || index.subpat.is_some() || types.contains_key(&index.ident.to_string()) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let option = self.ty(&known.expr, &types)?;
        let item = argument(&option, "Option")?;
        if !scalar(item) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let _old = types.insert(index.ident.to_string(), item.clone());
        let [Stmt::Expr(Expr::If(on_stack), _)] = visited.then_branch.stmts.as_slice() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !on_stack.attrs.is_empty() || on_stack.else_branch.is_some() || self.ty(&on_stack.cond, &types)? != syn::parse_quote!(bool) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let [Stmt::Expr(Expr::Assign(update), _)] = on_stack.then_branch.stmts.as_slice() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let minimum = method(&update.right, "min", 1)?;
        if !update.attrs.is_empty()
            || update.left != minimum.receiver
            || self.ty(&update.left, &types)? != *item
            || name(&minimum.args[0]).as_deref() != Some(&index.ident.to_string())
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // The only write is an idempotent primitive minimum. Its vector must be distinct from
        // every vector that selects this branch, and the frame stack itself is untouched.
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Index(written) = &*update.left else { return None };
        let written = name(&written.expr)?;
        let mut reads = Reads {
            written: &written,
            found: false,
        };
        // #[gamma::skip(collection.reverse_array, reason = "The visitor only ORs immutable name matches into found, so expression order is irrelevant.", tag = "equivalent")]
        for expression in [&*loop_.cond, node_value, &*edge.cond, &*known.expr, &*on_stack.cond] {
            reads.visit_expr(expression);
        }
        // #[gamma::skip(logical.and_remove_right, reason = "The required last_mut loop condition reads stack, so written == stack necessarily already sets reads.found.", tag = "equivalent")]
        (!reads.found && written != stack).then_some(())
    }
}
