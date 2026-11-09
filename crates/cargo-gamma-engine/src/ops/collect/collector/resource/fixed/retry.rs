// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolved population-cache prefixes, not whole retry-loop exclusions.
//!
//! The caller must resolve the call and evaluate its arguments before consulting this evidence.
//! Field paths identify the exact snapshots to supply; a hit says nothing about a cache miss,
//! the remainder of the helper, or the progress and escape effects of the surrounding loop.

use rustc_hash::FxBuildHasher;
use syn::{Expr, FnArg, Item, Pat, Stmt, Type, UnOp};

use crate::{HashMap, HashSet};

mod effects;
pub(super) mod population;
pub(in super::super::super) use effects::{CacheEntries, Effects};

/// A read-only successful prefix of a fallible local method with a mutable receiver.
///
/// Construction requires a complete, closed declaration scope. Nonstandard imports, conditional
/// definitions, aliases, and generated items withhold evidence.
pub(in super::super::super) struct CachePrefix {
    population: String,
    ordinal: String,
    cache: Vec<String>,
    empty_guard: Option<Vec<String>>,
    parameters: Vec<String>,
}

impl CachePrefix {
    /// Certifies only the prefix preceding the first cache-hit return.
    ///
    /// `owner` and `method` must denote the already resolved inherent call target. This bounded
    /// closed-scope entry point deliberately cannot certify the unresolved campaign fixture.
    #[expect(
        clippy::too_many_lines,
        reason = "checks the complete resolved cache-hit prefix before recording its read paths"
    )]
    pub(in super::super::super) fn resolve(file: &syn::File, owner: &str, method: &str) -> Option<Self> {
        let scope = Scope::new(file)?;
        let function = scope.methods.get(&(owner.to_owned(), method.to_owned()))?;
        if !function.attrs.is_empty()
            || !plain_signature(&function.sig)
            || !fallible_verdict(&function.sig.output)
            || !shadow_free(&function.block)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut parameters = HashMap::with_capacity_and_hasher(function.sig.inputs.len(), FxBuildHasher);
        let mut parameter_order = Vec::with_capacity(function.sig.inputs.len());
        let mut receiver = false;
        for parameter in &function.sig.inputs {
            match parameter {
                FnArg::Receiver(argument)
                    if argument.attrs.is_empty() && matches!(argument.kind, syn::ReceiverKind::Reference(_, _, Some(_))) =>
                {
                    if
                    // #[gamma::skip(cond.always_false, reason = "Supported Rust signatures permit at most one receiver, and typed arguments never set this flag.", tag = "equivalent")]
                    receiver {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    receiver = true;
                }
                FnArg::Typed(argument) if argument.attrs.is_empty() => {
                    if !matches!(&*argument.ty, Type::Reference(_))
                        && !primitive(&argument.ty, "u32")
                        && !primitive(&argument.ty, "bool")
                        && !scope.copy_record(&argument.ty)
                    {
                        // Unused owned arguments can still run a destructor at a cache-hit return.
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    let name = ident(&argument.pat)?;
                    parameter_order.push(name.clone());
                    if matches!(name.as_str(), "Ok" | "Some" | "None") || parameters.insert(name, &*argument.ty).is_some() {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                }
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                _ => return None,
            }
        }
        if !receiver {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut statements = function.block.stmts.iter();
        let mut statement = statements.next()?;
        let empty_guard = if let Stmt::Expr(Expr::If(branch), _) = statement {
            if !branch.attrs.is_empty() || branch.else_branch.is_some() {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Expr::Unary(negation) = &*branch.cond else { return None };
            if !negation.attrs.is_empty() || !matches!(negation.op, UnOp::Not(_)) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            let Expr::MethodCall(empty) = &*negation.expr else { return None };
            if !method_call(empty, "is_empty", 0) || !returns(&branch.then_branch, &syn::parse_quote!(Ok(None))) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let path = place(&empty.receiver)?;
            let ty = scope.field_type(&parameters, &path)?;
            if !standard_map(ty).is_some_and(|(key, _)| primitive(key, "u32")) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            statement = statements.next()?;
            Some(path)
        } else {
            None
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Stmt::Local(key) = statement else { return None };
        if !key.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let key_name = ident(&key.pat)?;
        if
        // #[gamma::skip(logical.or_remove_right, reason = "The preceding shadow_free traversal independently rejects bindings named Ok, Some or None.", tag = "equivalent")]
        parameters.contains_key(&key_name) || matches!(key_name.as_str(), "Ok" | "Some" | "None") {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let initialization = key.init.as_ref()?;
        if initialization.diverge.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Call(call) = &*initialization.expr else { return None };
        if !call.attrs.is_empty() || call.args.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Path(path) = &*call.func else { return None };
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "A qualified trait call cannot resolve to the closed local inherent method table; inherent qself retains a leading colon rejected here.", tag = "equivalent")]
            !path.attrs.is_empty() || path.qself.is_some()
        ) || path.path.leading_colon.is_some()
            || path.path.segments.len() != 2
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let parts = path.path.segments.iter().collect::<Vec<_>>();
        if parts.iter().any(|part| !matches!(part.arguments, syn::PathArguments::None)) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let key_owner = parts[0].ident.to_string();
        let key_method = parts[1].ident.to_string();
        let key_function = scope.methods.get(&(key_owner, key_method))?;
        let population = bare(&call.args[0])?;
        let element = population_element(parameters.get(&population)?)?;
        let ordinal = scope.key_projection(key_function, &element)?;

        let Stmt::Expr(Expr::If(branch), _) = statements.next()? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !branch.attrs.is_empty() || branch.else_branch.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Let(binding) = &*branch.cond else { return None };
        if !binding.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Pat::TupleStruct(pattern) = &*binding.pat else { return None };
        if (
            // #[gamma::skip(logical.or_remove_left, reason = "Pinned Syn parses if-let tuple-struct patterns with empty attributes; expression attributes belong to ExprLet instead.", tag = "equivalent")]
            !pattern.attrs.is_empty() || !pattern.path.is_ident("Some")
        ) || pattern.elems.len() != 1
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let cached = ident(&pattern.elems[0])?;
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::MethodCall(get) = &*binding.expr else { return None };
        if !method_call(get, "get", 1) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Reference(reference) = &get.args[0] else { return None };
        if !reference.attrs.is_empty() || reference.mutability.is_some() || bare(&reference.expr)? != key_name {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let cache = place(&get.receiver)?;
        let (key_type, value_type) = standard_map(scope.field_type(&parameters, &cache)?)?;
        if !vector_u32(key_type) || !option_bool(value_type) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, reason = "cached is a parsed identifier, including any raw prefix, so this interpolation always parses.", tag = "equivalent")]
        let returned: Expr = syn::parse_str(&format!("Ok(*{cached})")).ok()?;
        if !returns(&branch.then_branch, &returned) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        Some(Self {
            population,
            ordinal,
            cache,
            empty_guard,
            parameters: parameter_order,
        })
    }

    pub(in super::super::super) fn population(&self) -> (&str, &str) {
        (&self.population, &self.ordinal)
    }

    pub(in super::super::super) fn cache(&self) -> &[String] {
        &self.cache
    }

    pub(in super::super::super) fn empty_guard(&self) -> Option<&[String]> {
        self.empty_guard.as_deref()
    }

    /// Reads a bounded exact snapshot without executing the unknown miss suffix.
    ///
    /// Ordinals must be the resolved primitive field of each element of `population()`, in order.
    /// Entries and the optional emptiness fact must come from `cache()` and `empty_guard()`.
    /// Outer `None` means unknown (including a miss or exhausted work); `Some(None)` is a
    /// definitive cached inconclusive result, not a miss and not a successful reduction.
    #[expect(
        clippy::option_option,
        reason = "outer None withholds evidence; inner None is a definitive cached inconclusive verdict"
    )]
    pub(in super::super::super) fn hit(
        &self,
        ordinals: &[u32],
        entries: &[(Vec<u32>, Option<bool>)],
        guard_empty: Option<bool>,
        budget: &mut usize,
    ) -> Option<Option<bool>> {
        if ordinals.len() > 32 || entries.len() > 32 || entries.iter().any(|(key, _)| key.len() > 32) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding length guards bound the total by 33 + 32 * 33, which cannot overflow usize.", tag = "equivalent")]
        let work = entries
            .iter()
            .try_fold(ordinals.len() + 1, |work, (key, _)| work.checked_add(key.len() + 1))?;
        *budget = budget.checked_sub(work)?;
        if self.empty_guard.is_some() {
            if !guard_empty? {
                return Some(None);
            }
        } else if guard_empty.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut key = ordinals.to_vec();
        key.sort_unstable();
        // #[gamma::skip(expr.increment, tag = "equivalent", reason = "Increasing this HashSet preallocation does not change insertion results or duplicate detection.")]
        let mut seen = HashSet::with_capacity_and_hasher(entries.len(), FxBuildHasher);
        let mut result = None;
        for (known, verdict) in entries {
            // HashMap snapshots cannot contain two entries for an equal key.
            if !seen.insert(known) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            if known == &key {
                result = Some(*verdict);
            }
        }
        result
    }
}

