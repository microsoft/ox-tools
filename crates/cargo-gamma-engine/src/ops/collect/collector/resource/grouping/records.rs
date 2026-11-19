// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Closed declaration effects for replaying a record-valued grouping iteration.

use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::{Expr, FnArg, GenericArgument, Item, Member, Pat, PathArguments, ReturnType, Stmt, Token, Type};

use super::{bare, field, local, method, name, record, text};
use crate::ops::collect::resource_namespaces::{Namespaces, declaration_attributes};
use crate::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Scalar(String),
    String,
    Path(String),
    Record(String),
    Option(Box<Self>),
}

fn closed_record(file: &syn::File, record_name: &str) -> bool {
    let Some(schema) = record(file, record_name) else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
        return false; // #[gamma::skip(literal.bool_flip, reason = "Both callers already found this same immutable record declaration before calling closed_record.", tag = "equivalent")]
    };
    // Namespace evidence does not expose impl bodies from other physical files.
    // Private records without descendants keep all legal custom implementations
    // in the declaration file; exported records need that missing evidence.
    matches!(schema.vis, syn::Visibility::Inherited)
        && !file.items.iter().any(|item| matches!(item, Item::Mod(_)))
        && !file.items.iter().any(|item| {
            matches!(item, Item::Type(alias)
            if !matches!(alias.vis, syn::Visibility::Inherited))
        })
}

struct Effects<'a> {
    file: &'a syn::File,
    sources: &'a Namespaces,
    source: &'a str,
    inline: &'a [String],
    shadows: HashSet<String>,
    remaining: usize,
}

impl Effects<'_> {
    fn spend(&mut self) -> Option<()> {
        self.remaining = self.remaining.checked_sub(1)?;
        Some(())
    }

    fn kind(&mut self, ty: &Type, depth: usize) -> Option<Kind> {
        self.spend()?;
        if depth > 12 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Path(path) = ty else { return None };
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "Parsed qself paths retain either a leading colon or multiple segments, independently rejected by the other path checks.", tag = "equivalent")]
            !path.attrs.is_empty() || path.qself.is_some()
        ) || path.path.leading_colon.is_some()
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        if self.sources.stable_path_for(self.source, self.inline, ty) {
            return Some(if path.path.is_ident("String") {
                Kind::String
            } else {
                Kind::Path(text(ty))
            });
        }
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "Parsed TypePath and ExprStruct paths are nonempty; the following check permits only a singleton before using the selected segment.", tag = "equivalent")]
        let segment = path.path.segments.first()?;
        if path.path.segments.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        if segment.ident == "Option" {
            let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            };
            let [GenericArgument::Type(inner)] = arguments.args.iter().collect::<Vec<_>>().as_slice() else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            };
            return Some(Kind::Option(Box::new(self.kind(inner, depth + 1)?)));
        }
        if !matches!(segment.arguments, PathArguments::None) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let ident = segment.ident.to_string();
        if matches!(
            ident.as_str(),
            "bool" | "char" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize"
        ) {
            return Some(Kind::Scalar(ident));
        }
        let aliases = self
            .file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Type(alias) if alias.ident == ident => Some(alias),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let [alias] = aliases.as_slice() {
            if !alias.generics.params.is_empty() || !plain_attributes(&alias.attrs) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            return self.kind(&alias.ty, depth + 1);
        }
        if
        // #[gamma::skip(logical.or_remove_left, reason = "The single alias already returned; duplicate aliases cannot also form the unique local record required by namespace evidence.", tag = "equivalent")]
        !aliases.is_empty() || !self.sources.local_record_for(self.source, self.inline, &ident) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let schema = record(self.file, &ident)?;
        if !plain_attributes(&schema.attrs)
            || !matches!(schema.fields, syn::Fields::Named(_))
            || schema.fields.len() > 32
            || !closed_record(self.file, &ident)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // Even a pure-looking Clone implementation or an unrelated inherent method can
        // change dispatch. Only standard derives and structurally inert fields are used.
        if conflicting_impl(self.file, &ident) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        for projected in &schema.fields {
            if !field_attributes(&projected.attrs) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let _kind = self.kind(&projected.ty, depth + 1)?;
        }
        Some(Kind::Record(ident))
    }

    fn projected(&mut self, kind: &Kind, member: &Member) -> Option<Kind> {
        let (Kind::Record(record_name), Member::Named(member)) = (kind, member) else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        self.kind(
            field(
                // #[gamma::skip(try.propagate_to_unwrap, reason = "A Record kind is created only after this immutable record declaration has been validated.", tag = "equivalent")]
                record(self.file, record_name)?,
                &member.to_string(),
            )?,
            // #[gamma::skip(literal.int_increment, reason = "Record validation already traversed every field from depth at least one, so restarting this field at one cannot exceed that validated depth.", tag = "equivalent")]
            0,
        )
    }

    fn derived(&mut self, kind: &Kind, wanted: &str) -> Option<()> {
        self.spend()?;
        match kind {
            Kind::Record(record_name) => {
                // #[gamma::skip(try.propagate_to_unwrap, reason = "A Record kind is created only after this immutable record declaration has been validated.", tag = "equivalent")]
                let schema = record(self.file, record_name)?;
                let found = schema
                    .attrs
                    .iter()
                    .filter(|attribute| attribute.path().is_ident("derive"))
                    .filter_map(|attribute| attribute.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated).ok())
                    .flatten()
                    .any(|path| path.is_ident(wanted));
                if !found {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
                for projected in &schema.fields {
                    let kind = self.kind(
                        &projected.ty,
                        // #[gamma::skip(literal.int_increment, reason = "The enclosing Record kind already validated this field from depth at least one.", tag = "equivalent")]
                        0,
                    )?;
                    self.derived(&kind, wanted)?;
                }
                Some(())
            }
            Kind::Option(inner) => self.derived(inner, wanted),
            Kind::Scalar(_) | Kind::String | Kind::Path(_) => Some(()),
        }
    }

    fn projection(&mut self, expression: &Expr, parameter: &str, kind: &Kind) -> Option<Kind> {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Field(projected) = bare(expression) else { return None };
        if !projected.attrs.is_empty() || name(&projected.base).as_deref() != Some(parameter) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        self.projected(kind, &projected.member)
    }

    fn clone_projection(&mut self, expression: &Expr, parameter: &str, kind: &Kind) -> Option<Kind> {
        let call = method(bare(expression), "clone", 0)?;
        let projected = self.projection(&call.receiver, parameter, kind)?;
        self.derived(&projected, "Clone")?;
        Some(projected)
    }

    fn comparator(&mut self, expression: &Expr, kind: &Kind) -> Option<()> {
        let (parameters, body) = closure(expression, 2)?;
        let compare = method(bare(body), "cmp", 1)?;
        for expression in
            // #[gamma::skip(collection.reverse_array, reason = "Both immutable call expressions are checked for parameter-shadowed helper names without changing evidence or fuel.", tag = "equivalent")]
            [
                &*compare.receiver,
                reference(
                    // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "method(cmp, 1) already established exactly one argument.", tag = "equivalent")]
                    compare.args.first()?,
                )?,
            ]
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Expr::Call(call) = bare(expression) else { return None };
            if parameters.contains(&name(&call.func)?) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        }
        let left = self.key(&compare.receiver, &parameters[0], kind)?;
        let right = self.key(
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The singleton cmp argument and its immutable reference shape were both successfully checked in the preceding loop.", tag = "equivalent")]
            reference(compare.args.first()?)?,
            &parameters[1],
            kind,
        )?;
        (left == right).then_some(())
    }

    fn key(&mut self, expression: &Expr, parameter: &str, kind: &Kind) -> Option<Vec<String>> {
        self.spend()?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Call(call) = bare(expression) else { return None };
        if !call.attrs.is_empty()
            || call.args.len() != 1
            || name(
                // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding short-circuit arity check established exactly one argument.", tag = "equivalent")]
                call.args.first()?,
            )
            .as_deref()
                != Some(parameter)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let helper = name(&call.func)?;
        if self.shadows.contains(&helper) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let resolved = self.sources.function(self.source, self.inline, std::slice::from_ref(&helper))?;
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Namespace function syntax is retained from a parsed ItemFn via token rendering.", tag = "equivalent")]
        let function = syn::parse_str::<syn::ItemFn>(resolved.syntax).ok()?;
        let declarations = self
            .file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Fn(function) if function.sig.ident == helper => Some(function),
                _ => None,
            })
            .collect::<Vec<_>>();
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let [declaration] = declarations.as_slice() else { return None };
        if text(*declaration) != text(&function) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let unsupported_signature = !plain_attributes(&function.attrs)
            || !function.sig.generics.params.is_empty()
            || function.sig.asyncness.is_some()
            || !matches!(function.sig.safety, syn::Safety::Default)
            || function.sig.abi.is_some();
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "Supported variadic functions require an extern ABI, already included in unsupported_signature.", tag = "equivalent")]
            unsupported_signature || function.sig.variadic.is_some()
        ) || function.sig.inputs.len() != 1
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding signature guard established exactly one argument.", tag = "equivalent")]
        let FnArg::Typed(argument) = function.sig.inputs.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None; // #[gamma::skip(option.none_to_some, reason = "This helper resolves only free ItemFn declarations; receiver parameters are not supported Rust free-function syntax.", tag = "equivalent")]
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::Ident(pattern) = &*argument.pat else { return None };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Reference(ty) = &*argument.ty else { return None };
        if !argument.attrs.is_empty()
            || pattern.mutability.is_some()
            || pattern.by_ref.is_some()
            || pattern.subpat.is_some()
            || ty.mutability.is_some()
            || self.kind(&ty.elem, 0)? != *kind
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let [Stmt::Expr(Expr::Tuple(tuple), None)] = function.block.stmts.as_slice() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let ReturnType::Type(_, output) = &function.sig.output else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Tuple(output) = &**output else { return None };
        if tuple.elems.is_empty() || tuple.elems.len() > 16 || tuple.elems.len() != output.elems.len() || !tuple.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut keys = Vec::with_capacity(tuple.elems.len());
        for (projected, output) in tuple.elems.iter().zip(&output.elems) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Type::Reference(output) = output else { return None };
            if output.mutability.is_some() || text(&output.elem) != "str" {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let string = method(projected, "as_str", 0)?;
            if self.projection(&string.receiver, &pattern.ident.to_string(), kind)? != Kind::String {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Expr::Field(projected) = &*string.receiver else { return None };
            keys.push(text(&projected.member));
        }
        Some(keys)
    }
}

fn plain_attributes(attributes: &[syn::Attribute]) -> bool {
    declaration_attributes(attributes)
        && attributes
            .iter()
            .all(|attribute| !attribute.path().is_ident("cfg") && !attribute.path().is_ident("cfg_attr"))
}

