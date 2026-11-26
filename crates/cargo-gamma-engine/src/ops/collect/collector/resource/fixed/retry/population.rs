// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Finite standard population operations under resolved record/cache evidence.

use syn::{BinOp, Expr, Pat};

use super::super::{Machine, Value, bare_name, place_name};

// Conservative work/storage caps: raising them explores more states or partitions
// at greater cost; exhaustion withholds evidence rather than proving repetition.
const MAX_ITERATIONS: usize = 32;
const MAX_PARTITIONS: usize = 32;
// Independent small-count policy, leaving arithmetic headroom in u16.
// A larger domain admits more results, still subject to checked arithmetic.
const MAX_COUNT: u16 = 32767;

fn count(value: &Value) -> Option<u16> {
    match value {
        Value::Count(value) => Some(*value),
        Value::IntegerZero => Some(0),
        Value::IntegerSmall(value) => Some(u16::from(value.get())),
        _ => None,
    }
}

fn method<'a>(expression: &'a Expr, name: &str, arity: usize) -> Option<&'a syn::ExprMethodCall> {
    let Expr::MethodCall(call) = expression else { return None };
    (call.attrs.is_empty() && call.method == name && call.args.len() == arity && call.turbofish.is_none()).then_some(call)
}

pub(in super::super) fn repeats(machine: &mut Machine<'_>, condition: &Expr, body: &syn::Block) -> bool {
    let population = machine
        .values
        .iter()
        .filter(|(_, value)| matches!(value, Value::Population(_)))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<crate::HashMap<_, _>>();
    let mut seen = Vec::new();
    for _ in 0..MAX_ITERATIONS {
        if machine.eval(
            condition,
            // #[gamma::skip(option.some_to_none, reason = "Bool-producing forms determine their own kind; the comparison still requires Bool(true).", tag = "equivalent")]
            Some(super::super::Binding::Bool),
        ) != Some(Value::Bool(true))
        {
            return false;
        }
        if seen.contains(&machine.values) {
            return true;
        }
        seen.push(machine.values.clone());
        machine.restarts = false;
        // #[gamma::skip(stmt.delete_assign, reason = "Entry starts without a break, and every iteration that sets breaks returns immediately below.", tag = "equivalent")]
        machine.breaks = false;
        if machine.block(body).is_none() || machine.breaks || population.iter().any(|(name, value)| machine.values.get(name) != Some(value))
        {
            return false;
        }
    }
    false
}

