// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::Expr;

use super::super::{Machine, Value, place_name};
use super::CachePrefix;
use crate::HashMap;

pub(in super::super::super::super) type CacheEntries = Vec<(Vec<u32>, Option<bool>)>;

// Conservative analysis-cost limits, not Rust limits or measured capacities.
// Raising them admits larger snapshots at greater cloning/comparison cost;
// lowering them withholds evidence for more inputs.
const MAX_CALLS: usize = 32;
const MAX_SNAPSHOTS: usize = 64;
const MAX_POPULATION_LEN: usize = 32;
// The modeled small-count domain leaves arithmetic headroom in u16.
// Raising it admits larger counts; checked arithmetic can still withhold evidence.
const MAX_COUNT: u16 = 32767;

/// Exact entry snapshots and resolved call targets supplied by the declaration/flow analysis.
///
/// The caller must establish these snapshots at the loop entry, resolve each inherent target
/// (keys are `receiver.method`), and certify the population's primitive field projection.
/// `some_constructor` must be the resolved standard Option constructor at comparison sites;
/// absent that declaration evidence even a helper named `Some` remains an unknown call.
/// These are evidence inputs, not assumptions that may be inferred from call or binding names.
pub(in super::super::super::super) struct Effects<'a> {
    pub(in super::super::super::super) calls: HashMap<String, &'a CachePrefix>,
    pub(in super::super::super::super) populations: HashMap<String, Vec<u32>>,
    pub(in super::super::super::super) caches: HashMap<String, CacheEntries>,
    pub(in super::super::super::super) some_constructor: Option<String>,
    pub(in super::super::super::super) counts: HashMap<String, u16>,
    pub(in super::super::super::super) scalars: HashMap<String, syn::ImplItemFn>,
    pub(in super::super::super::super) ordinals: crate::HashSet<String>,
    pub(in super::super::super::super) globals: HashMap<String, u16>,
}

