// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded primitive and owned-vector loop-state interpretation.

#![expect(clippy::renamed_function_params, reason = "syn names all visitor parameters i")]

use core::num::NonZeroU8;
use core::ops::Range;

use quote::ToTokens as _;
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{BinOp, Block, Expr, Lit, Pat, Stmt, Type, UnOp};

use super::{Binding, bare_name};
use crate::{HashMap, HashSet};

mod cache;
mod paths;
mod ranges;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod boundary_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum Primitive {
    Unsigned(u8),
    Signed(u8),
    Float(u8),
}

impl Primitive {
    pub(in super::super) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "u8" => Self::Unsigned(8),
            "u16" => Self::Unsigned(16),
            "u32" => Self::Unsigned(32),
            "u64" => Self::Unsigned(64),
            "u128" => Self::Unsigned(128),
            "i8" => Self::Signed(8),
            "i16" => Self::Signed(16),
            "i32" => Self::Signed(32),
            "i64" => Self::Signed(64),
            "i128" => Self::Signed(128),
            "f32" => Self::Float(32),
            "f64" => Self::Float(64),
            // Target pointer width is not a host property.
            _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        })
    }

    fn mask(self) -> Option<u128> {
        let (Self::Unsigned(width) | Self::Signed(width)) = self else {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        };
        Some(u128::MAX >> (128 - width))
    }
}

/// Abstract loop values: concrete supported scalars, stable symbolic identities, or bounded ranges.
///
/// Unsupported expressions never acquire a value and therefore cannot establish an exclusion.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Bool(bool),
    IntegerZero,
    IntegerSmall(NonZeroU8),
    CharReference(char),
    MaybeChar(char),
    BoolCache(Vec<(u32, bool)>),
    Int(u128, Primitive),
    Float(u64, Primitive),
    UnsignedRange(u128, u128, Primitive, String),
    FiniteRange(u64, u64, Primitive, String),
    Char(char),
    Symbol(String, Binding),
}

impl Value {
    /// Keeps untyped literal and arithmetic evidence representable in every supported integer type.
    fn integer(value: u8) -> Option<Self> {
        (value <= 127).then(|| NonZeroU8::new(value).map_or(Self::IntegerZero, Self::IntegerSmall))
    }

    fn kind(&self) -> Binding {
        match self {
            Self::Bool(_) => Binding::Bool,
            Self::IntegerZero | Self::IntegerSmall(_) => Binding::Integer,
            Self::CharReference(_) => Binding::CharReference,
            Self::MaybeChar(_) => Binding::MaybeChar,
            Self::BoolCache(_) => Binding::BoolCache,
            Self::Char(_) => Binding::Char,
            Self::Int(_, primitive)
            | Self::Float(_, primitive)
            | Self::UnsignedRange(_, _, primitive, _)
            | Self::FiniteRange(_, _, primitive, _) => Binding::Primitive(*primitive),
            Self::Symbol(_, kind) => *kind,
        }
    }
}

/// Finds bounded consecutive constant declarations that still hold at the target loop's entry.
pub(super) fn initializers(file: &syn::File, target: &Range<usize>) -> HashMap<String, Expr> {
    struct Find<'a> {
        target: &'a Range<usize>,
        found: HashMap<String, Expr>,
        budget: usize,
    }
    impl<'ast> Visit<'ast> for Find<'_> {
        fn visit_file(&mut self, file: &'ast syn::File) {
            if file.items.len() <= 256 {
                visit::visit_file(self, file);
            }
        }
        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            if
            // #[gamma::skip(logical.and_remove_left, reason = "Every reached block independently checks target containment; unrelated modules consume no block budget.", tag = "equivalent")]
            module.span().byte_range().contains(&self.target.start)
                && module.content.as_ref().is_some_and(|(_, items)| items.len() <= 256)
            {
                visit::visit_item_mod(self, module);
            }
        }
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            self.visit_block(&function.block);
        }
        fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
            self.visit_block(&function.block);
        }
        fn visit_block(&mut self, block: &'ast Block) {
            if self.budget == 0 || !block.span().byte_range().contains(&self.target.start) {
                return;
            }
            self.budget -= 1;
            let mut seeds = HashMap::default();
            for statement in block.stmts.iter().take(64) {
                let span = statement.span().byte_range();
                if
                // #[gamma::skip(logical.and_remove_left, reason = "The first source-ordered statement ending after this whole target is its containing statement.", tag = "equivalent")]
                span.start <= self.target.start && span.end >= self.target.end {
                    self.found = seeds;
                    visit::visit_stmt(self, statement);
                    return;
                }
                // Calls or control flow before the loop can invalidate earlier values through
                // aliases. Only consecutive scalar declarations are carried into the proof.
                if let Stmt::Local(local) = statement
                    && local.attrs.is_empty()
                    && let Some(init) = &local.init
                    && init.diverge.is_none()
                {
                    let pattern = match &local.pat {
                        Pat::Type(pat) => &*pat.pat,
                        pattern => pattern,
                    };
                    if let Pat::Ident(pat) = pattern
                        && pat.by_ref.is_none()
                        && pat.subpat.is_none()
                        && init.expr.span().byte_range().len() <= 1024
                        && constant_syntax(&init.expr)
                    {
                        let _prior = seeds.insert(pat.ident.to_string(), (*init.expr).clone());
                        continue;
                    }
                }
                seeds.clear();
            }
        }
    }
    let mut find = Find {
        target,
        found: HashMap::default(),
        budget: 64,
    };
    find.visit_file(file);
    find.found
}

fn constant_syntax(expression: &Expr) -> bool {
    match expression {
        Expr::Lit(_) => true,
        Expr::Unary(node) => constant_syntax(&node.expr),
        Expr::Paren(node) => constant_syntax(&node.expr),
        Expr::Cast(node) => constant_syntax(&node.expr),
        Expr::Array(node) => node.elems.iter().all(constant_syntax),
        Expr::Tuple(node) => node.elems.iter().all(constant_syntax),
        Expr::Call(node) if cache::constructor(&node.func) => node.args.len() == 1 && constant_syntax(&node.args[0]),
        _ => false,
    }
}

fn place_name(expression: &Expr) -> Option<String> {
    if let Expr::Field(field) = expression
        && field.attrs.is_empty()
    {
        let base = place_name(&field.base)?;
        return Some(format!("{base}.{}", field.member.to_token_stream()));
    }
    bare_name(expression)
}

/// Associates bounded field-projection identities with the source expressions supplying evidence.
pub(in super::super) fn projection_sites(expression: &Expr) -> Vec<(String, Expr)> {
    struct Fields {
        sites: Vec<(String, Expr)>,
    }
    impl<'ast> Visit<'ast> for Fields {
        fn visit_expr_field(&mut self, field: &'ast syn::ExprField) {
            if self.sites.len() == 64 {
                return;
            }
            let expression = Expr::Field(field.clone());
            if let Some(name) = place_name(&expression) {
                self.sites.push((name, expression));
            }
            visit::visit_expr_field(self, field);
        }
    }
    let mut fields = Fields { sites: Vec::new() };
    fields.visit_expr(expression);
    fields.sites
}

/// Retains small direct-call declarations only where local shadowing can be checked completely.
pub(super) fn helpers(file: &syn::File, target: &Range<usize>) -> HashMap<String, syn::ItemFn> {
    struct Shadows {
        names: HashSet<String>,
        opaque: bool,
        budget: usize,
    }
    impl<'ast> Visit<'ast> for Shadows {
        fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
            if self.budget == 0 {
                self.opaque = true;
                return;
            }
            self.budget -= 1;
            let _new = self.names.insert(node.ident.to_string());
            visit::visit_pat_ident(self, node);
        }
        fn visit_item(&mut self, _: &'ast syn::Item) {
            self.opaque = true;
        }
        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.opaque = true;
        }
        fn visit_expr(&mut self, expression: &'ast Expr) {
            if self.budget == 0 {
                self.opaque = true;
                return;
            }
            self.budget -= 1;
            visit::visit_expr(self, expression);
        }
    }
    if file.items.len() > 64 || file.items.iter().any(|item| !matches!(item, syn::Item::Fn(_))) {
        return HashMap::default();
    }
    let mut shadows = Shadows {
        names: HashSet::default(),
        opaque: false,
        budget: 256,
    };
    for item in &file.items {
        if let syn::Item::Fn(function) = item
            && function.block.span().byte_range().contains(&target.start)
        {
            shadows.visit_signature(&function.sig);
            shadows.visit_block(&function.block);
        }
    }
    if shadows.opaque {
        return HashMap::default();
    }
    file.items
        .iter()
        .filter_map(|item| {
            let syn::Item::Fn(function) = item else { return None };
            let name = function.sig.ident.to_string();
            (function.attrs.is_empty()
                && !shadows.names.contains(&name)
                && function.sig.generics.params.is_empty()
                && function.sig.asyncness.is_none()
                && matches!(function.sig.safety, syn::Safety::Default)
                && function.sig.abi.is_none()
                && function.span().byte_range().len() <= 1024)
                .then(|| (name, function.clone()))
        })
        .collect()
}