fn conflicting_impl(file: &syn::File, record_name: &str) -> bool {
    let mut names = HashSet::from_iter([record_name.to_owned()]);
    for _ in 0..16 {
        let before = names.len();
        for item in &file.items {
            if let Item::Type(alias) = item
                && let Type::Path(path) = &*alias.ty
                && path
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| names.contains(&segment.ident.to_string()))
            {
                let _new = names.insert(alias.ident.to_string());
            }
        }
        if names.len() == before {
            return file.items.iter().any(|item| match item {
                Item::Impl(implementation) => match &*implementation.self_ty {
                    Type::Path(path) => {
                        !implementation.generics.params.is_empty()
                            || path
                                .path
                                .segments
                                .last()
                                .is_some_and(|segment| names.contains(&segment.ident.to_string()))
                    }
                    _ => true,
                },
                _ => false,
            });
        }
    }
    true
}

fn field_attributes(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        attribute.path().is_ident("doc")
            || attribute.path().is_ident("serde")
                && attribute
                    .parse_args_with(Punctuated::<syn::Meta, Token![,]>::parse_terminated)
                    .is_ok_and(|metas| {
                        metas.iter().all(|meta| match meta {
                            syn::Meta::Path(path) => path.is_ident("default"),
                            syn::Meta::NameValue(meta) => {
                                meta.path.is_ident("skip_serializing_if")
                                    && matches!(&meta.value, Expr::Lit(literal) if matches!(literal.lit, syn::Lit::Str(_)))
                            }
                            syn::Meta::List(_) => false,
                        })
                    })
    })
}

fn closure(expression: &Expr, count: usize) -> Option<(Vec<String>, &Expr)> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Closure(closure) = bare(expression) else { return None };
    let unsupported_closure = !closure.attrs.is_empty() || closure.inputs.len() != count || closure.asyncness.is_some();
    if (
        // #[gamma::skip(logical.or_remove_right, reason = "Pinned Syn 3.0.9 ClosureModifiers is fieldless and require_empty always succeeds.", tag = "equivalent")]
        unsupported_closure || closure.modifiers.require_empty().is_err()
    ) || closure.lifetimes.is_some()
        || closure.capture.is_some()
        || closure.constness.is_some()
        || !matches!(closure.output, ReturnType::Default)
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let parameters = closure
        .inputs
        .iter()
        .map(|pattern| match pattern {
            Pat::Ident(pattern) if pattern.by_ref.is_none() && pattern.mutability.is_none() && pattern.subpat.is_none() => {
                Some(pattern.ident.to_string())
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let unique = parameters.iter().collect::<HashSet<_>>();
    (unique.len() == count).then_some((parameters, &closure.body))
}

fn reference(expression: &Expr) -> Option<&Expr> {
    let Expr::Reference(reference) = bare(expression) else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    (reference.mutability.is_none() && reference.attrs.is_empty()).then_some(&reference.expr)
}

struct Pool {
    kind: Kind,
    field: String,
    sorted: bool,
    deduplicated: bool,
}

fn iterated(expression: &Expr) -> Option<String> {
    name(&method(expression, "iter", 0)?.receiver)
}

fn collected(expression: &Expr) -> Option<(&syn::ExprMethodCall, &Expr)> {
    let collect = method(expression, "collect", 0)?;
    if let Some(arguments) = &collect.turbofish {
        let [GenericArgument::Type(Type::Path(path))] = arguments.args.iter().collect::<Vec<_>>().as_slice() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        let segments = path.path.segments.iter().collect::<Vec<_>>();
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let [segment] = segments.as_slice() else { return None };
        if !path.attrs.is_empty() || path.qself.is_some() || segment.ident != "Vec" {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
    }
    let Expr::MethodCall(adapter) = &*collect.receiver else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    if !adapter.attrs.is_empty() || adapter.turbofish.is_some() || adapter.args.len() != 1 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    Some((
        adapter,
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding guard established exactly one adapter argument.", tag = "equivalent")]
        adapter.args.first()?,
    ))
}

fn pool_projection(effects: &mut Effects<'_>, expression: &Expr, input: &Kind, slice: &str) -> Option<Pool> {
    let (adapter, mapper) = collected(expression)?;
    if adapter.method != "filter_map" || iterated(&adapter.receiver).as_deref() != Some(slice) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let (parameters, body) = closure(mapper, 1)?;
    let Kind::Option(kind) = effects.clone_projection(body, &parameters[0], input)? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    effects.derived(&kind, "PartialEq")?;
    let clone = method(body, "clone", 0)?;
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Field(projected) = &*clone.receiver else { return None };
    Some(Pool {
        kind: *kind,
        field: text(&projected.member),
        sorted: false,
        deduplicated: false,
    })
}

fn mapped_record(effects: &mut Effects<'_>, expression: &Expr, input: &Kind, slice: &str, pools: &HashMap<String, Pool>) -> Option<Kind> {
    let (adapter, mapper) = collected(expression)?;
    if adapter.method != "map" || iterated(&adapter.receiver).as_deref() != Some(slice) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let (parameters, body) = closure(mapper, 1)?;
    if pools.contains_key(&parameters[0]) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Struct(output) = bare(body) else { return None };
    let unsupported_output = !output.attrs.is_empty() || output.rest.is_some();
    if
    // #[gamma::skip(logical.or_remove_right, reason = "The derived TypePath retains the qualified path's leading colon or multiple segments, independently rejected by effects.kind.", tag = "equivalent")]
    unsupported_output || output.qself.is_some() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let ty = Type::Path(syn::TypePath {
        attrs: Vec::new(),
        qself: None,
        path: output.path.clone(),
    });
    let kind = effects.kind(&ty, 0)?;
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Kind::Record(record_name) = &kind else { return None };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding effects.kind call created this Record kind only after validating the same declaration.", tag = "equivalent")]
    let schema = record(effects.file, record_name)?;
    if output.fields.len() != schema.fields.len() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let mut fields = HashSet::default();
    for projected in &output.fields {
        if !projected.attrs.is_empty() || !fields.insert(text(&projected.member)) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let expected = effects.projected(&kind, &projected.member)?;
        if let Some(actual) = effects.clone_projection(&projected.expr, &parameters[0], input) {
            if expected != actual {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        } else if let Some(actual @ Kind::Scalar(_)) = effects.projection(&projected.expr, &parameters[0], input) {
            if expected != actual {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        } else {
            indexed_option(effects, &projected.expr, &parameters[0], input, &expected, pools)?;
        }
    }
    Some(kind)
}

fn indexed_option(
    effects: &mut Effects<'_>,
    expression: &Expr,
    parameter: &str,
    input: &Kind,
    expected: &Kind,
    pools: &HashMap<String, Pool>,
) -> Option<()> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Kind::Option(expected) = expected else { return None };
    let map = method(expression, "map", 1)?;
    let as_ref = method(&map.receiver, "as_ref", 0)?;
    let Kind::Option(kind) = effects.projection(&as_ref.receiver, parameter, input)? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Field(projected) = &*as_ref.receiver else { return None };
    let (parameters, body) = closure(
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "method(map, 1) already established exactly one argument.", tag = "equivalent")]
        map.args.first()?,
        1,
    )?;
    if parameters[0] == parameter || pools.contains_key(&parameters[0]) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Block(block) = bare(body) else { return None };
    if !block.attrs.is_empty() || block.label.is_some() || block.block.stmts.len() != 2 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let (index, search) = local(&block.block.stmts[0])?;
    if index == parameters[0] || index == parameter || pools.contains_key(&index) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let search = expect(search)?;
    let position = method(search, "position", 1)?;
    let pool = pools.get(&iterated(&position.receiver)?)?;
    if pool.kind != *kind || pool.field != text(&projected.member) || !pool.sorted || !pool.deduplicated {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let (known, predicate) = closure(
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "method(position, 1) already established exactly one argument.", tag = "equivalent")]
        position.args.first()?,
        1,
    )?;
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Binary(predicate) = bare(predicate) else { return None };
    if !predicate.attrs.is_empty()
        || !matches!(predicate.op, syn::BinOp::Eq(_))
        || name(&predicate.left).as_deref() != Some(&known[0])
        || name(&predicate.right).as_deref() != Some(&parameters[0])
        || known[0] == parameters[0]
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    effects.derived(&kind, "PartialEq")?;
    let Stmt::Expr(conversion, None) = &block.block.stmts[1] else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Call(conversion) = expect(conversion)? else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Path(path) = &*conversion.func else { return None };
    let segments = path.path.segments.iter().collect::<Vec<_>>();
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let [ty, function] = segments.as_slice() else { return None };
    if !conversion.attrs.is_empty()
        || conversion.args.len() != 1
        || path.qself.is_some()
        || path.path.leading_colon.is_some()
        || function.ident != "try_from"
        || !matches!(ty.arguments, PathArguments::None)
        || !matches!(function.arguments, PathArguments::None)
        || name(
            // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding short-circuit arity check established exactly one conversion argument.", tag = "equivalent")]
            conversion.args.first()?,
        )
        .as_deref()
            != Some(&index)
        || **expected != Kind::Scalar(ty.ident.to_string())
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // Narrowing may reject an oversized artifact on its first iteration. It is not
    // assumed infallible: successful completion is replayed with the same pool and
    // index, so this check cannot become a later escape from a growing recurrence.
    matches!(ty.ident.to_string().as_str(), "u32" | "usize").then_some(())
}

fn expect(expression: &Expr) -> Option<&Expr> {
    let call = method(expression, "expect", 1)?;
    matches!(call.args.first(), Some(Expr::Lit(literal)) if matches!(literal.lit, syn::Lit::Str(_))).then_some(&call.receiver)
}

fn closed_dispatch(file: &syn::File, sources: &Namespaces, source: &str, inline: &[String]) -> bool {
    // The shared closure covers the older grouping operations. These additional
    // operations need their own evidence until the namespace index models them.
    fn imported(tree: &syn::UseTree, names: &mut Vec<String>) -> bool {
        match tree {
            syn::UseTree::Path(path) => imported(&path.tree, names),
            syn::UseTree::Name(name) => {
                names.push(name.ident.to_string());
                true
            }
            syn::UseTree::Rename(rename) => {
                names.push(rename.rename.to_string());
                true
            }
            syn::UseTree::Group(group) => group.items.iter().all(|tree| imported(tree, names)),
            syn::UseTree::Glob(_) => false,
        }
    }
    file.items.iter().all(|item| match item {
        Item::Trait(trait_) => !trait_.items.iter().any(|item| {
            matches!(item,
            syn::TraitItem::Fn(function) if matches!(function.sig.ident.to_string().as_str(), "cmp" | "as_ref" | "try_from"))
        }),
        Item::Use(import) => {
            let mut names = Vec::new();
            imported(&import.tree, &mut names)
                && names
                    .iter()
                    .all(|name| syn::parse_str::<Type>(name).is_ok_and(|ty| sources.stable_path_for(source, inline, &ty)))
        }
        _ => true,
    })
}

/// Certifies the effects between the already-verified grouping boundaries.
#[expect(
    clippy::too_many_lines,
    reason = "one bounded pass tracks declaration effects, pool identities, and retained record growth together"
)]
pub(super) fn prove(
    file: &syn::File,
    loop_: &syn::ExprWhile,
    sources: &Namespaces,
    source: &str,
    inline: &[String],
    outputs: &HashSet<String>,
) -> Option<String> {
    if !sources.primitive_scope_for(source, inline)
        || !super::super::fixed::numeric_names(file, &loop_.span().byte_range())
        || !closed_dispatch(file, sources, source, inline)
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(try.propagate_to_unwrap, reason = "The sole caller Grouping::new already resolved this same immutable enclosing function.", tag = "equivalent")]
    let (signature, block) = super::function(file, &Expr::While(loop_.clone()))?;
    let mut shadows = block.stmts.iter().filter_map(local).map(|(name, _)| name).collect::<HashSet<_>>();
    for argument in &signature.inputs {
        if let FnArg::Typed(argument) = argument
            && let Pat::Ident(pattern) = &*argument.pat
        {
            let _new = shadows.insert(pattern.ident.to_string());
        }
    }
    for statement in &loop_.body.stmts {
        if let Some((name, _)) = local(statement) {
            let _new = shadows.insert(name);
        }
    }
    let mut effects = Effects {
        file,
        sources,
        source,
        inline,
        shadows,
        remaining: 512,
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated the unchanged first statement as a copied local.", tag = "equivalent")]
    let (_, copied) = local(loop_.body.stmts.first()?)?;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated this unchanged initializer as a zero-argument clone.", tag = "equivalent")]
    let copy = method(copied, "clone", 0)?;
    let Expr::Field(projected) = &*copy.receiver else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    let Expr::Index(index) = &*projected.base else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    let Expr::Field(input) = &*index.expr else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already resolved this same input root while validating the copied input index.", tag = "equivalent")]
    let root = name(&input.base)?;
    let parameter = signature.inputs.iter().find_map(|argument| match argument {
        FnArg::Typed(argument) if matches!(&*argument.pat, Pat::Ident(pattern) if pattern.ident == root) => Some(&*argument.ty),
        _ => None,
    })?;
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Reference(parameter) = parameter else { return None };
    let Type::Path(parameter) = &*parameter.elem else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated this same bare parameter identifier and its immutable record declaration.", tag = "equivalent")]
    let schema = record(file, &parameter.path.get_ident()?.to_string())?;
    if !plain_attributes(&schema.attrs) || conflicting_impl(file, &schema.ident.to_string()) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let Member::Named(member) = &input.member else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already found this same immutable named input field.", tag = "equivalent")]
    let input_field = schema.fields.iter().find(|field| field.ident.as_ref() == Some(member))?;
    if !field_attributes(&input_field.attrs) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already found this same immutable named input field.", tag = "equivalent")]
    let Type::Path(vector) = field(schema, &member.to_string())? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Parsed TypePath values contain at least one path segment.", tag = "equivalent")]
    let PathArguments::AngleBracketed(arguments) = &vector.path.segments.first()?.arguments else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already required the first type argument of this same input vector.", tag = "equivalent")]
    let GenericArgument::Type(element) = arguments.args.first()? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    let input_kind = effects.kind(element, 0)?;
    let (slice, slice_value) = local(
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new requires two distinct locals and a separate output push, so the body contains at least three statements.", tag = "equivalent")]
        loop_.body.stmts.get(2)?,
    )?;
    let Expr::Index(slice_index) = reference(slice_value)? else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    let Expr::Range(range) = &*slice_index.index else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
    };
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated this unchanged second statement as the boundary local.", tag = "equivalent")]
    let (boundary, _) = local(loop_.body.stmts.get(1)?)?;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already required this same immutable input index to be the named cursor.", tag = "equivalent")]
    let cursor = name(&index.index)?;
    if text(&slice_index.expr) != text(&index.expr)
        || !matches!(range.limits, syn::RangeLimits::HalfOpen(_))
        || range.start.as_deref().and_then(name).as_deref() != Some(&cursor)
        || range.end.as_deref().and_then(name).as_deref() != Some(&boundary)
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let mut pools = HashMap::default();
    let mut vectors = HashMap::default();
    let mut bound = HashSet::from_iter(
        // #[gamma::skip(collection.reverse_array, reason = "These identities are immediately collected into an unordered membership set.", tag = "equivalent")]
        [
            root,
            cursor,
            boundary,
            slice.clone(),
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated this unchanged first statement as the copied local.", tag = "equivalent")]
            local(loop_.body.stmts.first()?)?.0,
        ],
    );
    let mut grew = false;
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Successful first/get(2) lookups above already establish a nonempty body.", tag = "equivalent")]
    let end = loop_.body.stmts.len().checked_sub(1)?;
    for statement in &loop_.body.stmts[3..end] {
        effects.spend()?;
        if grew {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        if let Some((binding, initializer)) = local(statement) {
            if !bound.insert(binding.clone()) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            if let Some(pool) = pool_projection(&mut effects, initializer, &input_kind, &slice) {
                local_vector(&mut effects, statement, &pool.kind)?;
                let _old = pools.insert(binding, pool);
            } else {
                let kind = mapped_record(&mut effects, initializer, &input_kind, &slice, &pools)?;
                local_vector(&mut effects, statement, &kind)?;
                let _old = vectors.insert(binding, kind);
            }
            continue;
        }
        let Stmt::Expr(Expr::MethodCall(call), Some(_)) = statement else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !call.attrs.is_empty() || call.turbofish.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let receiver = name(&call.receiver)?;
        if let Some(pool) = pools.get_mut(&receiver) {
            if call.method == "sort_by"
                && call.args.len() == 1
                && (
                    // #[gamma::skip(logical.and_remove_right, reason = "deduplicated is set only after sorted, and sorted is never reset, so !sorted already implies !deduplicated.", tag = "equivalent")]
                    !pool.sorted && !pool.deduplicated
                )
            {
                effects.comparator(
                    // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The branch guard established exactly one comparator argument.", tag = "equivalent")]
                    call.args.first()?,
                    &pool.kind,
                )?;
                pool.sorted = true;
            } else if call.method == "dedup" && call.args.is_empty() && pool.sorted && !pool.deduplicated {
                effects.derived(&pool.kind, "PartialEq")?;
                pool.deduplicated = true;
            } else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            continue;
        }
        if call.method != "push" || call.args.len() != 1 || bound.contains(&receiver) || !outputs.contains(&receiver) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding output-push guard established exactly one argument.", tag = "equivalent")]
        let Expr::Struct(output) = call.args.first()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
        };
        let output_name = output.path.get_ident()?.to_string();
        let output_schema = record(file, &output_name)?;
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "The preceding output.path.get_ident already rejects qualified-self paths by their segment count or leading colon.", tag = "equivalent")]
            !sources.local_record_for(source, inline, &output_name) || output.qself.is_some()
        ) || !closed_record(file, &output_name)
            || !plain_attributes(&output_schema.attrs)
            || !output.attrs.is_empty()
            || output.rest.is_some()
            || output.fields.len() != output_schema.fields.len()
            || conflicting_impl(file, &output_name)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut members = HashSet::default();
        for projected in &output.fields {
            let Member::Named(member) = &projected.member else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None; // #[gamma::skip(option.none_to_some, reason = "An empty returned body certificate cannot equal any token-rendered Rust block in the sole consumer.", tag = "equivalent")]
            };
            if
            // #[gamma::skip(logical.or_remove_right, reason = "Supported struct literals cannot initialize the same named field twice; attributed fields remain independently rejected.", tag = "equivalent")]
            !projected.attrs.is_empty() || !members.insert(member.to_string()) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let schema_field = output_schema.fields.iter().find(|field| field.ident.as_ref() == Some(member))?;
            if !field_attributes(&schema_field.attrs) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let value = name(&projected.expr)?;
            if let Some(pool) = pools.remove(&value) {
                vector_element(&mut effects, &schema_field.ty, &pool.kind)?;
            } else if let Some(kind) = vectors.remove(&value) {
                vector_element(&mut effects, &schema_field.ty, &kind)?;
            } else {
                let copied_kind = effects.projected(
                    &input_kind,
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The copy receiver was already destructured as the same immutable field expression.", tag = "equivalent")]
                    &super_member(copy)?,
                )?;
                if value !=
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "Grouping::new already validated this unchanged first statement as the copied local.", tag = "equivalent")]
                    local(loop_.body.stmts.first()?)?.0
                    || effects.kind(&schema_field.ty, 0)? != copied_kind
                {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    return None;
                }
            }
        }
        grew = pools.is_empty() && vectors.is_empty();
    }
    if !grew {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let mut body = loop_.body.clone();
    // #[gamma::skip(try.propagate_to_unwrap, reason = "Successful first/get(2) lookups established this cloned body is nonempty.", tag = "equivalent")]
    let _progress = body.stmts.pop()?;
    Some(text(&body))
}

