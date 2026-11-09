// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-backed exact entry states for bounded population-cache retry proofs.

use syn::spanned::Spanned as _;
use syn::{Expr, Item, Pat, Stmt, Type};

use crate::ops::collect::collector::resource::Binding;
use crate::ops::collect::collector::resource::fixed::retry::{CacheEntries, CachePrefix, Effects};
use crate::ops::collect::collector::resource::fixed::{self};
use crate::{HashMap, HashSet};

// Conservative source/analysis caps, not Rust limits or measured capacities.
// Raising them admits larger inputs at greater traversal/storage cost; lowering
// them withholds more proofs. Depth exhaustion leaves a constant unknown.
const MAX_CALLS: usize = 128;
const MAX_LOOP_SPAN_BYTES: usize = 4096;
const MAX_FILE_ITEMS: usize = 64;
const CONSTANT_DEPTH_BUDGET: usize = 64;
const MAX_BINDINGS: usize = 64;
const MAX_STATEMENTS: usize = 64;
// Small-count policy leaves arithmetic headroom in u16; raising it admits
// larger constants but does not bypass checked multiplication.
const MAX_COUNT: u16 = 32767;

#[path = "retry/entry.rs"]
mod entry;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retry/tests.rs"]
mod tests;

/// A closed local declaration scope and entry state, never a campaign-site allowlist.
pub(in super::super) struct Retry {
    calls: HashMap<String, CachePrefix>,
    bindings: HashMap<String, Binding>,
    seeds: HashMap<String, Expr>,
    populations: HashMap<String, Vec<u32>>,
    caches: HashMap<String, CacheEntries>,
    counts: HashMap<String, u16>,
    scalars: HashMap<String, syn::ImplItemFn>,
    ordinals: HashSet<String>,
    globals: HashMap<String, u16>,
}

