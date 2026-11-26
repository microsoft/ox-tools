// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lexical numeric-use and ownership-guard evidence.
//!
//! Shared syntax checks record attribute ranges, temporary-value uses, and possible `format!`
//! macro shadows even in cfg-disabled or otherwise skipped subtrees. These checks supply no
//! lexical facts and do not make excluded code eligible for mutation.

use core::ops::Range;
use std::rc::Rc;

#[cfg(test)]
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, Pat};

use super::predicates::{expr_attrs, is_int_literal, is_numeric_receiver, stmt_attrs};
use crate::cfg::CfgSet;
use crate::ops::collect::collector::read_only;
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::parse::Extent as _;
use crate::{HashMap, HashSet};

type Continuation<'ast> = Rc<[&'ast Expr]>;

/// Conservative analysis-cost cap, not a Rust limit or measured capacity.
/// Raising it admits longer ownership continuations at greater analysis cost;
/// lowering it leaves more continuations unknown.
const MAX_CONTINUATION_LEN: usize = 32;

/// Conservative recursion budget for finding the first overlapping use.
/// Raising it permits deeper prefix searches; lowering it retains more of the
/// full continuation when a narrower prefix cannot be proved.
const MAX_USE_PREFIX_DEPTH: usize = 64;

#[derive(Default, PartialEq, Eq)]
pub(super) struct Evidence<'ast> {
    pub(super) read_only: read_only::Evidence,
    pub(super) numeric: HashSet<Range<usize>>,
    pub(super) unsigned: HashSet<Range<usize>>,
    pub(super) reinitializations: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) exits: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) structures: HashMap<String, Option<HashMap<String, syn::Type>>>,
    pub(super) ownership_regions: HashMap<Range<usize>, Range<usize>>,
    pub(super) invalid_ownership: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) invalid_transitions: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) reborrowable: HashSet<Range<usize>>,
    pub(super) ownership_continuations: HashMap<Range<usize>, HashMap<Range<usize>, Continuation<'ast>>>,
    pub(super) returned_types: HashMap<Range<usize>, syn::Type>,
    pub(super) receiver_moves: HashMap<Range<usize>, String>,
}

pub(super) fn evidence<'ast>(file: &'ast syn::File, cfg: &CfgSet) -> Evidence<'ast> {
    let mut walk = Walk {
        read_only: read_only::Walk::default(),
        cfg,
        bindings: HashMap::default(),
        binding_undo: Vec::new(),
        sites: Vec::new(),
        place_sites: Vec::new(),
        numeric: HashSet::default(),
        unsigned: HashSet::default(),
        moved: HashMap::default(),
        reinitializations: HashMap::default(),
        exits: Vec::new(),
        loops: Vec::new(),
        structures: HashMap::default(),
        blocks: Vec::new(),
        contexts: HashMap::default(),
        following: HashMap::default(),
        move_blocks: HashMap::default(),
        unreachable: Vec::new(),
        restorations: Vec::new(),
        writes: HashSet::default(),
        reborrowable: HashSet::default(),
        continuation: None,
        continuations: HashMap::default(),
        alternatives: Vec::new(),
        return_type: None,
        returned_types: HashMap::default(),
        receiver_moves: HashMap::default(),
    };
    walk.visit_file(file);
    let mut evidence = Evidence::default();
    for (site, binding) in &walk.sites {
        if walk.numeric.contains(binding) {
            let _inserted = evidence.numeric.insert(site.clone());
        }
        if walk.unsigned.contains(binding) {
            let _inserted = evidence.unsigned.insert(site.clone());
        }
    }
    reinitializations(&walk, &mut evidence);
    exits(&walk, &mut evidence);
    evidence.structures = walk.structures;
    evidence.reborrowable = walk.reborrowable;
    evidence.returned_types = walk.returned_types;
    evidence.receiver_moves = walk.receiver_moves;
    evidence.read_only = walk.read_only.finish();
    evidence
}

fn reinitializations<'ast>(walk: &Walk<'ast, '_>, evidence: &mut Evidence<'ast>) {
    for (span, moved) in &walk.reinitializations {
        if let Some(context) = walk.contexts.get(span) {
            let invalid = moved
                .iter()
                .filter(|(place, expression)| walk.invalid_reinitialization(span, context, place, expression))
                .map(|(_, expression)| expression.clone())
                .collect::<Vec<_>>();
            if !invalid.is_empty() {
                let _prior = evidence.invalid_ownership.insert(span.clone(), invalid);
                if let Some(continuation) = walk.continuations.get(span) {
                    let checks = moved
                        .iter()
                        .filter(|(place, _)| !walk.following_invalid(span, context, place))
                        .map(|(_, expression)| (expression.extent().byte_range(), Rc::clone(continuation)))
                        .collect();
                    let _prior = evidence.ownership_continuations.insert(span.clone(), checks);
                }
            }
            let _prior = evidence.ownership_regions.insert(span.clone(), context.region());
        }
        let _prior = evidence
            .reinitializations
            .insert(span.clone(), moved.iter().map(|(_, expression)| expression.clone()).collect());
    }
}

/// Separates edits that invalidate a moved place from legal edits needing a wider guard region.
///
/// Ordinary invalidity covers later uses and unrestored loop back edges. Transition invalidity
/// instead checks the path introduced by swapping break/continue; region-only evidence preserves
/// viable alternatives by lifting their guard rather than rejecting them.
fn exits<'ast>(walk: &Walk<'ast, '_>, evidence: &mut Evidence<'ast>) {
    for exit in &walk.exits {
        let mut invalid = Vec::new();
        let mut continuation_checks = HashMap::default();
        let mut invalid_transitions = Vec::new();
        let names = exit
            .moved
            .iter()
            .filter(|(place, _)| {
                walk.place_sites.iter().any(|(site, used)| {
                    place.overlaps(used)
                        && (site.start > exit.span.end || exit.loop_start.is_some_and(|start| place.binding < start && site.start >= start))
                })
            })
            .map(|(place, expression)| {
                if exit.plain && walk.move_dominates(expression, &exit.context) {
                    let carried = exit.loop_start.is_some_and(|start| place.binding < start);
                    let backedge = carried && !walk.restored_in_loop(&exit.context, place, None, None);
                    // Continue-to-break exposes uses after the loop. Break-to-continue instead
                    // introduces a back edge, so a carried place must be restored before this exit.
                    let transition_invalid = if exit.continuing {
                        exit.context
                            .loop_region
                            .as_ref()
                            .is_some_and(|region| walk.used_after(region.end, place))
                    } else {
                        carried && !walk.restored_in_loop(&exit.context, place, None, Some(exit.span.start))
                    };
                    if transition_invalid {
                        invalid_transitions.push(expression.clone());
                    }
                    if (walk.used_after_compatible(&exit.span, place) || backedge)
                        && let Some(prefix) = walk.exit_prefix(exit, place)
                    {
                        invalid.push(expression.clone());
                        let _prior = continuation_checks.insert(expression.extent().byte_range(), prefix);
                    }
                }
                expression.clone()
            })
            .collect::<Vec<_>>();
        if !names.is_empty() {
            let _prior = evidence.ownership_regions.insert(exit.span.clone(), exit.context.region());
            if !invalid.is_empty() {
                let _prior = evidence.invalid_ownership.insert(exit.span.clone(), invalid);
                let _prior = evidence.ownership_continuations.insert(exit.span.clone(), continuation_checks);
            }
            if !invalid_transitions.is_empty() {
                let _prior = evidence.invalid_transitions.insert(exit.span.clone(), invalid_transitions);
            }
            let _previous = evidence.exits.insert(exit.span.clone(), names);
        }
    }
}

struct Exit<'ast> {
    span: Range<usize>,
    moved: Vec<(Place, Expr)>,
    loop_start: Option<usize>,
    context: Context,
    plain: bool,
    continuing: bool,
    continuation: Option<Continuation<'ast>>,
}

struct Context {
    block: Range<usize>,
    loop_region: Option<Range<usize>>,
}

impl Context {
    fn region(&self) -> Range<usize> {
        self.loop_region.as_ref().unwrap_or(&self.block).clone()
    }
}