struct Scope<'a> {
    records: HashMap<String, &'a syn::ItemStruct>,
    methods: HashMap<(String, String), &'a syn::ImplItemFn>,
}

impl<'a> Scope<'a> {
    fn new(file: &'a syn::File) -> Option<Self> {
        if !file.attrs.is_empty() || file.items.len() > 64 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut scope = Self {
            records: HashMap::default(),
            methods: HashMap::default(),
        };
        let mut cost = 0_usize;
        for item in &file.items {
            use syn::spanned::Spanned as _;
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Prior cost is at most 65536 and a parsed item span is bounded by the source String length, at most isize::MAX.", tag = "equivalent")]
            cost = cost.checked_add(item.span().byte_range().len())?;
            if cost > 65536 {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            let unannotated_inherent_impl =
                matches!(item, Item::Impl(implementation) if implementation.attrs.is_empty() && implementation.trait_.is_none());
            match item {
                Item::Struct(record)
                    if record_attributes(&record.attrs)
                        && record
                            .generics
                            .params
                            .iter()
                            .all(|parameter| matches!(parameter, syn::GenericParam::Lifetime(_)))
                        && record.generics.where_clause.is_none()
                        && record.fields.iter().all(|field| field.attrs.is_empty()) =>
                {
                    let name = record.ident.to_string();
                    if matches!(
                        name.as_str(),
                        "std"
                            | "core"
                            | "Vec"
                            | "Option"
                            | "Result"
                            | "Some"
                            | "None"
                            | "Ok"
                            | "u32"
                            | "bool"
                            | "usize"
                            | "Copy"
                            | "Clone"
                            | "HashSet"
                    ) || scope.records.insert(name, record).is_some()
                    {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                }
                Item::Impl(implementation)
                    if (
                        // #[gamma::skip(logical.and_remove_right, reason = "Supported unsafe/default/negative implementations are trait implementations, already excluded by unannotated_inherent_impl.", tag = "equivalent")]
                        unannotated_inherent_impl && implementation.unsafety.is_none() && implementation.modifiers.require_empty().is_ok()
                    ) && implementation.generics.params.is_empty()
                        && implementation.generics.where_clause.is_none() =>
                {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    let Type::Path(owner) = &*implementation.self_ty else { return None };
                    if
                    // #[gamma::skip(cond.always_false, reason = "The following get_ident independently rejects qualified paths through its singleton/no-leading-colon requirements.", tag = "equivalent")]
                    owner.qself.is_some() {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    let owner = owner.path.get_ident()?.to_string();
                    if
                    // #[gamma::skip(cond.always_false, literal.str_to_empty, reason = "Final owner validation requires a local struct; neither Self nor an empty string can name a parsed struct.", tag = "equivalent")]
                    owner == "Self" {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        return None;
                    }
                    for item in &implementation.items {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        let syn::ImplItem::Fn(function) = item else { return None };
                        if scope.methods.len() >= 64
                            || scope
                                .methods
                                .insert((owner.clone(), function.sig.ident.to_string()), function)
                                .is_some()
                        {
                            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                            return None;
                        }
                    }
                }
                Item::Fn(function)
                    if function.attrs.is_empty() && !matches!(function.sig.ident.to_string().as_str(), "Some" | "None" | "Ok") => {}
                Item::Const(constant)
                    if constant.attrs.is_empty()
                        && (primitive(&constant.ty, "usize") || primitive(&constant.ty, "u32") || primitive(&constant.ty, "bool"))
                        && !matches!(constant.ident.to_string().as_str(), "Some" | "None" | "Ok") => {}
                Item::Use(import)
                    if import.attrs.is_empty()
                        && import.leading_colon.is_none()
                        && import.tree == syn::parse_quote!(std::collections::HashSet) => {}
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                _ => return None,
            }
        }
        scope
            .methods
            .keys()
            .all(|(owner, _)| scope.records.contains_key(owner))
            .then_some(scope)
    }

    fn copy_record(&self, ty: &Type) -> bool {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
        let Type::Path(path) = ty else { return false };
        if (
            // #[gamma::skip(logical.or_remove_left, reason = "The retained leading-colon and singleton checks reject parsed inherent and trait-qualified self paths respectively.", tag = "equivalent")]
            path.qself.is_some() || path.path.leading_colon.is_some()
        ) || path.path.segments.len() != 1
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false;
        }
        // #[gamma::skip(iter.first_to_last, reason = "The preceding exact singleton check makes first and last identical.", tag = "equivalent")]
        let Some(segment) = path.path.segments.first() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
            return false; // #[gamma::skip(literal.bool_flip, reason = "The preceding exact singleton check guarantees this segment exists.", tag = "equivalent")]
        };
        self.records.get(&segment.ident.to_string()).is_some_and(|record| {
            record.attrs.iter().any(|attribute| {
                // #[gamma::skip(logical.and_remove_left, reason = "Scope only retains record attributes already certified as derive(Copy/Clone) by record_attributes.", tag = "equivalent")]
                attribute.path().is_ident("derive")
                    && attribute
                        .parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                        .is_ok_and(|paths| paths.iter().any(|path| path.is_ident("Copy")))
            })
        })
    }

    fn field_type(&self, parameters: &HashMap<String, &Type>, path: &[String]) -> Option<&'a Type> {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let [base, field] = path else { return None };
        let Type::Reference(reference) = *parameters.get(base)? else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Type::Path(record) = &*reference.elem else { return None };
        if
        // #[gamma::skip(cond.always_false, reason = "The following path.get_ident independently rejects parsed qualified-self paths.", tag = "equivalent")]
        record.qself.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }

