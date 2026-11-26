// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use rustc_hash::FxBuildHasher;
use syn::{Expr, FnArg, Item, Pat, Stmt, Type};

use super::{Binding, CacheEntries, HashMap, Retry};

// Conservative evaluation/storage caps, not Rust limits or measured capacities.
// Raising them admits larger entry states at greater analysis cost; lowering them
// withholds evidence sooner. Exhaustion never certifies an unknown entry state.
const EVAL_BUDGET: usize = 256;
const MAX_ELEMENTS: usize = 32;
const MAX_BINDINGS: usize = 64;
// Keep the modeled count domain below u16's full range for arithmetic headroom.
// Raising this admits more constants, still subject to checked arithmetic.
const MAX_SMALL_COUNT: u16 = 32767;

#[derive(Clone)]
enum Value {
    Count(u16),
    Ordinal(u32),
    Bool(bool),
    Record(String, HashMap<String, Self>),
    Population(Vec<u32>),
    Cache(CacheEntries),
    EmptyMap,
    Opaque,
}

pub(super) struct State<'a> {
    records: HashMap<String, &'a syn::ItemStruct>,
    types: HashMap<String, Type>,
    values: HashMap<String, Value>,
    ordinal: &'a str,
    budget: usize,
}

impl<'a> State<'a> {
    pub(super) fn new(file: &'a syn::File, owner: &str, signature: &syn::Signature, ordinal: &'a str) -> Option<Self> {
        let mut state = Self {
            records: file
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Struct(record) => Some((record.ident.to_string(), record)),
                    _ => None,
                })
                .collect(),
            types: HashMap::default(),
            values: HashMap::default(),
            ordinal,
            budget: EVAL_BUDGET,
        };
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The owner was obtained from a parsed bare impl identifier, so it always parses as a Type.", tag = "equivalent")]
        let owner: Type = syn::parse_str(owner).ok()?;
        let _old = state.types.insert("self".into(), owner);
        let _old = state.values.insert("self".into(), Value::Opaque);
        for input in &signature.inputs {
            if let FnArg::Typed(parameter) = input {
                let Pat::Ident(pattern) = &*parameter.pat else { return None };
                if (
                    // #[gamma::skip(logical.or_remove_right, reason = "Pinned Syn stores function argument attributes on PatType, leaving its simple inner PatIdent attributes empty.", tag = "equivalent")]
                    !parameter.attrs.is_empty() || !pattern.attrs.is_empty()
                ) || pattern.by_ref.is_some()
                    || pattern.subpat.is_some()
                {
                    return None;
                }
                let name = pattern.ident.to_string();
                let _old = state.types.insert(name.clone(), (*parameter.ty).clone());
                let _old = state.values.insert(name, Value::Opaque);
            }
        }
        Some(state)
    }

    pub(super) fn statement(&mut self, statement: &Stmt) -> Option<()> {
        self.budget = self.budget.checked_sub(1)?;
        match statement {
            Stmt::Local(local) if local.attrs.is_empty() => {
                let (pattern, ty) = match &local.pat {
                    Pat::Type(pattern) => (&*pattern.pat, Some(&*pattern.ty)),
                    pattern => (pattern, None),
                };
                let Pat::Ident(pattern) = pattern else { return None };
                if (
                    // #[gamma::skip(logical.or_remove_left, reason = "Pinned Syn stores parsed local attributes on Local, not its simple PatIdent.", tag = "equivalent")]
                    !pattern.attrs.is_empty() || pattern.by_ref.is_some()
                ) || pattern.subpat.is_some()
                {
                    return None;
                }
                let init = local.init.as_ref()?;
                if init.diverge.is_some() {
                    return None;
                }
                let value = self.eval(&init.expr, ty)?;
                let name = pattern.ident.to_string();
                let prefix = format!("{name}.");
                self.values.retain(|key, _| !key.starts_with(&prefix));
                let _old = self.types.remove(&name);
                if let Some(ty) = ty {
                    let _old = self.types.insert(name.clone(), ty.clone());
                }
                let _old = self.values.insert(name, value);
                Some(())
            }
            Stmt::Expr(Expr::Assign(assignment), Some(_)) if assignment.attrs.is_empty() => {
                let name = place(&assignment.left)?;
                let ty = self.ty(&name)?;
                let value = self.eval(&assignment.right, Some(&ty))?;
                if !matches!(value, Value::Count(_) | Value::Bool(_) | Value::Cache(_) | Value::EmptyMap) {
                    return None;
                }
                let _old = self.values.insert(name, value);
                Some(())
            }
            _ => None,
        }
    }

    fn ty(&self, path: &str) -> Option<Type> {
        let mut components = path.split('.');
        let mut ty = self
            .types
            .get(
                // #[gamma::skip(try.propagate_to_unwrap, reason = "str::split always yields at least one component, even for an empty string.", tag = "equivalent")]
                components.next()?,
            )?
            .clone();
        for component in components {
            if let Type::Reference(reference) = ty {
                ty = *reference.elem;
            }
            let Type::Path(record) = &ty else { return None };
            let record = self.records.get(&record.path.get_ident()?.to_string())?;
            ty = record
                .fields
                .iter()
                .find(|field| field.ident.as_ref().is_some_and(|name| name == component))?
                .ty
                .clone();
        }
        Some(ty)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the closed entry grammar shares one conservative evaluation budget"
    )]
    fn eval(&mut self, expression: &Expr, expected: Option<&Type>) -> Option<Value> {
        self.budget = self.budget.checked_sub(1)?;
        let standard_cache_constructor = matches!(expression, Expr::Call(call)
            if call.attrs.is_empty() && *call.func == syn::parse_quote!(std::collections::HashMap::from));
        match expression {
            Expr::Path(_) => self.values.get(&name(expression)?).cloned(),
            Expr::Reference(reference) if reference.attrs.is_empty() && reference.mutability.is_none() => {
                self.eval(&reference.expr, expected)
            }
            Expr::Lit(literal) if literal.attrs.is_empty() => match &literal.lit {
                syn::Lit::Bool(value) => Some(Value::Bool(value.value)),
                syn::Lit::Int(value) if expected.is_some_and(|ty| type_name(ty, "usize")) || value.suffix() == "usize" => {
                    Some(Value::Count(small(expression)?))
                }
                syn::Lit::Int(value) if matches!(value.suffix(), "" | "u32") => Some(Value::Ordinal(value.base10_parse().ok()?)),
                _ => None,
            },
            Expr::Struct(record) if record.attrs.is_empty() && record.rest.is_none() => {
                let record_name = record.path.get_ident()?.to_string();
                let declaration = *self.records.get(&record_name)?;
                if
                    // #[gamma::skip(logical.or_remove_left, reason = "A supported struct literal without rest initializes every declared field; unknown and duplicate names are independently rejected below.", tag = "equivalent")]
                    record.fields.len() != declaration.fields.len() || record.fields.len() > MAX_ELEMENTS
                {
                    return None;
                }
                let mut fields = HashMap::with_capacity_and_hasher(record.fields.len(), FxBuildHasher);
                for field in &record.fields {
                    if !field.attrs.is_empty() {
                        return None;
                    }
                    let syn::Member::Named(name) = &field.member else { return None };
                    let ty = &declaration.fields.iter().find(|field| field.ident.as_ref() == Some(name))?.ty;
                    let value = self.eval(&field.expr, Some(ty))?;
                    if fields.insert(name.to_string(), value).is_some() {
                        return None;
                    }
                }
                Some(Value::Record(record_name, fields))
            }
            Expr::Array(array) if array.attrs.is_empty() && array.elems.len() <= MAX_ELEMENTS => {
                let mut population = Vec::with_capacity(array.elems.len());
                let mut kind = None;
                for element in &array.elems {
                    let Value::Record(record_name, fields) = self.eval(element, None)? else {
                        return None;
                    };
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "Value::Record is constructed only after this same immutable record declaration was found.", tag = "equivalent")]
                    let declaration = self.records.get(&record_name)?;
                    if !declaration
                        .fields
                        .iter()
                        // #[gamma::skip(logical.and_remove_left, reason = "A different u32 field cannot produce a population: the subsequent fields.get still requires the certified ordinal name.", tag = "equivalent")]
                        .any(|field| field.ident.as_ref().is_some_and(|field| field == self.ordinal) && type_name(&field.ty, "u32"))
                    {
                        return None;
                    }
                    if kind.as_ref().is_some_and(|kind| *kind != record_name) {
                        return None;
                    }
                    kind = Some(record_name);
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The declaration contains the ordinal, and Record construction verified complete unique declared fields.", tag = "equivalent")]
                    let Value::Ordinal(ordinal) = fields.get(self.ordinal)? else {
                        return None;
                    };
                    population.push(*ordinal);
                }
                Some(Value::Population(population))
            }
            Expr::MethodCall(call)
                if call.attrs.is_empty() && call.turbofish.is_none() && call.args.is_empty() && call.method == "to_vec" =>
            {
                let value = self.eval(&call.receiver, None)?;
                matches!(value, Value::Population(_)).then_some(value)
            }
            Expr::Call(call)
                // #[gamma::skip(logical.and_remove_right, reason = "The retained exact standard HashMap::from constructor requires exactly one argument in supported Rust.", tag = "equivalent")]
                if standard_cache_constructor && call.args.len() == 1 =>
            {
                let Expr::Array(entries) = &call.args[0] else { return None };
                if !entries.attrs.is_empty() || entries.elems.len() > MAX_ELEMENTS {
                    return None;
                }
                if entries.elems.is_empty() {
                    return Some(Value::EmptyMap);
                }
                let mut cache = Vec::with_capacity(entries.elems.len());
                for entry in &entries.elems {
                    let Expr::Tuple(pair) = entry else { return None };
                    if
                        // #[gamma::skip(logical.or_remove_right, reason = "The exact standard HashMap::from array element is a two-tuple in supported Rust.", tag = "equivalent")]
                        !pair.attrs.is_empty() || pair.elems.len() != 2
                    {
                        return None;
                    }
                    let Expr::Call(key) = &pair.elems[0] else { return None };
                    if !key.attrs.is_empty() || *key.func != syn::parse_quote!(std::vec::Vec::from) || key.args.len() != 1 {
                        return None;
                    }
                    let Expr::Array(ordinals) = &key.args[0] else { return None };
                    if !ordinals.attrs.is_empty() || ordinals.elems.len() > MAX_ELEMENTS {
                        return None;
                    }
                    let ordinals = ordinals
                        .elems
                        .iter()
                        .map(|ordinal| match ordinal {
                            Expr::Lit(literal) if literal.attrs.is_empty() => match &literal.lit {
                                syn::Lit::Int(value) if matches!(value.suffix(), "" | "u32") => value.base10_parse().ok(),
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect::<Option<Vec<u32>>>()?;
                    let verdict = match &pair.elems[1] {
                        Expr::Path(path) if path.attrs.is_empty() && path.path.is_ident("None") => None,
                        Expr::Call(some)
                            if some.attrs.is_empty() && name(&some.func).as_deref() == Some("Some") && some.args.len() == 1 =>
                        {
                            let Expr::Lit(literal) = &some.args[0] else { return None };
                            let syn::Lit::Bool(value) = &literal.lit else { return None };
                            if !literal.attrs.is_empty() {
                                return None;
                            }
                            Some(value.value)
                        }
                        _ => return None,
                    };
                    cache.push((ordinals, verdict));
                }
                Some(Value::Cache(cache))
            }
            _ => None,
        }
    }

    pub(super) fn install(self, retry: &mut Retry) -> Option<()> {
        fn install(name: String, value: Value, retry: &mut Retry) -> Option<()> {
            let _old = retry.bindings.insert(name.clone(), Binding::Opaque);
            match value {
                Value::Count(value) => {
                    let _old = retry.bindings.insert(name.clone(), Binding::Integer);
                    let _old = retry.counts.insert(name, value);
                }
                Value::Bool(value) => {
                    let _old = retry.bindings.insert(name.clone(), Binding::Bool);
                    let _old = retry.seeds.insert(name, syn::parse_quote!(#value));
                }
                Value::Record(_, fields) => {
                    for (field, value) in fields {
                        // #[gamma::skip(try.propagate_to_unwrap, reason = "Every finite Value variant installs successfully; recursive Record installation has no failure branch.", tag = "equivalent")]
                        install(format!("{name}.{field}"), value, retry)?;
                    }
                }
                Value::Population(population) => {
                    let _old = retry.populations.insert(name, population);
                }
                Value::Cache(entries) => {
                    let _old = retry.caches.insert(name, entries);
                }
                Value::EmptyMap => {
                    // Its type and use are checked by the cache-prefix certificate.
                    let _old = retry.bindings.insert(name.clone(), Binding::Bool);
                    let _old = retry.seeds.insert(name, syn::parse_quote!(true));
                }
                Value::Ordinal(_) | Value::Opaque => {}
            }
            Some(())
        }
        let mut values = self.values.into_iter().collect::<Vec<_>>();
        // #[gamma::skip(relational.eq_to_ne, reason = "Both dot count and non-dot-byte count strictly increase from a root to its nonempty field projections, preserving ancestor-before-override order.", tag = "equivalent")]
        values.sort_by_key(|(name, _)| name.bytes().filter(|byte| *byte == b'.').count());
        for (name, value) in values {
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Every finite Value variant installs successfully; the recursive helper has no failure branch.", tag = "equivalent")]
            install(name, value, retry)?;
        }
        (retry.bindings.len() <= MAX_BINDINGS).then_some(())
    }
}

pub(super) fn name(expression: &Expr) -> Option<String> {
    let Expr::Path(path) = expression else { return None };
    // #[gamma::skip(logical.and_remove_right, reason = "The following get_ident independently rejects qualified-self paths by leading colon or segment count.", tag = "equivalent")]
    (path.attrs.is_empty() && path.qself.is_none())
        .then(|| path.path.get_ident().map(ToString::to_string))
        .flatten()
}

fn place(expression: &Expr) -> Option<String> {
    if let Some(name) = name(expression) {
        return Some(name);
    }
    let Expr::Field(field) = expression else {
        return None; // #[gamma::skip(option.none_to_some, reason = "The only consumer immediately resolves State::ty; an empty place has no typed root and rejects just like None.", tag = "equivalent")]
    };
    let syn::Member::Named(name) = &field.member else {
        return None; // #[gamma::skip(option.none_to_some, reason = "The only consumer immediately resolves State::ty; an empty place has no typed root and rejects just like None.", tag = "equivalent")]
    };
    field
        .attrs
        .is_empty()
        .then(|| Some(format!("{}.{name}", place(&field.base)?)))
        .flatten()
}

fn type_name(ty: &Type, name: &str) -> bool {
    matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident(name))
}

pub(super) fn small(expression: &Expr) -> Option<u16> {
    let Expr::Lit(literal) = expression else { return None };
    let syn::Lit::Int(value) = &literal.lit else { return None };
    if !literal.attrs.is_empty() || !matches!(value.suffix(), "" | "usize" | "u32") {
        return None;
    }
    let value = value.base10_parse::<u16>().ok()?;
    (value <= MAX_SMALL_COUNT).then_some(value)
}