struct Following {
    span: Range<usize>,
    breaks: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Place {
    binding: usize,
    root: String,
    fields: Vec<String>,
}

impl Place {
    fn contains(&self, other: &Self) -> bool {
        self.binding == other.binding && other.fields.starts_with(&self.fields)
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.contains(other) || other.contains(self)
    }
}

struct Walk<'ast, 'cfg> {
    read_only: read_only::Walk<'ast>,
    cfg: &'cfg CfgSet,
    bindings: HashMap<String, usize>,
    binding_undo: Vec<(String, Option<usize>)>,
    sites: Vec<(Range<usize>, usize)>,
    place_sites: Vec<(Range<usize>, Place)>,
    numeric: HashSet<usize>,
    unsigned: HashSet<usize>,
    moved: HashMap<Place, Expr>,
    reinitializations: HashMap<Range<usize>, Vec<(Place, Expr)>>,
    exits: Vec<Exit<'ast>>,
    loops: Vec<Range<usize>>,
    structures: HashMap<String, Option<HashMap<String, syn::Type>>>,
    blocks: Vec<Range<usize>>,
    contexts: HashMap<Range<usize>, Context>,
    following: HashMap<Range<usize>, Following>,
    move_blocks: HashMap<Range<usize>, Range<usize>>,
    unreachable: Vec<Range<usize>>,
    restorations: Vec<(Range<usize>, Place)>,
    writes: HashSet<Range<usize>>,
    reborrowable: HashSet<Range<usize>>,
    continuation: Option<Continuation<'ast>>,
    continuations: HashMap<Range<usize>, Continuation<'ast>>,
    alternatives: Vec<Vec<Range<usize>>>,
    return_type: Option<syn::Type>,
    returned_types: HashMap<Range<usize>, syn::Type>,
    receiver_moves: HashMap<Range<usize>, String>,
}

impl<'ast> Walk<'ast, '_> {
    fn continuation(&self, statements: &'ast [syn::Stmt], outer: &Continuation<'ast>) -> Option<Continuation<'ast>> {
        let mut suffix = Vec::new();
        for later in statements {
            if self.cfg.skip_gate(stmt_attrs(later)) {
                continue;
            }
            match later {
                syn::Stmt::Expr(expression, _) => suffix.push(expression),
                syn::Stmt::Local(local) if local.init.as_ref().is_some_and(|init| init.diverge.is_none()) => {
                    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The match guard immediately above requires local.init to be Some with no divergence; neither local nor its initializer is mutated between that guard and this access.")]
                    suffix.push(local.init.as_ref()?.expr.as_ref());
                }
                _ => return None,
            }
            if suffix.len() + outer.len() > MAX_CONTINUATION_LEN {
                return None;
            }
        }
        if suffix.is_empty() {
            Some(Rc::clone(outer))
        } else {
            suffix.extend(outer.iter().copied());
            Some(suffix.into())
        }
    }

    fn exit_prefix(&self, exit: &Exit<'ast>, place: &Place) -> Option<Continuation<'ast>> {
        let continuation = exit.continuation.as_ref()?;
        for (index, expression) in continuation.iter().enumerate() {
            if let Some(inner) = self.use_prefix(expression, place, MAX_USE_PREFIX_DEPTH) {
                // #[gamma::skip(arith.add_to_mul, reason = "This changes only reserved capacity; the same continuation slices are appended.", tag = "equivalent")]
                let mut prefix = Vec::with_capacity(index + inner.len());
                prefix.extend_from_slice(&continuation[..index]);
                prefix.extend(inner);
                return Some(prefix.into());
            }
        }
        Some(Rc::clone(continuation))
    }

    fn use_prefix(&self, expression: &'ast Expr, place: &Place, remaining: usize) -> Option<Vec<&'ast Expr>> {
        let next = remaining.checked_sub(1)?;
        if self
            .place_sites
            .iter()
            .any(|(site, used)| *site == expression.extent().byte_range() && place.overlaps(used) && !self.writes.contains(site))
        {
            return Some(Vec::new());
        }
        match expression {
            Expr::Paren(paren) => self.use_prefix(&paren.expr, place, next),
            Expr::Group(group) => self.use_prefix(&group.expr, place, next),
            Expr::Reference(reference) => self.use_prefix(&reference.expr, place, next),
            Expr::Return(returned) => self.use_prefix(returned.expr.as_ref()?, place, next),
            Expr::Call(call) => {
                let mut prefix = vec![call.func.as_ref()];
                for argument in &call.args {
                    if let Some(inner) = self.use_prefix(argument, place, next) {
                        prefix.extend(inner);
                        return Some(prefix);
                    }
                    prefix.push(argument);
                }
                None
            }
            Expr::MethodCall(call) => {
                if let Some(prefix) = self.use_prefix(&call.receiver, place, next) {
                    return Some(prefix);
                }
                let mut prefix = vec![call.receiver.as_ref()];
                for argument in &call.args {
                    if let Some(inner) = self.use_prefix(argument, place, next) {
                        prefix.extend(inner);
                        return Some(prefix);
                    }
                    prefix.push(argument);
                }
                None
            }
            _ => None,
        }
    }

    fn compatible(&self, left: &Range<usize>, right: &Range<usize>) -> bool {
        let within = |inner: &Range<usize>, outer: &Range<usize>| outer.start <= inner.start && inner.end <= outer.end;
        !self.alternatives.iter().any(|branches| {
            let left = branches.iter().position(|branch| within(left, branch));
            let right = branches.iter().position(|branch| within(right, branch));
            left.zip(right).is_some_and(|(left, right)| left != right)
        })
    }

    fn move_dominates(&self, expression: &Expr, context: &Context) -> bool {
        self.move_blocks
            .get(&expression.extent().byte_range())
            .is_some_and(|block| block.start <= context.block.start && context.block.end <= block.end)
    }

    fn used_after_compatible(&self, position: &Range<usize>, place: &Place) -> bool {
        self.place_sites.iter().any(|(site, used)| {
            !self.writes.contains(site)
                && site.start > position.end
                && place.overlaps(used)
                && self.compatible(position, site)
                && !self
                    .unreachable
                    .iter()
                    .any(|region| region.start <= site.start && site.end <= region.end)
                && !self.restorations.iter().any(|(span, restored)| {
                    position.end < span.start && span.end < site.start && restored.contains(place) && self.compatible(position, span)
                })
        })
    }

    fn used_after(&self, position: usize, place: &Place) -> bool {
        let restoration = self
            .restorations
            .iter()
            .filter(|(span, restored)| span.start > position && restored.contains(place))
            .map(|(span, _)| span.start)
            .min();
        self.place_sites.iter().any(|(site, used)| {
            !self.writes.contains(site)
                && site.start > position
                && place.overlaps(used)
                && restoration.is_none_or(|restore| site.start < restore)
        })
    }

    fn restored_in_loop(&self, context: &Context, place: &Place, except: Option<&Range<usize>>, before: Option<usize>) -> bool {
        context.loop_region.as_ref().is_some_and(|region| {
            self.restorations.iter().any(|(span, restored)| {
                Some(span) != except
                    && region.start <= span.start
                    && span.end <= region.end
                    && before.is_none_or(|end| span.end <= end)
                    && restored.contains(place)
            })
        })
    }

    fn invalid_reinitialization(&self, span: &Range<usize>, context: &Context, place: &Place, expression: &Expr) -> bool {
        if self
            .unreachable
            .iter()
            .any(|region| region.start <= span.start && span.end <= region.end)
            || !self.move_dominates(expression, context)
        {
            return false;
        }
        if self.following_invalid(span, context, place) {
            return true;
        }
        if self.continuations.get(span).is_some_and(|continuation| {
            continuation
                .iter()
                .any(|expression| matches!(expression, Expr::Break(_) | Expr::Return(_)))
        }) {
            return false;
        }
        context
            .loop_region
            .as_ref()
            .is_some_and(|region| self.continuations.contains_key(span) && place.binding < region.start)
            && !self.restored_in_loop(context, place, Some(span), None)
    }

    fn following_invalid(&self, span: &Range<usize>, context: &Context, place: &Place) -> bool {
        self.following.get(span).is_some_and(|following| {
            self.place_sites.iter().any(|(site, used)| {
                !self.writes.contains(site) && following.span.start <= site.start && site.end <= following.span.end && place.overlaps(used)
            }) || following.breaks
                && context
                    .loop_region
                    .as_ref()
                    .is_some_and(|region| self.used_after(region.end, place))
        })
    }

    fn bind(&mut self, pattern: &Pat) {
        struct Names(Vec<syn::Ident>);
        impl<'ast> Visit<'ast> for Names {
            fn visit_pat_ident(&mut self, i: &'ast syn::PatIdent) {
                self.0.push(i.ident.clone());
                visit::visit_pat_ident(self, i);
            }
        }
        let mut names = Names(Vec::new());
        names.visit_pat(pattern);
        for name in names.0 {
            let span = name.span().byte_range();
            let name = name.to_string();
            let prior = self.bindings.insert(name.clone(), span.start);
            self.binding_undo.push((name, prior));
            self.sites.push((span.clone(), span.start));
        }
    }

    fn restore_bindings(&mut self, checkpoint: usize) {
        while self.binding_undo.len() > checkpoint {
            let (name, prior) = self.binding_undo.pop().expect("the undo length exceeds the scope checkpoint");
            if let Some(prior) = prior {
                let _replaced = self.bindings.insert(name, prior);
            } else {
                let _removed = self.bindings.remove(&name);
            }
        }
    }

    fn note(&mut self, expression: &Expr, unsigned: bool) {
        match expression {
            Expr::Path(path) => {
                if let Some(binding) = path.path.get_ident().and_then(|name| self.bindings.get(&name.to_string())) {
                    let _inserted = self.numeric.insert(*binding);
                    if unsigned {
                        let _inserted = self.unsigned.insert(*binding);
                    }
                }
            }
            Expr::Paren(paren) => self.note(&paren.expr, unsigned),
            Expr::Group(group) => self.note(&group.expr, unsigned),
            Expr::Reference(reference) => self.note(&reference.expr, unsigned),
            Expr::Range(range) if unsigned => {
                for endpoint in range.start.iter().chain(range.end.iter()) {
                    self.note(endpoint, true);
                }
            }
            _ => {}
        }
    }

    fn place(&self, expression: &Expr) -> Option<Place> {
        match expression {
            Expr::Path(path) => {
                let root = path.path.get_ident()?.to_string();
                Some(Place {
                    binding: *self.bindings.get(&root)?,
                    root,
                    fields: Vec::new(),
                })
            }
            Expr::Field(field) => {
                let mut place = self.place(&field.base)?;
                place.fields.push(match &field.member {
                    syn::Member::Named(name) => name.to_string(),
                    syn::Member::Unnamed(index) => index.index.to_string(),
                });
                Some(place)
            }
            Expr::Paren(paren) => self.place(&paren.expr),
            Expr::Group(group) => self.place(&group.expr),
            _ => None,
        }
    }

    fn moved_argument(&mut self, expression: &Expr) {
        if let Some(place) = self.place(expression) {
            if let Some(block) = self.blocks.last() {
                let _prior = self.move_blocks.insert(expression.extent().byte_range(), block.clone());
            }
            let _prior = self.moved.insert(place, expression.clone());
        }
    }

    fn context(&self) -> Option<Context> {
        Some(Context {
            block: self.blocks.last()?.clone(),
            loop_region: self.loops.last().cloned(),
        })
    }

    fn exit(&mut self, expression: Range<usize>, plain: bool, continuing: bool) {
        let Some(context) = self.context() else { return };
        let moved = self
            .moved
            .iter()
            .filter(|(place, _)| self.bindings.get(&place.root) == Some(&place.binding))
            .map(|(place, expression)| (place.clone(), expression.clone()))
            .collect();
        self.exits.push(Exit {
            span: expression,
            moved,
            loop_start: self.loops.last().map(|span| span.start),
            context,
            plain,
            continuing,
            continuation: self.continuation.clone(),
        });
    }

    fn function(&mut self, signature: &syn::Signature, body: Option<&'ast syn::Block>) {
        let outer = core::mem::take(&mut self.bindings);
        let outer_undo = core::mem::take(&mut self.binding_undo);
        let outer_moved = core::mem::take(&mut self.moved);
        let outer_loops = core::mem::take(&mut self.loops);
        let outer_continuation = self.continuation.take();
        let outer_return = self.return_type.take();
        if let syn::ReturnType::Type(_, ty) = &signature.output {
            self.return_type = Some((**ty).clone());
        }
        for input in &signature.inputs {
            if let syn::FnArg::Typed(typed) = input
                && !self.cfg.skip_gate(&typed.attrs)
            {
                self.bind(&typed.pat);
            }
        }
        if let Some(body) = body {
            self.visit_block(body);
        }
        self.bindings = outer;
        self.binding_undo = outer_undo;
        self.moved = outer_moved;
        self.loops = outer_loops;
        self.continuation = outer_continuation;
        self.return_type = outer_return;
    }
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters use descriptive names")]
impl<'ast> Visit<'ast> for Walk<'ast, '_> {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        if self.cfg.skip_gate(item_attrs(node)) {
            self.read_only.visit_item(node);
        } else {
            visit::visit_item(self, node);
        }
    }

    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        if self.cfg.skip_gate(impl_item_attrs(node)) {
            self.read_only.visit_impl_item(node);
        } else {
            visit::visit_impl_item(self, node);
        }
    }

    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        if self.cfg.skip_gate(trait_item_attrs(node)) {
            self.read_only.visit_trait_item(node);
        } else {
            visit::visit_trait_item(self, node);
        }
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        if self.cfg.skip_gate(stmt_attrs(node)) {
            self.read_only.visit_stmt(node);
        } else {
            visit::visit_stmt(self, node);
        }
    }

    fn visit_expr(&mut self, node: &'ast Expr) {
        self.read_only.enter(node);
        if self.cfg.skip_gate(expr_attrs(node)) {
            visit::visit_expr(&mut self.read_only, node);
        } else {
            visit::visit_expr(self, node);
        }
        self.read_only.leave();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.read_only.visit_signature(&node.sig);
        self.function(&node.sig, Some(&node.block));
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        self.read_only.visit_item_struct(node);
        let fields = node
            .fields
            .iter()
            .filter(|field| !self.cfg.skip_gate(&field.attrs))
            .enumerate()
            .map(|(index, field)| {
                (
                    field.ident.as_ref().map_or_else(|| index.to_string(), ToString::to_string),
                    field.ty.clone(),
                )
            })
            .collect();
        self.structures
            .entry(node.ident.to_string())
            .and_modify(|prior| *prior = None)
            .or_insert(Some(fields));
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.read_only.visit_signature(&node.sig);
        self.function(&node.sig, Some(&node.block));
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.read_only.visit_signature(&node.sig);
        self.function(&node.sig, node.default.as_ref());
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        self.read_only.block(node);
        let checkpoint = self.binding_undo.len();
        self.blocks.push(node.extent().byte_range());
        let mut exited = false;
        for statement in &node.stmts {
            if self.cfg.skip_gate(stmt_attrs(statement)) {
                continue;
            }
            if exited {
                self.unreachable.push(statement.extent().byte_range());
            }
            exited |= matches!(statement, syn::Stmt::Expr(Expr::Break(_) | Expr::Continue(_) | Expr::Return(_), _));
        }
        for pair in node.stmts.windows(2) {
            if let syn::Stmt::Expr(expression, _) = &pair[0] {
                let _prior = self.following.insert(
                    expression.extent().byte_range(),
                    Following {
                        span: pair[1].extent().byte_range(),
                        breaks: matches!(&pair[1], syn::Stmt::Expr(Expr::Break(brk), _) if brk.label.is_none()),
                    },
                );
            }
        }
        let outer_continuation = self.continuation.clone();
        for (index, statement) in node.stmts.iter().enumerate() {
            self.continuation = outer_continuation
                .as_ref()
                .and_then(|outer| self.continuation(&node.stmts[index + 1..], outer));
            self.visit_stmt(statement);
        }
        // #[gamma::skip(stmt.delete_assign, tag = "equivalent", reason = "On the final statement iteration the later-statement suffix is empty, so continuation is already exactly outer_continuation. Nested block/function/closure/loop visitors restore the suffix they received; a skipped final statement leaves it untouched. An empty block never changes it.")]
        self.continuation = outer_continuation;
        let _block = self.blocks.pop();
        self.restore_bindings(checkpoint);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        self.read_only.visit_pat(&node.pat);
        if let Some(init) = &node.init {
            self.visit_expr(&init.expr);
            self.moved_argument(&init.expr);
            if let Some((_, otherwise)) = &init.diverge {
                self.visit_expr(otherwise);
            }
        }
        self.bind(&node.pat);
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        for input in &node.inputs {
            self.read_only.visit_pat(input);
        }
        self.read_only.visit_return_type(&node.output);
        let checkpoint = self.binding_undo.len();
        let outer_loops = core::mem::take(&mut self.loops);
        let outer_moved = core::mem::take(&mut self.moved);
        let outer_continuation = self.continuation.take();
        let outer_return = self.return_type.take();
        for input in &node.inputs {
            self.bind(input);
        }
        self.visit_expr(&node.body);
        self.restore_bindings(checkpoint);
        self.loops = outer_loops;
        self.moved = outer_moved;
        self.continuation = outer_continuation;
        self.return_type = outer_return;
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        if let Some((_, otherwise)) = &node.else_branch {
            self.alternatives.push(
                // #[gamma::skip(collection.reverse_vec, tag = "equivalent", reason = "The only consumer compatible finds each span's branch index and compares the two indices for inequality. Reversing this disjoint then/else pair preserves membership and whether the indices differ.")]
                vec![node.then_branch.extent().byte_range(), otherwise.extent().byte_range()],
            );
        }
        let checkpoint = self.binding_undo.len();
        self.visit_expr(&node.cond);
        self.visit_block(&node.then_branch);
        self.restore_bindings(checkpoint);
        if let Some((_, otherwise)) = &node.else_branch {
            self.visit_expr(otherwise);
        }
        // #[gamma::skip(stmt.delete_call, reason = "The then branch is restored before visiting else. A source-parsed if else is a block or another if; visit_block restores its checkpoint, and nested visit_expr_if restores its then checkpoint before its own else. Induction over the finite else-if chain therefore leaves no binding undo entries for the final duplicate restore to remove.", tag = "equivalent")]
        self.restore_bindings(checkpoint);
    }

    fn visit_expr_let(&mut self, node: &'ast syn::ExprLet) {
        self.read_only.visit_pat(&node.pat);
        self.visit_expr(&node.expr);
        self.bind(&node.pat);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.read_only.visit_pat(&node.pat);
        self.visit_expr(&node.expr);
        let checkpoint = self.binding_undo.len();
        self.bind(&node.pat);
        if matches!(&*node.expr, Expr::Range(_))
            && let Pat::Ident(ident) = &*node.pat
        {
            let _inserted = self.numeric.insert(ident.ident.span().byte_range().start);
        }
        self.loops.push(node.extent().byte_range());
        let outer_continuation = self.continuation.replace(Rc::default());
        self.visit_block(&node.body);
        let _loop = self.loops.pop();
        self.continuation = outer_continuation;
        self.restore_bindings(checkpoint);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if self.cfg.skip_gate(&node.attrs) {
            self.read_only.visit_arm(node);
            return;
        }

        if let Pat::Guard(guarded) = &node.pat {
            // Visit the guard in the combined walk only after registering bindings, preserving
            // flow-sensitive processing without observing the guard twice.
            self.read_only.visit_pat(&guarded.pat);
        } else {
            self.read_only.visit_pat(&node.pat);
        }
        let checkpoint = self.binding_undo.len();
        self.bind(&node.pat);
        if let Pat::Guard(guarded) = &node.pat {
            self.visit_expr(&guarded.guard);
        }
        self.visit_expr(&node.body);
        self.restore_bindings(checkpoint);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        self.alternatives
            .push(node.arms.iter().map(|arm| arm.extent().byte_range()).collect());
        visit::visit_expr_match(self, node);
    }

    fn visit_expr_return(&mut self, node: &'ast syn::ExprReturn) {
        if let Some(expression) = &node.expr
            && let Some(place) = self.place(expression)
            && let Some(ty) = &self.return_type
        {
            for (site, used) in &self.place_sites {
                if used == &place {
                    let _prior = self.returned_types.insert(site.clone(), ty.clone());
                }
            }
        }
        visit::visit_expr_return(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        visit::visit_expr_path(&mut self.read_only, node);
        if let Some(binding) = node.path.get_ident().and_then(|name| self.bindings.get(&name.to_string())) {
            self.sites.push((node.extent().byte_range(), *binding));
            self.place_sites.push((
                node.extent().byte_range(),
                Place {
                    binding: *binding,
                    root: node
                        .path
                        .get_ident()
                        .expect("the binding lookup required an identifier")
                        .to_string(),
                    fields: Vec::new(),
                },
            ));
        }
    }

    fn visit_expr_field(&mut self, node: &'ast syn::ExprField) {
        if let Some(place) = self.place(&Expr::Field(node.clone())) {
            // Record the complete place once, not its root or field prefixes as extra uses.
            // The read-only walk still observes nested syntax such as receiver attributes.
            visit::visit_expr_field(&mut self.read_only, node);
            self.place_sites.push((node.extent().byte_range(), place));
        } else {
            visit::visit_expr_field(self, node);
        }
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if matches!(
            node.op,
            BinOp::Sub(_)
                | BinOp::Mul(_)
                | BinOp::Div(_)
                | BinOp::Rem(_)
                | BinOp::SubAssign(_)
                | BinOp::MulAssign(_)
                | BinOp::DivAssign(_)
                | BinOp::RemAssign(_)
        ) {
            self.note(&node.left, false);
            self.note(&node.right, false);
        } else if matches!(
            node.op,
            BinOp::Add(_) | BinOp::AddAssign(_) | BinOp::Lt(_) | BinOp::Le(_) | BinOp::Gt(_) | BinOp::Ge(_)
        ) {
            if is_int_literal(&node.right) {
                self.note(&node.left, false);
            }
            if is_int_literal(&node.left) {
                self.note(&node.right, false);
            }
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast syn::ExprIndex) {
        self.note(&node.index, true);
        visit::visit_expr_index(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if is_numeric_receiver(&node.method.to_string()) {
            self.note(&node.receiver, false);
        }
        visit::visit_expr_method_call(self, node);
        for argument in &node.args {
            let _inserted = self.reborrowable.insert(argument.extent().byte_range());
            self.moved_argument(argument);
        }
        let method = node.method.to_string();
        if
        // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "moved_argument records only syntactic places. Dropping this guard adds receiver_moves/reborrowable spans only for non-place expressions, which cannot occur in reinitialization/exit place evidence. Those exact-span entries are never queried by noncopy_place or ownership_known_place for an admitted edit.")]
        matches!(
            method.as_str(),
            "into_iter" | "into_bytes" | "into_boxed_str" | "into_string" | "into_os_string" | "into_vec"
        ) && self.place(&node.receiver).is_some()
        {
            let _prior = self.receiver_moves.insert(node.receiver.extent().byte_range(), method);
            let _inserted = self.reborrowable.insert(node.receiver.extent().byte_range());
            self.moved_argument(&node.receiver);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        visit::visit_expr_call(self, node);
        for argument in &node.args {
            let _inserted = self.reborrowable.insert(argument.extent().byte_range());
            self.moved_argument(argument);
        }
    }

    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        self.read_only.assignment(node);
        let assigned = self.place(&node.left);
        if
        // #[gamma::skip(cond.always_true, reason = "writes is consulted only for exact spans in place_sites. A non-place assignment LHS has no place_sites entry for its complete span; only any smaller place children are recorded. Inserting the non-place LHS span cannot exclude any recorded place use. The binding/place analysis, restoration recording and traversal are unchanged.", tag = "equivalent")]
        assigned.is_some() {
            let _prior = self.writes.insert(node.left.extent().byte_range());
        }
        // Moving and restoring within this assignment is safe; only earlier moves make it load-bearing.
        let restored = assigned
            .as_ref()
            .map(|assigned| {
                self.moved
                    .iter()
                    .filter(|(place, _)| assigned.contains(place))
                    .map(|(place, expression)| (place.clone(), expression.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        visit::visit_expr_assign(self, node);
        if let Some(assigned) = assigned {
            self.restorations.push((node.extent().byte_range(), assigned.clone()));
            self.moved.retain(|place, _| !assigned.contains(place));
            if !restored.is_empty() {
                if let Some(context) = self.context() {
                    let _prior = self.contexts.insert(node.extent().byte_range(), context);
                }
                let _previous = self.reinitializations.insert(node.extent().byte_range(), restored);
                if let Some(continuation) = &self.continuation {
                    let _prior = self.continuations.insert(node.extent().byte_range(), Rc::clone(continuation));
                }
            }
        }
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.loops.push(node.extent().byte_range());
        let outer_continuation = self.continuation.replace(Rc::default());
        visit::visit_expr_loop(self, node);
        let _loop = self.loops.pop();
        self.continuation = outer_continuation;
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.loops.push(node.extent().byte_range());
        let outer_continuation = self.continuation.replace(Rc::default());
        let checkpoint = self.binding_undo.len();
        self.visit_expr(&node.cond);
        self.visit_block(&node.body);
        self.restore_bindings(checkpoint);
        let _loop = self.loops.pop();
        self.continuation = outer_continuation;
    }

    fn visit_expr_continue(&mut self, node: &'ast syn::ExprContinue) {
        self.exit(node.extent().byte_range(), node.label.is_none(), true);
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        self.read_only.item_macro(node);
    }

    fn visit_stmt_macro(&mut self, _: &'ast syn::StmtMacro) {
        self.read_only.statement_macro();
    }

    fn visit_item_extern_crate(&mut self, node: &'ast syn::ItemExternCrate) {
        self.read_only.extern_crate(node);
    }

    fn visit_expr_break(&mut self, node: &'ast syn::ExprBreak) {
        self.exit(node.extent().byte_range(), node.label.is_none(), false);
        visit::visit_expr_break(self, node);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use syn::{Block, ExprGroup, parse_file, parse_quote};

    use super::*;

    #[test]
    fn read_only_consumers_survive_cfg_delegation_and_specialized_lexical_visitors() {
        for source in [
            "struct S; impl S { #[cfg(any())] fn hidden() { count(bytes().first()); } }",
            "trait T { #[cfg(any())] fn hidden() { count(bytes().first()); } }",
            "fn f() { let _: [u8; 0] = [#[cfg(any())] { count(bytes().first()); 1 }]; }",
            "fn f(value: [u8; count(bytes().first())]) {}",
            "struct S; impl S { fn f(value: [u8; count(bytes().first())]) {} }",
            "trait T { fn f(value: [u8; count(bytes().first())]); }",
            "struct S([u8; count(bytes().first())]);",
            "fn f() { let value: [u8; count(bytes().first())] = [0]; }",
            "fn f() { let closure = |value: [u8; count(bytes().first())]| (); }",
            "fn f() { let closure = || -> [u8; count(bytes().first())] { [0] }; }",
            "struct Shape<const N: usize>(u8); fn f(shape: Shape<1>) { if let Shape::<{ count(bytes().first()) }>(value) = shape {} }",
            "struct Shape<const N: usize>(u8); fn f(values: Vec<Shape<1>>) { for Shape::<{ count(bytes().first()) }>(value) in values {} }",
            "struct Shape<const N: usize>(u8); fn f(shape: Shape<1>) { match shape { Shape::<{ count(bytes().first()) }>(value) => () } }",
            "struct Shape<const N: usize>(u8); fn f(shape: Shape<1>) { match shape { Shape::<{ count(bytes().first()) }>(value) if value > 0 => (), _ => () } }",
            "fn target<const N: usize>() {} fn f() { target::<{ count(bytes().first()) }>(); }",
            "struct Shape<const N: usize>; struct Record { field: u8 } impl<const N: usize> Shape<N> { const VALUE: Record = Record { field: 1 }; } fn f(VALUE: u8) { let _ = <Shape<{ count(bytes().first()) }>>::VALUE.field; }",
        ] {
            let file = parse_file(source).unwrap();
            let actual = evidence(&file, &CfgSet::parse(""));
            let borrowed = source.find("bytes().first()").unwrap();
            let consumer = source.find("count(bytes().first())").unwrap();
            assert_eq!(
                actual.read_only.temporaries.get(&(borrowed..borrowed + "bytes().first()".len())),
                Some(&(consumer..consumer + "count(bytes().first())".len())),
                "{source}"
            );
            assert_eq!(actual.read_only, read_only::Evidence::of(&file), "{source}");
        }
    }

    #[test]
    fn field_receivers_preserve_attributes_inside_a_known_place() {
        for receiver in ["(#[allow(unused)] value)", "((#[allow(unused)] value))"] {
            let source =
                format!("#![feature(stmt_expr_attributes)] struct Record {{ field: u8 }} fn f(value: Record) -> u8 {{ {receiver}.field }}");
            let file = parse_file(&source).unwrap();
            let actual = evidence(&file, &CfgSet::parse(""));
            let start = source.find("#[allow(unused)] value").unwrap();
            let end = start + "#[allow(unused)] value".len();
            assert_eq!(
                actual.read_only.expressions.get(&(start..end)),
                Some(&(start + "#[allow(unused)]".len()..end)),
                "{source}"
            );
            assert_eq!(actual.read_only, read_only::Evidence::of(&file), "{source}");
        }
    }

    #[test]
    fn statement_macro_invocations_shadow_format_even_in_cfg_excluded_bodies() {
        for gate in ["", "#[cfg(any())]"] {
            let source = format!("macro_rules! emit {{ () => {{}} }} {gate} fn f() {{ emit!(); }}");
            let file = parse_file(&source).unwrap();
            assert!(evidence(&file, &CfgSet::parse("")).read_only.format_macro_shadowed, "{source}");
        }
        let file = parse_file("macro_rules! emit { () => {} } fn f() {}").unwrap();
        assert!(!evidence(&file, &CfgSet::parse("")).read_only.format_macro_shadowed);
    }

    #[test]
    fn shared_walk_keeps_ungated_observations_and_lexical_cfg_separate() {
        let file = parse_file(
            r"
            #[cfg(any())] mod hidden {
                macro_rules! format { () => {} }
                fn hidden() { consume(make().as_str()); }
            }
            mod outer { mod inner {
                struct S { value: [u8; 1 + 2] }
                fn f<const N: usize>(mut input: S) {
                    if true {} #[allow(unused)] { value(); }
                    #[cfg(any())] consume(disabled().as_str());
                    let value = make().as_str();
                    let closure = |[x, y]: [u8; 2]| if true { x } else { y };
                    input.value[0] = 7;
                    match input.value {
                        [value, ..] if value > 0 => consume(make().as_str()),
                        #[cfg(any())] _ => consume(disabled().as_str()),
                        _ => (),
                    }
                    for x in 0..N { while x > 0 { if x > 2 { continue; } break; } }
                    let [first, second] = [make(), make()];
                    [*first, *second] = values();
                }
            } }
        ",
        )
        .unwrap();
        let expected = read_only::Evidence::of(&file);
        for cfg in [CfgSet::unconditional(), CfgSet::parse("")] {
            let actual = evidence(&file, &cfg);
            assert_eq!(actual.read_only, expected);
        }
    }

    fn copied(continuation: &[&Expr]) -> Vec<Expr> {
        continuation.iter().map(|expression| (*expression).clone()).collect()
    }

    fn expression_nodes(expressions: &[&Expr]) -> usize {
        struct Nodes(usize);
        impl<'ast> Visit<'ast> for Nodes {
            fn visit_expr(&mut self, i: &'ast Expr) {
                self.0 += 1;
                visit::visit_expr(self, i);
            }
        }
        let mut nodes = Nodes(0);
        for expression in expressions {
            nodes.visit_expr(expression);
        }
        nodes.0
    }

    fn empty_walk<'ast>(cfg: &CfgSet) -> Walk<'ast, '_> {
        Walk {
            read_only: read_only::Walk::default(),
            cfg,
            bindings: HashMap::default(),
            binding_undo: Vec::new(),
            sites: Vec::new(),
            place_sites: Vec::new(),
            numeric: HashSet::default(),
            unsigned: HashSet::default(),
            moved: HashMap::default(),
            reinitializations: HashMap::default(),
            exits: Vec::new(),
            loops: Vec::new(),
            structures: HashMap::default(),
            blocks: Vec::new(),
            contexts: HashMap::default(),
            following: HashMap::default(),
            move_blocks: HashMap::default(),
            unreachable: Vec::new(),
            restorations: Vec::new(),
            writes: HashSet::default(),
            reborrowable: HashSet::default(),
            continuation: None,
            continuations: HashMap::default(),
            alternatives: Vec::new(),
            return_type: None,
            returned_types: HashMap::default(),
            receiver_moves: HashMap::default(),
        }
    }

    #[test]
    fn residual_grouped_prefixes_keep_the_effects_before_a_nested_read() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(value));
        let target = walk.place(&parse_quote!(value)).unwrap();
        let expression = Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(syn::parse_str("callee(first(), value, after())").unwrap()),
        });
        walk.visit_expr(&expression);
        assert_eq!(
            walk.use_prefix(&expression, &target, 64)
                .map(|prefix| prefix.into_iter().cloned().collect::<Vec<_>>()),
            Some(vec![parse_quote!(callee), parse_quote!(first())])
        );
    }

    #[test]
    fn residual_nested_function_and_closure_returns_do_not_replace_the_outer_return_or_exit_suffix() {
        let cfg = CfgSet::parse("");
        for nested in ["function", "closure"] {
            let mut walk = empty_walk(&cfg);
            walk.bind(&parse_quote!(value));
            let outer_use: Expr = syn::parse_str("value").unwrap();
            walk.visit_expr(&outer_use);
            walk.moved_argument(&outer_use);
            walk.blocks.push(0..1000);
            let outer_effect: Expr = parse_quote!(outer_effect());
            walk.continuation = Some(vec![&outer_effect].into());
            walk.return_type = Some(parse_quote!(bool));
            let inner: syn::ItemFn = parse_quote! {
                fn inner() -> char {
                    let value = 'x';
                    return value;
                }
            };
            let closure = syn::parse_str("|| { let value = 'x'; return value; }").unwrap();
            if nested == "function" {
                walk.function(&inner.sig, Some(&inner.block));
            } else {
                walk.visit_expr_closure(&closure);
            }
            let returned = syn::parse_str::<Expr>("return value").unwrap();
            walk.visit_expr(&returned);
            assert_eq!(
                walk.returned_types.get(&outer_use.span().byte_range()),
                Some(&parse_quote!(bool)),
                "{nested}"
            );
            walk.exit(900..901, true, false);
            assert_eq!(
                walk.exits.last().unwrap().continuation.as_deref().map(copied),
                Some(vec![parse_quote!(outer_effect())]),
                "{nested}"
            );
        }
    }

    #[test]
    fn residual_each_loop_restores_the_enclosing_exit_continuation_after_its_own_suffix() {
        let cfg = CfgSet::parse("");
        for expression in [
            "for item in items { break; inner_effect(); }",
            "loop { break; inner_effect(); }",
            "while ready { break; inner_effect(); }",
        ] {
            let mut walk = empty_walk(&cfg);
            walk.blocks.push(0..1000);
            let outer_effect: Expr = parse_quote!(outer_effect());
            walk.continuation = Some(vec![&outer_effect].into());
            let parsed = syn::parse_str::<Expr>(expression).unwrap();
            walk.visit_expr(&parsed);
            assert_eq!(
                walk.exits[0].continuation.as_deref().map(copied),
                Some(vec![parse_quote!(inner_effect())]),
                "{expression}"
            );
            walk.exit(900..901, true, false);
            assert_eq!(
                walk.exits.last().unwrap().continuation.as_deref().map(copied),
                Some(vec![parse_quote!(outer_effect())]),
                "{expression}"
            );
        }
    }

    #[test]
    fn residual_match_arms_are_alternatives_and_return_types_do_not_spread_to_other_places() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        let expression: Expr = syn::parse_str("match option { Some(value) => consume(value), None => observe(other) }").unwrap();
        walk.visit_expr(&expression);
        let Expr::Match(matched) = &expression else {
            panic!("match fixture")
        };
        assert!(!walk.compatible(&matched.arms[0].span().byte_range(), &matched.arms[1].span().byte_range()));
        let file = parse_file("fn f(left: String, right: bool) -> bool { consume(left); observe(right); return right; }").unwrap();
        walk.visit_file(&file);
        let mut classified = 0;
        for (span, place) in &walk.place_sites {
            if place.root == "left" {
                assert_eq!(walk.returned_types.get(span), None);
            } else if place.root == "right" && walk.returned_types.contains_key(span) {
                assert_eq!(walk.returned_types.get(span), Some(&parse_quote!(bool)));
                classified += 1;
            }
        }
        assert_eq!(classified, 1);
    }

    #[test]
    fn cohort_use_prefix_preserves_evaluation_order_and_stops_before_the_first_read() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(value));
        let target = walk.place(&parse_quote!(value)).unwrap();
        for (expression, expected) in [
            (
                "receiver.method(first(), consume(value), last())",
                Some(vec!["receiver", "first()", "consume"]),
            ),
            (
                "callee(first(), receiver.method(value), last())",
                Some(vec!["callee", "first()", "receiver"]),
            ),
            ("(&value).method(later())", Some(vec![])),
            ("return &((value))", Some(vec![])),
            ("return", None),
            ("callee(unrelated)", None),
            ("receiver.method(unrelated)", None),
        ] {
            let expression: Expr = syn::parse_str(expression).unwrap();
            let mut walk = empty_walk(&cfg);
            walk.bind(&parse_quote!(value));
            walk.visit_expr(&expression);
            assert_eq!(
                walk.use_prefix(&expression, &target, 64)
                    .map(|prefix| prefix.into_iter().cloned().collect::<Vec<_>>()),
                expected.map(|prefix| prefix.into_iter().map(|text| syn::parse_str::<Expr>(text).unwrap()).collect()),
                "{expression:?}"
            );
        }
        let expression: Expr = syn::parse_str("value").unwrap();
        walk.visit_expr(&expression);
        assert_eq!(walk.use_prefix(&expression, &target, 0), None);
        assert_eq!(walk.use_prefix(&expression, &target, 1), Some(Vec::new()));
        walk.writes.insert(expression.span().byte_range());
        assert_eq!(walk.use_prefix(&expression, &target, 1), None);
        walk.writes.clear();
        assert_eq!(walk.use_prefix(&expression, &place(target.binding + 1, &[]), 1), None);
    }

    #[test]
    fn cohort_exit_prefix_retains_only_effects_before_the_read_and_distinguishes_unknown_continuations() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(value));
        let target = walk.place(&parse_quote!(value)).unwrap();
        let continuation =
            ["before()", "callee(first(), value, after())", "unreachable_after_read()"].map(|text| syn::parse_str::<Expr>(text).unwrap());
        for expression in &continuation {
            walk.visit_expr(expression);
        }
        let mut exit = Exit {
            span: 0..1,
            moved: Vec::new(),
            loop_start: None,
            context: Context {
                block: 0..1,
                loop_region: None,
            },
            plain: true,
            continuing: false,
            continuation: Some(continuation.iter().collect()),
        };
        assert_eq!(
            walk.exit_prefix(&exit, &target).as_deref().map(copied),
            Some(vec![parse_quote!(before()), parse_quote!(callee), parse_quote!(first())])
        );
        exit.continuation = Some(vec![&continuation[0]].into());
        assert_eq!(
            walk.exit_prefix(&exit, &target).as_deref().map(copied),
            Some(vec![parse_quote!(before())])
        );
        assert!(Rc::ptr_eq(
            exit.continuation.as_ref().unwrap(),
            &walk.exit_prefix(&exit, &target).unwrap()
        ));
        exit.continuation = Some(Rc::default());
        assert_eq!(walk.exit_prefix(&exit, &target).as_deref().map(copied), Some(Vec::new()));
        exit.continuation = None;
        assert_eq!(walk.exit_prefix(&exit, &target), None);
    }

    #[test]
    fn cohort_block_continuations_include_only_active_initializers_and_restore_the_outer_suffix() {
        let cfg = CfgSet::parse("");
        for (text, expected) in [
            (
                "{ break; let restored = reset(); observe(value); }",
                Some(vec!["reset()", "observe(value)", "outer()"]),
            ),
            (
                "{ break; #[cfg(any())] hidden(); observe(value); }",
                Some(vec!["observe(value)", "outer()"]),
            ),
            ("{ break; let uninitialized; observe(value); }", None),
            ("{ break; let Some(restored) = reset() else { return }; observe(value); }", None),
            ("{ break; struct Local; observe(value); }", None),
        ] {
            let mut walk = empty_walk(&cfg);
            let outer: Expr = parse_quote!(outer());
            walk.continuation = Some(vec![&outer].into());
            let block: Block = syn::parse_str(text).unwrap();
            walk.visit_block(&block);
            assert_eq!(
                walk.exits[0].continuation.as_deref().map(copied),
                expected.map(|suffix| suffix.into_iter().map(|text| syn::parse_str::<Expr>(text).unwrap()).collect()),
                "{text}"
            );
            assert_eq!(walk.continuation.as_deref().map(copied), Some(vec![parse_quote!(outer())]));
        }
        let mut walk = empty_walk(&cfg);
        let block = syn::parse_str::<Block>("{ break; observe(value); }").unwrap();
        walk.visit_block(&block);
        assert!(walk.exits[0].continuation.is_none());
    }

    #[test]
    fn cohort_use_prefix_depth_limit_counts_every_wrapper_and_the_final_read() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(value));
        let target = walk.place(&parse_quote!(value)).unwrap();
        let at_limit = concat!(
            "((((((((", "((((((((", "((((((((", "((((((((", "((((((((", "((((((((", "((((((((", "(((((((", "value", ")))))))", "))))))))",
            "))))))))", "))))))))", "))))))))", "))))))))", "))))))))", "))))))))"
        );
        let expression: Expr = syn::parse_str(at_limit).unwrap();
        walk.visit_expr(&expression);
        assert_eq!(walk.use_prefix(&expression, &target, 64), Some(Vec::new()));
        let mut exit = Exit {
            span: 0..1,
            moved: Vec::new(),
            loop_start: None,
            context: Context {
                block: 0..1,
                loop_region: None,
            },
            plain: true,
            continuing: false,
            continuation: Some(vec![&expression].into()),
        };
        assert_eq!(walk.exit_prefix(&exit, &target).as_deref().map(copied), Some(Vec::new()));
        let wrapped = Expr::Paren(syn::ExprParen {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            expr: Box::new(expression.clone()),
        });
        assert_eq!(walk.use_prefix(&wrapped, &target, 64), None);
        exit.continuation = Some(vec![&wrapped].into());
        assert_eq!(walk.exit_prefix(&exit, &target).as_deref().map(copied), Some(vec![wrapped]));
    }

    #[test]
    fn cohort_continuation_budget_admits_exactly_thirty_two_following_expressions() {
        let cfg = CfgSet::parse("");
        let block: Block = syn::parse_str(concat!(
            "{ break;",
            "a(); b(); c(); d(); e(); f(); g(); h();",
            "a(); b(); c(); d(); e(); f(); g(); h();",
            "a(); b(); c(); d(); e(); f(); g(); h();",
            "a(); b(); c(); d(); e(); f(); g(); h();",
            "}",
        ))
        .unwrap();
        let outer: Expr = parse_quote!(outer());
        for (outer, expected) in [(Vec::new(), true), (vec![&outer], false)] {
            let mut walk = empty_walk(&cfg);
            walk.continuation = Some(outer.into());
            walk.visit_block(&block);
            assert_eq!(walk.exits[0].continuation.is_some(), expected);
            if expected {
                let expected = block
                    .stmts
                    .iter()
                    .skip(1)
                    .map(|statement| {
                        let syn::Stmt::Expr(expression, _) = statement else {
                            panic!("fixture expression")
                        };
                        expression.clone()
                    })
                    .collect::<Vec<_>>();
                assert_eq!(walk.exits[0].continuation.as_deref().map(copied), Some(expected));
            }
        }
    }

    #[test]
    fn source_backed_continuations_count_construction_capture_and_partial_prefix_storage() {
        let cfg = CfgSet::parse("");
        let block: Block = syn::parse_str("{ break; let restored = reset(); observe(value); }").unwrap();
        let outer_expression: Expr = parse_quote!(outer());
        let outer: Rc<[&Expr]> = vec![&outer_expression].into();
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(value));
        let target = walk.place(&parse_quote!(value)).unwrap();
        walk.continuation = Some(Rc::clone(&outer));
        walk.visit_block(&block);
        let captured = walk.exits[0].continuation.as_ref().unwrap();
        let prefix = walk.exit_prefix(&walk.exits[0], &target).unwrap();
        let suffixes = (0..block.stmts.len())
            .map(|index| walk.continuation(&block.stmts[index + 1..], &outer).unwrap())
            .collect::<Vec<_>>();
        assert!(Rc::ptr_eq(suffixes.last().unwrap(), &outer));
        assert!(Rc::ptr_eq(walk.continuation.as_ref().unwrap(), &outer));
        assert_eq!(copied(&prefix), vec![parse_quote!(reset()), parse_quote!(observe)]);
        assert!(core::ptr::eq(prefix[0], captured[0]));
        let Expr::Call(reset) = captured[0] else {
            panic!("reset call fixture")
        };
        let Expr::Call(observe) = captured[1] else {
            panic!("observe call fixture")
        };
        assert!(core::ptr::eq(prefix[1], observe.func.as_ref()));
        assert!(core::ptr::eq(
            core::ptr::from_ref(captured[2]),
            core::ptr::from_ref(&outer_expression)
        ));

        // The copied implementation cloned the outer suffix, every constructed suffix, the
        // captured exit, and the partial prefix (including reset's discarded callee prefix).
        let copied_nodes = expression_nodes(&outer)
            + suffixes.iter().map(|suffix| expression_nodes(suffix)).sum::<usize>()
            + expression_nodes(captured)
            + expression_nodes(&prefix)
            + expression_nodes(&[&reset.func]);
        let constructed_sequence_handles = outer.len()
            + suffixes
                .iter()
                .filter(|suffix| !Rc::ptr_eq(suffix, &outer))
                .map(|suffix| suffix.len())
                .sum::<usize>()
            + prefix.len();
        let retained_handles = outer.len() + captured.len() + prefix.len();
        assert_eq!((copied_nodes, constructed_sequence_handles, retained_handles), (27, 8, 6));
    }

    #[test]
    fn source_backed_reinitialization_shares_one_captured_sequence_across_moved_places() {
        let cfg = CfgSet::parse("");
        let file =
            parse_file("fn f(mut pair: Pair) { loop { consume(pair.left); consume(pair.right); pair = make(); after(); } }").unwrap();
        let mut walk = empty_walk(&cfg);
        walk.visit_file(&file);
        let mut actual = Evidence::default();
        reinitializations(&walk, &mut actual);
        let (site, captured) = walk.continuations.iter().next().unwrap();
        let checks = &actual.ownership_continuations[site];
        assert_eq!(actual.invalid_ownership[site].len(), 2);
        assert_eq!(checks.len(), 2);
        assert!(checks.values().all(|sequence| Rc::ptr_eq(sequence, captured)));
        assert_eq!(copied(captured), vec![parse_quote!(after())]);
        let copied_nodes = expression_nodes(captured) * (1 + checks.len());
        assert_eq!((copied_nodes, captured.len(), checks.len()), (6, 1, 2));
    }

    #[test]
    fn cohort_branch_and_closure_visitors_preserve_binding_identity_and_loop_continuations() {
        let cfg = CfgSet::parse("");
        let file = parse_file(
            "fn f(value: bool) {
                loop {
                    if let Some(value) = option { consume(value); } else { consume(value); }
                    let closure = || { while let Some(value) = next() { consume(value); } };
                    consume(value);
                    break;
                }
            }",
        )
        .unwrap();
        let mut walk = empty_walk(&cfg);
        walk.visit_file(&file);
        assert_eq!(walk.alternatives.len(), 1);
        let branches = &walk.alternatives[0];
        assert_eq!(branches.len(), 2);
        let mut outer_reads = walk.place_sites.iter().filter(|(_, place)| place.root == "value");
        let (_, conditional) = outer_reads.next().unwrap();
        let (_, otherwise) = outer_reads.next().unwrap();
        let (_, closure) = outer_reads.next().unwrap();
        let (_, after) = outer_reads.next().unwrap();
        assert_ne!(conditional.binding, otherwise.binding);
        assert_eq!(otherwise.binding, after.binding);
        assert_ne!(closure.binding, after.binding);
        assert!(!walk.compatible(&branches[0], &branches[1]));
        assert_eq!(walk.loops, Vec::<Range<usize>>::new());
        assert!(walk.continuation.is_none());
        assert!(walk.bindings.is_empty());
    }

    #[test]
    fn cohort_branch_compatibility_checks_both_span_endpoints_and_branch_identity() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.alternatives.push(vec![10..20, 30..40]);
        for (left, right, expected) in [
            (10..20, 30..40, false),
            (11..19, 31..39, false),
            (11..19, 11..19, true),
            (11..19, 12..18, true),
            (9..19, 31..39, true),
            (11..21, 31..39, true),
            (11..19, 29..39, true),
            (11..19, 31..41, true),
            (0..5, 50..60, true),
        ] {
            assert_eq!(walk.compatible(&left, &right), expected, "{left:?}: {right:?}");
            assert_eq!(walk.compatible(&right, &left), expected, "{right:?}: {left:?}");
        }
    }

    #[test]
    fn cohort_move_dominance_requires_a_complete_enclosing_block() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        let expression: Expr = syn::parse_str("consume(value)").unwrap();
        let context = Context {
            block: 10..20,
            loop_region: None,
        };
        for (block, expected) in [(10..20, true), (9..21, true), (11..20, false), (10..19, false)] {
            walk.move_blocks.insert(expression.span().byte_range(), block.clone());
            assert_eq!(walk.move_dominates(&expression, &context), expected, "{block:?}");
        }
        walk.move_blocks.clear();
        assert!(!walk.move_dominates(&expression, &context));
    }

    #[test]
    fn cohort_later_reads_ignore_only_reachable_compatible_complete_restorations() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        let target = place(1, &["left"]);
        let position = 10..20;
        walk.place_sites = vec![(40..50, target.clone())];
        assert!(walk.used_after_compatible(&position, &target));
        assert!(!walk.used_after_compatible(&(10..40), &target));
        assert!(!walk.used_after_compatible(&position, &place(2, &["left"])));
        assert!(!walk.used_after_compatible(&position, &place(1, &["right"])));
        walk.writes.insert(40..50);
        assert!(!walk.used_after_compatible(&position, &target));
        walk.writes.clear();
        for (unreachable, expected) in [(40..50, false), (39..51, false), (41..50, true), (40..49, true)] {
            walk.unreachable = vec![unreachable.clone()];
            assert_eq!(walk.used_after_compatible(&position, &target), expected, "{unreachable:?}");
        }
        walk.unreachable.clear();
        for (restored, binding, fields, expected) in [
            (21..39, 1, vec![], false),
            (21..39, 1, vec!["left"], false),
            (21..39, 1, vec!["left", "nested"], true),
            (21..39, 2, vec!["left"], true),
            (20..39, 1, vec!["left"], true),
            (21..40, 1, vec!["left"], true),
            (0..9, 1, vec!["left"], true),
            (51..60, 1, vec!["left"], true),
        ] {
            walk.restorations = vec![(restored.clone(), place(binding, &fields))];
            assert_eq!(walk.used_after_compatible(&position, &target), expected, "{restored:?}: {fields:?}");
        }
        walk.restorations = vec![(25..30, target.clone())];
        walk.alternatives = vec![vec![10..20, 25..30]];
        assert!(walk.used_after_compatible(&position, &target));
        walk.restorations.clear();
        walk.alternatives = vec![vec![10..20, 40..50]];
        assert!(!walk.used_after_compatible(&position, &target));
    }

    #[test]
    fn fresh_context_grouped_places_and_numeric_notes_preserve_optional_state() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        assert!(walk.context().is_none());
        walk.bind(&parse_quote!(whole @ Some(inner)));
        assert!(walk.bindings.contains_key("whole"));
        assert!(walk.bindings.contains_key("inner"));
        walk.bind(&parse_quote!(start));
        walk.bind(&parse_quote!(end));
        walk.note(&parse_quote!(start..end), false);
        assert!(walk.numeric.is_empty());
        assert!(walk.unsigned.is_empty());
        let group = Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(parse_quote!(inner)),
        });
        walk.note(&group, false);
        assert!(walk.numeric.contains(&walk.bindings["inner"]));
        assert!(walk.unsigned.is_empty());
        let place = walk.place(&group).unwrap();
        assert_eq!(place.root, "inner");
        assert_eq!(place.binding, walk.bindings["inner"]);
    }

    #[test]
    fn nested_function_traversal_restores_the_outer_binding_undo_log() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        walk.bind(&parse_quote!(outer));
        let undo = walk.binding_undo.clone();
        let bindings = walk.bindings.clone();
        let body = parse_quote!({
            let local = parameter;
        });
        walk.function(&parse_quote!(fn nested(parameter: u8)), Some(&body));
        assert_eq!(walk.binding_undo, undo);
        assert_eq!(walk.bindings, bindings);
    }

    #[test]
    fn unreachable_prescan_skips_inactive_statements_without_stopping_the_active_tail() {
        let cfg = CfgSet::parse("");
        let mut walk = empty_walk(&cfg);
        let block: Block = parse_quote!({
            return;
            #[cfg(any())]
            hidden();
            live();
        });
        walk.visit_block(&block);
        assert_eq!(walk.unreachable, [block.stmts.last().unwrap().span().byte_range()]);
    }

    fn place(binding: usize, fields: &[&str]) -> Place {
        Place {
            binding,
            root: "value".into(),
            fields: fields.iter().map(|field| (*field).into()).collect(),
        }
    }

    #[test]
    fn place_containment_uses_binding_identity_and_overlap_is_symmetric_for_ancestor_fields() {
        let root = place(1, &[]);
        let child = place(1, &["left"]);
        let nested = place(1, &["left", "count"]);
        let sibling = place(1, &["right"]);
        let shadow = place(2, &["left"]);
        assert!(root.contains(&child));
        assert!(!child.contains(&root));
        assert!(child.contains(&nested));
        assert!(!child.contains(&sibling));
        assert!(!child.contains(&shadow));
        for (left, right) in [(&root, &child), (&child, &nested)] {
            assert!(left.overlaps(right));
            assert!(right.overlaps(left));
        }
        assert!(!child.overlaps(&sibling));
        assert!(!child.overlaps(&shadow));
    }

    #[test]
    fn later_uses_stop_at_the_earliest_matching_restoration_and_ignore_writes_and_shadows() {
        let cfg = CfgSet::unconditional();
        let mut walk = empty_walk(&cfg);
        let field = place(1, &["left"]);
        walk.place_sites.push((15..16, field.clone()));
        assert!(walk.used_after(10, &field));
        assert!(!walk.used_after(15, &field));
        assert!(!walk.used_after(10, &place(2, &["left"])));
        assert!(!walk.used_after(10, &place(1, &["right"])));
        walk.writes.insert(15..16);
        assert!(!walk.used_after(10, &field));
        walk.writes.clear();
        walk.restorations.push((20..21, field.clone()));
        assert!(walk.used_after(10, &field));
        walk.restorations.push((12..13, place(2, &["left"])));
        assert!(walk.used_after(10, &field));
        walk.restorations.push((12..13, field.clone()));
        assert!(!walk.used_after(10, &field));
        walk.restorations.clear();
        walk.restorations.push((15..16, field.clone()));
        assert!(!walk.used_after(10, &field));
        walk.restorations.clear();
        walk.restorations.push((10..11, field.clone()));
        assert!(walk.used_after(10, &field));
    }

    #[test]
    fn loop_restoration_checks_both_endpoints_exclusions_and_the_before_cutoff() {
        let cfg = CfgSet::unconditional();
        let mut walk = empty_walk(&cfg);
        let field = place(1, &["left"]);
        let context = Context {
            block: 0..40,
            loop_region: Some(10..30),
        };
        for (span, binding, before, expected) in [
            (10..30, 1, None, true),
            (9..29, 1, None, false),
            (11..31, 1, None, false),
            (11..29, 2, None, false),
            (11..29, 1, Some(28), false),
            (11..29, 1, Some(29), true),
            (11..29, 1, Some(30), true),
        ] {
            walk.restorations = vec![(span.clone(), place(binding, &["left"]))];
            assert_eq!(
                walk.restored_in_loop(&context, &field, None, before),
                expected,
                "{span:?}: {before:?}"
            );
            assert!(!walk.restored_in_loop(&context, &field, Some(&span), before));
        }
        let context = Context {
            block: 0..40,
            loop_region: None,
        };
        assert!(!walk.restored_in_loop(&context, &field, None, None));
    }

    #[test]
    fn reinitialization_requires_same_move_block_reachability_and_an_unrestored_loop_backedge() {
        let cfg = CfgSet::unconditional();
        let mut walk = empty_walk(&cfg);
        let expression: Expr = parse_quote!(consume(value));
        let context = Context {
            block: 0..100,
            loop_region: Some(10..90),
        };
        let field = place(1, &["left"]);
        let span = 30..40;
        walk.move_blocks.insert(expression.span().byte_range(), context.block.clone());
        walk.continuations.insert(span.clone(), Rc::default());
        assert!(walk.invalid_reinitialization(&span, &context, &field, &expression));
        assert!(!walk.invalid_reinitialization(&span, &context, &place(10, &["left"]), &expression));
        walk.restorations.push((40..50, field.clone()));
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.restorations.clear();
        walk.continuations.clear();
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.continuations.insert(span.clone(), Rc::default());
        for (unreachable, invalid) in [(0..40, false), (35..50, true), (20..35, true)] {
            walk.unreachable = vec![unreachable.clone()];
            assert_eq!(
                walk.invalid_reinitialization(&span, &context, &field, &expression),
                invalid,
                "{unreachable:?}"
            );
        }
        walk.unreachable.clear();
        walk.move_blocks.clear();
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
    }

    #[test]
    fn following_control_flow_distinguishes_reads_writes_and_exposed_uses_after_break() {
        let cfg = CfgSet::unconditional();
        let mut walk = empty_walk(&cfg);
        let expression: Expr = parse_quote!(consume(value));
        let context = Context {
            block: 0..100,
            loop_region: Some(10..90),
        };
        let field = place(1, &["left"]);
        let span = 30..40;
        walk.move_blocks.insert(expression.span().byte_range(), context.block.clone());
        walk.following.insert(
            span.clone(),
            Following {
                span: 40..60,
                breaks: false,
            },
        );
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.place_sites.push((45..46, field.clone()));
        assert!(walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.writes.insert(45..46);
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.place_sites = vec![(95..96, field.clone())];
        assert!(!walk.invalid_reinitialization(&span, &context, &field, &expression));
        walk.following.get_mut(&span).unwrap().breaks = true;
        assert!(walk.invalid_reinitialization(&span, &context, &field, &expression));
    }

    #[test]
    fn exits_without_any_relevant_later_use_do_not_publish_empty_ownership_regions() {
        let cfg = CfgSet::unconditional();
        let mut walk = empty_walk(&cfg);
        walk.exits.push(Exit {
            span: 30..40,
            moved: vec![(place(1, &[]), parse_quote!(value))],
            loop_start: None,
            context: Context {
                block: 0..100,
                loop_region: None,
            },
            plain: true,
            continuing: false,
            continuation: Some(Rc::default()),
        });
        let mut actual = Evidence::default();
        exits(&walk, &mut actual);
        assert!(actual.exits.is_empty());
        assert!(actual.ownership_regions.is_empty());
        assert!(actual.invalid_ownership.is_empty());
        assert!(actual.invalid_transitions.is_empty());
    }

    #[test]
    fn exit_evidence_separates_loop_carried_moves_restorations_and_break_continue_transitions() {
        let cfg = CfgSet::unconditional();
        // Binding IDs are declaration offsets: only declarations before the loop start are
        // loop-carried. Reads straddle the loop start, exit end, and loop end; restorations lie
        // before the exit, later within the loop, or beyond it to separate ownership from reentry.
        for (binding, looped, read, restoration, continuing, ownership, transition) in [
            (1, false, 41..42, None, false, Some(true), false),
            (1, false, 40..41, None, false, None, false),
            (1, false, 39..40, None, false, None, false),
            (1, true, 10..11, None, false, Some(true), true),
            (1, true, 9..10, None, false, None, false),
            (10, true, 15..16, None, false, None, false),
            (10, true, 41..42, None, false, Some(true), false),
            (11, true, 15..16, None, false, None, false),
            (1, true, 15..16, Some(20..25), false, Some(false), false),
            (1, true, 15..16, Some(50..60), false, Some(false), true),
            (1, true, 15..16, Some(50..60), true, Some(false), false),
            (1, true, 95..96, Some(50..60), true, Some(false), true),
            (11, true, 95..96, Some(91..92), false, Some(false), false),
        ] {
            let mut walk = empty_walk(&cfg);
            let moved = place(binding, &["left"]);
            let expression: Expr = parse_quote!(value.left);
            let context = Context {
                block: 0..100,
                loop_region: looped.then_some(10..90),
            };
            walk.move_blocks.insert(expression.span().byte_range(), context.block.clone());
            walk.place_sites = vec![(read.clone(), moved.clone()), (99..100, place(999, &["left"]))];
            if let Some(span) = restoration.clone() {
                walk.restorations.push((span, moved.clone()));
            }
            walk.exits.push(Exit {
                span: 30..40,
                moved: vec![(moved, expression)],
                loop_start: looped.then_some(10),
                context,
                plain: true,
                continuing,
                continuation: Some(Rc::default()),
            });
            let mut actual = Evidence::default();
            exits(&walk, &mut actual);
            let case = format!("binding={binding} loop={looped} read={read:?} restoration={restoration:?} continuing={continuing}");
            assert_eq!(actual.exits.contains_key(&(30..40)), ownership.is_some(), "{case}");
            assert_eq!(actual.ownership_regions.contains_key(&(30..40)), ownership.is_some(), "{case}");
            assert_eq!(actual.invalid_ownership.contains_key(&(30..40)), ownership == Some(true), "{case}");
            assert_eq!(actual.invalid_transitions.contains_key(&(30..40)), transition, "{case}");
        }
    }

    #[test]
    fn lexical_scope_rollback_preserves_shadowed_and_branch_local_binding_identities() {
        for (source, numeric_sites) in [
            ("fn f(value: Unknown) { { let value = other; value - 1; } value; }", &[1, 2][..]),
            ("fn f(value: Unknown) { let closure = |value| value - 1; value; }", &[1, 2][..]),
            (
                "fn f(value: Unknown) { if let Some(value) = other { value - 1; } else { value; } value; }",
                &[1, 2][..],
            ),
            (
                "fn f(value: Unknown) { while let Some(value) = other { value - 1; } value; }",
                &[1, 2][..],
            ),
            ("fn f(value: Unknown) { for value in items { value - 1; } value; }", &[1, 2][..]),
            (
                "fn f(value: Unknown) { match other { Some(value) => value - 1, _ => { value; } } value; }",
                &[1, 2][..],
            ),
            (
                "fn f(value: Unknown) { fn nested(value: Unknown) { value - 1; } value; }",
                &[1, 2][..],
            ),
            (
                "fn f(value: Unknown) { { let value = other; let value = other; value - 1; } value; }",
                &[2, 3][..],
            ),
            ("fn f(value: Unknown) { { let value = other; } value - 1; }", &[0, 2][..]),
        ] {
            let file = parse_file(source).unwrap();
            let evidence = evidence(&file, &CfgSet::unconditional());
            for (index, (start, name)) in source.match_indices("value").enumerate() {
                assert_eq!(
                    evidence.numeric.contains(&(start..start + name.len())),
                    numeric_sites.contains(&index),
                    "{source}: value occurrence {index}"
                );
            }
        }
    }
}
