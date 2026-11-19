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

mod records;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod record_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "fixtures/grouping.rs"]
mod fixture;

/// Evidence for stalled grouping over immutable input with retained output growth.
///
/// Empty iterators do not execute callbacks; nonempty groups require primitive projections
/// and ordering instead of assuming those callbacks are harmless.
pub(super) struct Grouping {
    cursor: String,
    input: String,
    field: String,
    copied: String,
    boundary: String,
    outputs: HashSet<String>,
    output_record: String,
    vector_fields: HashSet<String>,
    scalar_fields: HashSet<String>,
    record_body: Option<String>,
}

fn text(value: &impl quote::ToTokens) -> String {
    value.to_token_stream().to_string()
}

fn name(expression: &Expr) -> Option<String> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Path(path) = bare(expression) else { return None };
    path.path.get_ident().filter(|_| path.attrs.is_empty()).map(ToString::to_string)
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
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::MethodCall(call) = expression else { return None };
    (call.method == method && call.args.len() == arguments && call.attrs.is_empty() && (method == "collect" || call.turbofish.is_none()))
        .then_some(call)
}

fn local(statement: &Stmt) -> Option<(String, &Expr)> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Stmt::Local(local) = statement else { return None };
    let pat = match &local.pat {
        Pat::Type(pattern) => &*pattern.pat,
        pattern => pattern,
    };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Pat::Ident(pattern) = pat else { return None };
    if !local.attrs.is_empty() || pattern.by_ref.is_some() || pattern.subpat.is_some() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
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
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
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
        reason = "the bounded setup checks the complete immutable input and output schema"
    )]
    pub(super) fn new(
        file: &syn::File,
        expression: &Expr,
        bindings: &HashMap<String, Binding>,
        sources: &Namespaces,
        source: &str,
        inline: &[String],
    ) -> Option<Self> {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::While(loop_) = expression else { return None };
        if file.items.len() > 256
            || loop_.body.stmts.len() > 16
            || !loop_.attrs.is_empty()
            || loop_.label.is_some()
            || !sources.closed_methods_for(source, inline)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Binary(condition) = &*loop_.cond else { return None };
        if !matches!(condition.op, BinOp::Lt(_)) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let cursor = name(&condition.left)?;
        let length = method(&condition.right, "len", 0)?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(input) = &*length.receiver else { return None };
        let root = name(&input.base)?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Member::Named(input_field) = &input.member else { return None };
        let input_text = text(input);
        if !matches!(
            bindings.get(&format!("{root}.{input_field}")),
            Some(Binding::Vec | Binding::VecChar | Binding::VecString)
        ) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (signature, block) = function(file, expression)?;
        if block.stmts.len() > 64 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let at = block
            .stmts
            .iter()
            .position(|statement| statement.span().byte_range() == expression.span().byte_range())?;
        if block.stmts[
            // #[gamma::skip(range.exclusive_to_inclusive, reason = "Including the target While adds no Item, Macro, or Local to this setup-only scan.", tag = "equivalent")]
            ..at]
            .iter()
            .any(|statement| matches!(statement, Stmt::Item(_) | Stmt::Macro(_)))
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (initial_name, initial) = local(&block.stmts[at.checked_sub(1)?])?;
        if initial_name != cursor
            || !matches!(initial, Expr::Lit(literal) if matches!(&literal.lit, syn::Lit::Int(value) if value.base10_parse::<u128>().ok() == Some(0)))
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let parameter = signature.inputs.iter().find_map(|argument| match argument {
            FnArg::Typed(argument) if matches!(&*argument.pat, Pat::Ident(pattern) if pattern.ident == root) => Some(&*argument.ty),
            _ => None,
        })?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Reference(parameter) = parameter else { return None };
        if parameter.mutability.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Path(parameter) = &*parameter.elem else { return None };
        let record_name = parameter.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &record_name) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let Type::Path(vector) = field(record(file, &record_name)?, &input_field.to_string())? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let PathArguments::AngleBracketed(arguments) = &
            // #[gamma::skip(try.propagate_to_unwrap, reason = "A parsed type path always has at least one segment.", tag = "equivalent")]
            vector.path.segments.last()?.arguments
        else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let GenericArgument::Type(Type::Path(element)) = arguments.args.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let element_name = element.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &element_name) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (copied, copy) = local(loop_.body.stmts.first()?)?;
        let copy = method(copy, "clone", 0)?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(projected) = &*copy.receiver else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Index(index) = &*projected.base else { return None };
        let Member::Named(projected_field) = &projected.member else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let copied_type = field(record(file, &element_name)?, &projected_field.to_string())?;
        if text(&index.expr) != input_text
            || name(&index.index).as_deref() != Some(&cursor)
            || !sources.stable_path_for(source, inline, copied_type)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (boundary, _) = local(loop_.body.stmts.get(1)?)?;
        let output = loop_.body.stmts.iter().find_map(|statement| match statement {
            Stmt::Expr(Expr::MethodCall(call), _) if call.method == "push" => call.args.first(),
            _ => None,
        })?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Struct(output) = output else { return None };
        let output_record = output.path.get_ident()?.to_string();
        if !sources.local_record_for(source, inline, &output_record) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let schema = record(file, &output_record)?;
        let mut vector_fields = HashSet::default();
        for projected in &output.fields {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Member::Named(member) = &projected.member else { return None };
            let ty = field(schema, &member.to_string())?;
            if matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1
                && path.path.segments.first().is_some_and(|segment| segment.ident == "Vec"))
            {
                let _new = vector_fields.insert(member.to_string());
            } else if text(ty) != text(copied_type) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        }
        let mut captures = Captures {
            sources,
            source,
            inline,
            bound: HashSet::default(),
            allowed:
                // #[gamma::skip(collection.reverse_array, reason = "These identities are immediately collected into an unordered set.", tag = "equivalent")]
                [root, copied.clone(), cursor.clone()].into_iter().collect(),
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
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let element =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The same immutable file and element name already passed record lookup when copied_type was obtained above.", tag = "equivalent")]
            record(file, &element_name)?;
        let scalar_names = super::fixed::numeric_names(file, &expression.span().byte_range());
        let scalar_fields = element
            .fields
            .iter()
            .filter(|field| scalar_names && field.attrs.is_empty() && field.span().byte_range().len() <= 256)
            .filter_map(|projected| projected.ident.as_ref().map(ToString::to_string))
            .filter(|name| {
                field(element, name).is_some_and(|ty| {
                    let Type::Path(path) = ty else { return false };
                    let Some(ident) = path.path.get_ident().filter(|_| path.qself.is_none()) else {
                        return false;
                    };
                    matches!(
                        ident.to_string().as_str(),
                        "bool" | "char" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
                    )
                })
            })
            .collect();
        let mut outputs = bindings
            .iter()
            .filter(|(_, kind)| matches!(kind, Binding::Vec | Binding::VecString | Binding::VecChar))
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        for statement in &block.stmts[
            // #[gamma::skip(range.exclusive_to_inclusive, reason = "Including the target While adds no Item, Macro, or Local to this setup-only scan.", tag = "equivalent")]
            ..at]
        {
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
        let record_body = records::prove(file, loop_, sources, source, inline, &outputs);
        Some(Self {
            cursor,
            input: input_text,
            field: projected_field.to_string(),
            copied,
            boundary,
            outputs,
            output_record,
            vector_fields,
            scalar_fields,
            record_body,
        })
    }

    fn boundary<'a>(&self, expression: &'a Expr, multiply: bool) -> Option<&'a Expr> {
        let call = method(expression, "map_or", 2)?;
        let count = method(
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
            call.args.first()?,
            "len",
            0,
        )?;
        if text(&count.receiver) != self.input {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Closure(closure) =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
            call.args.last()? else { return None };
        if closure.inputs.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::Ident(offset) =
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
            closure.inputs.first()? else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Binary(binary) = bare(&closure.body) else { return None };
        if (multiply && !matches!(binary.op, BinOp::Mul(_)))
            || (!multiply && !matches!(binary.op, BinOp::Add(_)))
            || name(&binary.left).as_deref() != Some(&self.cursor)
            || name(&binary.right).as_deref() != Some(&offset.ident.to_string())
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let position = method(&call.receiver, "position", 1)?;
        let iter = method(&position.receiver, "iter", 0)?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Index(index) = &*iter.receiver else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Range(range) = &*index.index else { return None };
        if text(&index.expr) != self.input || range.end.is_some() || range.start.as_deref().and_then(name).as_deref() != Some(&self.cursor)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let Expr::Closure(predicate) =
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
            position.args.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if predicate.inputs.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let Pat::Ident(parameter) =
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
            predicate.inputs.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Binary(predicate) = &*predicate.body else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(projected) = &*predicate.left else { return None };
        if !matches!(predicate.op, BinOp::Ne(_))
            || name(&projected.base).as_deref() != Some(&parameter.ident.to_string())
            || text(&projected.member) != self.field
            || name(&predicate.right).as_deref() != Some(&self.copied)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        Some(&closure.body)
    }

    pub(super) fn proves(&self, original: &Expr, mutated: &Expr) -> bool {
        self.check(original, mutated).is_some() || self.nonempty(original, mutated).is_some()
    }

    fn check(&self, original: &Expr, mutated: &Expr) -> Option<()> {
        let (Expr::While(before), Expr::While(after)) = (original, mutated) else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let (_, original_boundary) = local(before.body.stmts.get(1)?)?;
        let (_, boundary) = local(after.body.stmts.get(1)?)?;
        let _progress = self.boundary(original_boundary, false)?;
        let _stationary = self.boundary(boundary, true)?;
        let mut normalized = mutated.clone();
        let Expr::While(normalized_loop) = &mut normalized else {
            return None; // #[gamma::skip(option.none_to_some, return_value.default, reason = "Cloning the already-validated While and initialized second Local preserves their variants and indices. None is the Option default.", tag = "equivalent")]
        };
        let Stmt::Local(normalized_boundary) =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Cloning the already-validated While and initialized second Local preserves their variants and indices.", tag = "equivalent")]
            normalized_loop.body.stmts.get_mut(1)? else {
            return None; // #[gamma::skip(option.none_to_some, return_value.default, reason = "Cloning the already-validated While and initialized second Local preserves their variants and indices. None is the Option default.", tag = "equivalent")]
        };
        *
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Cloning the already-validated While and initialized second Local preserves their variants and indices.", tag = "equivalent")]
            normalized_boundary.init.as_mut()?.expr = original_boundary.clone();
        if text(&normalized) != text(original) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        self.recurrence(after, true)
    }

    fn nonempty(&self, original: &Expr, mutated: &Expr) -> Option<()> {
        let (Expr::While(before), Expr::While(after)) = (original, mutated) else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if
        // #[gamma::skip(cond.always_false, reason = "The later token equality with before after popping exactly one statement independently requires after to have precisely one fewer statement.", tag = "equivalent")]
        before.body.stmts.len().checked_sub(1)? != after.body.stmts.len() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (_, boundary) = local(before.body.stmts.get(1)?)?;
        let _progress = self.boundary(boundary, false)?;
        let Some(Stmt::Expr(Expr::Assign(assign), _)) = before.body.stmts.last() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !assign.attrs.is_empty()
            || name(&assign.left).as_deref() != Some(&self.cursor)
            || name(&assign.right).as_deref() != Some(&self.boundary)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut normalized = before.clone();
        let _removed =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding last() pattern matched an assignment, so the cloned statement vector is nonempty.", tag = "equivalent")]
            normalized.body.stmts.pop()?;
        if text(&normalized) != text(after) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        self.recurrence(after, false)
    }

    fn scalar_mapper(&self, expression: &Expr) -> Option<()> {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Closure(closure) = expression else { return None };
        if !closure.attrs.is_empty() || closure.inputs.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let Pat::Ident(parameter) =
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding arity check requires exactly one input, so first and last are identical and present.", tag = "equivalent")]
            closure.inputs.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(projected) = bare(&closure.body) else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Member::Named(member) = &projected.member else { return None };
        (projected.attrs.is_empty()
            && name(&projected.base).as_deref() == Some(&parameter.ident.to_string())
            && self.scalar_fields.contains(&member.to_string()))
        .then_some(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one bounded recurrence checks slicing, mapping, ordering, retained output, and progress together"
    )]
    fn recurrence(&self, after: &syn::ExprWhile, empty_groups: bool) -> Option<()> {
        if !empty_groups && self.record_body.as_ref().is_some_and(|body| *body == text(&after.body)) {
            return Some(());
        }
        let mut collected = HashSet::default();
        let mut slices = HashSet::default();
        let mut grew = false;
        let mut assigned = false;
        let mut names: HashSet<String> =
            // #[gamma::skip(collection.reverse_array, reason = "These identities are immediately collected into an unordered set.", tag = "equivalent")]
            [self.cursor.clone(), self.copied.clone(), self.boundary.clone()]
            .into_iter()
            .collect();
        for (at, statement) in after.body.stmts.iter().enumerate().skip(2) {
            if let Some((name, initializer)) = local(statement) {
                if !names.insert(name.clone()) {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
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
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    };
                    if !matches!(adapter.method.to_string().as_str(), "map" | "filter_map")
                        || adapter.args.len() != 1
                        || !matches!(adapter.args.first(), Some(Expr::Closure(_)))
                        || !adapter.attrs.is_empty()
                        || adapter.turbofish.is_some()
                    {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    if !empty_groups {
                        if adapter.method != "map" {
                            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                            return None;
                        }
                        self.scalar_mapper(
                            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The adapter guard above requires exactly one closure argument, so first and last are identical and present.", tag = "equivalent")]
                            adapter.args.first()?,
                        )?;
                    }
                    let iter = method(&adapter.receiver, "iter", 0)?;
                    if !slices.contains(&super::grouping::name(&iter.receiver)?) {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    let _new = collected.insert(name);
                }
                continue;
            }
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Stmt::Expr(expression, _) = statement else { return None };
            if let Expr::MethodCall(call) = expression {
                if !call.attrs.is_empty() || call.turbofish.is_some() {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                let receiver = name(&call.receiver)?;
                if collected.contains(&receiver)
                    && ((call.method == "dedup" && call.args.is_empty())
                        || (call.method == "sort_by" && call.args.len() == 1 && matches!(call.args.first(), Some(Expr::Closure(_)))))
                {
                    if !empty_groups && call.method == "sort_by" {
                        scalar_comparator(
                            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The sort_by guard above requires exactly one closure argument, so first and last are identical and present.", tag = "equivalent")]
                            call.args.first()?,
                        )?;
                    }
                    continue;
                }
                if call.method != "push" || call.args.len() != 1 || !self.outputs.contains(&receiver) {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                let Expr::Struct(record) =
                    // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact arity check guarantees these arguments exist; singleton first and last coincide.", tag = "equivalent")]
                    call.args.first()? else { return None };
                if record.rest.is_some()
                    || !record.attrs.is_empty()
                    || text(&record.path) != self.output_record
                    || !record.fields.iter().all(|field| {
                        name(&field.expr).is_some_and(|value| {
                            if self.vector_fields.contains(&text(&field.member)) {
                                collected.remove(&value)
                            } else {
                                value == self.copied
                            }
                        })
                    })
                {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                grew = true;
            } else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                let Expr::Assign(assign) = expression else { return None };
                if
                // #[gamma::skip(logical.or_remove_left, reason = "Accepting an assignment in nonempty mode still sets assigned=true; the final assigned == empty_groups check then rejects that recurrence.", tag = "equivalent")]
                !empty_groups
                    || at + 1 != after.body.stmts.len()
                    || name(&assign.left).as_deref() != Some(&self.cursor)
                    || name(&assign.right).as_deref() != Some(&self.boundary)
                {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                assigned = true;
            }
        }
        (grew && assigned == empty_groups && collected.is_empty()).then_some(())
    }
}

fn scalar_comparator(expression: &Expr) -> Option<()> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Closure(closure) = expression else { return None };
    if !closure.attrs.is_empty() || closure.inputs.len() != 2 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Pat::Ident(left) =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding arity check requires exactly two inputs, so first and last are both present.", tag = "equivalent")]
        closure.inputs.first()? else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Pat::Ident(right) =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding arity check requires exactly two inputs, so first and last are both present.", tag = "equivalent")]
        closure.inputs.last()? else { return None };
    let compare = method(bare(&closure.body), "cmp", 1)?;
    (name(&compare.receiver).as_deref() == Some(&left.ident.to_string())
        && name(
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "method(..., cmp, 1) requires a single argument, so first and last are identical and present.", tag = "equivalent")]
            compare.args.first()?,
        )
        .as_deref()
            == Some(&right.ident.to_string()))
    .then_some(())
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
