// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lexical numeric-use and ownership-guard evidence.

use core::ops::Range;

use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, Pat};

use super::predicates::{expr_attrs, is_int_literal, is_numeric_receiver, stmt_attrs};
use crate::cfg::CfgSet;
use crate::ops::collect::defaults::{impl_item_attrs, item_attrs, trait_item_attrs};
use crate::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct Evidence {
    pub(super) numeric: HashSet<Range<usize>>,
    pub(super) unsigned: HashSet<Range<usize>>,
    pub(super) reinitializations: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) exits: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) structures: HashMap<String, Option<HashMap<String, syn::Type>>>,
    pub(super) ownership_regions: HashMap<Range<usize>, Range<usize>>,
    pub(super) invalid_ownership: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) invalid_transitions: HashMap<Range<usize>, Vec<Expr>>,
    pub(super) reborrowable: HashSet<Range<usize>>,
}

pub(super) fn evidence(file: &syn::File, cfg: &CfgSet) -> Evidence {
    let mut walk = Walk {
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
        loop_bodies: HashMap::default(),
        unreachable: Vec::new(),
        restorations: Vec::new(),
        writes: HashSet::default(),
        reborrowable: HashSet::default(),
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
    evidence
}

fn reinitializations(walk: &Walk<'_>, evidence: &mut Evidence) {
    for (span, moved) in &walk.reinitializations {
        if let Some(context) = walk.contexts.get(span) {
            let invalid = moved
                .iter()
                .filter(|(place, expression)| walk.invalid_reinitialization(span, context, place, expression))
                .map(|(_, expression)| expression.clone())
                .collect::<Vec<_>>();
            if !invalid.is_empty() {
                let _prior = evidence.invalid_ownership.insert(span.clone(), invalid);
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
fn exits(walk: &Walk<'_>, evidence: &mut Evidence) {
    for exit in &walk.exits {
        let mut invalid = Vec::new();
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
                if exit.plain && walk.move_blocks.get(&expression.span().byte_range()) == Some(&exit.context.block) {
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
                    if walk.used_after(exit.span.end, place) || backedge {
                        invalid.push(expression.clone());
                    }
                }
                expression.clone()
            })
            .collect::<Vec<_>>();
        if !names.is_empty() {
            let _prior = evidence.ownership_regions.insert(exit.span.clone(), exit.context.region());
            if !invalid.is_empty() {
                let _prior = evidence.invalid_ownership.insert(exit.span.clone(), invalid);
            }
            if !invalid_transitions.is_empty() {
                let _prior = evidence.invalid_transitions.insert(exit.span.clone(), invalid_transitions);
            }
            let _previous = evidence.exits.insert(exit.span.clone(), names);
        }
    }
}

struct Exit {
    span: Range<usize>,
    moved: Vec<(Place, Expr)>,
    loop_start: Option<usize>,
    context: Context,
    plain: bool,
    continuing: bool,
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

struct Walk<'a> {
    cfg: &'a CfgSet,
    bindings: HashMap<String, usize>,
    binding_undo: Vec<(String, Option<usize>)>,
    sites: Vec<(Range<usize>, usize)>,
    place_sites: Vec<(Range<usize>, Place)>,
    numeric: HashSet<usize>,
    unsigned: HashSet<usize>,
    moved: HashMap<Place, Expr>,
    reinitializations: HashMap<Range<usize>, Vec<(Place, Expr)>>,
    exits: Vec<Exit>,
    loops: Vec<Range<usize>>,
    structures: HashMap<String, Option<HashMap<String, syn::Type>>>,
    blocks: Vec<Range<usize>>,
    contexts: HashMap<Range<usize>, Context>,
    following: HashMap<Range<usize>, Following>,
    move_blocks: HashMap<Range<usize>, Range<usize>>,
    loop_bodies: HashMap<Range<usize>, Range<usize>>,
    unreachable: Vec<Range<usize>>,
    restorations: Vec<(Range<usize>, Place)>,
    writes: HashSet<Range<usize>>,
    reborrowable: HashSet<Range<usize>>,
}

impl Walk<'_> {
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
            || self.move_blocks.get(&expression.span().byte_range()) != Some(&context.block)
        {
            return false;
        }
        if let Some(following) = self.following.get(span) {
            if self.place_sites.iter().any(|(site, used)| {
                !self.writes.contains(site) && following.span.start <= site.start && site.end <= following.span.end && place.overlaps(used)
            }) {
                return true;
            }
            return following.breaks
                && context
                    .loop_region
                    .as_ref()
                    .is_some_and(|region| self.used_after(region.end, place));
        }
        context
            .loop_region
            .as_ref()
            .is_some_and(|region| self.loop_bodies.get(region) == Some(&context.block) && place.binding < region.start)
            && !self.restored_in_loop(context, place, Some(span), None)
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
                let _prior = self.move_blocks.insert(expression.span().byte_range(), block.clone());
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
        });
    }

    fn function(&mut self, signature: &syn::Signature, body: Option<&syn::Block>) {
        let outer = core::mem::take(&mut self.bindings);
        let outer_undo = core::mem::take(&mut self.binding_undo);
        let outer_moved = core::mem::take(&mut self.moved);
        let outer_loops = core::mem::take(&mut self.loops);
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
    }
}