/// Establishes whether primitive-looking names have their built-in meaning in the target's scope.
///
/// Opaque syntax or exhausted inspection withholds primitive arithmetic and cast semantics.
pub(super) fn numeric_names(file: &syn::File, target: &Range<usize>) -> bool {
    struct Check<'a> {
        target: &'a Range<usize>,
        valid: bool,
        budget: usize,
    }
    impl<'ast> Visit<'ast> for Check<'_> {
        fn visit_file(&mut self, file: &'ast syn::File) {
            if
            // #[gamma::skip(cond.always_false, relational.gt_to_ge, reason = "The shared 256-visit budget rejects these item-count boundaries before any proof can succeed.", tag = "equivalent")]
            file.items.len() >
                // #[gamma::skip(literal.int_decrement, literal.int_increment, reason = "The shared 256-visit budget rejects these item-count boundaries before any proof can succeed.", tag = "equivalent")]
                256
            {
                self.valid = false;
                return;
            }
            visit::visit_file(self, file);
        }
        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            if
            // #[gamma::skip(cond.always_false, reason = "The shared 256-visit budget rejects these item-count boundaries before any proof can succeed.", tag = "equivalent")]
            module.content.as_ref().is_some_and(|(_, items)|
                // #[gamma::skip(relational.gt_to_ge, reason = "The shared 256-visit budget rejects these item-count boundaries before any proof can succeed.", tag = "equivalent")]
                items.len() >
                // #[gamma::skip(literal.int_decrement, literal.int_increment, reason = "The shared 256-visit budget rejects these item-count boundaries before any proof can succeed.", tag = "equivalent")]
                256)
            {
                self.valid = false;
                return;
            }
            visit::visit_item_mod(self, module);
        }
        fn visit_item(&mut self, item: &'ast syn::Item) {
            if self.budget == 0 {
                self.valid = false;
                return;
            }
            self.budget -= 1;
            match item {
                syn::Item::Fn(function) if !function.span().byte_range().contains(&self.target.start) => {}
                syn::Item::Mod(module) if !module.span().byte_range().contains(&self.target.start) => self.check(&module.ident),
                syn::Item::Mod(module) => {
                    self.check(&module.ident);
                    visit::visit_item(self, item);
                }
                syn::Item::ExternCrate(item) => self.check(item.rename.as_ref().map_or(&item.ident, |(_, name)| name)),
                syn::Item::Type(alias) => self.check(&alias.ident),
                syn::Item::Struct(item) => self.check(&item.ident),
                syn::Item::Enum(item) => self.check(&item.ident),
                syn::Item::Union(item) => self.check(&item.ident),
                syn::Item::Trait(item) => self.check(&item.ident),
                _ => visit::visit_item(self, item),
            }
        }
        fn visit_use_tree(&mut self, tree: &'ast syn::UseTree) {
            match tree {
                syn::UseTree::Glob(_) => self.valid = false,
                syn::UseTree::Name(name) => self.check(&name.ident),
                syn::UseTree::Rename(rename) => self.check(&rename.rename),
                _ => visit::visit_use_tree(self, tree),
            }
        }
        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.valid = false;
        }
        fn visit_type_param(&mut self, parameter: &'ast syn::TypeParam) {
            self.check(&parameter.ident);
            visit::visit_type_param(self, parameter);
        }
        fn visit_expr(&mut self, expression: &'ast Expr) {
            if self.budget == 0 {
                self.valid = false;
                return;
            }
            self.budget -= 1;
            visit::visit_expr(self, expression);
        }
    }
    impl Check<'_> {
        fn check(&mut self, ident: &syn::Ident) {
            if Primitive::named(&ident.to_string()).is_some() || ident == "bool" || ident == "Default" {
                self.valid = false;
            }
        }
    }
    let mut check = Check {
        target,
        valid: true,
        budget: 256,
    };
    check.visit_file(file);
    check.valid
}

#[cfg(test)]
pub(super) fn proves(
    original: &Expr,
    mutated: &Expr,
    bindings: &HashMap<String, Binding>,
    seeds: &HashMap<String, Expr>,
    helpers: &HashMap<String, syn::ItemFn>,
    numeric_names: bool,
) -> bool {
    proves_with_effects(original, mutated, bindings, seeds, helpers, numeric_names, &HashMap::default())
}

