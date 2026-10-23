// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, FnArg, GenericArgument, Item, Member, Pat, PathArguments, Stmt, Type};

use super::Binding;
use crate::ops::collect::resource_namespaces::Namespaces;
use crate::{HashMap, HashSet};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "fixtures/grouping.rs"]
mod fixture;

/// Evidence for a zero-origin grouping recurrence after replacing cursor-plus-offset with a product.
///
/// The changed boundary leaves an empty slice on the recurring path. Immutable input and the final
/// cursor assignment preserve that zero boundary; consumed empty vectors never invoke their lazy
/// mapper/filter or sorting callbacks, while each pass still appends an output group.
pub(super) struct Grouping {
    cursor: String,
    input: String,
    field: String,
    copied: String,
    boundary: String,
    outputs: HashSet<String>,
    output_record: String,
    vector_fields: HashSet<String>,
}

fn text(value: &impl quote::ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn name(expression: &Expr) -> Option<String> {
    let Expr::Path(path) = bare(expression) else { return None };
    path.path.get_ident().map(ToString::to_string)
}

fn bare(mut expression: &Expr) -> &Expr {
    for _ in 0..16 {
        expression = match expression {
            Expr::Paren(paren) => &paren.expr,
            Expr::Group(group) => &group.expr,
            _ => return expression,
        };
    }
    expression
}

fn method<'a>(expression: &'a Expr, method: &str, arguments: usize) -> Option<&'a syn::ExprMethodCall> {
    let Expr::MethodCall(call) = expression else { return None };
    (call.method == method && call.args.len() == arguments && call.attrs.is_empty() && (method == "collect" || call.turbofish.is_none()))
        .then_some(call)
}

fn local(statement: &Stmt) -> Option<(String, &Expr)> {
    let Stmt::Local(local) = statement else { return None };
    let pat = match &local.pat {
        Pat::Type(pattern) => &*pattern.pat,
        pattern => pattern,
    };
    let Pat::Ident(pattern) = pat else { return None };
    if !local.attrs.is_empty() || pattern.by_ref.is_some() || pattern.subpat.is_some() {
        return None;
    }
    Some((
        pattern.ident.to_string(),
        &local.init.as_ref().filter(|init| init.diverge.is_none())?.expr,
    ))
}

fn record<'a>(file: &'a syn::File, name: &str) -> Option<&'a syn::ItemStruct> {
    let mut records = file.items.iter().filter_map(|item| match item {
        Item::Struct(record) if record.ident == name && record.generics.params.is_empty() => Some(record),
        _ => None,
    });
    let record = records.next()?;
    records.next().is_none().then_some(record)
}

fn field<'a>(record: &'a syn::ItemStruct, name: &str) -> Option<&'a Type> {
    if record.fields.len() > 64 {
        return None;
    }
    let mut fields = record
        .fields
        .iter()
        .filter(|field| field.ident.as_ref().is_some_and(|ident| ident == name));
    let field = fields.next()?;
    fields.next().is_none().then_some(&field.ty)
}

fn function<'a>(file: &'a syn::File, target: &Expr) -> Option<(&'a syn::Signature, &'a syn::Block)> {
    file.items.iter().take(256).find_map(|item| match item {
        Item::Fn(function)
            if function.block.span().byte_range().contains(&target.span().byte_range().start)
                && function.sig.generics.params.is_empty() =>
        {
            Some((&function.sig, &*function.block))
        }
        Item::Impl(implementation) if implementation.generics.params.is_empty() => {
            implementation.items.iter().take(256).find_map(|item| match item {
                syn::ImplItem::Fn(function)
                    if function.block.span().byte_range().contains(&target.span().byte_range().start)
                        && function.sig.generics.params.is_empty() =>
                {
                    Some((&function.sig, &function.block))
                }
                _ => None,
            })
        }
        _ => None,
    })
}