pub(in super::super) fn eval(machine: &mut Machine<'_>, expression: &Expr) -> Option<Value> {
    match expression {
        Expr::Binary(binary) if binary.attrs.is_empty() => {
            let left = machine.eval(&binary.left, None)?;
            let Value::Count(a) = left else { return None };
            let b = count(&machine.eval(&binary.right, Some(super::super::Binding::Integer))?)?;
            Some(Value::Bool(match binary.op {
                BinOp::Lt(_) => a < b,
                BinOp::Le(_) => a <= b,
                BinOp::Gt(_) => a > b,
                BinOp::Ge(_) => a >= b,
                BinOp::Eq(_) => a == b,
                BinOp::Ne(_) => a != b,
                _ => return None,
            }))
        }
        Expr::Call(call)
            // #[gamma::skip(match_guard.always_true, reason = "Effects::scalar independently rejects nonempty call attributes before evaluating arguments.", tag = "equivalent")]
            if call.attrs.is_empty() =>
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Machine dispatches to population evaluation only when retry evidence is present.", tag = "equivalent")]
            machine.retry?.scalar(machine, call),
        Expr::MethodCall(call) if call.attrs.is_empty() => {
            if call.method == "collect" {
                return collect(machine, call);
            }
            if call.turbofish.is_some() {
                return None;
            }
            let receiver = machine.eval(&call.receiver, None)?;
            match (receiver, call.method.to_string().as_str(), call.args.len()) {
                (Value::Population(population), "len", 0) => Some(Value::Count(
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "Installed populations have at most 32 entries and every modeled operation preserves or subsets them.", tag = "equivalent")]
                    u16::try_from(population.len()).ok()?,
                )),
                (Value::Population(population), "is_empty", 0) => Some(Value::Bool(population.is_empty())),
                (Value::Population(population), "to_vec" | "clone", 0) => Some(Value::Population(population)),
                (Value::Count(left), name, 1) => {
                    let argument = machine.eval(&call.args[0], Some(super::super::Binding::Integer))?;
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "A successfully evaluated well-typed usize operand has Count, IntegerZero or IntegerSmall form.", tag = "equivalent")]
                    let right = count(&argument)?;
                    let result = match name {
                        "div_ceil" if right != 0 => left.div_ceil(right),
                        "saturating_sub" => left.saturating_sub(right),
                        "saturating_mul" => left.checked_mul(right)?,
                        "min" => left.min(right),
                        "max" => left.max(right),
                        _ => return None,
                    };
                    (result <= MAX_COUNT).then_some(Value::Count(result))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn collect(machine: &mut Machine<'_>, collect: &syn::ExprMethodCall) -> Option<Value> {
    if !collect.args.is_empty() || !collection_type(collect) {
        return None;
    }
    if let Some(map) = method(&collect.receiver, "map", 1) {
        if let Some(chunks) = method(&map.receiver, "chunks", 1) {
            let Value::Population(population) = machine.eval(&chunks.receiver, None)? else {
                return None;
            };
            let argument = machine.eval(&chunks.args[0], Some(super::super::Binding::Integer))?;
            // #[gamma::skip(try.propagate_to_unwrap, reason = "A successfully evaluated well-typed chunks width has Count, IntegerZero or IntegerSmall form.", tag = "equivalent")]
            let width = count(&argument)?;
            if width == 0 || map.args[0] != syn::parse_quote!(<[_]>::to_vec) || !vector_collection(collect) {
                return None;
            }
            return Some(Value::Partitions(
                population.chunks(usize::from(width)).map(<[_]>::to_vec).collect(),
            ));
        }
        let iter = method(&map.receiver, "iter", 0)?;
        let Value::Population(population) = machine.eval(&iter.receiver, None)? else {
            return None;
        };
        let (parameter, body) = closure(&map.args[0])?;
        let Expr::Field(field) = body else { return None };
        let syn::Member::Named(ordinal) = &field.member else { return None };
        if !field.attrs.is_empty()
            || bare_name(&field.base)? != parameter
            ||
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Machine dispatches to population evaluation only with retry evidence.", tag = "equivalent")]
            !machine.retry?.ordinals.contains(&ordinal.to_string())
            || !set_collection(collect)
        {
            return None;
        }
        return Some(Value::Ordinals(population));
    }
    let filter = method(&collect.receiver, "filter", 1)?;
    let copied = method(&filter.receiver, "copied", 0)?;
    let iter = method(&copied.receiver, "iter", 0)?;
    let Value::Population(population) = machine.eval(&iter.receiver, None)? else {
        return None;
    };
    let (parameter, body) = closure(&filter.args[0])?;
    let Expr::Unary(negation) = body else { return None };
    if !negation.attrs.is_empty() || !matches!(negation.op, syn::UnOp::Not(_)) || !vector_collection(collect) {
        return None;
    }
    let contains = method(&negation.expr, "contains", 1)?;
    let Value::Ordinals(removed) = machine.eval(&contains.receiver, None)? else {
        return None;
    };
    let Expr::Reference(reference) = &contains.args[0] else {
        return None;
    };
    let Expr::Field(field) = &*reference.expr else { return None };
    let syn::Member::Named(ordinal) = &field.member else { return None };
    if !reference.attrs.is_empty()
        || reference.mutability.is_some()
        || !field.attrs.is_empty()
        || bare_name(&field.base)? != parameter
        ||
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Machine dispatches to population evaluation only with retry evidence.", tag = "equivalent")]
        !machine.retry?.ordinals.contains(&ordinal.to_string())
    {
        return None;
    }
    Some(Value::Population(
        population.into_iter().filter(|ordinal| !removed.contains(ordinal)).collect(),
    ))
}

fn closure(expression: &Expr) -> Option<(String, &Expr)> {
    let Expr::Closure(closure) = expression else { return None };
    let unsupported_closure = !closure.attrs.is_empty()
        || closure.inputs.len() != 1
        || closure.asyncness.is_some()
        || closure.constness.is_some()
        || closure.capture.is_some();
    if
    // #[gamma::skip(logical.or_remove_right, reason = "Explicit binders require typed parameters/output rejected below; explicit output requires a block rejected by map/filter, and pinned Syn has empty ClosureModifiers.", tag = "equivalent")]
    unsupported_closure
        || closure.lifetimes.is_some()
        || closure.modifiers.require_empty().is_err()
        || !matches!(closure.output, syn::ReturnType::Default)
    {
        return None;
    }
    let Pat::Ident(parameter) = &closure.inputs[0] else { return None };
    (parameter.attrs.is_empty() && parameter.by_ref.is_none() && parameter.subpat.is_none())
        .then(|| (parameter.ident.to_string(), &*closure.body))
}

fn collection_type(call: &syn::ExprMethodCall) -> bool {
    let Some(arguments) = &call.turbofish else { return true };
    let mut arguments = arguments.args.iter();
    let Some(syn::GenericArgument::Type(syn::Type::Path(ty))) = arguments.next() else {
        return false; // #[gamma::skip(literal.bool_flip, reason = "Both vector_collection and set_collection independently reject a non-TypePath collection argument.", tag = "equivalent")]
    };
    if arguments.next().is_some() || ty.qself.is_some() || ty.path.segments.len() != 1 {
        return false;
    }
    // #[gamma::skip(iter.first_to_last, reason = "The preceding exact singleton path check makes first and last identical.", tag = "equivalent")]
    let Some(segment) = ty.path.segments.first() else {
        return false; // #[gamma::skip(literal.bool_flip, reason = "The preceding exact singleton path check guarantees this segment exists.", tag = "equivalent")]
    };
    let syn::PathArguments::AngleBracketed(element) = &segment.arguments else {
        return false;
    };
    // #[gamma::skip(logical.and_remove_left, reason = "Every collection branch independently requires vector_collection or set_collection with the same accepted first-segment names.", tag = "equivalent")]
    matches!(segment.ident.to_string().as_str(), "Vec" | "HashSet")
        && matches!(
            element.args.iter().collect::<Vec<_>>().as_slice(),
            [syn::GenericArgument::Type(syn::Type::Infer(_))]
        )
}

fn vector_collection(call: &syn::ExprMethodCall) -> bool {
    call.turbofish.as_ref().is_none_or(|arguments| {
        matches!(arguments.args.first(), Some(syn::GenericArgument::Type(syn::Type::Path(ty))) if ty.path.segments.first().is_some_and(|segment| segment.ident == "Vec"))
    })
}

fn set_collection(call: &syn::ExprMethodCall) -> bool {
    call.turbofish.as_ref().is_some_and(|arguments| {
        matches!(arguments.args.first(), Some(syn::GenericArgument::Type(syn::Type::Path(ty))) if ty.path.segments.first().is_some_and(|segment| segment.ident == "HashSet"))
    })
}

pub(in super::super) fn clone_from(machine: &mut Machine<'_>, call: &syn::ExprMethodCall) -> Option<()> {
    if !call.attrs.is_empty() || call.turbofish.is_some() || call.args.len() != 1 {
        return None;
    }
    let receiver = place_name(&call.receiver)?;
    if !matches!(machine.values.get(&receiver), Some(Value::Population(_))) {
        return None;
    }
    let Value::Population(population) = machine.eval(&call.args[0], None)? else {
        return None;
    };
    let _old = machine.values.insert(receiver.clone(), Value::Population(population));
    let _new = machine.writes.insert(receiver);
    Some(())
}

pub(in super::super) fn for_loop(machine: &mut Machine<'_>, loop_: &syn::ExprForLoop) -> Option<()> {
    if !loop_.attrs.is_empty() || loop_.label.is_some() {
        return None;
    }
    let Pat::Ident(pattern) = &*loop_.pat else { return None };
    if (
        // #[gamma::skip(logical.or_remove_left, reason = "Pinned Syn stores parsed for-loop attributes on ExprForLoop and initializes this simple PatIdent attributes list empty.", tag = "equivalent")]
        !pattern.attrs.is_empty() || pattern.by_ref.is_some()
    ) || pattern.subpat.is_some()
    {
        return None;
    }
    let iterable = match &*loop_.expr {
        Expr::Reference(reference) if reference.attrs.is_empty() && reference.mutability.is_none() => &*reference.expr,
        expression => expression,
    };
    let Value::Partitions(partitions) = machine.eval(iterable, None)? else {
        return None;
    };
    if partitions.len() > MAX_PARTITIONS {
        return None;
    }
    let name = pattern.ident.to_string();
    let previous = machine.values.get(&name).cloned();
    let previous_local = machine.locals.contains(&name);
    let prefix = format!("{name}.");
    if machine.values.keys().any(|key| key.starts_with(&prefix)) {
        return None;
    }
    let _new = machine.locals.insert(name.clone());
    for partition in partitions {
        machine.spend()?;
        let _old = machine.values.insert(name.clone(), Value::Population(partition));
        machine.block(&loop_.body)?;
        if machine.breaks {
            machine.breaks = false;
            break;
        }
        machine.restarts = false;
    }
    if let Some(previous) = previous {
        let _old = machine.values.insert(name.clone(), previous);
    } else {
        let _old = machine.values.remove(&name);
    }
    if !previous_local {
        let _old = machine.locals.remove(&name);
    }
    Some(())
}