/// Checks entry feasibility, loop recurrence, and replayed path assumptions under certified effects.
///
/// Unknown evaluation or exhausted work withholds the proof; helper evidence is immutable.
#[expect(
    clippy::too_many_lines,
    reason = "entry, recurrence, and bounded replay obligations form one proof"
)]
pub(super) fn proves_with_effects(
    original: &Expr,
    mutated: &Expr,
    bindings: &HashMap<String, Binding>,
    seeds: &HashMap<String, Expr>,
    helpers: &HashMap<String, syn::ItemFn>,
    numeric_names: bool,
    stable_calls: &HashMap<String, super::StableCall>,
) -> bool {
    let (Expr::While(original), Expr::While(mutated)) = (original, mutated) else {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    };
    // Condition changes can introduce finite wrong-result paths, even when another input spins.
    if original.cond.to_token_stream().to_string() != mutated.cond.to_token_stream().to_string()
        || !mutated.attrs.is_empty()
        || mutated.label.is_some()
    {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    let mut machine = Machine {
        values: bindings
            .iter()
            .map(|(name, kind)| {
                let kind = if !numeric_names && matches!(kind, Binding::Primitive(_)) {
                    Binding::Integer
                } else {
                    *kind
                };
                (name.clone(), Value::Symbol(name.clone(), kind))
            })
            .collect(),
        budget: 256,
        writes: HashSet::default(),
        locals: HashSet::default(),
        facts: HashMap::default(),
        helpers: Some(helpers),
        restarts: false,
        numeric_names,
        stable_calls: Some(stable_calls),
        paths: HashMap::default(),
        assumptions: Vec::new(),
    };
    for (name, expression) in seeds {
        if let Some(kind) = machine.values.get(name).map(Value::kind)
            && let Some(value) = machine.eval(expression, Some(kind))
        {
            let _prior = machine.values.insert(name.clone(), value);
        }
    }
    let Some(before) = machine.eval(
        &mutated.cond,
        // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
        Some(Binding::Bool),
    ) else {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    };
    if before == Value::Bool(false) || before.kind() != Binding::Bool {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    let initial = machine.values.clone();
    let mut selector = None;
    let mut pattern_binding = None;
    let (original_body, body) = if let Some((scrutinee, pattern, original_body, body)) = changed_arm(&original.body, &mutated.body) {
        let Some(value @ Value::Symbol(_, Binding::Char)) = machine.eval(scrutinee, None) else {
            return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
        };
        let Some(fact) = identity(format_args!("{value:?}")) else {
            return false; // #[gamma::skip(literal.bool_flip, return_value.default, reason = "The Char selector was already evaluated with the same bounded identity before this check. false is the bool default.", tag = "equivalent")]
        };
        match pattern {
            Pat::Lit(pattern) => {
                let Lit::Char(character) = &pattern.lit else { return false }; // #[gamma::skip(literal.bool_flip, return_value.default, reason = "changed_arm already restricts patterns to Char literals, identifiers, and wildcards. false is the bool default.", tag = "equivalent")]
                let _prior = machine.facts.insert(fact, Value::Char(character.value()));
            }
            Pat::Ident(pattern) => {
                let name = pattern.ident.to_string();
                let previous = machine.values.insert(name.clone(), value.clone());
                pattern_binding = Some((name, previous));
            }
            Pat::Wild(_) => {}
            _ => return false, // #[gamma::skip(literal.bool_flip, return_value.default, reason = "changed_arm already restricts patterns to Char literals, identifiers, and wildcards. false is the bool default.", tag = "equivalent")]
        }
        selector = Some((scrutinee, value));
        (original_body, body)
    } else {
        (&original.body, &mutated.body)
    };
    machine.paths = paths::conditions(original_body, body);
    let branch_facts = machine.facts.clone();
    if machine.block(body).is_none() {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    if let Some((name, previous)) = pattern_binding {
        if let Some(previous) = previous {
            let _prior = machine.values.insert(name, previous);
        } else {
            let _removed = machine.values.remove(&name);
        }
    }
    let focused = selector.is_some();
    if let Some((expression, expected)) = selector {
        machine.facts.clear();
        if machine.eval(expression, None).as_ref() != Some(&expected) {
            return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
        }
    }
    machine.facts.clear();
    if machine
        .eval(
            &mutated.cond,
            // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
            Some(Binding::Bool),
        )
        .as_ref()
        != Some(&before)
    {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    // Every outer scalar write must itself be a fixed point, not merely leave the current
    // condition unchanged. A hidden advancing state must not be mistaken for a recurrence.
    let fixed = machine.writes.iter().all(|name| {
        machine.values.get(name) == initial.get(name)
            || matches!(
                initial.get(name).map(Value::kind),
                Some(Binding::Vec | Binding::VecChar | Binding::VecString)
            )
    });
    if fixed {
        if !machine.assumptions.is_empty() {
            let assumptions = core::mem::take(&mut machine.assumptions);
            machine.facts = branch_facts;
            machine.restarts = false;
            let before_values = machine.values.clone();
            if machine.block(body).is_none() || machine.assumptions != assumptions {
                return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
            }
            machine.facts.clear();
            return machine
                .eval(
                    &mutated.cond,
                    // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
                    Some(Binding::Bool),
                )
                .as_ref()
                == Some(&before)
                && machine.writes.iter().all(|name| {
                    machine.values.get(name) == before_values.get(name)
                        || matches!(
                            before_values.get(name).map(Value::kind),
                            Some(Binding::Vec | Binding::VecChar | Binding::VecString)
                        )
                });
        }
        return true;
    }
    if focused {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    // One bounded warm-up may establish an inductive scalar invariant. Interval identities
    // remain distinct in predicates, so changing a ranged guard cannot masquerade as stable.
    let invariant = machine.values.clone();
    machine.restarts = false;
    if machine.block(body).is_none()
        || machine
            .eval(
                &mutated.cond,
                // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
                Some(Binding::Bool),
            )
            .as_ref()
            != Some(&before)
    {
        return false; // #[gamma::skip(return_value.default, reason = "false is the bool default.", tag = "equivalent")]
    }
    machine.writes.iter().all(|name| {
        match (invariant.get(name), machine.values.get(name)) {
            (Some(before), Some(after)) => ranges::same_invariant(before, after),
            (None, None) => true,
            _ => false, // #[gamma::skip(literal.bool_flip, reason = "Successful blocks preserve outer keys and restore or remove temporary keys on both snapshots.", tag = "equivalent")]
        }
    })
}

/// Focuses a single edited character-match arm without importing facts from unsupported patterns.
fn changed_arm<'a>(original: &'a Block, mutated: &'a Block) -> Option<(&'a Expr, &'a Pat, &'a Block, &'a Block)> {
    let ([Stmt::Expr(Expr::Match(original), _)], [Stmt::Expr(Expr::Match(mutated), _)]) =
        (original.stmts.as_slice(), mutated.stmts.as_slice())
    else {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    };
    if !mutated.attrs.is_empty() || original.expr != mutated.expr || original.arms.len() != mutated.arms.len() {
        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
    }
    let mut changed = None;
    for (left, right) in original.arms.iter().zip(&mutated.arms) {
        if left.pat != right.pat || !right.attrs.is_empty() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        if !matches!(&right.pat, Pat::Lit(pattern) if pattern.attrs.is_empty() && matches!(pattern.lit, Lit::Char(_)))
            && !matches!(&right.pat, Pat::Ident(pattern) if pattern.attrs.is_empty() && pattern.subpat.is_none() && pattern.by_ref.is_none())
            && !matches!(&right.pat, Pat::Wild(pattern) if pattern.attrs.is_empty())
        {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        if left.body == right.body {
            continue;
        }
        if changed.is_some() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let Expr::Block(body) = &*right.body else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        let Expr::Block(original_body) = &*left.body else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        if !body.attrs.is_empty() || body.label.is_some() {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        changed = Some((&*mutated.expr, &right.pat, &original_body.block, &body.block));
    }
    changed
}

/// One proof's mutable abstract state, with borrowed helper evidence and a shared work budget.
///
/// Writes and locals distinguish outer recurrence obligations from temporary bindings; facts and
/// assumptions track path-sensitive evidence that must remain stable when the loop is replayed.
/// Absent helper maps admit no helper calls, preserving the nonrecursive helper-evaluation boundary.
struct Machine<'a> {
    values: HashMap<String, Value>,
    budget: usize,
    writes: HashSet<String>,
    locals: HashSet<String>,
    facts: HashMap<String, Value>,
    helpers: Option<&'a HashMap<String, syn::ItemFn>>,
    restarts: bool,
    numeric_names: bool,
    stable_calls: Option<&'a HashMap<String, super::StableCall>>,
    paths: HashMap<String, bool>,
    assumptions: Vec<(String, Value, bool)>,
}

impl Machine<'_> {
    fn spend(&mut self) -> Option<()> {
        self.budget = self.budget.checked_sub(1)?;
        Some(())
    }

    /// Restores shadowed locals after supported effects; failure abandons the proof.
    fn block(&mut self, block: &Block) -> Option<()> {
        let saved_locals = self.locals.clone();
        let mut declared = Vec::new();
        for statement in &block.stmts {
            self.spend()?;
            match statement {
                Stmt::Local(local) if inert_attributes(&local.attrs) => {
                    let (pattern, kind) = match &local.pat {
                        Pat::Type(pat) if self.numeric_names => (&*pat.pat, primitive_type(&pat.ty).map(Binding::Primitive)),
                        pattern => (pattern, None),
                    };
                    let Pat::Ident(pat) = pattern else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    if pat.by_ref.is_some() || pat.subpat.is_some() {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let init = local.init.as_ref()?;
                    if init.diverge.is_some() {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let value = self.eval(&init.expr, kind)?;
                    let name = pat.ident.to_string();
                    let previous = self.values.get(&name).cloned();
                    let prefix = format!("{name}.");
                    let projections = self
                        .values
                        .iter()
                        .filter(|(place, _)| place.starts_with(&prefix))
                        .map(|(place, value)| (place.clone(), value.clone()))
                        .collect::<Vec<_>>();
                    self.values
                        .retain(|key, _| !key.strip_prefix(&name).is_some_and(|tail| tail.starts_with('.')));
                    declared.push((name.clone(), previous, projections));
                    let _new = self.locals.insert(name.clone());
                    let _prior = self.values.insert(name, value);
                }
                Stmt::Expr(Expr::Continue(node), _) if node.attrs.is_empty() && node.label.is_none() => {
                    self.restarts = true;
                    break;
                }
                Stmt::Expr(expression, _) => {
                    self.effect(expression)?;
                    if self.restarts {
                        break;
                    }
                }
                _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            }
        }
        for (name, previous, projections) in declared.into_iter().rev() {
            for (place, value) in projections {
                let _prior = self.values.insert(place, value);
            }
            if let Some(value) = previous {
                let _prior = self.values.insert(name, value);
            } else {
                let _removed = self.values.remove(&name);
            }
        }
        self.locals = saved_locals;
        Some(())
    }

    /// Updates modeled state; unknown branches must agree on their effects to retain a proof.
    fn effect(&mut self, expression: &Expr) -> Option<()> {
        self.spend()?;
        match expression {
            Expr::Assign(node) if node.attrs.is_empty() => {
                let name = place_name(&node.left)?;
                let kind = self.values.get(&name)?.kind();
                if !matches!(kind, Binding::Bool | Binding::Integer | Binding::Primitive(_)) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let value = self.eval(&node.right, Some(kind))?;
                let _prior = self.values.insert(name.clone(), value);
                let _new = self.writes.insert(name);
                Some(())
            }
            Expr::Binary(node) if node.attrs.is_empty() => {
                let op: BinOp = match node.op {
                    BinOp::AddAssign(_) => syn::parse_quote!(+),
                    BinOp::SubAssign(_) => syn::parse_quote!(-),
                    BinOp::MulAssign(_) => syn::parse_quote!(*),
                    _ => return self.eval(expression, None).map(|_| ()),
                };
                let name = place_name(&node.left)?;
                let left = self.values.get(&name)?.clone();
                let right = self.eval(&node.right, Some(left.kind()))?;
                let value = binary(&op, left, right)?;
                let _prior = self.values.insert(name.clone(), value);
                let _new = self.writes.insert(name);
                Some(())
            }
            Expr::Block(node) if inert_attributes(&node.attrs) && node.label.is_none() => self.block(&node.block),
            Expr::If(node) if node.attrs.is_empty() => match self.guard(&node.cond)? {
                Value::Bool(true) => self.block(&node.then_branch),
                Value::Bool(false) => {
                    if let Some((_, otherwise)) = &node.else_branch {
                        self.effect(otherwise)
                    } else {
                        Some(())
                    }
                }
                _ => {
                    let before = self.values.clone();
                    let writes = self.writes.clone();
                    self.block(&node.then_branch)?;
                    let then_values = self.values.clone();
                    let then_writes = self.writes.clone();
                    let then_restarts = self.restarts;
                    self.values = before;
                    // #[gamma::skip(stmt.delete_assign, reason = "The final union restores both branch write sets; writes does not affect branch evaluation.", tag = "equivalent")]
                    self.writes = writes;
                    self.restarts = false;
                    if let Some((_, otherwise)) = &node.else_branch {
                        self.effect(otherwise)?;
                    }
                    if self.values != then_values || self.restarts != then_restarts {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    self.writes.extend(then_writes);
                    Some(())
                }
            },
            Expr::While(node) if node.attrs.is_empty() && node.label.is_none() => {
                (self.eval(
                    &node.cond,
                    // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
                    Some(Binding::Bool),
                )? == Value::Bool(false))
                .then_some(())
            }
            // Standard owned vector appends cannot change a different vector's length.
            Expr::MethodCall(node)
                if node.attrs.is_empty() && node.turbofish.is_none() && node.method == "push" && node.args.len() == 1 =>
            {
                let name = bare_name(&node.receiver)?;
                let kind = self.values.get(&name)?.kind();
                if !matches!(kind, Binding::Vec | Binding::VecChar | Binding::VecString | Binding::String) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                if !matches!(&node.args[0], Expr::Path(path) if path.attrs.is_empty() && path.qself.is_none() && path.path.segments.len() > 1)
                {
                    let _argument = self.eval(&node.args[0], None)?;
                }
                let _new = self.writes.insert(name.clone());
                let grown = identity(format_args!("{:?}.grown", self.values.get(&name)?))?;
                let _prior = self.values.insert(name, Value::Symbol(grown, kind));
                Some(())
            }
            _ => self.eval(expression, None).map(|_| ()),
        }
    }

    /// Refines an evaluated symbolic identity with any established path-local fact.
    fn eval(&mut self, expression: &Expr, expected: Option<Binding>) -> Option<Value> {
        let value = self.eval_inner(expression, expected)?;
        if matches!(value, Value::Symbol(_, _)) {
            return Some(self.facts.get(&identity(format_args!("{value:?}"))?).cloned().unwrap_or(value));
        }
        Some(value)
    }

    /// Checks a focused path choice and records its value/assumption for recurrence replay.
    fn guard(&mut self, expression: &Expr) -> Option<Value> {
        let value = self.eval(
            expression,
            // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
            Some(Binding::Bool),
        )?;
        let key = expression.to_token_stream().to_string();
        let Some(expected) = self.paths.get(&key).copied() else {
            return Some(value);
        };
        if value.kind() != Binding::Bool || matches!(&value, Value::Bool(value) if *value != expected) {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        self.assumptions.push((key, value.clone(), expected));
        let _old = self.facts.insert(
            // #[gamma::skip(try.propagate_to_unwrap, reason = "Bool values have a fixed identity or the same symbolic identity already checked during evaluation.", tag = "equivalent")]
            identity(format_args!("{value:?}"))?,
            Value::Bool(expected),
        );
        Some(Value::Bool(expected))
    }

    /// Evaluates the closed scalar grammar; unknown syntax or exhausted work returns no evidence.
    #[expect(clippy::too_many_lines, reason = "the closed expression grammar shares one evaluation budget")]
    fn eval_inner(&mut self, expression: &Expr, expected: Option<Binding>) -> Option<Value> {
        self.spend()?;
        match expression {
            Expr::Path(_) => self.values.get(&bare_name(expression)?).cloned(),
            Expr::Field(_) => self.values.get(&place_name(expression)?).cloned(),
            Expr::Paren(node) if node.attrs.is_empty() => self.eval(&node.expr, expected),
            Expr::Group(node) if node.attrs.is_empty() => self.eval(&node.expr, expected),
            Expr::Lit(node) if node.attrs.is_empty() => match &node.lit {
                Lit::Bool(literal) => Some(Value::Bool(literal.value)),
                Lit::Char(literal) => Some(Value::Char(literal.value())),
                Lit::Str(literal) => Some(Value::Symbol(identity(format_args!("literal:{literal:?}"))?, Binding::StrReference)),
                Lit::Int(literal) => {
                    let kind = Primitive::named(literal.suffix()).map(Binding::Primitive).or(expected)?;
                    if kind == Binding::Integer {
                        let value = literal.base10_parse::<u8>().ok()?;
                        return Value::integer(value);
                    }
                    let Binding::Primitive(primitive) = kind else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    let value = literal.base10_parse::<u128>().ok()?;
                    (value <= primitive.mask()?).then_some(Value::Int(value, primitive))
                }
                Lit::Float(literal) => {
                    let primitive = Primitive::named(literal.suffix()).or(match expected {
                        Some(Binding::Primitive(primitive)) => Some(primitive),
                        _ => None,
                    })?;
                    let value = if primitive == Primitive::Float(32) {
                        f64::from(
                            // #[gamma::skip(try.propagate_to_unwrap, reason = "Parsed decimal float digits are valid parser inputs; extreme exponents produce infinity or zero, not errors.", tag = "equivalent")]
                            literal.base10_parse::<f32>().ok()?,
                        )
                    } else {
                        // #[gamma::skip(try.propagate_to_unwrap, reason = "Parsed decimal float digits are valid parser inputs; extreme exponents produce infinity or zero, not errors.", tag = "equivalent")]
                        literal.base10_parse::<f64>().ok()?
                    };
                    float(value, primitive)
                }
                _ => None,
            },
            Expr::Unary(node) if node.attrs.is_empty() => {
                let value = self.eval(&node.expr, expected)?;
                match (node.op, value) {
                    (UnOp::Not(_), Value::Bool(value)) => Some(Value::Bool(!value)),
                    (UnOp::Neg(_), Value::Int(value, primitive @ Primitive::Signed(width))) => {
                        if value == 1u128 << (width - 1) && matches!(&*node.expr, Expr::Lit(literal) if matches!(literal.lit, Lit::Int(_)))
                        {
                            return Some(Value::Int(value, primitive));
                        }
                        let signed = (value << (128 - width)).cast_signed() >> (128 - width);
                        let negated = signed.checked_neg()?;
                        if negated > (i128::MAX >> (128 - width)) {
                            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                        }
                        Some(Value::Int(
                            negated.cast_unsigned() &
                            // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                            primitive.mask()?,
                            primitive,
                        ))
                    }
                    (UnOp::Neg(_), Value::Float(bits, primitive)) => float(-f64::from_bits(bits), primitive),
                    _ => None,
                }
            }
            Expr::Binary(node) if node.attrs.is_empty() => {
                let left = self.eval(&node.left, expected)?;
                if matches!(
                    (&node.op, &left),
                    (BinOp::And(_), Value::Bool(false)) | (BinOp::Or(_), Value::Bool(true))
                ) {
                    return Some(left);
                }
                let right = self.eval(&node.right, Some(left.kind()))?;
                binary(&node.op, left, right)
            }
            Expr::Cast(node) if node.attrs.is_empty() && self.numeric_names => {
                let value = self.eval(&node.expr, None)?;
                cast(value, primitive_type(&node.ty)?)
            }
            Expr::MethodCall(node) if node.attrs.is_empty() && node.turbofish.is_none() && node.args.is_empty() && node.method == "len" => {
                let name = bare_name(&node.receiver)?;
                let value = self.values.get(&name)?;
                if !matches!(
                    value.kind(),
                    Binding::Vec | Binding::VecChar | Binding::VecString | Binding::StrReference | Binding::String
                ) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                Some(Value::Symbol(identity(format_args!("{value:?}.len"))?, Binding::Integer))
            }
            Expr::MethodCall(node)
                if node.attrs.is_empty() && node.turbofish.is_none() && node.method == "expect" && node.args.len() == 1 =>
            {
                if !matches!(&node.args[0], Expr::Lit(literal) if literal.attrs.is_empty() && matches!(literal.lit, Lit::Str(_))) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let Expr::MethodCall(next) = &*node.receiver else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                let Expr::MethodCall(chars) = &*next.receiver else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                if next.method != "next"
                    || !next.attrs.is_empty()
                    || next.turbofish.is_some()
                    || !next.args.is_empty()
                    || chars.method != "chars"
                    || !chars.attrs.is_empty()
                    || chars.turbofish.is_some()
                    || !chars.args.is_empty()
                {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let source = self.eval(
                    &chars.receiver,
                    // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
                    Some(Binding::StrReference),
                )?;
                if source.kind() != Binding::StrReference {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                Some(Value::Symbol(identity(format_args!("{source:?}.first"))?, Binding::Char))
            }
            Expr::MethodCall(node)
                if node.attrs.is_empty()
                    && node.turbofish.is_none()
                    && node.method == "unwrap_or"
                    && node.args.len() == 1
                    && self.stable_calls.is_some_and(|calls| calls.contains_key(super::CHAR_WIDTH)) =>
            {
                let Expr::MethodCall(width) = &*node.receiver else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                if width.method != "width"
                    || !width.attrs.is_empty()
                    || width.turbofish.is_some()
                    || !width.args.is_empty()
                    || self.eval(&node.args[0], Some(Binding::Integer))? != Value::IntegerZero
                {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let character = self.eval(
                    &width.receiver,
                    // #[gamma::skip(option.some_to_none, reason = "This nonprimitive expected-kind hint enables no additional evaluator form; successful values determine their own kind.", tag = "equivalent")]
                    Some(Binding::Char),
                )?;
                if character.kind() != Binding::Char {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                Some(Value::Symbol(
                    identity(format_args!("{character:?}.width_or_zero"))?,
                    Binding::Integer,
                ))
            }
            Expr::MethodCall(node) if node.attrs.is_empty() && node.turbofish.is_none() && node.method == "get" && node.args.len() == 1 => {
                let receiver = self.eval(&node.receiver, None)?;
                if
                // #[gamma::skip(cond.always_false, reason = "Reachable Char facts cannot match non-VecChar indexing identities; the following fact lookup still fails.", tag = "equivalent")]
                receiver.kind() != Binding::VecChar {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let index = self.eval(&node.args[0], Some(Binding::Primitive(Primitive::Unsigned(64))))?;
                let value = Value::Symbol(identity(format_args!("{receiver:?}[{index:?}]"))?, Binding::Char);
                let Value::Char(character) = self.facts.get(&identity(format_args!("{value:?}"))?)?.clone() else {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                };
                Some(Value::MaybeChar(character))
            }
            Expr::MethodCall(node) if node.attrs.is_empty() && node.turbofish.is_none() && node.args.len() == 1 => {
                let left = self.eval(&node.receiver, expected)?;
                let right = self.eval(&node.args[0], Some(left.kind()))?;
                if let Some(value) = ranges::wrapping(&node.method.to_string(), &left, &right) {
                    return Some(value);
                }
                let Value::Int(b, primitive) = right else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                let mask =
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                    primitive.mask()?;
                let method = node.method.to_string();
                if let Value::Int(a, same) = left {
                    if same != primitive {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let value = match method.as_str() {
                        "wrapping_add" => a.wrapping_add(b),
                        "wrapping_sub" => a.wrapping_sub(b),
                        "wrapping_mul" => a.wrapping_mul(b),
                        _ => return None, // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    };
                    Some(Value::Int(value & mask, primitive))
                } else if b == 0 && left.kind() == Binding::Primitive(primitive) {
                    match method.as_str() {
                        "wrapping_add" | "wrapping_sub" => Some(left),
                        "wrapping_mul" => Some(Value::Int(0, primitive)),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            Expr::Index(node) if node.attrs.is_empty() => {
                let receiver = self.eval(&node.expr, None)?;
                if receiver.kind() == Binding::StrReference {
                    let Expr::Range(range) = &*node.index else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    if !range.attrs.is_empty() || range.end.is_some() {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let start = self.eval(range.start.as_deref()?, Some(Binding::Integer))?;
                    return Some(Value::Symbol(
                        identity(format_args!("{receiver:?}[{start:?}..]"))?,
                        Binding::StrReference,
                    ));
                }
                if receiver.kind() == Binding::VecString {
                    let index = self.eval(&node.index, Some(Binding::Integer))?;
                    return Some(Value::Symbol(identity(format_args!("{receiver:?}[{index:?}]"))?, Binding::String));
                }
                if receiver.kind() != Binding::VecChar {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let index = self.eval(&node.index, Some(Binding::Primitive(Primitive::Unsigned(64))))?;
                let value = Value::Symbol(identity(format_args!("{receiver:?}[{index:?}]"))?, Binding::Char);
                Some(self.facts.get(&identity(format_args!("{value:?}"))?).cloned().unwrap_or(value))
            }
            Expr::Reference(reference) if reference.attrs.is_empty() && reference.mutability.is_none() => {
                let value = self.eval(&reference.expr, None)?;
                if matches!(value.kind(), Binding::String | Binding::StrReference) {
                    Some(Value::Symbol(identity(format_args!("&{value:?}"))?, Binding::StrReference))
                } else if let Value::Char(character) = value {
                    Some(Value::CharReference(character))
                } else {
                    None
                }
            }
            Expr::Call(node) if node.attrs.is_empty() => {
                if let Some(call) = self
                    .stable_calls
                    .and_then(|calls| calls.get(&node.func.to_token_stream().to_string()))
                {
                    if call.parameters.len() != node.args.len() {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let mut values = Vec::new();
                    for (argument, &expected) in node.args.iter().zip(&call.parameters) {
                        let value = self.eval(argument, Some(expected))?;
                        if value.kind() != expected {
                            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                        }
                        values.push(value);
                    }
                    if call.char_option {
                        let [Value::CharReference(character)] = values.as_slice() else {
                            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                        };
                        return Some(Value::MaybeChar(*character));
                    }
                    return Some(Value::Symbol(
                        identity(format_args!("{:?}({values:?})", node.func.to_token_stream().to_string()))?,
                        call.output,
                    ));
                }
                if !self.numeric_names {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                if let Some(value) = self.primitive_call(node, expected) {
                    return Some(value);
                }
                if expected == Some(Binding::BoolCache) {
                    return cache::initial_value(node);
                }
                let name = bare_name(&node.func)?;
                if self.values.contains_key(&name) {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let function = self.helpers?.get(&name)?;
                if let Some(value) = cache::hit(self, function, node) {
                    return Some(value);
                }
                if function.sig.inputs.len() != node.args.len() {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let mut values = HashMap::default();
                for (parameter, argument) in function.sig.inputs.iter().zip(&node.args) {
                    let syn::FnArg::Typed(parameter) = parameter else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    let Pat::Ident(pattern) = &*parameter.pat else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    if pattern.by_ref.is_some() || pattern.subpat.is_some() {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let kind = if matches!(&*parameter.ty, Type::Path(path) if path.path.is_ident("bool")) {
                        Binding::Bool
                    } else {
                        Binding::Primitive(primitive_type(&parameter.ty)?)
                    };
                    let value = self.eval(argument, Some(kind))?;
                    if value.kind() != kind {
                        return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                    }
                    let _prior = values.insert(pattern.ident.to_string(), value);
                }
                let syn::ReturnType::Type(_, result) = &function.sig.output else {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                };
                let result = if matches!(&**result, Type::Path(path) if path.path.is_ident("bool")) {
                    Binding::Bool
                } else {
                    Binding::Primitive(primitive_type(result)?)
                };
                let (Stmt::Expr(tail, None), prefix) = function.block.stmts.split_last()? else {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                };
                if !prefix.is_empty() {
                    return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
                }
                let mut nested = Machine {
                    values,
                    budget: self.budget,
                    writes: HashSet::default(),
                    locals: HashSet::default(),
                    facts: HashMap::default(),
                    helpers: None,
                    restarts: false, // #[gamma::skip(literal.bool_flip, reason = "This helper evaluates only a scalar tail; restart state is read by statement execution, not scalar evaluation.", tag = "equivalent")]
                    numeric_names: self.numeric_names,
                    stable_calls: None,
                    paths: HashMap::default(),
                    assumptions: Vec::new(),
                };
                let value = nested.eval(tail, Some(result))?;
                self.budget = nested.budget;
                (value.kind() == result).then_some(value)
            }
            _ => None,
        }
    }

    /// Recognizes primitive conversions only after their names and operand kinds are established.
    fn primitive_call(&mut self, node: &syn::ExprCall, expected: Option<Binding>) -> Option<Value> {
        let Expr::Path(path) = &*node.func else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        if path.qself.is_some() || path.path.leading_colon.is_some() || path.path.segments.len() != 2 {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let mut segments = path.path.segments.iter();
        let owner =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding exact two-segment path check guarantees both iterator elements exist.", tag = "equivalent")]
            segments.next()?;
        let method =
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The preceding exact two-segment path check guarantees both iterator elements exist.", tag = "equivalent")]
            segments.next()?;
        if !matches!(owner.arguments, syn::PathArguments::None) || !matches!(method.arguments, syn::PathArguments::None) {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        if owner.ident == "Default" && method.ident == "default" && node.args.is_empty() {
            let Binding::Primitive(primitive) = expected? else { return None }; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
            return match primitive {
                Primitive::Unsigned(_) | Primitive::Signed(_) => Some(Value::Int(0, primitive)),
                Primitive::Float(_) => float(0.0, primitive),
            };
        }
        if method.ident != "from" || node.args.len() != 1 {
            return None; // #[gamma::skip(return_value.default, reason = "None is the Option default.", tag = "equivalent")]
        }
        let target = Primitive::named(&owner.ident.to_string())?;
        let value = self.eval(&node.args[0], Some(Binding::Primitive(Primitive::Signed(32))))?;
        ranges::from(value, target)
    }
}

fn inert_attributes(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().all(|attribute| {
        attribute
            .path()
            .get_ident()
            .is_some_and(|name| matches!(name.to_string().as_str(), "allow" | "expect" | "warn" | "deny" | "forbid" | "doc"))
    })
}

fn identity(arguments: core::fmt::Arguments<'_>) -> Option<String> {
    let value = arguments.to_string();
    (value.len() <= 8192).then_some(value)
}

mod numeric {
    use super::*;

    pub(super) fn primitive_type(ty: &Type) -> Option<Primitive> {
        let Type::Path(path) = ty else { return None };
        Primitive::named(&path.path.get_ident()?.to_string())
    }

    #[expect(clippy::cast_possible_truncation, reason = "primitive f32 rounding is the operation being modeled")]
    pub(super) fn float(value: f64, primitive: Primitive) -> Option<Value> {
        let value = match primitive {
            Primitive::Float(32) => f64::from(value as f32),
            Primitive::Float(64) => value,
            _ => return None,
        };
        Some(Value::Float(value.to_bits(), primitive))
    }

    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Rust primitive cast rounding and saturation are the operations being modeled"
    )]
    pub(super) fn cast(value: Value, target: Primitive) -> Option<Value> {
        if matches!(value, Value::UnsignedRange(..) | Value::FiniteRange(..)) {
            return ranges::cast(&value, target);
        }
        match (value, target) {
            (Value::Int(value, source), Primitive::Unsigned(_) | Primitive::Signed(_)) => {
                let extended = match source {
                    Primitive::Signed(width) if value & (1 << (width - 1)) != 0 => {
                        value
                            | !
                        // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                        source.mask()?
                    }
                    _ => value,
                };
                Some(Value::Int(
                    extended &
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                    target.mask()?,
                    target,
                ))
            }
            (Value::Int(value, source), Primitive::Float(_)) => {
                if target == Primitive::Float(32) {
                    let value = match source {
                        Primitive::Signed(width) => ((value << (128 - width)).cast_signed() >> (128 - width)) as f32,
                        _ => value as f32,
                    };
                    return float(f64::from(value), target);
                }
                let value = match source {
                    Primitive::Signed(width) => ((value << (128 - width)).cast_signed() >> (128 - width)) as f64,
                    _ => value as f64,
                };
                float(value, target)
            }
            (Value::Float(bits, _), Primitive::Float(_)) => float(f64::from_bits(bits), target),
            (Value::Float(bits, _), Primitive::Unsigned(_)) => Some(Value::Int(
                (f64::from_bits(bits) as u128).min(
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                    target.mask()?,
                ),
                target,
            )),
            (Value::Float(bits, _), Primitive::Signed(width)) => {
                let value = f64::from_bits(bits) as i128;
                let minimum = i128::MIN >> (128 - width);
                let maximum = i128::MAX >> (128 - width);
                Some(Value::Int(
                    value.clamp(minimum, maximum).cast_unsigned() &
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                    target.mask()?,
                    target,
                ))
            }
            _ => None,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the closed abstract arithmetic domain dispatches each operand class once"
    )]
    pub(super) fn binary(op: &BinOp, left: Value, right: Value) -> Option<Value> {
        if matches!(op, BinOp::Sub(_)) && left == right && left.kind() == Binding::Integer {
            return Some(Value::IntegerZero);
        }
        if let (Value::MaybeChar(a), Value::MaybeChar(b)) = (&left, &right) {
            return match op {
                BinOp::Eq(_) => Some(Value::Bool(a == b)),
                BinOp::Ne(_) => Some(Value::Bool(a != b)),
                _ => None,
            };
        }
        let small = |value: &Value| match value {
            Value::IntegerZero => Some(0),
            Value::IntegerSmall(value) => Some(value.get()),
            _ => None,
        };
        if let (Some(a), Some(b)) = (small(&left), small(&right)) {
            let value = match op {
                BinOp::Eq(_) => return Some(Value::Bool(a == b)),
                BinOp::Ne(_) => return Some(Value::Bool(a != b)),
                BinOp::Lt(_) => return Some(Value::Bool(a < b)),
                BinOp::Le(_) => return Some(Value::Bool(a <= b)),
                BinOp::Gt(_) => return Some(Value::Bool(a > b)),
                BinOp::Ge(_) => return Some(Value::Bool(a >= b)),
                BinOp::Add(_) =>
                // #[gamma::skip(try.propagate_to_unwrap, reason = "Both extracted small integers are at most 127, so their u8 sum is at most 254.", tag = "equivalent")]
                {
                    a.checked_add(b)?
                }
                BinOp::Sub(_) => a.checked_sub(b)?,
                BinOp::Mul(_) => a.checked_mul(b)?,
                _ => return None,
            };
            return Value::integer(value);
        }
        if let Some(value) = ranges::binary(op, &left, &right) {
            return Some(value);
        }
        if left.kind() == Binding::Integer && right == Value::IntegerZero && matches!(op, BinOp::Add(_) | BinOp::Sub(_) | BinOp::Mul(_)) {
            return match op {
                BinOp::Add(_) | BinOp::Sub(_) => Some(left),
                BinOp::Mul(_) => Some(Value::IntegerZero),
                _ => None,
            };
        }
        if left == Value::IntegerZero && right.kind() == Binding::Integer {
            match op {
                BinOp::Add(_) => return Some(right),
                BinOp::Mul(_) => return Some(Value::IntegerZero),
                _ => {}
            }
        }
        if left.kind() == Binding::Integer && matches!(right, Value::Int(0, _)) && matches!(op, BinOp::Add(_) | BinOp::Sub(_)) {
            return Some(left);
        }
        if matches!(
            (op, &right),
            (BinOp::And(_), Value::Bool(false)) | (BinOp::Or(_), Value::Bool(true))
        ) && left.kind() == Binding::Bool
        {
            return Some(right);
        }
        if left.kind() == Binding::Bool && right.kind() == Binding::Bool && matches!(op, BinOp::And(_) | BinOp::Or(_)) {
            if let (Value::Bool(a), Value::Bool(_)) = (&left, &right) {
                // Absorbing right operands returned above; only AND true / OR false remain.
                return Some(Value::Bool(*a));
            }
            return Some(Value::Symbol(identity(format_args!("{left:?}{op:?}{right:?}"))?, Binding::Bool));
        }
        if let (Value::Char(a), Value::Char(b)) = (&left, &right) {
            return match op {
                BinOp::Eq(_) => Some(Value::Bool(a == b)),
                BinOp::Ne(_) => Some(Value::Bool(a != b)),
                _ => None,
            };
        }
        if left.kind() != right.kind()
            && !matches!(
                (left.kind(), right.kind()),
                (Binding::Integer, Binding::Primitive(_)) | (Binding::Primitive(_), Binding::Integer)
            )
        {
            return None;
        }
        if matches!(op, BinOp::Mul(_))
            && let Value::Int(0, _) = &left
            && matches!(right.kind(), Binding::Primitive(Primitive::Signed(_) | Primitive::Unsigned(_)))
        {
            return Some(left);
        }
        match (&left, &right) {
            (Value::Int(a, primitive), Value::Int(b, _)) => integer(op, *a, *b, *primitive),
            (Value::Float(a, primitive), Value::Float(b, _)) => floating(op, *a, *b, *primitive),
            _ => {
                if matches!(
                    op,
                    BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_) | BinOp::Eq(_) | BinOp::Ne(_)
                ) {
                    return Some(Value::Symbol(identity(format_args!("{left:?}{op:?}{right:?}"))?, Binding::Bool));
                }
                if matches!(left.kind(), Binding::Primitive(Primitive::Unsigned(_) | Primitive::Signed(_)))
                    && let Value::Int(0, _) = right
                {
                    return match op {
                        BinOp::Add(_) | BinOp::Sub(_) => Some(left),
                        BinOp::Mul(_) => Some(right),
                        _ => None,
                    };
                }
                None
            }
        }
    }

    fn integer(op: &BinOp, a: u128, b: u128, primitive: Primitive) -> Option<Value> {
        let value = match op {
            BinOp::Eq(_) => return Some(Value::Bool(a == b)),
            BinOp::Ne(_) => return Some(Value::Bool(a != b)),
            BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_) => {
                let ordering = if let Primitive::Signed(width) = primitive {
                    let a = (a << (128 - width)).cast_signed() >> (128 - width);
                    let b = (b << (128 - width)).cast_signed() >> (128 - width);
                    a.cmp(&b)
                } else {
                    a.cmp(&b)
                };
                return Some(Value::Bool(match op {
                    BinOp::Lt(_) => ordering.is_lt(),
                    BinOp::Le(_) => ordering.is_le(),
                    BinOp::Gt(_) => ordering.is_gt(),
                    _ => ordering.is_ge(),
                }));
            }
            BinOp::Shl(_) => a.checked_shl(u32::try_from(b).ok()?)? &
                // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                primitive.mask()?,
            BinOp::Shr(_) if matches!(primitive, Primitive::Unsigned(_)) => a.checked_shr(u32::try_from(b).ok()?)?,
            BinOp::Add(_) if matches!(primitive, Primitive::Unsigned(_)) => a.checked_add(b)?,
            BinOp::Sub(_) if matches!(primitive, Primitive::Unsigned(_)) => a.checked_sub(b)?,
            BinOp::Mul(_) if matches!(primitive, Primitive::Unsigned(_)) => a.checked_mul(b)?,
            BinOp::Add(_) | BinOp::Sub(_) | BinOp::Mul(_) if
                // #[gamma::skip(match_guard.always_true, reason = "Earlier arms consume Unsigned; the following Signed destructuring independently enforces this guard.", tag = "equivalent")]
                matches!(primitive, Primitive::Signed(_)) => {
                let Primitive::Signed(width) = primitive else { return None };
                let a = (a << (128 - width)).cast_signed() >> (128 - width);
                let b = (b << (128 - width)).cast_signed() >> (128 - width);
                let result = match op {
                    BinOp::Add(_) => a.checked_add(b)?,
                    BinOp::Sub(_) => a.checked_sub(b)?,
                    _ => a.checked_mul(b)?,
                };
                if result < (i128::MIN >> (128 - width)) || result > (i128::MAX >> (128 - width)) {
                    return None;
                }
                result.cast_unsigned() &
                    // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
                    primitive.mask()?
            }
            _ => return None,
        };
        let (Primitive::Unsigned(width) | Primitive::Signed(width)) = primitive else {
            return None;
        };
        if matches!(op, BinOp::Shl(_) | BinOp::Shr(_)) && b >= u128::from(width) {
            return None;
        }
        (value <=
            // #[gamma::skip(try.propagate_to_unwrap, reason = "The established named integer widths 8, 16, 32, 64, and 128 always have a mask.", tag = "equivalent")]
            primitive.mask()?)
        .then_some(Value::Int(value, primitive))
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::float_cmp,
        reason = "exact primitive floating point operations are the semantics being modeled"
    )]
    fn floating(op: &BinOp, a: u64, b: u64, primitive: Primitive) -> Option<Value> {
        let (a, b) = (f64::from_bits(a), f64::from_bits(b));
        // #[gamma::skip(cond.always_false, literal.int_decrement, literal.int_increment, literal.int_to_one, literal.int_to_zero, reason = "Already-rounded f32 operands produce the same final f32 rounding through f64 arithmetic; NaN payloads are not a portable contract.", tag = "equivalent")]
        if primitive == Primitive::Float(32) {
            let (a, b) = (a as f32, b as f32);
            return match op {
                BinOp::Add(_) => float(f64::from(a + b), primitive),
                BinOp::Sub(_) => float(f64::from(a - b), primitive),
                BinOp::Mul(_) => float(f64::from(a * b), primitive),
                BinOp::Div(_) => float(f64::from(a / b), primitive),
                BinOp::Lt(_) => Some(Value::Bool(a < b)),
                BinOp::Eq(_) => Some(Value::Bool(a == b)),
                _ => None,
            };
        }
        match op {
            BinOp::Add(_) => float(a + b, primitive),
            BinOp::Sub(_) => float(a - b, primitive),
            BinOp::Mul(_) => float(a * b, primitive),
            BinOp::Div(_) => float(a / b, primitive),
            BinOp::Lt(_) => Some(Value::Bool(a < b)),
            BinOp::Eq(_) => Some(Value::Bool(a == b)),
            _ => None,
        }
    }
}

use numeric::{binary, cast, float, primitive_type};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod exact_survivors;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn recursive_provenance_escaping_exhausts_its_bound_without_exponential_allocation() {
        let ty = Primitive::Unsigned(64);
        let mut value = Value::Symbol("key".into(), Binding::Primitive(ty));
        for _ in 0..64 {
            let Some(next) = ranges::wrapping("wrapping_add", &value, &Value::Int(1, ty)) else {
                return;
            };
            value = next;
        }
        panic!("nested escaped identities must become unknown within the fixed representation bound");
    }

    #[test]
    fn shadowing_restores_the_outer_state_after_earlier_progress_not_at_block_entry() {
        assert!(!proof(
            "while i < n { i += 1; let i: f64 = hidden(); tokens.push(i); }",
            "while i < n { i += 1; let i: f64 = 0.0; tokens.push(i); }",
            &[("i", "0_u32")],
        ));
    }

    fn bindings() -> HashMap<String, Binding> {
        HashMap::from_iter([
            ("i".into(), Binding::Primitive(Primitive::Unsigned(32))),
            ("offset".into(), Binding::Primitive(Primitive::Unsigned(32))),
            ("n".into(), Binding::Primitive(Primitive::Unsigned(32))),
            ("ready".into(), Binding::Bool),
            ("left".into(), Binding::Vec),
            ("right".into(), Binding::Vec),
            ("characters".into(), Binding::VecChar),
            ("tokens".into(), Binding::Vec),
        ])
    }

    fn proof(original: &str, mutant: &str, seeds: &[(&str, &str)]) -> bool {
        let seeds = seeds
            .iter()
            .map(|(name, expr)| ((*name).into(), syn::parse_str(expr).unwrap()))
            .collect();
        proves(
            &syn::parse_str(original).unwrap(),
            &syn::parse_str(mutant).unwrap(),
            &bindings(),
            &seeds,
            &HashMap::default(),
            true,
        )
    }

    #[test]
    fn deleted_standard_vector_length_progress_is_proven_without_negating_conditions() {
        let original = "while left.len() < right.len() { left.insert(0, ' '); }";
        assert!(proof(original, "while left.len() < right.len() {}", &[]));
        for mutant in [
            "while !(left.len() < right.len()) { left.insert(0, ' '); }",
            "while left.len() < right.len() { left.clear(); }",
            "while left.len() < right.len() { left.remove(0); }",
            "while left.len() < right.len() { left.push(' '); }",
            "while left.len() < right.len() { unknown(&mut left); }",
            "while left.len() < right.len() { if ready { break; } }",
            "while left.len() < right.len() { return; }",
        ] {
            assert!(!proof(original, mutant, &[]), "{mutant}");
        }
        let mut custom = bindings();
        let _old = custom.remove("left");
        assert!(!proves(
            &syn::parse_str(original).unwrap(),
            &syn::parse_str("while left.len() < right.len() {}").unwrap(),
            &custom,
            &HashMap::default(),
            &HashMap::default(),
            true
        ));
    }

    #[test]
    fn primitive_fixed_points_use_declared_widths_and_preserve_advancing_variants() {
        let original = "while i < n { i += 1; }";
        for mutant in [
            "while i < n { i = 1_u32 >> 31; }",
            "while i < n { i = 2_u32 << 31; }",
            "while i < n { i = 255_u8.wrapping_add(1) as u32; }",
            "while i < n { i = (256_u32 as u8) as u32; }",
            "while i < n { i *= 0; }",
        ] {
            assert!(proof(original, mutant, &[("i", "0")]), "{mutant}");
        }
        for mutant in [
            "while i < n { i += 1; }",
            "while i < n { i = 1_u32 << 31; }",
            "while i < n { i = 2_u32 >> 31; i += 1; }",
            "while i < n { i = 255_u8 + 1_u8; }",
            "while i < n { i = 1_u32 << 32; }",
            "while i < n { i = numeric(); }",
            "while i < n { i = i.checked_add(0); }",
            "while i < n { i = 0; offset += 1; }",
        ] {
            assert!(!proof(original, mutant, &[("i", "0")]), "{mutant}");
        }
        assert!(!proof(original, "while i < n { i = 0; }", &[]));
    }

    #[test]
    fn zero_offset_relations_require_a_fixed_typed_cursor_and_stable_input() {
        let original = "while i < n { let end = i + offset; tokens.push(end); i = end; }";
        assert!(proof(
            original,
            "while i < n { let end = i * offset; tokens.push(end); i = end; }",
            &[("i", "0")]
        ));
        assert!(proof(
            original,
            "while i < n { let end = i * offset; tokens.push(end); }",
            &[("i", "0")]
        ));
        for mutant in [
            "while i < n { let end = i + offset; tokens.push(end); i = end; }",
            "while i < n { let end = i * offset; mutate(&mut offset); tokens.push(end); i = end; }",
            "while i < n { let end = i * offset; tokens.push(end); i = end; n -= 1; }",
            "while i < n { let end = i * offset; tokens.clear(); i = end; }",
        ] {
            assert!(!proof(original, mutant, &[("i", "0")]), "{mutant}");
        }
        assert!(!proof(
            original,
            "while i < n { let end = i * offset; tokens.push(end); i = end; }",
            &[("i", "1")]
        ));
    }

    #[test]
    fn lexical_shadow_writes_do_not_advance_the_outer_cursor() {
        assert!(proof(
            "while i < n { i += 1; }",
            "while i < n { { let mut i: u32 = 0; i += 1; } }",
            &[]
        ));
        assert!(!proof(
            "while i < n { i += 1; }",
            "while i < n { { let mut i: u32 = 0; i += 1; } i += 1; }",
            &[]
        ));
    }

    #[test]
    fn nested_match_facts_prove_a_disabled_inner_star_scan() {
        let original = "while i < characters.len() { match characters[i] { '*' => { while i < characters.len() && characters[i] == '*' { i += 1; } tokens.push(Token::Star); }, '?' => { i += 1; }, _ => { i += 1; } } }";
        let mutant = "while i < characters.len() { match characters[i] { '*' => { while i < characters.len() && characters[i] != '*' { i += 1; } tokens.push(Token::Star); }, '?' => { i += 1; }, _ => { i += 1; } } }";
        assert!(proof(original, mutant, &[]));
        for unsupported in [
            "while i < characters.len() { match characters[i] { '*' => { if ready { i += 1; } tokens.push(Token::Star); }, '?' => { i += 1; }, _ => { i += 1; } } }",
            "while i < characters.len() { match characters[i] { '*' => { unknown(); tokens.push(Token::Star); }, '?' => { i += 1; }, _ => { i += 1; } } }",
            "while i < characters.len() { match characters[i] { '*' => { return; }, '?' => { i += 1; }, _ => { i += 1; } } }",
            "while i < characters.len() { match characters[i] { '*' => { characters.push('*'); tokens.push(Token::Star); }, '?' => { i += 1; }, _ => { i += 1; } } }",
        ] {
            assert!(!proof(original, unsupported, &[]), "{unsupported}");
        }
    }

    #[test]
    fn float_nan_infinity_saturation_and_negative_zero_follow_primitive_semantics() {
        for (expression, expected) in [
            ("(0.0_f64 / 0.0_f64) as u32", 0),
            ("(1.0_f64 / 0.0_f64) as u8", 255),
            ("(-1.0_f64 / 0.0_f64) as u8", 0),
            ("(-1.0_f64 / 0.0_f64) as i8", 128),
            ("(-0.0_f64) as u32", 0),
            ("(-1_i8) as u32", u128::from(u32::MAX)),
            ("-(-1_i8)", 1),
        ] {
            let mut machine = Machine {
                values: HashMap::default(),
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
            };
            let value = machine.eval(&syn::parse_str(expression).unwrap(), None).unwrap();
            assert!(
                matches!(value, Value::Int(actual, _) if actual == expected),
                "{expression}: {value:?}"
            );
        }
        assert!(!proof(
            "while i < n { i += 1; }",
            "while i < n { i = ((0.0_f64 / 0.0_f64) * 0.0_f64) as u32; }",
            &[]
        ));
    }

    #[test]
    fn initial_value_discovery_does_not_cross_effects_or_shadowing() {
        let file = syn::parse_file("fn f(){ let mut i:u32=0; unknown(&mut i); while i<7 { i=0; } }").unwrap();
        let syn::Item::Fn(function) = &file.items[0] else { unreachable!() };
        let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
            unreachable!()
        };
        assert!(initializers(&file, &expression.span().byte_range()).is_empty());
        let file = syn::parse_file("fn f(){ let mut i:u32=0; { let i:u32=1; while i<7 { i=0; } } }").unwrap();
        let syn::Item::Fn(function) = &file.items[0] else { unreachable!() };
        let Stmt::Expr(Expr::Block(block), _) = &function.block.stmts[1] else {
            unreachable!()
        };
        let Stmt::Expr(expression, _) = &block.block.stmts[1] else {
            unreachable!()
        };
        assert_eq!(initializers(&file, &expression.span().byte_range())["i"], syn::parse_quote!(1));
    }

    #[test]
    fn open_ended_and_custom_iteration_never_establish_unbounded_unique_growth() {
        let original: Expr = syn::parse_quote!(for ordinal in 2_u64.. {
            if used.insert(ordinal) {
                break;
            }
        });
        for mutant in [
            syn::parse_quote!(for ordinal in 2_u64.. {
                used.insert(ordinal);
            }),
            syn::parse_quote!(for ordinal in 2_u64.. {
                if used.insert(ordinal) {
                    continue;
                }
            }),
            syn::parse_quote!(for ordinal in 2_u64.. {
                if !used.insert(ordinal) {
                    break;
                }
            }),
            syn::parse_quote!(for ordinal in 2_u64..7 {
                used.insert(ordinal);
            }),
            syn::parse_quote!(for ordinal in custom {
                used.insert(ordinal);
            }),
            syn::parse_quote!(for ordinal in core::iter::repeat(2_u64) {
                used.insert(ordinal);
            }),
        ] {
            assert!(!proves(
                &original,
                &mutant,
                &bindings(),
                &HashMap::default(),
                &HashMap::default(),
                true
            ));
        }
    }

    #[test]
    fn numeric_aliases_and_macros_do_not_borrow_primitive_cast_semantics() {
        for source in [
            "type u8=u32; fn f(){while true{}}",
            "use external::u8; fn f(){while true{}}",
            "use external::*; fn f(){while true{}}",
            "make_types!(); fn f(){while true{}}",
            "fn f(){type u8=u32;while true{}}",
        ] {
            let file = syn::parse_file(source).unwrap();
            let syn::Item::Fn(function) = file.items.last().unwrap() else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = function.block.stmts.last().unwrap() else {
                unreachable!()
            };
            assert!(!numeric_names(&file, &expression.span().byte_range()), "{source}");
        }
        assert!(!proves(
            &syn::parse_quote!(while i < n {
                i += 1;
            }),
            &syn::parse_quote!(while i < n {
                i = i + (256_u32 as u8) as u32;
            }),
            &bindings(),
            &HashMap::default(),
            &HashMap::default(),
            false,
        ));
        assert!(!proves(
            &syn::parse_quote!(while i < n {
                i += 1;
            }),
            &syn::parse_quote!(while i < n {
                i = 2 << 7;
            }),
            &HashMap::from_iter([
                ("i".into(), Binding::Primitive(Primitive::Unsigned(8))),
                ("n".into(), Binding::Primitive(Primitive::Unsigned(8))),
            ]),
            &HashMap::from_iter([("i".into(), syn::parse_quote!(0))]),
            &HashMap::default(),
            false,
        ));
    }

    #[test]
    fn interpretation_budget_and_mutable_helper_effects_remain_unknown() {
        let mut machine = Machine {
            values: bindings()
                .into_iter()
                .map(|(name, kind)| (name.clone(), Value::Symbol(name, kind)))
                .collect(),
            budget: 1,
            writes: HashSet::default(),
            locals: HashSet::default(),
            facts: HashMap::default(),
            helpers: None,
            restarts: false,
            numeric_names: true,
            stable_calls: None,
            paths: HashMap::default(),
            assumptions: Vec::new(),
        };
        assert!(machine.eval(&syn::parse_quote!((i)), None).is_none());
        machine.budget = 256;
        assert!(machine.eval(&syn::parse_quote!(-(-128_i8)), None).is_none());
        assert!(machine.eval(&syn::parse_quote!(127_i8 + 1_i8), None).is_none());
        let file = syn::parse_file("fn choose(value:&mut u32)->bool{*value+=1;false} fn f(){while true{}}").unwrap();
        let syn::Item::Fn(function) = &file.items[1] else { unreachable!() };
        let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let helper = helpers(&file, &expression.span().byte_range());
        assert!(!proves(
            &syn::parse_quote!(while i < n {
                i += 1;
            }),
            &syn::parse_quote!(while i < n {
                let reduced = choose(&mut i);
                if true {
                    continue;
                }
            }),
            &bindings(),
            &HashMap::default(),
            &helper,
            true,
        ));
    }

    #[test]
    fn finite_width_range_from_cannot_supply_unbounded_distinct_set_members() {
        let range = std::panic::catch_unwind(|| (0_u8..).take(257).collect::<Vec<_>>());
        if let Ok(values) = range {
            assert_eq!(values.len(), 257);
            assert_eq!(values[0], values[256]);
            assert_eq!(values.into_iter().collect::<std::collections::HashSet<_>>().len(), 256);
        }
        let infinite = core::iter::repeat(7_u8);
        let repeated = infinite.take(257).collect::<Vec<_>>();
        assert_eq!(repeated.len(), 257);
        assert_eq!(repeated.into_iter().collect::<std::collections::HashSet<_>>().len(), 1);
        assert_eq!((2_u8..8).collect::<Vec<_>>(), [2, 3, 4, 5, 6, 7]);
    }
}