impl Grouping {
    #[expect(
        clippy::too_many_lines,
        reason = "the bounded setup checks the complete immutable input and empty-output schema"
    )]
    pub(super) fn new(
        file: &syn::File,
        expression: &Expr,
        bindings: &HashMap<String, Binding>,
        sources: &Namespaces,
        source: &str,
        inline: &[String],
    ) -> Option<Self> {
        let Expr::While(loop_) = expression else { return None };
        if file.items.len() > 256
            || loop_.body.stmts.len() > 16
            || !loop_.attrs.is_empty()
            || loop_.label.is_some()
            || !sources.closed_methods_for(source, inline)
            || !sources.primitive_scope_for(source, inline)
        {
            return None;
        }
        let Expr::Binary(condition) = &*loop_.cond else { return None };
        if !matches!(condition.op, BinOp::Lt(_)) {
            return None;
        }
        let cursor = name(&condition.left)?;
        let length = method(&condition.right, "len", 0)?;
        let Expr::Field(input) = &*length.receiver else { return None };
        let root = name(&input.base)?;
        let Member::Named(input_field) = &input.member else { return None };
        let input_text = text(input);
        if !matches!(
            bindings.get(&format!("{root}.{input_field}")),
            Some(Binding::Vec | Binding::VecChar | Binding::VecString)
        ) {
            return None;
        }
        let (signature, block) = function(file, expression)?;
        if block.stmts.len() > 64 {
            return None;
        }
        let at = block
            .stmts
            .iter()
            .position(|statement| statement.span().byte_range() == expression.span().byte_range())?;
        if block.stmts[..at]
            .iter()
            .any(|statement| matches!(statement, Stmt::Item(_) | Stmt::Macro(_)))
        {
            return None;
        }
        let (initial_name, initial) = local(block.stmts.get(at.checked_sub(1)?)?)?;
        if initial_name != cursor
            || !matches!(initial, Expr::Lit(literal) if matches!(&literal.lit, syn::Lit::Int(value) if value.base10_parse::<u128>().ok() == Some(0)))
        {
            return None;
        }
        let parameter = signature.inputs.iter().find_map(|argument| match argument {
            FnArg::Typed(argument) if matches!(&*argument.pat, Pat::Ident(pattern) if pattern.ident == root) => Some(&*argument.ty),
            _ => None,
        })?;
        let Type::Reference(parameter) = parameter else { return None };
        if parameter.mutability.is_some() {
            return None;
        }
        let Type::Path(parameter) = &*parameter.elem else { return None };
        let record_name = parameter.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &record_name) {
            return None;
        }
        let Type::Path(vector) = field(record(file, &record_name)?, &input_field.to_string())? else {
            return None;
        };
        let PathArguments::AngleBracketed(arguments) = &vector.path.segments.last()?.arguments else {
            return None;
        };
        let GenericArgument::Type(Type::Path(element)) = arguments.args.first()? else {
            return None;
        };
        let element_name = element.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &element_name) {
            return None;
        }
        let (copied, copy) = local(loop_.body.stmts.first()?)?;
        let copy = method(copy, "clone", 0)?;
        let Expr::Field(projected) = &*copy.receiver else { return None };
        let Expr::Index(index) = &*projected.base else { return None };
        let Member::Named(projected_field) = &projected.member else {
            return None;
        };
        let copied_type = field(record(file, &element_name)?, &projected_field.to_string())?;
        if text(&index.expr) != input_text
            || name(&index.index).as_deref() != Some(&cursor)
            || !sources.stable_path_for(source, inline, copied_type)
        {
            return None;
        }
        let (boundary, _) = local(loop_.body.stmts.get(1)?)?;
        let output = loop_.body.stmts.iter().find_map(|statement| match statement {
            Stmt::Expr(Expr::MethodCall(call), _) if call.method == "push" => call.args.first(),
            _ => None,
        })?;
        let Expr::Struct(output) = output else { return None };
        let output_record = output.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &output_record) {
            return None;
        }
        let schema = record(file, &output_record)?;
        let mut vector_fields = HashSet::default();
        for projected in &output.fields {
            let Member::Named(member) = &projected.member else { return None };
            let ty = field(schema, &member.to_string())?;
            if matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1
                && path.path.segments.first().is_some_and(|segment| segment.ident == "Vec"))
            {
                let _new = vector_fields.insert(member.to_string());
            } else if text(ty) != text(copied_type) || !sources.stable_path_for(source, inline, ty) {
                return None;
            }
        }
        let mut captures = Captures {
            sources,
            source,
            inline,
            bound: HashSet::default(),
            allowed: [root, copied.clone(), cursor.clone()].into_iter().collect(),
            remaining: 256,
            valid: true,
        };
        for statement in &loop_.body.stmts {
            captures.visit_stmt(statement);
            if let Some((name, _)) = local(statement) {
                let _new = captures.allowed.insert(name);
            }
        }
        if !captures.valid || captures.remaining == 0 {
            return None;
        }
        let mut outputs = bindings
            .iter()
            .filter(|(_, kind)| matches!(kind, Binding::Vec | Binding::VecString | Binding::VecChar))
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        for statement in &block.stmts[..at] {
            if let Some((name, initializer)) = local(statement) {
                let _old = outputs.remove(&name);
                if matches!(initializer, Expr::Call(call) if call.args.is_empty() && call.attrs.is_empty()
                    && matches!(&*call.func, Expr::Path(path) if path.qself.is_none() && path.path.leading_colon.is_none()
                        && path.path.segments.len() == 2 && path.path.segments.first().is_some_and(|segment| segment.ident == "Vec")
                        && path.path.segments.last().is_some_and(|segment| segment.ident == "new")))
                {
                    let _new = outputs.insert(name);
                }
            }
        }
        Some(Self {
            cursor,
            input: input_text,
            field: projected_field.to_string(),
            copied,
            boundary,
            outputs,
            output_record,
            vector_fields,
        })
    }

    fn boundary<'a>(&self, expression: &'a Expr, multiply: bool) -> Option<&'a Expr> {
        let call = method(expression, "map_or", 2)?;
        let count = method(call.args.first()?, "len", 0)?;
        if text(&count.receiver) != self.input {
            return None;
        }
        let Expr::Closure(closure) = call.args.last()? else { return None };
        if closure.inputs.len() != 1 {
            return None;
        }
        let Pat::Ident(offset) = closure.inputs.first()? else { return None };
        let Expr::Binary(binary) = bare(&closure.body) else { return None };
        if (multiply && !matches!(binary.op, BinOp::Mul(_)))
            || (!multiply && !matches!(binary.op, BinOp::Add(_)))
            || name(&binary.left).as_deref() != Some(&self.cursor)
            || name(&binary.right).as_deref() != Some(&offset.ident.to_string())
        {
            return None;
        }
        let position = method(&call.receiver, "position", 1)?;
        let iter = method(&position.receiver, "iter", 0)?;
        let Expr::Index(index) = &*iter.receiver else { return None };
        let Expr::Range(range) = &*index.index else { return None };
        if text(&index.expr) != self.input
            || range.end.is_some()
            || range.start.as_deref().and_then(name).as_deref() != Some(&self.cursor)
            || !matches!(range.limits, syn::RangeLimits::HalfOpen(_))
        {
            return None;
        }
        let Expr::Closure(predicate) = position.args.first()? else {
            return None;
        };
        if predicate.inputs.len() != 1 {
            return None;
        }
        let Pat::Ident(parameter) = predicate.inputs.first()? else {
            return None;
        };
        let Expr::Binary(predicate) = &*predicate.body else { return None };
        let Expr::Field(projected) = &*predicate.left else { return None };
        if !matches!(predicate.op, BinOp::Ne(_))
            || name(&projected.base).as_deref() != Some(&parameter.ident.to_string())
            || text(&projected.member) != self.field
            || name(&predicate.right).as_deref() != Some(&self.copied)
        {
            return None;
        }
        Some(&closure.body)
    }

    pub(super) fn proves(&self, original: &Expr, mutated: &Expr) -> bool {
        self.check(original, mutated).is_some()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the closed empty-iterator grammar checks all recurrence effects in source order"
    )]
    fn check(&self, original: &Expr, mutated: &Expr) -> Option<()> {
        let (Expr::While(before), Expr::While(after)) = (original, mutated) else {
            return None;
        };
        let (_, original_boundary) = local(before.body.stmts.get(1)?)?;
        let (_, boundary) = local(after.body.stmts.get(1)?)?;
        let _progress = self.boundary(original_boundary, false)?;
        let _stationary = self.boundary(boundary, true)?;
        let mut normalized = mutated.clone();
        let Expr::While(normalized_loop) = &mut normalized else {
            return None;
        };
        let Stmt::Local(normalized_boundary) = normalized_loop.body.stmts.get_mut(1)? else {
            return None;
        };
        *normalized_boundary.init.as_mut()?.expr = original_boundary.clone();
        if text(&normalized) != text(original) {
            return None;
        }
        let mut empty = HashSet::default();
        let mut slices = HashSet::default();
        let mut grew = false;
        let mut assigned = false;
        let mut names: HashSet<String> = [self.cursor.clone(), self.copied.clone(), self.boundary.clone()]
            .into_iter()
            .collect();
        for (at, statement) in after.body.stmts.iter().enumerate().skip(2) {
            if let Some((name, initializer)) = local(statement) {
                if !names.insert(name.clone()) {
                    return None;
                }
                if let Expr::Reference(reference) = initializer
                    && reference.mutability.is_none()
                    && let Expr::Index(index) = &*reference.expr
                    && let Expr::Range(range) = &*index.index
                    && text(&index.expr) == self.input
                    && range.start.as_deref().and_then(super::grouping::name).as_deref() == Some(&self.cursor)
                    && range.end.as_deref().and_then(super::grouping::name).as_deref() == Some(&self.boundary)
                    && matches!(range.limits, syn::RangeLimits::HalfOpen(_))
                {
                    let _new = slices.insert(name);
                } else {
                    let collect = method(initializer, "collect", 0)?;
                    let Expr::MethodCall(adapter) = &*collect.receiver else {
                        return None;
                    };
                    if !matches!(adapter.method.to_string().as_str(), "map" | "filter_map")
                        || adapter.args.len() != 1
                        || !matches!(adapter.args.first(), Some(Expr::Closure(_)))
                        || !adapter.attrs.is_empty()
                        || adapter.turbofish.is_some()
                    {
                        return None;
                    }
                    let iter = method(&adapter.receiver, "iter", 0)?;
                    if !slices.contains(&super::grouping::name(&iter.receiver)?) {
                        return None;
                    }
                    let _new = empty.insert(name);
                }
                continue;
            }
            let Stmt::Expr(expression, _) = statement else { return None };
            if let Expr::MethodCall(call) = expression {
                if !call.attrs.is_empty() || call.turbofish.is_some() {
                    return None;
                }
                let receiver = name(&call.receiver)?;
                if empty.contains(&receiver)
                    && ((call.method == "dedup" && call.args.is_empty())
                        || (call.method == "sort_by" && call.args.len() == 1 && matches!(call.args.first(), Some(Expr::Closure(_)))))
                {
                    continue;
                }
                if call.method != "push" || call.args.len() != 1 || !self.outputs.contains(&receiver) {
                    return None;
                }
                let Expr::Struct(record) = call.args.first()? else { return None };
                if record.rest.is_some()
                    || !record.attrs.is_empty()
                    || text(&record.path) != self.output_record
                    || !record.fields.iter().all(|field| {
                        name(&field.expr).is_some_and(|value| {
                            if self.vector_fields.contains(&text(&field.member)) {
                                empty.remove(&value)
                            } else {
                                value == self.copied
                            }
                        })
                    })
                {
                    return None;
                }
                grew = true;
            } else {
                let Expr::Assign(assign) = expression else { return None };
                if at + 1 != after.body.stmts.len()
                    || name(&assign.left).as_deref() != Some(&self.cursor)
                    || name(&assign.right).as_deref() != Some(&self.boundary)
                {
                    return None;
                }
                assigned = true;
            }
        }
        (grew && assigned && empty.is_empty()).then_some(())
    }
}