impl Retry {
    /// Requires complete local declarations and constant, effect-free entry construction.
    ///
    /// Missing field layouts, helper bodies, imports, configurations, aliases, dynamic entry
    /// caches, or intervening unknown effects withhold the proof rather than inventing snapshots.
    pub(in super::super) fn new(file: &syn::File, expression: &Expr) -> Option<Self> {
        struct Calls(Vec<syn::ExprMethodCall>);
        impl<'ast> syn::visit::Visit<'ast> for Calls {
            fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
                if self.0.len() < MAX_CALLS {
                    self.0.push(call.clone());
                    syn::visit::visit_expr_method_call(self, call);
                }
            }
        }
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        let Expr::While(loop_) = expression else { return None };
        if
        // #[gamma::skip(logical.or_remove_right, reason = "Required successful cache-prefix resolution independently rejects more than 64 file items in Scope::new.", tag = "equivalent")]
        loop_.span().byte_range().len() > MAX_LOOP_SPAN_BYTES
            || file.items.len() >
                // #[gamma::skip(literal.int_increment, reason = "Required successful cache-prefix resolution independently rejects more than 64 file items in Scope::new.", tag = "equivalent")]
                MAX_FILE_ITEMS
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let (owner, signature, block) = enclosing(file, expression)?;
        let Some(syn::FnArg::Receiver(receiver)) = signature.inputs.first() else {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        };
        if !signature.generics.params.is_empty()
            || signature.generics.where_clause.is_some()
            || signature.asyncness.is_some()
            || signature.abi.is_some()
            || !matches!(signature.safety, syn::Safety::Default)
            || !receiver.attrs.is_empty()
            || !matches!(receiver.kind, syn::ReceiverKind::Reference(_, _, Some(_)))
            || !fixed::retry::shadow_free(block)
        {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        let mut result = Self {
            calls: HashMap::default(),
            bindings: HashMap::default(),
            seeds: HashMap::default(),
            populations: HashMap::default(),
            caches: HashMap::default(),
            counts: HashMap::default(),
            scalars: HashMap::default(),
            ordinals: HashSet::default(),
            globals: HashMap::default(),
        };
        let mut calls = Calls(Vec::new());
        syn::visit::Visit::visit_expr(&mut calls, expression);
        for call in calls.0 {
            if entry::name(&call.receiver).as_deref() == Some("self")
                && let Some(prefix) = CachePrefix::resolve(file, &owner, &call.method.to_string())
            {
                let _new = result.ordinals.insert(prefix.population().1.to_owned());
                let _old = result.calls.insert(format!("self.{}", call.method), prefix);
            }
        }
        // #[gamma::skip(logical.or_remove_left, reason = "Each resolved call inserts its ordinal at the same time; one ordinal implies a nonempty calls map.", tag = "equivalent")]
        if result.calls.is_empty() || result.ordinals.len() != 1 {
            // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
            return None;
        }
        // Successful prefix resolution establishes the complete closed declaration scope,
        // including the absence of imported/generated constructor and method shadowing.
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding check establishes exactly one ordinal.", tag = "equivalent")]
        let ordinal = result.ordinals.iter().next()?.clone();
        let mut state = entry::State::new(file, &owner, signature, &ordinal)?;
        for statement in &block.stmts {
            if statement.span().byte_range() == expression.span().byte_range() {
                break;
            }
            // #[gamma::skip(cond.always_false, relational.ge_to_gt, reason = "enclosing requires this exact top-level statement; ordered parsed spans reach it and break before this check can reject.", tag = "equivalent")]
            if statement.span().byte_range().start >= expression.span().byte_range().start {
                // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                return None;
            }
            state.statement(statement)?;
        }
        state.install(&mut result)?;
        for item in &file.items {
            match item {
                Item::Const(constant)
                    // #[gamma::skip(match_guard.always_true, reason = "Required prefix resolution already rejects every attributed constant in Scope::new.", tag = "equivalent")]
                    if constant.attrs.is_empty() => {
                    if let Some(count) = constant_count(&constant.expr, &result.globals, CONSTANT_DEPTH_BUDGET) {
                        let name = constant.ident.to_string();
                        let _old = result.globals.insert(name.clone(), count);
                        if !result.bindings.contains_key(&name) {
                            let _old = result.bindings.insert(name.clone(), Binding::Integer);
                            let _old = result.counts.insert(name, count);
                        }
                    }
                }
                Item::Impl(implementation) => {
                    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                    let Type::Path(ty) = &*implementation.self_ty else { return None };
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "Required prefix resolution already certifies every impl owner as a bare identifier.", tag = "equivalent")]
                    let target = ty.path.get_ident()?.to_string();
                    for item in &implementation.items {
                        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
                        let syn::ImplItem::Fn(function) = item else { return None };
                        if scalar(function) {
                            let name = function.sig.ident.to_string();
                            let _old = result.scalars.insert(format!("{target} :: {name}"), function.clone());
                            if target == owner {
                                let _old = result.scalars.insert(format!("Self :: {name}"), function.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (result.bindings.len() <= MAX_BINDINGS).then_some(result)
    }

    pub(in super::super) fn proves(&self, original: &Expr, mutated: &Expr) -> bool {
        let effects = Effects {
            calls: self.calls.iter().map(|(name, prefix)| (name.clone(), prefix)).collect(),
            populations: self.populations.clone(),
            caches: self.caches.clone(),
            counts: self.counts.clone(),
            scalars: self.scalars.clone(),
            ordinals: self.ordinals.clone(),
            some_constructor: Some("Some".into()),
            globals: self.globals.clone(),
        };
        let proves = |expression| {
            fixed::proves_with_retry_effects(
                original,
                expression,
                &self.bindings,
                &self.seeds,
                &HashMap::default(),
                true,
                &HashMap::default(),
                Some(&effects),
            )
        };
        !proves(original) && proves(mutated)
    }
}

fn constant_count(expression: &Expr, known: &HashMap<String, u16>, budget: usize) -> Option<u16> {
    let budget = budget.checked_sub(1)?;
    if let Some(value) = entry::small(expression) {
        return Some(value);
    }
    if let Some(name) = entry::name(expression) {
        return known.get(&name).copied();
    }
    // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
    let Expr::Binary(binary) = expression else { return None };
    if !binary.attrs.is_empty() || !matches!(binary.op, syn::BinOp::Mul(_)) {
        // #[gamma::skip(return_value.default, tag = "equivalent", reason = "None is already the default Option value.")]
        return None;
    }
    let left = constant_count(&binary.left, known, budget)?;
    let right = constant_count(&binary.right, known, budget)?;
    let value = left.checked_mul(right)?;
    (value <= MAX_COUNT).then_some(value)
}

fn scalar(function: &syn::ImplItemFn) -> bool {
    function.attrs.is_empty()
        && function.sig.generics.params.is_empty()
        && function.sig.generics.where_clause.is_none()
        && function.sig.asyncness.is_none()
        && matches!(function.sig.safety, syn::Safety::Default)
        && (
            // #[gamma::skip(logical.and_remove_right, reason = "Supported variadic definitions require an extern ABI, already excluded by the left conjunct.", tag = "equivalent")]
            function.sig.abi.is_none() && function.sig.variadic.is_none()
        )
        && function.sig.inputs.iter().all(|input| {
            matches!(input, syn::FnArg::Typed(argument) if argument.attrs.is_empty()
                && matches!(&*argument.pat, Pat::Ident(pattern) if pattern.attrs.is_empty() && pattern.by_ref.is_none() && pattern.subpat.is_none())
                && matches!(&*argument.ty, Type::Path(path) if path.qself.is_none() && (path.path.is_ident("usize") || path.path.is_ident("bool"))))
        })
        && matches!(function.block.stmts.as_slice(), [Stmt::Expr(_, None)])
}

fn enclosing<'a>(file: &'a syn::File, expression: &Expr) -> Option<(String, &'a syn::Signature, &'a syn::Block)> {
    let span = expression.span().byte_range();
    for item in &file.items {
        let Item::Impl(implementation) = item else { continue };
        let Type::Path(owner) = &*implementation.self_ty else {
            // #[gamma::skip(loop.continue_to_break, reason = "Required Scope::new resolution rejects any non-path impl owner anywhere in the file.", tag = "equivalent")]
            continue;
        };
        for item in &implementation.items {
            if let syn::ImplItem::Fn(function) = item
                && function.attrs.is_empty()
                && function.block.stmts.len() <= MAX_STATEMENTS
                && function.block.stmts.iter().any(|statement| statement.span().byte_range() == span)
            {
                // #[gamma::skip(try.propagate_to_unwrap, reason = "Required Scope::new resolution already certifies every impl owner as a bare identifier.", tag = "equivalent")]
                return Some((owner.path.get_ident()?.to_string(), &function.sig, &function.block));
            }
        }
    }
    None
}