        let record = self.records.get(&record.path.get_ident()?.to_string())?;
        record
            .fields
            .iter()
            .find(|candidate| candidate.ident.as_ref().is_some_and(|name| name == field))
            .map(|field| &field.ty)
    }

    fn key_projection(&self, function: &syn::ImplItemFn, element: &str) -> Option<String> {
        if !function.attrs.is_empty() || !plain_signature(&function.sig) || function.sig.inputs.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let FnArg::Typed(parameter) = &function.sig.inputs[0] else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !parameter.attrs.is_empty() || population_element(&parameter.ty)? != element {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let parameter_name = ident(&parameter.pat)?;
        let syn::ReturnType::Type(_, output) = &function.sig.output else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !vector_u32(output) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let [
            Stmt::Local(local),
            Stmt::Expr(Expr::MethodCall(sort), Some(_)),
            Stmt::Expr(tail, None),
        ] = function.block.stmts.as_slice()
        else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !local.attrs.is_empty() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let name = ident(&local.pat)?;
        if name == parameter_name || !method_call(sort, "sort_unstable", 0) || bare(&sort.receiver)? != name || bare(tail)? != name {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The exact remaining body immediately sorts and returns this local; without initialization that supported Rust body cannot type-check.", tag = "equivalent")]
        let initialization = local.init.as_ref()?;
        if initialization.diverge.is_some() {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let Expr::MethodCall(collect) = &*initialization.expr else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if (
            // #[gamma::skip(logical.or_remove_right, reason = "With the retained zero-argument standard iterator pipeline and Vec output, only collect produces the required supported return value.", tag = "equivalent")]
            !collect.attrs.is_empty() || collect.method != "collect"
        ) || !collect.args.is_empty()
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        if let Some(types) = &collect.turbofish {
            let [syn::GenericArgument::Type(ty)] = types.args.iter().collect::<Vec<_>>().as_slice() else {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            };
            let arguments = arguments(ty, "Vec", "std::vec::Vec")?;
            if !matches!(arguments.as_slice(), [element] if matches!(element, Type::Infer(_)) || primitive(element, "u32")) {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::MethodCall(map) = &*collect.receiver else { return None };
        if !method_call(map, "map", 1) {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::MethodCall(iter) = &*map.receiver else { return None };
        if !method_call(iter, "iter", 0) || bare(&iter.receiver)? != parameter_name {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::Closure(projection) = &map.args[0] else { return None };
        let unsupported_projection = (
            // #[gamma::skip(logical.or_remove_right, reason = "The supported standard map pipeline requires a unary callback.", tag = "equivalent")]
            !projection.attrs.is_empty() || projection.inputs.len() != 1
        ) || projection.asyncness.is_some()
            || projection.constness.is_some()
            || projection.capture.is_some();
        if
        // #[gamma::skip(logical.or_remove_right, reason = "Explicit binders require typed parameters/output rejected below; explicit output requires a block rejected below, and pinned Syn has empty ClosureModifiers.", tag = "equivalent")]
        unsupported_projection
            || projection.modifiers.require_empty().is_err()
            || projection.lifetimes.is_some()
            || !matches!(projection.output, syn::ReturnType::Default)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let projection_name = ident(&projection.inputs[0])?;
        let [base, field]: [String; 2] = place(&projection.body)?.try_into().ok()?;
        if base != projection_name {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let record = self.records.get(element)?;
        record
            .fields
            .iter()
            .any(|candidate| candidate.ident.as_ref().is_some_and(|name| name == &field) && primitive(&candidate.ty, "u32"))
            .then_some(field)
    }
}

fn record_attributes(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        attribute.path().is_ident("derive")
            && attribute
                .parse_args_with(syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .is_ok_and(|paths| paths.iter().all(|path| path.is_ident("Copy") || path.is_ident("Clone")))
    })
}

/// Local items and item-producing macros are hoisted even when written after a warm return.
pub(in super::super) fn shadow_free(block: &syn::Block) -> bool {
    struct Check {
        valid: bool,
        budget: usize,
    }
    impl<'ast> syn::visit::Visit<'ast> for Check {
        fn visit_expr(&mut self, expression: &'ast Expr) {
            if self.budget == 0 {
                self.valid = false;
                return;
            }
            self.budget -= 1;
            syn::visit::visit_expr(self, expression);
        }
        fn visit_item(&mut self, _: &'ast Item) {
            self.valid = false;
        }
        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.valid = false;
        }
        fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
            if !pattern.attrs.is_empty()
                || matches!(
                    pattern.ident.to_string().as_str(),
                    "Some" | "None" | "Ok" | "std" | "Vec" | "HashSet" | "u32" | "usize" | "bool"
                )
            {
                self.valid = false;
            }
            syn::visit::visit_pat_ident(self, pattern);
        }
    }
    let mut check = Check { valid: true, budget: 2048 };
    syn::visit::Visit::visit_block(&mut check, block);
    check.valid
}

fn plain_signature(signature: &syn::Signature) -> bool {
    signature.generics.params.is_empty()
        && signature.generics.where_clause.is_none()
        && signature.asyncness.is_none()
        && (
            // #[gamma::skip(logical.and_remove_right, reason = "Supported variadic definitions require an extern ABI, already excluded by the left conjunct.", tag = "equivalent")]
            signature.abi.is_none() && signature.variadic.is_none()
        )
        && matches!(signature.safety, syn::Safety::Default)
}

fn ident(pattern: &Pat) -> Option<String> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Pat::Ident(pattern) = pattern else { return None };
    (pattern.attrs.is_empty() && pattern.by_ref.is_none() && pattern.subpat.is_none()).then(|| pattern.ident.to_string())
}

fn bare(expression: &Expr) -> Option<String> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Path(path) = expression else { return None };
    // #[gamma::skip(logical.and_remove_right, reason = "The following get_ident independently rejects parsed qself paths by their segment count or leading colon.", tag = "equivalent")]
    (path.attrs.is_empty() && path.qself.is_none())
        .then(|| path.path.get_ident().map(ToString::to_string))
        .flatten()
}

fn place(expression: &Expr) -> Option<Vec<String>> {
    if let Some(name) = bare(expression) {
        return Some(vec![name]);
    }
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Field(field) = expression else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let syn::Member::Named(name) = &field.member else { return None };
    if !field.attrs.is_empty() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let mut path = place(&field.base)?;
    if path.len() >= 2 {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    path.push(name.to_string());
    Some(path)
}

fn method_call(call: &syn::ExprMethodCall, name: &str, arity: usize) -> bool {
    call.attrs.is_empty() && call.turbofish.is_none() && call.method == name && call.args.len() == arity
}

fn returns(block: &syn::Block, value: &Expr) -> bool {
    matches!(block.stmts.as_slice(), [Stmt::Expr(Expr::Return(returned), _)]
        if returned.attrs.is_empty() && returned.expr.as_deref() == Some(value))
}

fn primitive(ty: &Type, name: &str) -> bool {
    matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident(name))
}

fn arguments<'a>(ty: &'a Type, short: &str, qualified: &str) -> Option<Vec<&'a Type>> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Path(path) = ty else { return None };
    if path.qself.is_some() || path.path.leading_colon.is_some() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let names = path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::");
    if names != short && names != qualified {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    if path
        .path
        .segments
        .iter()
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The already-matched nonempty standard type spelling guarantees at least one path segment.", tag = "equivalent")]
        .take(path.path.segments.len().checked_sub(1)?)
        .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
    {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None; // #[gamma::skip(option.none_to_some, reason = "Every caller requires one or two generic type arguments, so an empty result rejects just like None.", tag = "equivalent")]
    }
    // #[gamma::skip(try.propagate_to_unwrap, reason = "The already-matched nonempty standard type spelling guarantees a final path segment.", tag = "equivalent")]
    let syn::PathArguments::AngleBracketed(arguments) = &path.path.segments.last()?.arguments else {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    };
    arguments
        .args
        .iter()
        .map(|argument| match argument {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .collect()
}

fn vector_u32(ty: &Type) -> bool {
    arguments(ty, "Vec", "std::vec::Vec").is_some_and(|arguments| matches!(arguments.as_slice(), [ty] if primitive(ty, "u32")))
}

fn option_bool(ty: &Type) -> bool {
    arguments(ty, "Option", "std::option::Option").is_some_and(|arguments| matches!(arguments.as_slice(), [ty] if primitive(ty, "bool")))
}

fn standard_map(ty: &Type) -> Option<(&Type, &Type)> {
    // #[gamma::skip(literal.str_to_empty, reason = "These accepted spellings are identical; changing either one to an impossible empty path leaves the other exact spelling.", tag = "equivalent")]
    let arguments = arguments(ty, "std::collections::HashMap", "std::collections::HashMap")?;
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let [key, value] = arguments.as_slice() else { return None };
    Some((*key, *value))
}

fn population_element(ty: &Type) -> Option<String> {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Reference(reference) = ty else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Slice(slice) = &*reference.elem else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Reference(element) = &*slice.elem else { return None };
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Type::Path(record) = &*element.elem else { return None };
    let mutable = reference.mutability.is_some() || element.mutability.is_some();
    if
    // #[gamma::skip(logical.or_remove_right, reason = "The final get_ident independently rejects every parsed qualified-self record path.", tag = "equivalent")]
    mutable || record.qself.is_some() {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    record.path.get_ident().map(ToString::to_string)
}

fn fallible_verdict(output: &syn::ReturnType) -> bool {
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "false is already the default bool value.")]
    let syn::ReturnType::Type(_, ty) = output else { return false };
    arguments(ty, "Result", "std::result::Result")
        .is_some_and(|arguments| matches!(arguments.as_slice(), [value, error] if option_bool(value) && primitive(error, "u32")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;