impl Effects<'_> {
    pub(in super::super) fn install(&self, machine: &mut Machine<'_>) -> Option<()> {
        if self.calls.len() > MAX_CALLS || self.populations.len() + self.caches.len() > MAX_SNAPSHOTS {
            return None;
        }
        for (name, population) in &self.populations {
            if population.len() > MAX_POPULATION_LEN || machine.values.get(name)?.kind() != super::super::Binding::Opaque {
                return None;
            }
            let _old = machine.values.insert(name.clone(), Value::Population(population.clone()));
        }
        for (name, cache) in &self.caches {
            if cache.len() > MAX_POPULATION_LEN
                || cache.iter().any(|(key, _)| key.len() > MAX_POPULATION_LEN)
                || machine.values.get(name)?.kind() != super::super::Binding::Opaque
                || self.populations.contains_key(name)
            {
                return None;
            }
            let _old = machine.values.insert(name.clone(), Value::PopulationCache(cache.clone()));
        }
        for (name, count) in &self.counts {
            if *count > MAX_COUNT || !machine.values.contains_key(name) {
                return None;
            }
            let _old = machine.values.insert(name.clone(), Value::Count(*count));
        }
        Some(())
    }

    pub(in super::super) fn scalar(&self, machine: &mut Machine<'_>, call: &syn::ExprCall) -> Option<Value> {
        use quote::ToTokens as _;
        let function = self.scalars.get(&call.func.to_token_stream().to_string())?;
        if !call.attrs.is_empty() || call.args.len() != function.sig.inputs.len() {
            return None;
        }
        let mut values = self
            .globals
            .iter()
            .map(|(name, count)| (name.clone(), Value::Count(*count)))
            .collect::<HashMap<_, _>>();
        for (parameter, argument) in function.sig.inputs.iter().zip(&call.args) {
            let syn::FnArg::Typed(parameter) = parameter else { return None };
            if !parameter.attrs.is_empty() {
                return None;
            }
            let name = super::ident(&parameter.pat)?;
            let value = machine.eval(
                argument,
                // #[gamma::skip(option.some_to_none, reason = "Accepted arguments remain Count or Bool; this hint only affects integer literal forms that the Count check rejects.", tag = "equivalent")]
                Some(super::super::Binding::Integer),
            )?;
            // #[gamma::skip(logical.or_remove_left, logical.or_remove_right, logical.or_to_and, reason = "The scalar registry permits only usize or bool parameters; after excluding usize, every well-typed call supplies a Bool to this bool parameter.", tag = "equivalent")]
            if super::primitive(&parameter.ty, "usize") {
                if !matches!(value, Value::Count(_)) {
                    return None;
                }
            } else if !super::primitive(&parameter.ty, "bool") || !matches!(value, Value::Bool(_)) {
                return None;
            }
            let _old = values.insert(name, value);
        }
        let [syn::Stmt::Expr(tail, None)] = function.block.stmts.as_slice() else {
            return None;
        };
        let output = match &function.sig.output {
            syn::ReturnType::Type(_, ty) if super::primitive(ty, "usize") => super::super::Binding::Integer,
            syn::ReturnType::Type(_, ty)
                // #[gamma::skip(match_guard.always_true, reason = "A non-usize/non-bool return type cannot have a well-typed Bool tail in the closed scalar grammar; the result-kind check still applies.", tag = "equivalent")]
                if super::primitive(ty, "bool") => super::super::Binding::Bool,
            _ => return None,
        };
        let closed = Effects {
            calls: HashMap::default(),
            populations: HashMap::default(),
            caches: HashMap::default(),
            some_constructor: None, // #[gamma::skip(option.none_to_some, reason = "Constructor dispatch recognizes only the exact string Some, so an empty constructor name also admits nothing.", tag = "equivalent")]
            counts: HashMap::default(),
            scalars: HashMap::default(),
            ordinals: crate::HashSet::default(),
            globals: HashMap::default(),
        };
        let mut nested = Machine {
            values,
            budget: machine.budget,
            writes: crate::HashSet::default(),
            locals: crate::HashSet::default(),
            facts: HashMap::default(),
            helpers: None,
            restarts: false, // #[gamma::skip(literal.bool_flip, reason = "This nested machine evaluates a scalar tail only; expression evaluation does not read restart state.", tag = "equivalent")]
            numeric_names: machine.numeric_names,
            stable_calls: None,
            paths: HashMap::default(),
            assumptions: Vec::new(),
            retry: Some(&closed),
            breaks: false, // #[gamma::skip(literal.bool_flip, reason = "This nested machine evaluates a scalar tail only; expression evaluation does not read break state.", tag = "equivalent")]
        };
        let value = nested.eval(tail, Some(output));
        machine.budget = nested.budget;
        let value = value?;
        if output == super::super::Binding::Integer {
            return match value {
                Value::Count(_) => Some(value),
                Value::IntegerZero => Some(Value::Count(0)),
                Value::IntegerSmall(value) => Some(Value::Count(u16::from(value.get()))),
                _ => None,
            };
        }
        (value.kind() == output).then_some(value)
    }

    pub(in super::super) fn hit(&self, machine: &mut Machine<'_>, call: &syn::ExprMethodCall) -> Option<Value> {
        if !call.attrs.is_empty() || call.turbofish.is_some() {
            return None;
        }
        let receiver = place_name(&call.receiver)?;
        if machine.values.get(&receiver)? != &Value::Symbol(receiver.clone(), super::super::Binding::Opaque) {
            return None;
        }
        let prefix = self.calls.get(&format!("{receiver}.{}", call.method))?;
        if call.args.len() != prefix.parameters.len() {
            return None;
        }
        let mut arguments = HashMap::default();
        for (parameter, expression) in prefix.parameters.iter().zip(&call.args) {
            let expression = if let Expr::Reference(reference) = expression {
                if !reference.attrs.is_empty() {
                    return None;
                }
                &*reference.expr
            } else {
                expression
            };
            let place = place_name(expression)?;
            // Only already evaluated places can be forwarded. Argument calls, dereferences,
            // indexing, and unknown aliases may have effects even on the eventual hit path.
            let _known = machine.values.get(&place)?;
            let _old = arguments.insert(parameter.clone(), place);
        }
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The arity-checked zip inserts every resolved typed parameter, including the certified population parameter.", tag = "equivalent")]
        let population = arguments.get(prefix.population().0)?;
        // #[gamma::skip(try.propagate_to_unwrap, reason = "Each forwarded argument place was already required to exist in this unchanged values map.", tag = "equivalent")]
        let Value::Population(ordinals) = machine.values.get(population)? else {
            return None;
        };
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The resolved cache path has exactly base and field, and the arity-checked zip inserted its typed parameter base.", tag = "equivalent")]
        let cache = project(&arguments, prefix.cache())?;
        let Value::PopulationCache(entries) = machine.values.get(&cache)? else {
            return None;
        };
        let guard = if let Some(path) = prefix.empty_guard() {
            let Value::Bool(empty) = machine.values.get(
                // #[gamma::skip(try.propagate_to_unwrap, reason = "The resolved guard path has exactly base and field, and the arity-checked zip inserted its typed parameter base.", tag = "equivalent")]
                &project(&arguments, path)?,
            )?
            else {
                return None;
            };
            Some(*empty)
        } else {
            None
        };
        let verdict = prefix.hit(ordinals, entries, guard, &mut machine.budget)?;
        Some(Value::MaybeBool(verdict))
    }
}

fn project(arguments: &HashMap<String, String>, path: &[String]) -> Option<String> {
    let [base, field] = path else {
        return None; // #[gamma::skip(option.none_to_some, reason = "The only callers supply resolved cache/guard field paths certified to contain exactly base and field.", tag = "equivalent")]
    };
    Some(format!("{}.{field}", arguments.get(base)?))
}