struct Captures<'a> {
    sources: &'a Namespaces,
    source: &'a str,
    inline: &'a [String],
    bound: HashSet<String>,
    allowed: HashSet<String>,
    remaining: usize,
    valid: bool,
}

#[expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]
impl<'ast> Visit<'ast> for Captures<'_> {
    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        let saved = self.bound.clone();
        for pattern in &closure.inputs {
            let Pat::Ident(pattern) = pattern else {
                self.valid = false;
                return;
            };
            let _new = self.bound.insert(pattern.ident.to_string());
        }
        self.visit_expr(&closure.body);
        self.bound = saved;
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let saved = self.bound.clone();
        for statement in &block.stmts {
            self.visit_stmt(statement);
            if let Some((name, _)) = local(statement) {
                let _new = self.bound.insert(name);
            }
        }
        self.bound = saved;
    }

    fn visit_expr(&mut self, expression: &'ast Expr) {
        if self.remaining == 0 {
            self.valid = false;
            return;
        }
        self.remaining -= 1;
        if !self.bound.is_empty()
            && let Some(name) = name(expression)
            && !self.bound.contains(&name)
            && !self.allowed.contains(&name)
            && self.sources.function(self.source, self.inline, &[name]).is_none()
        {
            self.valid = false;
        }
        visit::visit_expr(self, expression);
    }

    fn visit_macro(&mut self, _: &'ast syn::Macro) {
        self.valid = false;
    }

    fn visit_item(&mut self, _: &'ast Item) {
        self.valid = false;
    }

    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {
        self.valid = false;
    }
}
