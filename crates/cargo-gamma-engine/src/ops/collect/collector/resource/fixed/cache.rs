// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exact warm-cache prefixes of resolved local scalar helpers.

use syn::{Expr, FnArg, Lit, Pat, Stmt, Type, UnOp};

use super::{Binding, Machine, Primitive, Value, bare_name, primitive_type};

pub(super) fn constructor(expression: &Expr) -> bool {
    expression == &syn::parse_quote!(std::collections::HashMap::from)
}

pub(super) fn initial_value(call: &syn::ExprCall) -> Option<Value> {
    if !constructor(&call.func) || call.args.len() != 1 || !call.attrs.is_empty() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Expr::Array(array) = &call.args[0] else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    if !array.attrs.is_empty() || array.elems.len() > 16 {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let mut entries = Vec::new();
    for element in &array.elems {
        let Expr::Tuple(tuple) = element else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        if !tuple.attrs.is_empty() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        if tuple.elems.len() != 2 {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let (Expr::Lit(key), Expr::Lit(value)) = (&tuple.elems[0], &tuple.elems[1]) else {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        };
        if !key.attrs.is_empty() || !value.attrs.is_empty() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let (Lit::Int(key), Lit::Bool(value)) = (&key.lit, &value.lit) else {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        };
        if !matches!(key.suffix(), "" | "u32") {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let key = key.base10_parse::<u32>().ok()?;
        if entries.iter().any(|(known, _)| *known == key) {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        entries.push((key, value.value));
    }
    Some(Value::BoolCache(entries))
}

pub(super) fn hit(machine: &mut Machine<'_>, function: &syn::ItemFn, call: &syn::ExprCall) -> Option<Value> {
    if function.sig.inputs.len() != call.args.len() || function.sig.output != syn::parse_quote!(-> bool) {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Stmt::Expr(Expr::If(branch), _) = function.block.stmts.first()? else {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    };
    if !branch.attrs.is_empty() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Expr::Let(binding) = &*branch.cond else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    if !binding.attrs.is_empty() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Pat::TupleStruct(pattern) = &*binding.pat else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    if !pattern.attrs.is_empty() || !pattern.path.is_ident("Some") || pattern.elems.len() != 1 {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Pat::Ident(cached) =
        // #[gamma::skip(iter.first_to_last, try.propagate_to_unwrap, reason = "The preceding exact singleton check guarantees this element exists and first equals last.", tag = "equivalent")]
        pattern.elems.first()? else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    if !cached.attrs.is_empty() || cached.by_ref.is_some() || cached.subpat.is_some() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Expr::MethodCall(get) = &*binding.expr else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    if !get.attrs.is_empty() || get.method != "get" || get.turbofish.is_some() || get.args.len() != 1 {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let cache_name = bare_name(&get.receiver)?;
    let Expr::Reference(key_reference) = &get.args[0] else {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    };
    if !key_reference.attrs.is_empty() || key_reference.mutability.is_some() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let key_name = bare_name(&key_reference.expr)?;
    let mut cache = None;
    let mut key = None;
    for (parameter, argument) in function.sig.inputs.iter().zip(&call.args) {
        let FnArg::Typed(parameter) = parameter else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        if !parameter.attrs.is_empty() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let Pat::Ident(parameter_name) = &*parameter.pat else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        if !parameter_name.attrs.is_empty() || parameter_name.by_ref.is_some() || parameter_name.subpat.is_some() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let name = parameter_name.ident.to_string();
        if name == cache_name {
            let Type::Reference(reference) = &*parameter.ty else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            if reference.mutability.is_some() || !super::super::bool_cache_type(&reference.elem) {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
            let Expr::Reference(argument) = argument else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            if !argument.attrs.is_empty() || argument.mutability.is_some() {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
            let Value::BoolCache(entries) = machine.eval(&argument.expr, Some(Binding::BoolCache))? else {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            };
            cache = Some(entries);
        } else if name == key_name {
            if primitive_type(&parameter.ty) != Some(Primitive::Unsigned(32)) {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
            let Value::Int(value, Primitive::Unsigned(32)) = machine.eval(argument, Some(Binding::Primitive(Primitive::Unsigned(32))))?
            else {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            };
            key = u32::try_from(value).ok();
        } else {
            let Type::Reference(reference) = &*parameter.ty else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            let primitive = primitive_type(&reference.elem)?;
            let Expr::Reference(argument) = argument else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            if !argument.attrs.is_empty() || bare_name(&argument.expr).is_none() {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
            let value = machine.eval(&argument.expr, None)?;
            if value.kind() != Binding::Primitive(primitive) {
                return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
        }
    }
    let [Stmt::Expr(Expr::Return(returned), _)] = branch.then_branch.stmts.as_slice() else {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    };
    if !returned.attrs.is_empty() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let Expr::Unary(dereference) = &**returned.expr.as_ref()? else {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    };
    if !dereference.attrs.is_empty()
        || !matches!(dereference.op, UnOp::Deref(_))
        || bare_name(&dereference.expr).as_deref() != Some(&cached.ident.to_string())
    {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let key = key?;
    let (_, value) = cache?.into_iter().find(|(known, _)| *known == key)?;
    Some(Value::Bool(value))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{ExprCall, ItemFn, parse_quote, parse_str};

    use super::*;
    use crate::{HashMap, HashSet};

    #[test]
    fn exact_cache_get_arity_is_checked_before_reading_or_ignoring_arguments() {
        for get in ["cache.get()", "cache.get(&key, &key)"] {
            let function = parse_str(&format!(
                "fn lookup(cache: &std::collections::HashMap<u32,bool>, key:u32)->bool {{
                    if let Some(cached)={get} {{return *cached;}}
                    opaque()
                }}"
            ))
            .unwrap();
            assert_eq!(hit(&mut machine(), &function, &parse_quote!(lookup(&cache, 7))), None, "{get}");
        }
    }

    #[test]
    fn cache_prefix_attributes_and_missing_return_values_withhold_hits() {
        let source: ItemFn = parse_quote! {
            fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32) -> bool {
                if let Some(cached) = cache.get(&key) { return *cached; }
                opaque()
            }
        };
        let call: ExprCall = parse_quote!(lookup(&cache, 7));
        assert_eq!(hit(&mut machine(), &source, &call), Some(Value::Bool(true)));
        for fault in 0..8 {
            let mut function = source.clone();
            let mut call = call.clone();
            let Stmt::Expr(Expr::If(branch), _) = &mut function.block.stmts[0] else {
                panic!("the fixture begins with its cache branch");
            };
            let Expr::Let(binding) = &mut *branch.cond else {
                panic!("the branch binds its cache hit");
            };
            let Pat::TupleStruct(pattern) = &mut *binding.pat else {
                panic!("the cache hit uses Some(cached)");
            };
            let Expr::MethodCall(get) = &mut *binding.expr else {
                panic!("the cache hit calls get");
            };
            let attribute = parse_quote!(#[custom]);
            match fault {
                0 => pattern.attrs.push(attribute),
                1 => {
                    let Pat::Ident(cached) = &mut pattern.elems[0] else {
                        panic!("the Some payload is an identifier");
                    };
                    cached.attrs.push(attribute);
                }
                2 => get.attrs.push(attribute),
                3 => {
                    let Expr::Reference(key) = &mut get.args[0] else {
                        panic!("get borrows its key");
                    };
                    key.attrs.push(attribute);
                }
                4 => {
                    let FnArg::Typed(parameter) = &mut function.sig.inputs[0] else {
                        panic!("the cache parameter is typed");
                    };
                    let Pat::Ident(parameter) = &mut *parameter.pat else {
                        panic!("the cache parameter is an identifier");
                    };
                    parameter.attrs.push(attribute);
                }
                5 => {
                    let Expr::Reference(argument) = &mut call.args[0] else {
                        panic!("the call borrows its cache");
                    };
                    argument.attrs.push(attribute);
                }
                6 => branch.then_branch.stmts[0] = parse_quote!(return;),
                7 => pattern.elems.push(parse_quote!(extra)),
                _ => unreachable!(),
            }
            assert_eq!(hit(&mut machine(), &function, &call), None, "unsupported prefix {fault}");
        }
    }

    fn machine() -> Machine<'static> {
        Machine {
            values: HashMap::from_iter([
                ("cache".into(), Value::BoolCache(vec![(0, false), (7, true), (u32::MAX, false)])),
                ("counter".into(), Value::Int(3, Primitive::Unsigned(32))),
            ]),
            budget: 256,
            writes: HashSet::default(),
            locals: HashSet::default(),
            facts: HashMap::default(),
            helpers: None,
            restarts: false,
            numeric_names: true,
            stable_calls: None,
            paths: HashMap::default(),
            assumptions: Vec::new(),
        }
    }

    #[test]
    fn cache_initializers_require_exact_unique_u32_bool_entries() {
        for (source, expected) in [
            ("std::collections::HashMap::from([])", Some(vec![])),
            (
                "std::collections::HashMap::from([(0, false), (4294967295_u32, true)])",
                Some(vec![(0, false), (u32::MAX, true)]),
            ),
            ("std::collections::HashMap::from([(7, true), (7_u32, false)])", None),
            ("std::collections::HashMap::from([(4294967296, true)])", None),
            ("std::collections::HashMap::from([(7_u64, true)])", None),
            ("std::collections::HashMap::from([(7, 1)])", None),
            ("std::collections::HashMap::from([(key, true)])", None),
            ("std::collections::HashMap::from([(true, false)])", None),
            ("std::collections::HashMap::from([(7, true, false)])", None),
            ("std::collections::HashMap::from([7])", None),
            ("std::collections::HashMap::from(entries)", None),
            ("std::collections::HashMap::from()", None),
            ("std::collections::HashMap::from([], [])", None),
            ("HashMap::from([(7, true)])", None),
            ("core::collections::HashMap::from([(7, true)])", None),
            ("std::collections::HashMap::new([])", None),
            ("#[cfg(test)] std::collections::HashMap::from([])", None),
            ("std::collections::HashMap::from(#[cfg(test)] [])", None),
            ("std::collections::HashMap::from([#[cfg(test)] (7, true)])", None),
            ("std::collections::HashMap::from([(#[cfg(test)] 7, true)])", None),
            ("std::collections::HashMap::from([(7, #[cfg(test)] true)])", None),
        ] {
            let call = parse_str(source).unwrap();
            assert_eq!(initial_value(&call), expected.map(Value::BoolCache), "{source}");
        }
        let full: ExprCall = parse_quote!(std::collections::HashMap::from([
            (0, false),
            (1, true),
            (2, false),
            (3, true),
            (4, false),
            (5, true),
            (6, false),
            (7, true),
            (8, false),
            (9, true),
            (10, false),
            (11, true),
            (12, false),
            (13, true),
            (14, false),
            (15, true)
        ]));
        let Some(Value::BoolCache(entries)) = initial_value(&full) else {
            panic!("16-entry cache should be accepted")
        };
        assert_eq!(entries.len(), 16);
        assert_eq!(entries[15], (15, true));
        let mut oversized = full;
        let Expr::Array(array) = &mut oversized.args[0] else {
            unreachable!()
        };
        array.elems.push(parse_quote!((16, false)));
        assert!(initial_value(&oversized).is_none());
    }

    #[test]
    fn cache_hit_distinguishes_false_from_miss_and_checks_key_and_arguments() {
        let function = parse_quote! {
            fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32, counter: &mut u32) -> bool {
                if let Some(cached) = cache.get(&key) { return *cached; }
                unknown(counter)
            }
        };
        for (source, expected) in [
            ("lookup(&cache, 0, &mut counter)", Some(false)),
            ("lookup(&cache, 7, &mut counter)", Some(true)),
            ("lookup(&cache, 4294967295_u32, &mut counter)", Some(false)),
            ("lookup(&cache, 8, &mut counter)", None),
            ("lookup(&cache, 4294967296, &mut counter)", None),
            ("lookup(&cache, 7_u64, &mut counter)", None),
            ("lookup(&mut cache, 7, &mut counter)", None),
            ("lookup(cache, 7, &mut counter)", None),
            ("lookup(&cache, 7, counter)", None),
            ("lookup(&cache, 7, &mut unknown)", None),
            ("lookup(&cache, 7, &mut (counter + 1))", None),
            ("lookup(&cache, 7, #[cfg(test)] &mut counter)", None),
            ("lookup(&cache, 7)", None),
            ("lookup(&cache, 7, &mut counter, 0)", None),
        ] {
            assert_eq!(
                hit(&mut machine(), &function, &parse_str(source).unwrap()),
                expected.map(Value::Bool),
                "{source}"
            );
        }
    }

    #[test]
    fn cache_prefix_rejects_near_matches_with_different_effects_or_provenance() {
        let call = parse_quote!(lookup(&cache, 7));
        for source in [
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(value)=cache.get(&key){return *value;} false }",
        ] {
            assert_eq!(
                hit(&mut machine(), &parse_str(source).unwrap(), &call),
                Some(Value::Bool(true)),
                "{source}"
            );
        }
        for source in [
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Other(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(ref cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(#[cfg(test)] cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if #[cfg(test)] let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some((cached,))=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){#[cfg(test)] return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return #[cfg(test)] *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32) { if let Some(cached)=cache.get(&key){return *cached;} }",
            "fn lookup(cache:&mut std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u64,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u64)->bool { if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { side_effect(); if let Some(cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let None=cache.get(&key){return true;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(ref cached)=cache.get(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.remove(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&mut key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get::<u32>(&key){return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){side_effect();return *cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return !cached;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { if let Some(cached)=cache.get(&key){return *other;} false }",
            "fn lookup(cache:&std::collections::HashMap<u32,bool>,key:u32)->bool { #[cfg(test)] if let Some(cached)=cache.get(&key){return *cached;} false }",
        ] {
            assert!(hit(&mut machine(), &parse_str(source).unwrap(), &call).is_none(), "{source}");
        }
    }

    #[test]
    fn cache_hit_does_not_certify_arguments_with_mismatched_runtime_bindings() {
        let function = parse_quote! {
            fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32, counter: &mut u32) -> bool {
                if let Some(cached) = cache.get(&key) { return *cached; }
                false
            }
        };
        let call = parse_quote!(lookup(&cache, 7, &mut counter));
        for name in ["cache", "counter"] {
            let mut state = machine();
            state.values.insert(name.into(), Value::Bool(true));
            assert!(hit(&mut state, &function, &call).is_none(), "{name}");
        }
    }

    #[test]
    fn cache_literal_arguments_receive_contextual_types_without_treating_misses_as_false() {
        let function = parse_quote! {
            fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32) -> bool {
                if let Some(cached) = cache.get(&key) { return *cached; }
                opaque()
            }
        };
        for (source, expected) in [
            ("lookup(&std::collections::HashMap::from([(7, true)]), 7)", Some(Value::Bool(true))),
            (
                "lookup(&std::collections::HashMap::from([(7, false)]), 7)",
                Some(Value::Bool(false)),
            ),
            (
                "lookup(&std::collections::HashMap::from([(0, false), (7, true)]), 0)",
                Some(Value::Bool(false)),
            ),
            ("lookup(&std::collections::HashMap::from([(0, false), (7, true)]), 8)", None),
            ("lookup(&std::collections::HashMap::from([]), 7)", None),
            ("lookup(&std::collections::HashMap::from([(7, true), (7, false)]), 7)", None),
            ("lookup(&std::collections::HashMap::from([(7_u64, true)]), 7)", None),
        ] {
            let mut state = machine();
            let before = state.values.clone();
            assert_eq!(
                hit(
                    &mut state,
                    &function,
                    &parse_str(source).expect("the fixed cache call is valid Rust syntax")
                ),
                expected,
                "{source}"
            );
            assert_eq!(state.values, before, "cache inspection must not execute the opaque suffix");
            assert_eq!(state.writes, HashSet::default());
        }
    }

    #[test]
    fn cache_parameter_roles_follow_declaration_names_not_argument_positions() {
        for (source, call) in [
            (
                "fn lookup(key: u32, cache: &std::collections::HashMap<u32, bool>, counter: &mut u32) -> bool {
                    if let Some(cached) = cache.get(&key) { return *cached; } opaque(counter)
                 }",
                "lookup(7, &cache, &mut counter)",
            ),
            (
                "fn lookup(counter: &mut u32, query: u32, store: &std::collections::HashMap<u32, bool>) -> bool {
                    if let Some(found) = store.get(&query) { return *found; } opaque(counter)
                 }",
                "lookup(&mut counter, 7, &cache)",
            ),
        ] {
            let function = parse_str(source).expect("the fixed reordered helper is valid Rust syntax");
            let call = parse_str(call).expect("the fixed reordered call is valid Rust syntax");
            let mut state = machine();
            assert_eq!(hit(&mut state, &function, &call), Some(Value::Bool(true)), "{source}");
            assert_eq!(state.values["counter"], Value::Int(3, Primitive::Unsigned(32)));
            assert_eq!(state.writes, HashSet::default());
        }
    }

    #[test]
    fn cache_prefixes_do_not_substitute_projected_inputs_for_bare_parameter_evidence() {
        for (source, call) in [
            (
                "fn lookup(wrapped: &(std::collections::HashMap<u32, bool>,), key: u32) -> bool {
                    if let Some(cached) = wrapped.0.get(&key) { return *cached; } false
                 }",
                "lookup(&wrapped, 7)",
            ),
            (
                "fn lookup(cache: &std::collections::HashMap<u32, bool>, wrapped: (u32,)) -> bool {
                    if let Some(cached) = cache.get(&wrapped.0) { return *cached; } false
                 }",
                "lookup(&cache, wrapped)",
            ),
        ] {
            let mut state = machine();
            state
                .values
                .insert("wrapped".into(), Value::Symbol("wrapped".into(), Binding::Opaque));
            assert_eq!(
                hit(
                    &mut state,
                    &parse_str(source).expect("the fixed projected helper is valid Rust syntax"),
                    &parse_str(call).expect("the fixed projected call is valid Rust syntax")
                ),
                None,
                "{source}"
            );
            assert_eq!(state.writes, HashSet::default());
        }
    }

    #[test]
    fn cache_prefixes_with_global_inputs_or_nontrivial_bindings_withhold_evidence() {
        for (source, call) in [
            (
                "fn lookup(cache: &std::collections::HashMap<u32, bool>) -> bool {
                    if let Some(cached) = cache.get(&GLOBAL_KEY) { return *cached; } false
                 }",
                "lookup(&cache)",
            ),
            (
                "fn lookup(key: u32) -> bool {
                    if let Some(cached) = GLOBAL_CACHE.get(&key) { return *cached; } false
                 }",
                "lookup(7)",
            ),
            (
                "fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32) -> bool {
                    if let Some(cached @ _) = cache.get(&key) { return *cached; } false
                 }",
                "lookup(&cache, 7)",
            ),
            (
                "fn lookup(cache: &std::collections::HashMap<u32, bool>, key @ _: u32) -> bool {
                    if let Some(cached) = cache.get(&key) { return *cached; } false
                 }",
                "lookup(&cache, 7)",
            ),
            (
                "fn lookup(cache: &std::collections::HashMap<u32, bool>, key: u32, flag: &bool) -> bool {
                    if let Some(cached) = cache.get(&key) { return *cached; } *flag
                 }",
                "lookup(&cache, 7, &flag)",
            ),
        ] {
            let mut state = machine();
            state.values.insert("flag".into(), Value::Bool(true));
            assert_eq!(
                hit(
                    &mut state,
                    &parse_str(source).expect("syn accepts the fixed helper declaration with external global inputs"),
                    &parse_str(call).expect("the fixed cache call is valid Rust syntax")
                ),
                None,
                "{source}"
            );
            assert_eq!(state.writes, HashSet::default());
        }
    }
}