#[expect(clippy::renamed_function_params, reason = "visitor parameters use descriptive names")]
impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item(&mut self, node: &'ast syn::Item) {
        if !self.cfg.skip_gate(item_attrs(node)) {
            visit::visit_item(self, node);
        }
    }

    fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
        if !self.cfg.skip_gate(impl_item_attrs(node)) {
            visit::visit_impl_item(self, node);
        }
    }

    fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
        if !self.cfg.skip_gate(trait_item_attrs(node)) {
            visit::visit_trait_item(self, node);
        }
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        if !self.cfg.skip_gate(stmt_attrs(node)) {
            visit::visit_stmt(self, node);
        }
    }

    fn visit_expr(&mut self, node: &'ast Expr) {
        if !self.cfg.skip_gate(expr_attrs(node)) {
            visit::visit_expr(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.function(&node.sig, Some(&node.block));
    }

    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
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
        self.function(&node.sig, Some(&node.block));
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.function(&node.sig, node.default.as_ref());
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        let checkpoint = self.binding_undo.len();
        self.blocks.push(node.span().byte_range());
        let mut exited = false;
        for statement in &node.stmts {
            if self.cfg.skip_gate(stmt_attrs(statement)) {
                continue;
            }
            if exited {
                self.unreachable.push(statement.span().byte_range());
            }
            exited |= matches!(statement, syn::Stmt::Expr(Expr::Break(_) | Expr::Continue(_) | Expr::Return(_), _));
        }
        for pair in node.stmts.windows(2) {
            if let syn::Stmt::Expr(expression, _) = &pair[0] {
                let _prior = self.following.insert(
                    expression.span().byte_range(),
                    Following {
                        span: pair[1].span().byte_range(),
                        breaks: matches!(&pair[1], syn::Stmt::Expr(Expr::Break(brk), _) if brk.label.is_none()),
                    },
                );
            }
        }
        visit::visit_block(self, node);
        let _block = self.blocks.pop();
        self.restore_bindings(checkpoint);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
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
        let checkpoint = self.binding_undo.len();
        let outer_loops = core::mem::take(&mut self.loops);
        for input in &node.inputs {
            self.bind(input);
        }
        self.visit_expr(&node.body);
        self.restore_bindings(checkpoint);
        self.loops = outer_loops;
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        let checkpoint = self.binding_undo.len();
        self.visit_expr(&node.cond);
        self.visit_block(&node.then_branch);
        self.restore_bindings(checkpoint);
        if let Some((_, otherwise)) = &node.else_branch {
            self.visit_expr(otherwise);
        }
        self.restore_bindings(checkpoint);
    }

    fn visit_expr_let(&mut self, node: &'ast syn::ExprLet) {
        self.visit_expr(&node.expr);
        self.bind(&node.pat);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.visit_expr(&node.expr);
        let checkpoint = self.binding_undo.len();
        self.bind(&node.pat);
        if matches!(&*node.expr, Expr::Range(_))
            && let Pat::Ident(ident) = &*node.pat
        {
            let _inserted = self.numeric.insert(ident.ident.span().byte_range().start);
        }
        self.loops.push(node.span().byte_range());
        let _prior = self.loop_bodies.insert(node.span().byte_range(), node.body.span().byte_range());
        self.visit_block(&node.body);
        let _loop = self.loops.pop();
        self.restore_bindings(checkpoint);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if self.cfg.skip_gate(&node.attrs) {
            return;
        }
        let checkpoint = self.binding_undo.len();
        self.bind(&node.pat);
        if let Pat::Guard(guarded) = &node.pat {
            self.visit_expr(&guarded.guard);
        }
        self.visit_expr(&node.body);
        self.restore_bindings(checkpoint);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if let Some(binding) = node.path.get_ident().and_then(|name| self.bindings.get(&name.to_string())) {
            self.sites.push((node.span().byte_range(), *binding));
            self.place_sites.push((
                node.span().byte_range(),
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
            self.place_sites.push((node.span().byte_range(), place));
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
            let _inserted = self.reborrowable.insert(argument.span().byte_range());
            self.moved_argument(argument);
        }
        if matches!(
            node.method.to_string().as_str(),
            "into_iter" | "into_bytes" | "into_boxed_str" | "into_string" | "into_os_string" | "into_vec"
        ) {
            let _inserted = self.reborrowable.insert(node.receiver.span().byte_range());
            self.moved_argument(&node.receiver);
        }
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        visit::visit_expr_call(self, node);
        for argument in &node.args {
            let _inserted = self.reborrowable.insert(argument.span().byte_range());
            self.moved_argument(argument);
        }
    }

    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        let assigned = self.place(&node.left);
        if assigned.is_some() {
            let _prior = self.writes.insert(node.left.span().byte_range());
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
            self.restorations.push((node.span().byte_range(), assigned.clone()));
            self.moved.retain(|place, _| !assigned.contains(place));
            if !restored.is_empty() {
                if let Some(context) = self.context() {
                    let _prior = self.contexts.insert(node.span().byte_range(), context);
                }
                let _previous = self.reinitializations.insert(node.span().byte_range(), restored);
            }
        }
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.loops.push(node.span().byte_range());
        let _prior = self.loop_bodies.insert(node.span().byte_range(), node.body.span().byte_range());
        visit::visit_expr_loop(self, node);
        let _loop = self.loops.pop();
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.loops.push(node.span().byte_range());
        let _prior = self.loop_bodies.insert(node.span().byte_range(), node.body.span().byte_range());
        let checkpoint = self.binding_undo.len();
        self.visit_expr(&node.cond);
        self.visit_block(&node.body);
        self.restore_bindings(checkpoint);
        let _loop = self.loops.pop();
    }

    fn visit_expr_continue(&mut self, node: &'ast syn::ExprContinue) {
        self.exit(node.span().byte_range(), node.label.is_none(), true);
    }

    fn visit_expr_break(&mut self, node: &'ast syn::ExprBreak) {
        self.exit(node.span().byte_range(), node.label.is_none(), false);
        visit::visit_expr_break(self, node);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

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
            let file = syn::parse_file(source).unwrap();
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