fn super_member(copy: &syn::ExprMethodCall) -> Option<Member> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Field(projected) = &*copy.receiver else { return None };
    Some(projected.member.clone())
}

fn vector_element(effects: &mut Effects<'_>, ty: &Type, expected: &Kind) -> Option<()> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Path(path) = ty else { return None };
    let segments = path.path.segments.iter().collect::<Vec<_>>();
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let [segment] = segments.as_slice() else { return None };
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    let [GenericArgument::Type(element)] = arguments.args.iter().collect::<Vec<_>>().as_slice() else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    ((
        // #[gamma::skip(logical.and_remove_right, reason = "The retained singleton/no-leading-colon checks independently reject parsed qualified-self vector paths.", tag = "equivalent")]
        path.attrs.is_empty() && path.qself.is_none()
    ) && path.path.leading_colon.is_none()
        && segment.ident == "Vec"
        && effects.kind(element, 0)? == *expected)
        .then_some(())
}

fn local_vector(effects: &mut Effects<'_>, statement: &Stmt, expected: &Kind) -> Option<()> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Stmt::Local(local) = statement else { return None };
    match &local.pat {
        Pat::Type(pattern) => vector_element(effects, &pattern.ty, expected),
        Pat::Ident(_) => Some(()),
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::cfg::CfgSet;

    fn with_effects<T>(source: &str, check: impl FnOnce(&mut Effects<'_>) -> T) -> T {
        let file = syn::parse_file(source).unwrap();
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        check(&mut Effects {
            file: &file,
            sources: &sources,
            source: "",
            inline: &[],
            shadows: HashSet::default(),
            remaining: 512,
        })
    }

    const KEYS: &str = "
        #[derive(Clone, PartialEq)]
        struct Atom { first: String, second: String }
        fn key(row: &Atom) -> (&str, &str) {
            (row.first.as_str(), row.second.as_str())
        }";

    #[test]
    fn associated_record_kinds_do_not_inherit_the_spelling_of_a_numeric_type_parameter() {
        with_effects(
            "trait HasOutput { type Output; } fn accept<u32: HasOutput>(_: u32::Output) {}",
            |effects| {
                let Item::Fn(function) = &effects.file.items[1] else {
                    unreachable!()
                };
                let FnArg::Typed(argument) = &function.sig.inputs[0] else {
                    unreachable!()
                };
                let ty = (*argument.ty).clone();
                assert_eq!(effects.kind(&syn::parse_quote!(u32), 0), Some(Kind::Scalar("u32".into())));
                assert_eq!(effects.kind(&ty, 0), None);
            },
        );
    }

    #[test]
    fn collected_vectors_do_not_accept_an_associated_type_of_a_bounded_parameter() {
        let file: syn::File = syn::parse_quote! {
            trait Collections { type Vec: FromIterator<u32>; }
            fn collect<T: Collections>(input: &[u32]) -> T::Vec {
                input.iter().map(|value| *value).collect::<<T>::Vec>()
            }
        };
        let Item::Fn(function) = &file.items[1] else { unreachable!() };
        let Stmt::Expr(expression, None) = &function.block.stmts[0] else {
            unreachable!()
        };
        let positive: Expr = syn::parse_quote!(input.iter().map(|value| *value).collect::<Vec<_>>());
        let (adapter, mapper) = collected(&positive).unwrap();
        assert_eq!(adapter.method, "map");
        assert_eq!(mapper, &syn::parse_quote!(|value| *value));
        assert!(collected(expression).is_none());
    }

    #[test]
    fn indexed_optional_conversions_reject_valid_custom_qualified_and_generic_dispatch() {
        let records = "
            #[derive(Clone, PartialEq)] struct Atom { flag: bool }
            struct Row { atom: Option<Atom> }
        ";
        let expression = |conversion: &str| {
            format!(
                r#"row.atom.as_ref().map(|value| {{
                    let index = pool.iter().position(|known| known == value).expect("member");
                    {conversion}.expect("bounded")
                }})"#
            )
        };
        for (declaration, parameters, conversion, expected) in [
            ("", "", "u32::try_from(index)", Some(())),
            (
                "trait u32 { fn try_from(index: usize) -> Result<core::primitive::u32, std::num::TryFromIntError>; }",
                "<T: u32>",
                "<T as u32>::try_from(index)",
                None,
            ),
            (
                "struct u32<T>(T); impl u32<()> {
                    fn try_from(index: usize) -> Result<core::primitive::u32, std::num::TryFromIntError> {
                        core::primitive::u32::try_from(index)
                    }
                }",
                "",
                "u32::<()>::try_from(index)",
                None,
            ),
        ] {
            let source = format!(
                "{records} {declaration}
                fn lookup{parameters}(row: &Row, pool: &[Atom]) -> Option<core::primitive::u32> {{
                    {}
                }}",
                expression(conversion)
            );
            with_effects(&source, |effects| {
                let input = effects.kind(&syn::parse_quote!(Row), 0).unwrap();
                let output = Kind::Option(Box::new(Kind::Scalar("u32".into())));
                let pools = HashMap::from_iter([(
                    "pool".into(),
                    Pool {
                        kind: Kind::Record("Atom".into()),
                        field: "atom".into(),
                        sorted: true,
                        deduplicated: true,
                    },
                )]);
                assert_eq!(
                    indexed_option(
                        effects,
                        &syn::parse_str(&expression(conversion)).unwrap(),
                        "row",
                        &input,
                        &output,
                        &pools
                    ),
                    expected,
                    "{conversion}"
                );
            });
        }
    }

    #[test]
    fn record_comparator_rejects_unmodeled_callbacks_before_key_resolution() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| key(left).cmp(&key(right))), &atom),
                Some(())
            );
            for expression in [
                "compare",
                "|left, right| compare(left, right)",
                "|left, right| key(left).cmp(key(right))",
            ] {
                assert_eq!(
                    effects.comparator(&syn::parse_str(expression).unwrap(), &atom),
                    None,
                    "{expression}"
                );
            }
        });
    }

    #[test]
    fn record_ordering_key_rejects_a_well_formed_but_uninformative_unit_key() {
        let source = KEYS
            .replace("-> (&str, &str)", "-> ()")
            .replace("(row.first.as_str(), row.second.as_str())", "()");
        with_effects(KEYS, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                Some(vec!["first".into(), "second".into()])
            );
        });
        with_effects(&source, |effects| {
            assert!(effects.sources.function("", &[], &["key".into()]).is_some());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
    }

    #[test]
    fn record_ordering_key_checks_configuration_even_when_namespace_resolution_preserves_it() {
        with_effects(KEYS, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                Some(vec!["first".into(), "second".into()])
            );
        });
        let source = KEYS.replace("fn key(", "#[cfg_attr(any(), coverage(off))] fn key(");
        with_effects(&source, |effects| {
            assert!(effects.sources.function("", &[], &["key".into()]).is_some());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
    }

    #[test]
    fn record_adapters_reject_named_callbacks_after_recognizing_the_iterator() {
        let source = format!("{KEYS} struct Row {{ atom: Option<Atom> }} struct Output {{ first: String }}");
        with_effects(&source, |effects| {
            let input = effects.kind(&syn::parse_quote!(Row), 0).unwrap();
            assert!(
                pool_projection(
                    effects,
                    &syn::parse_quote!(group.iter().filter_map(|row| row.atom.clone()).collect()),
                    &input,
                    "group",
                )
                .is_some()
            );
            assert!(
                pool_projection(
                    effects,
                    &syn::parse_quote!(group.iter().filter_map(optional_atom).collect()),
                    &input,
                    "group",
                )
                .is_none()
            );
            assert!(
                pool_projection(
                    effects,
                    &syn::parse_quote!(group.iter().filter_map(|row| (row.atom.clone())).collect()),
                    &input,
                    "group",
                )
                .is_none()
            );
            let atom = effects.kind(&syn::parse_quote!(Atom), 0).unwrap();
            assert_eq!(
                mapped_record(
                    effects,
                    &syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect()),
                    &atom,
                    "group",
                    &HashMap::default(),
                ),
                Some(Kind::Record("Output".into()))
            );
            assert_eq!(
                mapped_record(
                    effects,
                    &syn::parse_quote!(group.iter().map(make_output).collect()),
                    &atom,
                    "group",
                    &HashMap::default(),
                ),
                None
            );
        });
    }

    #[test]
    fn record_mapped_output_requires_a_replayable_declared_field_type() {
        let source = format!("{KEYS} struct Output {{ first: String }}");
        let expression = syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect());
        with_effects(&source, |effects| {
            assert_eq!(
                mapped_record(effects, &expression, &Kind::Record("Atom".into()), "group", &HashMap::default()),
                Some(Kind::Record("Output".into()))
            );
        });
        let source = source.replace("struct Output { first: String }", "struct Output { first: Vec<u8> }");
        with_effects(&source, |effects| {
            assert!(effects.sources.local_record_for("", &[], "Output"));
            assert_eq!(
                mapped_record(
                    effects,
                    &syn::parse_quote!(
                        group
                            .iter()
                            .map(|row| Output {
                                first: row.first.clone().into_bytes()
                            })
                            .collect()
                    ),
                    &Kind::Record("Atom".into()),
                    "group",
                    &HashMap::default(),
                ),
                None
            );
        });
    }

    #[test]
    fn record_kinds_withhold_generic_defaults_even_when_the_nominal_name_is_indexed() {
        with_effects("struct Defaulted<T = u32> { value: T }", |effects| {
            assert!(effects.sources.local_record_for("", &[], "Defaulted"));
            assert_eq!(effects.kind(&syn::parse_quote!(Defaulted), 0), None);
        });
        with_effects("struct Defaulted { value: u32 }", |effects| {
            assert_eq!(
                effects.kind(&syn::parse_quote!(Defaulted), 0),
                Some(Kind::Record("Defaulted".into()))
            );
        });
    }

    #[test]
    fn record_adapters_and_mapped_record_values_check_their_own_metadata() {
        let source = format!("{KEYS} struct Output {{ first: String }}");
        with_effects(&source, |effects| {
            let input = effects.kind(&syn::parse_quote!(Atom), 0).unwrap();
            let original: Expr = syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect());
            assert_eq!(
                mapped_record(effects, &original, &input, "group", &HashMap::default()),
                Some(Kind::Record("Output".into()))
            );
            for record_value in [false, true] {
                let mut changed = original.clone();
                let Expr::MethodCall(collect) = &mut changed else { unreachable!() };
                let Expr::MethodCall(adapter) = &mut *collect.receiver else {
                    unreachable!()
                };
                let attributes = if record_value {
                    let Expr::Closure(mapper) = adapter.args.first_mut().unwrap() else {
                        unreachable!()
                    };
                    let Expr::Struct(value) = &mut *mapper.body else { unreachable!() };
                    &mut value.attrs
                } else {
                    &mut adapter.attrs
                };
                attributes.push(syn::parse_quote!(#[allow(unused)]));
                assert_eq!(
                    mapped_record(effects, &changed, &input, "group", &HashMap::default()),
                    None,
                    "record value: {record_value}"
                );
            }
        });
    }

    #[test]
    fn record_optional_indices_reject_alternate_computation_at_the_first_unmodeled_step() {
        let source = format!("{KEYS} struct Row {{ atom: Option<Atom> }}");
        let expression = r#"row.atom.as_ref().map(|value| {
            let index = pool.iter().position(|known| known == value).expect("member");
            u32::try_from(index).expect("bounded")
        })"#;
        with_effects(&source, |effects| {
            let input = effects.kind(&syn::parse_quote!(Row), 0).unwrap();
            let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
            let pools = HashMap::from_iter([(
                "pool".into(),
                Pool {
                    kind: Kind::Record("Atom".into()),
                    field: "atom".into(),
                    sorted: true,
                    deduplicated: true,
                },
            )]);
            assert_eq!(
                indexed_option(effects, &syn::parse_str(expression).unwrap(), "row", &input, &expected, &pools),
                Some(())
            );
            for changed in [
                "row.atom.clone()".to_owned(),
                expression.replace(
                    r#"let index = pool.iter().position(|known| known == value).expect("member");"#,
                    "observe(value);",
                ),
                expression.replace("pool.iter().position(|known| known == value)", "Some(0_usize)"),
                expression.replace("|known| known == value", "matches_value"),
                expression.replace("map(|value| {", "map(|value| 'index: {"),
            ] {
                assert_eq!(
                    indexed_option(effects, &syn::parse_str(&changed).unwrap(), "row", &input, &expected, &pools),
                    None,
                    "{changed}"
                );
            }
            let mut attributed: Expr = syn::parse_str(expression).unwrap();
            let Expr::MethodCall(map) = &mut attributed else { unreachable!() };
            let Expr::Closure(mapper) = map.args.first_mut().unwrap() else {
                unreachable!()
            };
            let Expr::Block(block) = &mut *mapper.body else { unreachable!() };
            let Stmt::Local(index) = &mut block.block.stmts[0] else {
                unreachable!()
            };
            let Expr::MethodCall(expect) = &mut *index.init.as_mut().unwrap().expr else {
                unreachable!()
            };
            let Expr::MethodCall(position) = &mut *expect.receiver else {
                unreachable!()
            };
            let Expr::Closure(predicate) = position.args.first_mut().unwrap() else {
                unreachable!()
            };
            let Expr::Binary(equal) = &mut *predicate.body else {
                unreachable!()
            };
            equal.attrs.push(syn::parse_quote!(#[allow(unused)]));
            assert_eq!(indexed_option(effects, &attributed, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_derivation_requires_fuel_and_derives_on_nested_fields() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(effects.derived(&atom, "Clone"), Some(()));
            effects.remaining = 1;
            assert_eq!(effects.derived(&atom, "Clone"), None);
            effects.remaining = 0;
            assert_eq!(effects.derived(&atom, "Clone"), None);
        });
        let source = "#[derive(Clone)] struct Outer { inner: Option<Inner> } struct Inner { value: u32 }";
        with_effects(source, |effects| {
            let outer = effects.kind(&syn::parse_quote!(Outer), 0).unwrap();
            assert_eq!(effects.derived(&outer, "Clone"), None);
        });
        with_effects(&source.replace("struct Inner", "#[derive(Clone)] struct Inner"), |effects| {
            let outer = effects.kind(&syn::parse_quote!(Outer), 0).unwrap();
            assert_eq!(effects.derived(&outer, "Clone"), Some(()));
        });
    }

    #[test]
    fn record_ordering_key_requires_fuel_before_resolving_a_valid_named_helper() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &atom),
                Some(vec!["first".into(), "second".into()])
            );
            effects.remaining = 0;
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &atom), None);
        });
    }

    #[test]
    fn record_ordering_helper_configuration_is_closed_by_namespace_resolution() {
        with_effects(KEYS, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                Some(vec!["first".into(), "second".into()])
            );
        });
        with_effects(&KEYS.replace("fn key(", "#[cfg(all())] fn key("), |effects| {
            assert!(effects.sources.function("", &[], &["key".into()]).is_none());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
        with_effects(&KEYS.replace("fn key(", "#[custom] fn key("), |effects| {
            assert!(effects.sources.function("", &[], &["key".into()]).is_none());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
        with_effects(
            &KEYS.replace("fn key(", "#[doc = \"key\"] #[allow(dead_code)] fn key("),
            |effects| {
                assert!(effects.sources.function("", &[], &["key".into()]).is_some());
                assert_eq!(
                    effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                    Some(vec!["first".into(), "second".into()])
                );
            },
        );
    }

    #[test]
    fn record_qualified_associated_output_paths_do_not_resolve_to_nominal_local_records() {
        let source = format!(
            "{KEYS} struct Output {{ first: String }}
            trait Factory {{ type Output; }} struct Provider;
            impl Factory for Provider {{ type Output = Output; }}"
        );
        with_effects(&source, |effects| {
            let input = Kind::Record("Atom".into());
            let ordinary: Expr = syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect());
            assert_eq!(
                mapped_record(effects, &ordinary, &input, "group", &HashMap::default()),
                Some(Kind::Record("Output".into()))
            );
            let associated: Expr = syn::parse_quote!(
                group
                    .iter()
                    .map(|row| <Provider as Factory>::Output { first: row.first.clone() })
                    .collect()
            );
            assert_eq!(mapped_record(effects, &associated, &input, "group", &HashMap::default()), None);
            let path: syn::ExprStruct = syn::parse_quote!(<Provider as Factory>::Output { first: row.first.clone() });
            assert!(path.qself.is_some());
            assert!(path.path.get_ident().is_none());
            let erased = Type::Path(syn::TypePath {
                attrs: Vec::new(),
                qself: None,
                path: path.path,
            });
            assert_eq!(effects.kind(&erased, 0), None);
        });
    }

    #[test]
    fn record_pool_projection_requires_clone_and_equality_after_inert_type_validation() {
        let source = format!("{KEYS} struct Row {{ atom: Option<Atom> }}");
        let input = Kind::Record("Row".into());
        let expression: Expr = syn::parse_quote!(group.iter().filter_map(|row| row.atom.clone()).collect());
        with_effects(&source, |effects| {
            assert_eq!(effects.kind(&syn::parse_quote!(Row), 0), Some(input.clone()));
            assert!(pool_projection(effects, &expression, &input, "group").is_some());
        });
        with_effects(&source.replace("Clone, PartialEq", "PartialEq"), |effects| {
            assert_eq!(effects.kind(&syn::parse_quote!(Row), 0), Some(input.clone()));
            assert_eq!(effects.clone_projection(&syn::parse_quote!(row.atom.clone()), "row", &input), None);
        });
        with_effects(&source.replace("Clone, PartialEq", "Clone"), |effects| {
            assert!(
                effects
                    .clone_projection(&syn::parse_quote!(row.atom.clone()), "row", &input)
                    .is_some()
            );
            assert!(pool_projection(effects, &expression, &input, "group").is_none());
        });
    }

    #[test]
    fn record_kinds_require_indexed_local_declaration_evidence() {
        with_effects(KEYS, |effects| {
            assert_eq!(effects.kind(&syn::parse_quote!(Atom), 0), Some(Kind::Record("Atom".into())));
        });
        let file = syn::parse_file(KEYS).unwrap();
        let sources = Namespaces::of(&syn::parse_file("").unwrap(), &CfgSet::unconditional());
        let mut effects = Effects {
            file: &file,
            sources: &sources,
            source: "",
            inline: &[],
            shadows: HashSet::default(),
            remaining: 512,
        };
        assert_eq!(effects.kind(&syn::parse_quote!(String), 0), Some(Kind::String));
        assert_eq!(effects.kind(&syn::parse_quote!(Atom), 0), None);
    }

    #[test]
    fn record_ordering_key_ignores_unrelated_function_declarations() {
        let source = format!("{KEYS} fn unrelated() {{}}");
        with_effects(&source, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                Some(vec!["first".into(), "second".into()])
            );
        });
        with_effects(&format!("{KEYS} {KEYS}"), |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
        let conditional =
            format!("{KEYS} #[cfg(any())] fn key(row: &Atom) -> (&str, &str) {{ (row.first.as_str(), row.second.as_str()) }}");
        with_effects(&conditional, |effects| {
            assert!(effects.sources.function("", &[], &["key".into()]).is_some());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
    }

    #[test]
    fn record_ordering_key_rejects_noncall_and_nonreference_signatures() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| key(left).cmp(&key(right))), &atom),
                Some(())
            );
            assert_eq!(effects.comparator(&syn::parse_quote!(|left, right| left.cmp(&right)), &atom), None);
            assert_eq!(effects.key(&syn::parse_quote!(row.first), "row", &atom), None);
        });
        with_effects(&KEYS.replace("row: &Atom", "row: Atom"), |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
        with_effects(&KEYS.replace("row: &Atom", "(row,): (&Atom,)"), |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())), None);
        });
    }

    #[test]
    fn record_ordering_helpers_require_named_calls_not_callable_expressions() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &atom),
                Some(vec!["first".into(), "second".into()])
            );
            assert_eq!(effects.key(&syn::parse_quote!((|| key)(row)), "row", &atom), None);
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| key(left).cmp(&key(right))), &atom),
                Some(())
            );
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| (|| key)()(left).cmp(&key(right))), &atom),
                None
            );
        });
    }

    #[test]
    fn record_ordering_helpers_require_explicit_output_and_projection_syntax() {
        let atom = Kind::Record("Atom".into());
        with_effects(KEYS, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &atom),
                Some(vec!["first".into(), "second".into()])
            );
        });
        with_effects("struct Atom { first: String } fn key(row: &Atom) { () }", |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &atom), None);
        });
        let tuple_alias = format!(
            "type Pair<'a> = (&'a str, &'a str); {}",
            KEYS.replace("-> (&str, &str)", "-> Pair<'_>")
        );
        with_effects(&tuple_alias, |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &atom), None);
        });
        let reference_alias = format!(
            "type Text<'a> = &'a str; {}",
            KEYS.replace("-> (&str, &str)", "-> (Text<'_>, &str)")
        );
        with_effects(&reference_alias, |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &atom), None);
        });
        with_effects(&KEYS.replace("row.first.as_str()", "(row.first).as_str()"), |effects| {
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &atom), None);
        });
    }

    #[test]
    fn record_plain_attributes_reject_expansion_independently_of_configuration() {
        let allowed = syn::parse_file("#[allow(dead_code)] struct Row;").unwrap();
        let expanding = syn::parse_file("#[custom] struct Row;").unwrap();
        let conditional = syn::parse_file("#[cfg(feature = \"row\")] struct Row;").unwrap();
        assert!(plain_attributes(&record(&allowed, "Row").unwrap().attrs));
        assert!(!plain_attributes(&record(&expanding, "Row").unwrap().attrs));
        assert!(!plain_attributes(&record(&conditional, "Row").unwrap().attrs));
    }

    #[test]
    fn record_field_wire_metadata_requires_the_serde_namespace() {
        let serde = syn::parse_file("struct Row { #[serde(default)] value: u32 }").unwrap();
        let custom = syn::parse_file("struct Row { #[custom(default)] value: u32 }").unwrap();
        assert!(field_attributes(
            &record(&serde, "Row").unwrap().fields.iter().next().unwrap().attrs
        ));
        assert!(!field_attributes(
            &record(&custom, "Row").unwrap().fields.iter().next().unwrap().attrs
        ));
    }

    #[test]
    fn record_collection_type_metadata_is_not_adapter_metadata() {
        let original: Expr = syn::parse_quote!(group.iter().map(|row| row.first).collect::<Vec<_>>());
        assert!(collected(&original).is_some());
        let mut attributed = original;
        let Expr::MethodCall(call) = &mut attributed else { unreachable!() };
        let GenericArgument::Type(Type::Path(path)) = call.turbofish.as_mut().unwrap().args.first_mut().unwrap() else {
            unreachable!()
        };
        path.attrs.push(syn::parse_quote!(#[custom]));
        assert!(collected(&attributed).is_none());
        let qualified: Expr = syn::parse_quote!(group.iter().map(|row| row.first).collect::<<Atom as Family>::Vec<_>>());
        assert!(collected(&qualified).is_none());
    }

    #[test]
    fn record_pool_and_record_adapters_require_their_distinct_methods() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom> }} struct Output {{ first: String }}");
        with_effects(&source, |effects| {
            let input = Kind::Record("Row".into());
            let pool: Expr = syn::parse_quote!(group.iter().filter_map(|row| row.killer.clone()).collect());
            assert!(pool_projection(effects, &pool, &input, "group").is_some());
            let wrong: Expr = syn::parse_quote!(group.iter().map(|row| row.killer.clone()).collect());
            assert!(pool_projection(effects, &wrong, &input, "group").is_none());
            let atom = Kind::Record("Atom".into());
            let mapped: Expr = syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect());
            assert_eq!(
                mapped_record(effects, &mapped, &atom, "group", &HashMap::default()),
                Some(Kind::Record("Output".into()))
            );
            let wrong: Expr = syn::parse_quote!(group.iter().filter_map(|row| Output { first: row.first.clone() }).collect());
            assert_eq!(mapped_record(effects, &wrong, &atom, "group", &HashMap::default()), None);
        });
    }

    #[test]
    fn record_mapped_outputs_require_every_field_and_unattributed_members() {
        let source = format!("{KEYS} struct Output {{ first: String, second: String }}");
        with_effects(&source, |effects| {
            let input = Kind::Record("Atom".into());
            let complete: Expr = syn::parse_quote!(
                group
                    .iter()
                    .map(|row| Output {
                        first: row.first.clone(),
                        second: row.second.clone()
                    })
                    .collect()
            );
            assert_eq!(
                mapped_record(effects, &complete, &input, "group", &HashMap::default()),
                Some(Kind::Record("Output".into()))
            );
            let incomplete: Expr = syn::parse_quote!(group.iter().map(|row| Output { first: row.first.clone() }).collect());
            assert_eq!(mapped_record(effects, &incomplete, &input, "group", &HashMap::default()), None);
            let attributed: Expr = syn::parse_quote!(
                group
                    .iter()
                    .map(|row| Output {
                        #[custom]
                        first: row.first.clone(),
                        second: row.second.clone()
                    })
                    .collect()
            );
            assert_eq!(mapped_record(effects, &attributed, &input, "group", &HashMap::default()), None);
        });
    }

    #[test]
    fn record_indexed_option_blocks_require_a_conversion_and_reject_metadata() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom> }}");
        let original: Expr = syn::parse_quote!(row.killer.as_ref().map(|value| {
            let index = pool.iter().position(|known| known == value).expect("member");
            u32::try_from(index).expect("bounded")
        }));
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        with_effects(&source, |effects| {
            assert_eq!(indexed_option(effects, &original, "row", &input, &expected, &pools), Some(()));
            let mut unfinished = original.clone();
            let Expr::MethodCall(map) = &mut unfinished else { unreachable!() };
            let Expr::Closure(mapper) = map.args.first_mut().unwrap() else {
                unreachable!()
            };
            let Expr::Block(block) = &mut *mapper.body else { unreachable!() };
            let _conversion = block.block.stmts.pop().unwrap();
            assert_eq!(indexed_option(effects, &unfinished, "row", &input, &expected, &pools), None);
            let mut attributed = original;
            let Expr::MethodCall(map) = &mut attributed else { unreachable!() };
            let Expr::Closure(mapper) = map.args.first_mut().unwrap() else {
                unreachable!()
            };
            let Expr::Block(block) = &mut *mapper.body else { unreachable!() };
            block.attrs.push(syn::parse_quote!(#[custom]));
            assert_eq!(indexed_option(effects, &attributed, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_indexed_option_rejects_nonblock_callbacks_after_valid_projection_and_binding() {
        let source = format!(
            "{KEYS} struct Row {{ killer: Option<Atom> }}
            fn external_index(value: &Atom) -> u32 {{ value.first.len() as u32 }}"
        );
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        with_effects(&source, |effects| {
            let certified: Expr = syn::parse_quote!(row.killer.as_ref().map(|value| {
                let index = pool.iter().position(|known| known == value).expect("member");
                u32::try_from(index).expect("bounded")
            }));
            assert_eq!(indexed_option(effects, &certified, "row", &input, &expected, &pools), Some(()));
            let unmodeled: Expr = syn::parse_quote!(row.killer.as_ref().map(|value| external_index(value)));
            assert_eq!(indexed_option(effects, &unmodeled, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_indexed_option_withholds_equality_when_projection_uses_the_last_fuel() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom> }}");
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        let expression: Expr = syn::parse_quote!(row.killer.as_ref().map(|value| {
            let index = pool.iter().position(|known| known == value).expect("member");
            u32::try_from(index).expect("bounded")
        }));
        with_effects(&source, |effects| {
            assert_eq!(indexed_option(effects, &expression, "row", &input, &expected, &pools), Some(()));
            effects.remaining = 4;
            assert_eq!(indexed_option(effects, &expression, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_indexed_option_requires_binary_membership_and_a_two_segment_conversion_path() {
        let source = format!(
            "{KEYS} struct Row {{ killer: Option<Atom> }}
            fn checked_index(index: usize) -> Result<u32, std::num::TryFromIntError> {{ u32::try_from(index) }}
            macro_rules! checked {{ ($index:expr) => {{ u32::try_from($index) }}; }}"
        );
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        let expression = |predicate: &str, conversion: &str| {
            syn::parse_str::<Expr>(&format!(
                "row.killer.as_ref().map(|value| {{
                let index = pool.iter().position(|known| {predicate}).expect(\"member\");
                {conversion}.expect(\"bounded\")
            }})"
            ))
            .unwrap()
        };
        with_effects(&source, |effects| {
            assert_eq!(
                indexed_option(
                    effects,
                    &expression("known == value", "u32::try_from(index)"),
                    "row",
                    &input,
                    &expected,
                    &pools
                ),
                Some(())
            );
            assert_eq!(
                indexed_option(
                    effects,
                    &expression("known.eq(value)", "u32::try_from(index)"),
                    "row",
                    &input,
                    &expected,
                    &pools
                ),
                None
            );
            assert_eq!(
                indexed_option(
                    effects,
                    &expression("known == value", "checked_index(index)"),
                    "row",
                    &input,
                    &expected,
                    &pools
                ),
                None
            );
            assert_eq!(
                indexed_option(
                    effects,
                    &expression("known == value", "(|ordinal| u32::try_from(ordinal))(index)"),
                    "row",
                    &input,
                    &expected,
                    &pools
                ),
                None
            );
            assert_eq!(
                indexed_option(
                    effects,
                    &expression("known == value", "checked!(index)"),
                    "row",
                    &input,
                    &expected,
                    &pools
                ),
                None
            );
        });
    }

    #[test]
    fn record_indexed_option_checks_receiver_and_conversion_metadata_independently() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom> }}");
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        let original: Expr = syn::parse_quote!(row.killer.as_ref().map(|value| {
            let index = pool.iter().position(|known| known == value).expect("member");
            u32::try_from(index).expect("bounded")
        }));
        with_effects(&source, |effects| {
            assert_eq!(indexed_option(effects, &original, "row", &input, &expected, &pools), Some(()));
            let mut parenthesized = original.clone();
            let Expr::MethodCall(map) = &mut parenthesized else {
                unreachable!()
            };
            let Expr::MethodCall(as_ref) = &mut *map.receiver else {
                unreachable!()
            };
            *as_ref.receiver = syn::parse_quote!((row.killer));
            assert_eq!(indexed_option(effects, &parenthesized, "row", &input, &expected, &pools), None);
            let mut attributed = original;
            let Expr::MethodCall(map) = &mut attributed else { unreachable!() };
            let Expr::Closure(mapper) = map.args.first_mut().unwrap() else {
                unreachable!()
            };
            let Expr::Block(block) = &mut *mapper.body else { unreachable!() };
            let Stmt::Expr(Expr::MethodCall(expect), None) = &mut block.block.stmts[1] else {
                unreachable!()
            };
            let Expr::Call(conversion) = &mut *expect.receiver else {
                unreachable!()
            };
            conversion.attrs.push(syn::parse_quote!(#[custom]));
            assert_eq!(indexed_option(effects, &attributed, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_dispatch_rejects_a_bare_glob_without_a_named_import_to_mask_it() {
        let file = syn::parse_file("use camino::*;").unwrap();
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        assert!(!closed_dispatch(&file, &sources, "", &[]));
        let empty = syn::parse_file("").unwrap();
        let sources = Namespaces::of(&empty, &CfgSet::unconditional());
        assert!(closed_dispatch(&empty, &sources, "", &[]));
    }

    #[test]
    fn record_checked_index_bindings_cannot_shadow_input_or_pool_even_when_conversion_matches() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom> }}");
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pools = HashMap::from_iter([(
            "pool".into(),
            Pool {
                kind: Kind::Record("Atom".into()),
                field: "killer".into(),
                sorted: true,
                deduplicated: true,
            },
        )]);
        let expression = |index: &str| {
            syn::parse_str::<Expr>(&format!(
                "row.killer.as_ref().map(|value| {{
                let {index} = pool.iter().position(|known| known == value).expect(\"member\");
                u32::try_from({index}).expect(\"bounded\")
            }})"
            ))
            .unwrap()
        };
        with_effects(&source, |effects| {
            assert_eq!(
                indexed_option(effects, &expression("index"), "row", &input, &expected, &pools),
                Some(())
            );
            assert_eq!(indexed_option(effects, &expression("row"), "row", &input, &expected, &pools), None);
            assert_eq!(indexed_option(effects, &expression("pool"), "row", &input, &expected, &pools), None);
            let callback_shadow: Expr = syn::parse_quote!(row.killer.as_ref().map(|row| {
                let index = pool.iter().position(|known| known == row).expect("member");
                u32::try_from(index).expect("bounded")
            }));
            assert_eq!(indexed_option(effects, &callback_shadow, "row", &input, &expected, &pools), None);
        });
    }

    #[test]
    fn record_effect_kinds_require_inert_closed_declarations_and_recursive_derives() {
        let source = format!(
            "{KEYS}
            type Alias = Atom;
            #[derive(Clone, PartialEq)]
            struct Row {{ atom: Option<Alias>, flag: bool }}
            struct NotDerived {{ atom: Atom }}"
        );
        with_effects(&source, |effects| {
            let row = effects.kind(&syn::parse_quote!(Row), 0).unwrap();
            assert_eq!(row, Kind::Record("Row".into()));
            assert_eq!(effects.derived(&row, "Clone"), Some(()));
            assert_eq!(effects.derived(&row, "PartialEq"), Some(()));
            assert_eq!(effects.derived(&row, "Ord"), None);
            assert_eq!(
                effects.projected(&row, &syn::parse_quote!(atom)),
                Some(Kind::Option(Box::new(Kind::Record("Atom".into()))))
            );
            assert_eq!(effects.projected(&row, &syn::parse_quote!(missing)), None);
            assert_eq!(effects.projected(&row, &syn::parse_quote!(0)), None);
            assert_eq!(effects.derived(&Kind::Record("NotDerived".into()), "Clone"), None);
        });
        for declaration in [
            "pub struct Atom { first: String, second: String }",
            "struct Atom(String, String);",
            "struct Atom { first: External, second: String }",
            "#[cfg(feature = \"x\")] struct Atom { first: String, second: String }",
            "struct Atom { #[cfg(feature = \"x\")] first: String, second: String }",
            "struct Atom { first: String, second: String } impl Atom { fn inspect(&self) {} }",
            "struct Atom { first: String, second: String } impl Drop for Atom { fn drop(&mut self) {} }",
            "struct Atom { first: String, second: String } type Alias = Atom; impl Drop for Alias { fn drop(&mut self) {} }",
            "struct Atom { first: String, second: String } impl<T> Drop for T { fn drop(&mut self) {} }",
            "struct Atom { first: String, second: String } impl Drop for (Atom,) { fn drop(&mut self) {} }",
            "struct Atom { first: String, second: String } mod child;",
            "struct Atom { first: String, second: String } pub type Alias = Atom;",
            "type Atom = String; type Atom = u32;",
            "type Atom<T> = String;",
            "#[cfg_attr(feature = \"x\", allow(dead_code))] type Atom = String;",
        ] {
            with_effects(declaration, |effects| {
                assert_eq!(effects.kind(&syn::parse_quote!(Atom), 0), None, "{declaration}");
            });
        }
        with_effects(KEYS, |effects| {
            for ty in [
                "&Atom",
                "::Atom",
                "module::Atom",
                "<Atom>::Atom",
                "Atom<u32>",
                "Option<>",
                "Option<u32, bool>",
                "Vec<Atom>",
            ] {
                assert_eq!(effects.kind(&syn::parse_str(ty).unwrap(), 0), None, "{ty}");
            }
            let row = Kind::Record("Atom".into());
            assert_eq!(effects.projection(&syn::parse_quote!(row.first), "row", &row), Some(Kind::String));
            assert_eq!(
                effects.projection(
                    &syn::parse_quote!(
                        #[custom]
                        row.first
                    ),
                    "row",
                    &row
                ),
                None
            );
            let mut attributed: Type = syn::parse_quote!(Atom);
            let Type::Path(path) = &mut attributed else { unreachable!() };
            path.attrs.push(syn::parse_quote!(#[custom]));
            assert_eq!(effects.kind(&attributed, 0), None);
            assert_eq!(
                effects.clone_projection(&syn::parse_quote!(row.first.clone()), "row", &row),
                Some(Kind::String)
            );
            for expression in [
                "other.first.clone()",
                "row.missing.clone()",
                "row.first.inspect()",
                "#[custom] row.first.clone()",
            ] {
                assert_eq!(
                    effects.clone_projection(&syn::parse_str(expression).unwrap(), "row", &row),
                    None,
                    "{expression}"
                );
            }
        });
    }

    #[test]
    fn record_effect_traversal_budgets_bound_nested_aliases_cycles_and_wide_records() {
        with_effects("", |effects| {
            effects.remaining = 2;
            for _ in 0..2 {
                assert_eq!(effects.kind(&syn::parse_quote!(u32), 0), Some(Kind::Scalar("u32".into())));
            }
            assert_eq!(effects.kind(&syn::parse_quote!(u32), 0), None);
        });
        for (levels, accepted) in [(11, true), (12, false)] {
            let mut source = "type A0 = u32;".to_owned();
            for level in 1..=levels {
                write!(source, "type A{level} = A{};", level - 1).unwrap();
            }
            with_effects(&source, |effects| {
                assert_eq!(effects.kind(&syn::parse_str(&format!("A{levels}")).unwrap(), 0).is_some(), accepted);
            });
        }
        for source in [
            "type Cycle = Cycle;",
            "type Cycle = Other; type Other = Cycle;",
            "struct Cycle { next: Option<Cycle> }",
        ] {
            with_effects(source, |effects| assert_eq!(effects.kind(&syn::parse_quote!(Cycle), 0), None));
        }
        for (width, accepted) in [(32, true), (33, false)] {
            let fields = (0..width).map(|index| format!("f{index}: u32")).collect::<Vec<_>>().join(",");
            with_effects(&format!("struct Wide {{ {fields} }}"), |effects| {
                assert_eq!(effects.kind(&syn::parse_quote!(Wide), 0).is_some(), accepted);
            });
        }
        with_effects("struct Row { value: u32 }", |effects| {
            effects.remaining = 1;
            assert_eq!(
                effects.kind(&syn::parse_quote!(Row), 0),
                None,
                "valid fields still require traversal fuel"
            );
        });
    }

    #[test]
    fn record_field_and_option_nesting_share_the_recursive_type_envelope() {
        for (levels, accepted) in [(12, true), (13, false)] {
            let mut source = "struct R0 { leaf: u32 }".to_owned();
            for level in 1..levels {
                write!(source, "struct R{level} {{ value: R{} }}", level - 1).unwrap();
            }
            with_effects(&source, |effects| {
                assert_eq!(
                    effects.kind(&syn::parse_str(&format!("R{}", levels - 1)).unwrap(), 0).is_some(),
                    accepted,
                    "record layers {levels}"
                );
            });
            let ty = format!("{}u32{}", "Option<".repeat(levels), ">".repeat(levels));
            with_effects("", |effects| {
                assert_eq!(effects.kind(&syn::parse_str(&ty).unwrap(), 0).is_some(), accepted, "{ty}");
            });
        }
    }

    #[test]
    fn record_ordering_keys_resolve_aliases_at_the_recursive_type_envelope() {
        let mut aliases = "type A0 = Atom;".to_owned();
        for level in 1..11 {
            write!(aliases, "type A{level} = A{};", level - 1).unwrap();
        }
        let source = format!("{} {aliases}", KEYS.replace("row: &Atom", "row: &A10"));
        with_effects(&source, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &Kind::Record("Atom".into())),
                Some(vec!["first".into(), "second".into()])
            );
        });
    }

    #[test]
    fn record_ordering_keys_require_exact_source_backed_pure_string_projections() {
        let row = Kind::Record("Atom".into());
        with_effects(KEYS, |effects| {
            assert_eq!(
                effects.key(&syn::parse_quote!(key(row)), "row", &row),
                Some(vec!["first".into(), "second".into()])
            );
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| key(left).cmp(&key(right))), &row),
                Some(())
            );
            for expression in [
                "|left, right| key(right).cmp(&key(left))",
                "|left, right| left(left).cmp(&key(right))",
                "|left, right| key(left).cmp(&mut key(right))",
                "|left, right| key(left).cmp(&key(left))",
            ] {
                assert_eq!(effects.comparator(&syn::parse_str(expression).unwrap(), &row), None, "{expression}");
            }
            for expression in ["key(other)", "key(row, other)", "missing(row)", "#[custom] key(row)"] {
                assert_eq!(effects.key(&syn::parse_str(expression).unwrap(), "row", &row), None, "{expression}");
            }
            let _shadow = effects.shadows.insert("key".into());
            assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &row), None);
        });
        for (from, to) in [
            ("fn key(", "#[cfg(feature = \"x\")] fn key("),
            ("fn key(", "#[cfg_attr(feature = \"x\", allow(dead_code))] fn key("),
            ("fn key(", "async fn key("),
            ("fn key(", "unsafe fn key("),
            ("fn key(", "extern \"C\" fn key("),
            ("fn key(", "fn key<T>("),
            ("row: &Atom", "row: &mut Atom"),
            ("row: &Atom", "mut row: &Atom"),
            ("row: &Atom", "ref row: &Atom"),
            ("row: &Atom", "row @ _: &Atom"),
            ("row: &Atom", "#[custom] row: &Atom"),
            ("row: &Atom", "row: &Other"),
            ("row: &Atom", "row: &Atom, other: &Atom"),
            ("(&str, &str)", "(&mut str, &str)"),
            ("(&str, &str)", "(&String, &str)"),
            ("(&str, &str)", "(&str,)"),
            ("(row.first.as_str(), row.second.as_str())", "(row.first.as_str(),)"),
            ("(row.first.as_str(), row.second.as_str())", "()"),
            (
                "(row.first.as_str(), row.second.as_str())",
                "#[custom] (row.first.as_str(), row.second.as_str())",
            ),
            (
                "(row.first.as_str(), row.second.as_str())",
                "{ observe(); (row.first.as_str(), row.second.as_str()) }",
            ),
            ("row.first.as_str()", "other.first.as_str()"),
            ("row.first.as_str()", "row.missing.as_str()"),
            ("row.first.as_str()", "row.first.inspect()"),
            ("first: String", "first: u32"),
        ] {
            let source = KEYS.replace(from, to);
            with_effects(&source, |effects| {
                assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &row), None, "{to}");
            });
        }
        let source = format!(
            "{KEYS}
            fn other(row: &Atom) -> (&str, &str) {{ (row.second.as_str(), row.first.as_str()) }}"
        );
        with_effects(&source, |effects| {
            assert_eq!(
                effects.comparator(&syn::parse_quote!(|left, right| key(left).cmp(&other(right))), &row),
                None
            );
        });
        let file = syn::parse_file(KEYS).unwrap();
        let sources = Namespaces::of(
            &syn::parse_file(&KEYS.replace("row.first.as_str()", "row.second.as_str()")).unwrap(),
            &CfgSet::unconditional(),
        );
        let mut effects = Effects {
            file: &file,
            sources: &sources,
            source: "",
            inline: &[],
            shadows: HashSet::default(),
            remaining: 512,
        };
        assert_eq!(
            effects.key(&syn::parse_quote!(key(row)), "row", &row),
            None,
            "a cached different body is not declaration evidence"
        );
        for (width, accepted) in [(16, true), (17, false)] {
            let outputs = vec!["&str"; width].join(",");
            let projections = vec!["row.first.as_str()"; width].join(",");
            let source = format!("struct Atom {{ first: String }} fn key(row: &Atom) -> ({outputs}) {{ ({projections}) }}");
            with_effects(&source, |effects| {
                assert_eq!(effects.key(&syn::parse_quote!(key(row)), "row", &row).is_some(), accepted);
            });
        }
    }

    #[test]
    fn record_callbacks_and_collection_adapters_reject_capture_metadata_and_wrong_arity() {
        for expression in ["|row| row.first", "(|row| row.first)", "{ |row| row.first }"] {
            // Only transparent parenthesized/grouped expressions, not blocks, are unwrapped.
            assert_eq!(
                closure(&syn::parse_str(expression).unwrap(), 1).is_some(),
                !expression.starts_with('{')
            );
        }
        for expression in [
            "move |row| row.first",
            "async |row| row.first",
            "const |row| row.first",
            "for<'a> |row| row.first",
            "|mut row| row.first",
            "|ref row| row.first",
            "|row @ _| row.first",
            "|(row,)| row.first",
            "|row: Atom| row.first",
            "|row, row| row.first",
            "|row| -> String { row.first }",
            "#[custom] |row| row.first",
        ] {
            assert!(closure(&syn::parse_str(expression).unwrap(), 1).is_none(), "{expression}");
        }
        assert!(closure(&syn::parse_quote!(|left, right| left), 2).is_some());
        assert!(closure(&syn::parse_quote!(|row, row| row), 2).is_none());
        assert!(reference(&syn::parse_quote!(&row)).is_some());
        assert!(reference(&syn::parse_quote!(&mut row)).is_none());
        assert!(
            reference(&syn::parse_quote!(
                #[custom]
                &row
            ))
            .is_none()
        );
        assert!(reference(&syn::parse_quote!(row)).is_none());
        for expression in [
            "group.iter().map(|row| row.first).collect()",
            "group.iter().map(|row| row.first).collect::<Vec<_>>()",
        ] {
            assert!(collected(&syn::parse_str(expression).unwrap()).is_some(), "{expression}");
        }
        for expression in [
            "group.iter().collect::<Vec<_>>()",
            "group.iter().map().collect()",
            "group.iter().map(|row| row.first, other).collect()",
            "group.iter().map::<_, _>(|row| row.first).collect()",
            "group.iter().map(|row| row.first).collect::<Other>()",
            "group.iter().map(|row| row.first).collect::<module::Vec<_>>()",
            "group.iter().map(|row| row.first).collect::<Vec<_>, Other>()",
            "(#[custom] group.iter().map(|row| row.first)).collect()",
        ] {
            assert!(collected(&syn::parse_str(expression).unwrap()).is_none(), "{expression}");
        }
        assert!(expect(&syn::parse_quote!(index.expect("membership"))).is_some());
        assert!(expect(&syn::parse_quote!(index.expect(message))).is_none());
        assert!(expect(&syn::parse_quote!(index.unwrap())).is_none());
    }

    #[test]
    fn record_field_attributes_accept_only_nonexpanding_wire_metadata() {
        for (attributes, accepted) in [
            ("", true),
            ("#[doc = \"field\"]", true),
            ("#[serde(default, skip_serializing_if = \"Option::is_none\")]", true),
            ("#[serde(skip_serializing_if = 1)]", false),
            ("#[serde(default = \"make\")]", false),
            ("#[serde(default, rename = \"other\")]", false),
            ("#[serde(default, with(codec))]", false),
            ("#[serde(default)] #[custom]", false),
            ("#[cfg(feature = \"x\")]", false),
            ("#[cfg_attr(feature = \"x\", doc = \"field\")]", false),
        ] {
            let file = syn::parse_file(&format!("struct Row {{ {attributes} value: String }}")).unwrap();
            let schema = record(&file, "Row").unwrap();
            assert_eq!(
                field_attributes(&schema.fields.iter().next().unwrap().attrs),
                accepted,
                "{attributes}"
            );
        }
    }

    #[test]
    fn record_dispatch_closure_checks_every_import_and_every_trait_method() {
        for (source, accepted) in [
            ("", true),
            ("trait Harmless { fn inspect(&self); fn other(&self); }", true),
            ("trait Custom { fn inspect(&self); fn cmp(&self); }", false),
            ("trait Custom { fn inspect(&self); fn as_ref(&self); }", false),
            ("trait Custom { fn inspect(&self); fn try_from(&self); }", false),
            ("use camino::Utf8PathBuf;", true),
            ("use camino::Utf8PathBuf as Path;", true),
            ("use camino::{Utf8PathBuf as Path, Utf8PathBuf as Other};", true),
            ("use camino::Unknown;", false),
            ("use camino::Unknown as Other;", false),
            ("use camino::{Utf8PathBuf, Unknown};", false),
            ("use camino::{Utf8PathBuf, *};", false),
        ] {
            let file = syn::parse_file(source).unwrap();
            let mut sources = Namespaces::of(&file, &CfgSet::unconditional());
            sources.dependencies(&[crate::ops::collect::ResourceDependency {
                crate_root: String::new(),
                import_name: "camino".into(),
                package_id: "controlled registry identity".into(),
                package_name: "camino".into(),
                version: "1.2.5".into(),
                source: "registry+https://github.com/rust-lang/crates.io-index".into(),
                features: Vec::new(),
                registry_closure: true,
                library_root: None,
            }]);
            assert_eq!(closed_dispatch(&file, &sources, "", &[]), accepted, "{source}");
        }
    }

    #[test]
    fn record_custom_dispatch_follows_aliases_until_a_closed_fixed_point() {
        for (source, conflict) in [
            ("struct Row; struct Other; impl Other { fn inspect(&self) {} }", false),
            ("struct Row; impl Row { fn inspect(&self) {} }", true),
            (
                "struct Row; type A = Row; type B = A; impl Drop for B { fn drop(&mut self) {} }",
                true,
            ),
            (
                "struct Row; type A = self::Row; impl Drop for self::A { fn drop(&mut self) {} }",
                true,
            ),
            ("struct Row; impl<T> Drop for T { fn drop(&mut self) {} }", true),
            ("struct Row; impl Drop for (Row,) { fn drop(&mut self) {} }", true),
        ] {
            assert_eq!(conflicting_impl(&syn::parse_file(source).unwrap(), "Row"), conflict, "{source}");
        }
        for (aliases, conflict) in [(15, false), (16, true)] {
            let mut source = "struct Row;".to_owned();
            // Reverse declaration order forces each pass to discover one more alias.
            for index in (0..aliases).rev() {
                let target = if index == 0 { "Row".to_owned() } else { format!("A{}", index - 1) };
                write!(source, "type A{index} = {target};").unwrap();
            }
            assert_eq!(conflicting_impl(&syn::parse_file(&source).unwrap(), "Row"), conflict);
        }
    }

    #[test]
    fn record_vector_evidence_rejects_custom_types_wrong_elements_and_untyped_patterns() {
        with_effects(KEYS, |effects| {
            let atom = Kind::Record("Atom".into());
            assert_eq!(vector_element(effects, &syn::parse_quote!(Vec<Atom>), &atom), Some(()));
            assert_eq!(
                local_vector(effects, &syn::parse_quote!(let rows: Vec<Atom> = input;), &atom),
                Some(())
            );
            assert_eq!(local_vector(effects, &syn::parse_quote!(let rows = input;), &atom), Some(()));
            for ty in [
                "Custom<Atom>",
                "Vec<u32>",
                "Vec<Atom, Other>",
                "Vec<>",
                "::Vec<Atom>",
                "module::Vec<Atom>",
                "<Atom>::Vec<Atom>",
                "&Vec<Atom>",
            ] {
                assert_eq!(vector_element(effects, &syn::parse_str(ty).unwrap(), &atom), None, "{ty}");
            }
            assert_eq!(local_vector(effects, &syn::parse_quote!(let (rows,) = input;), &atom), None);
            assert_eq!(local_vector(effects, &syn::parse_quote!(let rows: Vec<u32> = input;), &atom), None);
            assert_eq!(local_vector(effects, &syn::parse_quote!(rows = input;), &atom), None);
            let mut attributed: Type = syn::parse_quote!(Vec<Atom>);
            let Type::Path(path) = &mut attributed else { unreachable!() };
            path.attrs.push(syn::parse_quote!(#[custom]));
            assert_eq!(vector_element(effects, &attributed, &atom), None);
        });
    }

    #[test]
    fn record_indexed_options_require_the_same_sorted_deduplicated_membership_and_checked_index() {
        let source = format!("{KEYS} struct Row {{ killer: Option<Atom>, other: Option<Atom> }}");
        let expression = "row.killer.as_ref().map(|value| {
            let index = pool.iter().position(|known| known == value).expect(\"member\");
            u32::try_from(index).expect(\"bounded\")
        })";
        let input = Kind::Record("Row".into());
        let expected = Kind::Option(Box::new(Kind::Scalar("u32".into())));
        let pool = || Pool {
            kind: Kind::Record("Atom".into()),
            field: "killer".into(),
            sorted: true,
            deduplicated: true,
        };
        with_effects(&source, |effects| {
            let pools = HashMap::from_iter([("pool".into(), pool())]);
            assert_eq!(
                indexed_option(effects, &syn::parse_str(expression).unwrap(), "row", &input, &expected, &pools),
                Some(())
            );
        });
        for (from, to) in [
            ("row.killer", "other.killer"),
            ("row.killer", "row.other"),
            ("as_ref()", "as_ref(other)"),
            ("map(|value|", "map(move |value|"),
            ("map(|value|", "map(|mut value|"),
            ("map(|value|", "map(|pool|"),
            ("map(|value|", "map(|row|"),
            ("let index", "let value"),
            ("let index", "let row"),
            ("let index", "let pool"),
            ("pool.iter()", "missing.iter()"),
            ("pool.iter()", "pool.other()"),
            ("known == value", "known != value"),
            ("known == value", "value == known"),
            ("known == value", "known == other"),
            ("known == value", "#[custom] known == value"),
            ("|known| known == value", "|value| value == value"),
            ("u32::try_from(index)", "u32::try_from(other)"),
            ("u32::try_from(index)", "u32::try_from(index, other)"),
            ("u32::try_from(index)", "u32::from(index)"),
            ("u32::try_from(index)", "::u32::try_from(index)"),
            ("u32::try_from(index)", "u32::try_from::<usize>(index)"),
            ("u32::try_from(index)", "u8::try_from(index)"),
            ("u32::try_from(index)", "#[custom] u32::try_from(index)"),
            ("expect(\"member\")", "expect(message)"),
            ("expect(\"bounded\")", "expect(message)"),
            ("let index =", "observe(); let index ="),
            (
                "u32::try_from(index).expect(\"bounded\")",
                "u32::try_from(index).expect(\"bounded\");",
            ),
        ] {
            with_effects(&source, |effects| {
                let pools = HashMap::from_iter([("pool".into(), pool())]);
                let changed = syn::parse_str(&expression.replace(from, to)).unwrap();
                assert_eq!(indexed_option(effects, &changed, "row", &input, &expected, &pools), None, "{to}");
            });
        }
        for fault in ["unsorted", "undeduplicated", "different field", "different kind", "missing"] {
            with_effects(&source, |effects| {
                let mut changed = pool();
                match fault {
                    "unsorted" => changed.sorted = false,
                    "undeduplicated" => changed.deduplicated = false,
                    "different field" => changed.field = "other".into(),
                    "different kind" => changed.kind = Kind::String,
                    "missing" => {}
                    _ => unreachable!(),
                }
                let pools = if fault == "missing" {
                    HashMap::default()
                } else {
                    HashMap::from_iter([("pool".into(), changed)])
                };
                assert_eq!(
                    indexed_option(effects, &syn::parse_str(expression).unwrap(), "row", &input, &expected, &pools),
                    None,
                    "{fault}"
                );
            });
        }
        with_effects(&source, |effects| {
            let pools = HashMap::from_iter([("pool".into(), pool())]);
            assert_eq!(
                indexed_option(effects, &syn::parse_str(expression).unwrap(), "row", &input, &Kind::String, &pools),
                None
            );
            assert_eq!(
                indexed_option(
                    effects,
                    &syn::parse_str(expression).unwrap(),
                    "row",
                    &input,
                    &Kind::Option(Box::new(Kind::Scalar("usize".into()))),
                    &pools
                ),
                None
            );
        });
    }
}
