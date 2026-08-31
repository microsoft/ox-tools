// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Display;
use std::sync::Arc;

use compact_str::{CompactString, format_compact};
use proc_macro2::Span;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned as _;
use syn::token::Comma;
use syn::visit::{self, Visit};
use syn::{
    Attribute, BinOp, Block, Expr, ExprBinary, ExprBreak, ExprCall, ExprContinue, ExprForLoop, ExprIf, ExprIndex, ExprLit, ExprLoop,
    ExprMatch, ExprMethodCall, ExprRange, ExprReference, ExprRepeat, ExprReturn, ExprStruct, ExprUnary, ExprWhile, FnArg, GenericArgument,
    Generics, ImplItem, ImplItemConst, ImplItemFn, ItemConst, ItemFn, ItemImpl, ItemMod, ItemStatic, ItemTrait, Lit, Local, Macro, Member,
    Pat, RangeLimits, ReturnType, Signature, Stmt, TraitItemConst, TraitItemFn, Type, UnOp, Variant,
};

use super::stated::stated_range;
use super::{Candidate, Defaults, Shape};
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;
use crate::{HashMap, HashSet};

mod indexes;
mod noop;
pub(super) mod phase_one;
mod predicates;
mod tables;
mod types;
mod values;

#[cfg(test)]
mod tests;

use indexes::{Indexes, NumericUses, indexes};
use noop::is_noop;
use predicates::{
    binds_a_pattern, boolean_literal, callee_name, callee_type, declared_name, diverges, expr_attrs, is_assign_op, is_capacity_call,
    is_capacity_result, is_catch_all, is_constant_case, is_default_call, is_diagnostic_message, is_integer_zero_literal,
    is_numeric_binding, is_numeric_return, is_numeric_type, is_promotable, is_textual, loop_produces_value, returns_numeric,
    returns_result, stmt_attrs, type_name,
};
use tables::{binary_replacements, in_place_reorder, method_renames};
use types::{Types, returns_undefaultable_error, undefaulted_parameters};
use values::{Kind, resolve_type, return_values};

use super::defaults::{DefaultPaths, standard_defaulted_parameters};

/// The marker string every string-valued mutant is replaced by.
///
/// It is deliberately implausible: a test that happens to accept it was almost certainly not
/// asserting on the string at all.
const XYZZY: &str = "xyzzy";

/// One reversible change to the block-scoped binding evidence.
///
/// Remembered so a lexical block can undo exactly what it wrote — its own insertions and shadows —
/// rather than restoring a saved copy of the whole in-scope set. Replaying every entry recorded
/// since a block was entered, in reverse, returns `bindings` and `deferred` to the state they held
/// on entry even when a name was touched more than once.
enum Undo {
    /// Restores `bindings[name]` to the value it held before the block wrote it: `Some(was)` puts
    /// the prior type evidence back, `None` removes a name the block introduced.
    Binding(String, Option<bool>),

    /// Restores whether `deferred` contained `name` before the block changed it.
    Deferred(String, bool),
}

struct LoopContext {
    label: Option<String>,
    produces_value: bool,
}

/// The traversal state.
pub(super) struct Collector<'a> {
    file: &'a SourceFile,
    selection: &'a Selection,

    /// The enclosing item path at each nesting level, each entry already fully joined.
    ///
    /// Storing the joined form rather than the segments means emitting a candidate rebuilds nothing,
    /// and a large tree emits far more candidates than it opens scopes. Shared rather than owned so
    /// that emitting is a pointer copy: every candidate under one scope names the same path.
    scope: Vec<Arc<str>>,

    /// The path of a candidate outside any item, shared for the same reason.
    outermost: Arc<str>,

    candidates: Vec<Candidate>,

    /// Depth of nesting inside a context where mutation is not possible or not useful.
    ///
    /// Const and static initializers are the important case: the encoding wraps the original
    /// expression in an `if` over a function call, which is not permitted in a const context. A
    /// mutant that cannot compile is not a weak test, it is noise, so these are never generated
    /// rather than generated and then rolled back.
    inert_depth: usize,

    /// Whether the `impl` block being traversed implements `Default`.
    ///
    /// Replacing a function body with `Default::default()` is the fallback for a type this tool
    /// cannot name a value of, and inside `impl Default for T` that fallback names the very
    /// function being replaced. The mutant is unbounded recursion, so it neither compiles into
    /// something meaningful nor fails fast: it exhausts the stack, which costs a full timeout to
    /// discover and says nothing about the tests.
    in_default_impl: bool,

    /// The concrete type named by `Self` inside the enclosing `impl` block.
    impl_self_type: Option<Type>,

    /// Concrete associated types declared by the enclosing `impl` block.
    impl_self_associated: HashMap<String, Type>,

    /// The names this file uses for the standard `Default` trait and its fallback spelling.
    ///
    /// The fallback values this collector emits use `Default::default()`, but inherent methods and
    /// custom traits can have the same spelling. This resolver keeps no-op elimination tied to
    /// the standard trait while recognizing a bare shadow that the fallback text would recurse
    /// through.
    default_paths: DefaultPaths,

    /// Caller-supplied `Err(...)` payloads, from `--error`.
    errors: &'a [String],

    /// Whether the function being traversed returns a number.
    ///
    /// Perturbing a returned value only makes sense when the value is one that can be off by one.
    /// Without this the family offered `(Vec::new()) + 1` for every function returning a
    /// collection, and each of those costs a rollback round to discover it never compiled.
    numeric_return: bool,

    /// Whether the enclosing function returns a `Result` whose error type comes from another crate.
    ///
    /// `Ok(v)` becoming `Err(Default::default())` needs an error value, and the error type is fixed
    /// by the signature rather than visible at the call site. Recording it on the way in is what
    /// lets the site be screened.
    foreign_error_return: bool,

    /// Whether each named binding in the enclosing function holds a number.
    ///
    /// The perturbation family adds one to an expression, and the commonest thing it is offered is
    /// a bare identifier — for which nothing in the syntax says whether it is an integer or a
    /// `String`. Function parameters and annotated `let`s are the two places a local type is
    /// written down in the source, and recording them converts most of those guesses into an
    /// answer. A binding that is not here was never written down, and is left alone rather than
    /// assumed either way.
    ///
    /// `true` means the source says the name holds a number. Absent and `false` both mean the
    /// source does not say so, and are treated alike: the family is only offered on positive
    /// evidence.
    bindings: HashMap<String, bool>,

    /// Whether each field name declared anywhere in this file holds a number.
    ///
    /// Collected in one pass before traversal, because a field is very often read above the
    /// `struct` that declares it and an in-order record would miss exactly those uses. A name two
    /// structs declare with disagreeing types is recorded as unknown: without type resolution
    /// there is no way to tell which one a given `x.count` refers to.
    fields: HashMap<String, bool>,

    /// Names the file uses somewhere in a way only a number can be used.
    ///
    /// Read as evidence of last resort, after annotations and initialisers have had their say.
    numeric_uses: NumericUses,

    /// Whether each constant and static declared anywhere in this file holds a number.
    ///
    /// A screaming-case name is otherwise taken for a number, which is right for `MAX` and
    /// `DEFAULT_SIZE` and wrong for every `const PREFIX: &str` and `const CAP: Duration` — and
    /// adding one to those is the single largest source of mutants that cannot compile. The
    /// declaration says which is which, in the file, in writing, so it is read rather than guessed.
    /// Collected in one pass ahead of traversal for the same reason the field index is: a constant
    /// is very often used above the item that declares it.
    constants: HashMap<String, bool>,

    /// Names in the enclosing function declared by a `let` that supplies no initialiser.
    ///
    /// A `let scanned;` whose value is settled later by a plain assignment makes that assignment
    /// load-bearing: delete it and the binding is still uninitialised at its first use. That is a
    /// compile error rather than a mutant, and an unusually awkward one, because rustc reports
    /// E0381 at the *use*, which is a different statement from the one that was mutated. Withdrawal
    /// attributes a diagnostic to the mutant occupying its span, finds no mutant there, and gives
    /// up on the whole run rather than on the one bad mutant. Recording the deferred names lets
    /// their initialising assignments be passed over before any of that can happen.
    deferred: HashSet<String>,

    /// The undo log a lexical block replays on the way out, so leaving a block restores the binding
    /// evidence it changed without copying the whole in-scope set.
    ///
    /// A block can see its enclosing scope's `bindings` and `deferred`, so it inherits them rather
    /// than starting fresh; what it must not do is let its own insertions and shadows outlive it.
    /// Each entry records one touched name's prior state, and [`Collector::in_scope`] reverses its
    /// own suffix in reverse order on exit — undoing exactly what the block added, at a cost that
    /// grows with the names the block touches rather than with every name in scope. A nested
    /// function ([`Collector::in_function`]) swaps the maps out wholesale instead, so it discards
    /// its own suffix rather than replaying it.
    undo: Vec<Undo>,

    /// Type parameters in scope that are not known to implement `Default`.
    ///
    /// The `fn_value` family reaches for `Default::default()` whenever it cannot name a value of a
    /// type, and for an abstract type that is a guess rather than a fact: nothing says a caller's
    /// `E` or a trait's `Self::Value` has a `Default`, and on a serde-shaped API almost none of
    /// them do. Holding the names lets the guess be withheld exactly where it is unfounded.
    generics: Vec<String>,

    /// Type parameters in scope that are known to implement the standard `Default` trait.
    ///
    /// This is intentionally separate from `generics`: one controls whether a fallback value can
    /// be emitted, while this one says whether `T::default()` is exactly the standard fallback
    /// already present in a body.
    defaulted: Vec<String>,

    /// The configuration predicates that hold for the build this file will be part of.
    ///
    /// Code behind a predicate that does not hold is stripped by the compiler, so a guard there is
    /// never compiled, no test can activate it, and the mutant would be reported as a survivor no
    /// test could ever have caught.
    cfg: &'a CfgSet,

    /// The module path each name this file imports was brought in from.
    ///
    /// Read once before traversal, like the field and numeric indexes, because a type is used above
    /// its `use` as often as below it and an in-order record would miss exactly those uses.
    ///
    /// `None` marks a name two `use` items disagree about, which is as unknown as never having been
    /// imported.
    imports: HashMap<String, Option<Vec<String>>>,

    /// What the rest of the workspace implements `Default` for.
    defaults: &'a Defaults,

    /// The spans and replacement texts already recorded, so no two candidates can be the same edit.
    ///
    /// Mutators are written independently and several of them converge on the same small integers:
    /// `int_increment` and `int_to_one` both turn a literal `0` into `1`, and `int_decrement` and
    /// `int_to_zero` both turn a `1` into `0`. Two names for one edit is still one edit — the same
    /// build, the same tests, the same verdict — so the second is pure cost, and it also weights
    /// its site twice in the score.
    /// Keyed by span, holding the indices into `candidates` already emitted there.
    ///
    /// Indices rather than the replacement text: the text is already owned by the candidate, and
    /// copying it into a key would be one heap allocation per site for a value thrown away at the
    /// end of collection. A span carries one or two candidates in almost every case, so the scan is
    /// shorter than hashing the string would be.
    seen: HashMap<(usize, usize), Vec<u32>>,

    /// Enclosing loops, used to reject a valueless `break` when its target requires a value.
    loops: Vec<LoopContext>,
}

impl<'a> Collector<'a> {
    /// Creates a collector rooted at the file's top-level scope.
    pub(super) fn new(
        file: &'a SourceFile,
        selection: &'a Selection,
        errors: &'a [String],
        cfg: &'a CfgSet,
        defaults: &'a Defaults,
    ) -> Self {
        Self::with_indexes(file, selection, errors, cfg, defaults, indexes(&file.ast, selection))
    }

    /// Creates a collector from indexes a caller already built.
    ///
    /// Lets the fused phase-one pass (`collector::phase_one`) hand over the indexes it computed in
    /// its own single walk, so the collector's traversal does not pay for computing them a second
    /// time the way [`Collector::new`] otherwise would.
    pub(super) fn with_indexes(
        file: &'a SourceFile,
        selection: &'a Selection,
        errors: &'a [String],
        cfg: &'a CfgSet,
        defaults: &'a Defaults,
        indexes: Indexes,
    ) -> Self {
        let default_paths = DefaultPaths::of(&file.ast);

        Self {
            file,
            selection,
            scope: Vec::new(),
            outermost: Arc::from(""),
            candidates: Vec::new(),
            inert_depth: 0,
            in_default_impl: false,
            impl_self_type: None,
            impl_self_associated: HashMap::default(),
            default_paths,
            numeric_return: false,
            foreign_error_return: false,
            bindings: HashMap::default(),
            fields: indexes.fields,
            imports: indexes.imports,
            defaults,
            numeric_uses: indexes.numeric_uses,
            constants: indexes.constants,
            deferred: HashSet::default(),
            undo: Vec::new(),
            generics: Vec::new(),
            defaulted: Vec::new(),
            errors,
            cfg,
            seen: HashMap::default(),
            loops: Vec::new(),
        }
    }

    /// Consumes the collector, returning what it found.
    pub(super) fn finish(self) -> Vec<Candidate> {
        self.candidates
    }

    /// Records an expression-shaped candidate if its mutator is selected.
    fn emit(&mut self, mutator: &'static str, span: Span, replacement: impl Into<CompactString>, replacement_index: u32) {
        self.emit_shaped(mutator, span, replacement, replacement_index, Shape::Expr);
    }

    /// Records a candidate of a given shape if its mutator is selected.
    fn emit_shaped(
        &mut self,
        mutator: &'static str,
        span: Span,
        replacement: impl Into<CompactString>,
        replacement_index: u32,
        shape: Shape,
    ) {
        if !self.wants(mutator) {
            return;
        }

        self.emit_at(mutator, span.byte_range(), replacement, replacement_index, shape);
    }

    /// Records a candidate over an explicit byte range.
    ///
    /// Most sites come from a single syntax node and can use its span, but a site can also span
    /// several nodes — the elements of a `vec!` from the first to the last — with no one node
    /// covering exactly the text being replaced.
    fn emit_at(
        &mut self,
        mutator: &'static str,
        range: core::ops::Range<usize>,
        replacement: impl Into<CompactString>,
        replacement_index: u32,
        shape: Shape,
    ) {
        if !self.wants(mutator) {
            return;
        }

        // A range outside the file text means the node came from a macro expansion, where byte
        // offsets do not correspond to anything we can splice.
        if range.start >= range.end || range.end > self.file.text.len() {
            return;
        }

        let replacement = replacement.into();

        // A mutant that reproduces the code it replaces cannot be caught by any test, because
        // there is nothing to catch. Reporting it would accuse the suite of a gap that does not
        // exist, and testing it would spend a whole build and test cycle to learn nothing.
        if is_noop(
            &replacement,
            self.file.text.get(range.clone()).unwrap_or_default(),
            shape,
            &self.default_paths,
            &self.defaulted,
        ) {
            return;
        }

        // Whichever mutator reached this edit first keeps it. Selection is consulted before this
        // point, so a run that asks for only one of a colliding pair still gets the mutant.
        let span_key = (range.start, range.end);

        if self
            .seen
            .get(&span_key)
            .is_some_and(|emitted| emitted.iter().any(|&at| self.replacement_at(at) == replacement))
        {
            return;
        }

        let at = u32::try_from(self.candidates.len())
            .expect("a candidate vector cannot exceed u32::MAX entries before exhausting address space");

        self.candidates.push(Candidate {
            mutator,
            span: range,
            replacement,
            replacement_index,
            item_path: self.scope.last().map_or_else(|| Arc::clone(&self.outermost), Arc::clone),
            shape,
        });

        self.seen.entry(span_key).or_default().push(at);
    }

    /// The replacement text of an already-emitted candidate, for the dedup scan.
    fn replacement_at(&self, at: u32) -> &str {
        self.candidates
            .get(at as usize)
            .map_or("", |candidate| candidate.replacement.as_str())
    }

    /// Returns whether a mutator would produce anything here.
    ///
    /// Checked before building any replacement text, so a tree scanned with a mutator switched off
    /// pays nothing for the text it would have spliced.
    fn wants(&self, mutator: &str) -> bool {
        self.inert_depth == 0 && self.selection.contains(mutator)
    }

    /// Returns the source text a span covers, or an empty string if the span is not in the file.
    ///
    /// Borrowed from the file rather than the collector, so a caller can hold the result across a
    /// mutating call and no copy is made for a span that turns out not to be wanted.
    fn text_of(&self, span: Span) -> &'a str {
        self.file.text.get(span.byte_range()).unwrap_or("")
    }

    /// Returns the negation of the expression a span covers.
    ///
    /// The parentheses are not optional. `!` binds tighter than every binary operator, so negating
    /// `a == b` without them yields `!a == b`, which is a different expression and usually does
    /// not even type-check.
    fn negation_of(&self, span: Span) -> CompactString {
        format_compact!("!({})", self.text_of(span))
    }

    /// Runs a closure with a name pushed onto the scope stack.
    fn scoped<T>(&mut self, name: impl Display, body: impl FnOnce(&mut Self) -> T) -> T {
        let path = self
            .scope
            .last()
            .map_or_else(|| name.to_string(), |parent| format!("{parent}::{name}"));

        self.scope.push(Arc::from(path));

        let result = body(self);
        let _ = self.scope.pop();

        result
    }

    /// Returns the stable scope name of one inherent or trait implementation.
    fn impl_scope(&self, node: &ItemImpl) -> String {
        let self_type = type_name(&node.self_ty);

        let Some((trait_path, _for)) = &node.trait_ else {
            return self_type;
        };

        let trait_path = compact_path(self.text_of(trait_path.span()));

        if trait_path.is_empty() {
            return self_type;
        }

        format!("<{self_type} as {trait_path}>")
    }

    /// Emits the function-value mutants for a function with the given signature and body.
    ///
    /// The bluntest question that can be asked of a test suite: replacing a whole function body
    /// with a plausible constant asks whether the suite looks at the answer at all.
    ///
    /// The attributes are read as well as the signature, because a site may state the expression to
    /// substitute rather than leave it to be guessed from the return type. See [`stated_range`].
    fn function(&mut self, attrs: &[Attribute], sig: &Signature, body: &Block) {
        // The whole body of a `const fn` is a const context, so nothing in it can call the guard
        // predicate. `visit_const_fn` keeps the subtree inert; this only guards the body value.
        if sig.constness.is_some() {
            return;
        }

        // An empty body already produces the unit value, so replacing it with one changes nothing.
        // A mutant that cannot alter behavior can never be caught, and reporting it as a survivor
        // would be an accusation against the test suite for something no test could detect.
        if body.stmts.is_empty() {
            return;
        }

        let span = body.span();

        // Read before anything is emitted, because it decides whether the guessed values below are
        // offered at all. `None` covers both "nothing was stated" and "what was stated cannot be
        // read", and the second is already on its way to stopping the run — see `stated::check`.
        //
        // A stated value the run will not emit is no reason to withhold the guesses. The attribute
        // may change what a site substitutes; it may never take the site's only `fn_value` mutant
        // away, and a selection naming a sibling mutator but not `fn_value.stated` would otherwise
        // do exactly that — silently, since a site that emits nothing is a site nothing reports.
        let stated = stated_range(attrs).filter(|_range| self.wants("fn_value.stated"));

        // A method's own type parameters join the ones its `impl` block declares; both are in
        // scope in the signature being read here.
        let mut abstracts = self.generics.clone();

        abstracts.extend(undefaulted_parameters(&sig.generics, &self.default_paths));

        let values = return_values(
            &sig.output,
            &Types {
                abstracts: &abstracts,
                imports: &self.imports,
                defaults: self.defaults,
                self_type: self.impl_self_type.as_ref(),
                self_associated: Some(&self.impl_self_associated),
            },
        );

        // `Default::default()` inside `impl Default` is a call to this very function, so the
        // mutant is unbounded recursion rather than a different answer. It cannot be killed by a
        // test noticing a wrong value, only by the stack running out, which costs a full timeout
        // to reach and reports the slowest verdict there is for the least information.
        let recursive = self.in_default_impl && sig.ident == "default";

        // An `impl Iterator` return needs both arms of the guard wrapped so they share a type,
        // which is a different splice from every other return. See `Shape::IterBlock`.
        let shape = match &sig.output {
            ReturnType::Type(_, ty) if resolve_type(ty) == Kind::Iterator => Shape::IterBlock,
            _ => Shape::Block,
        };

        // The value a function ends on is the one its caller reasons about, so it is one of the
        // positions where being wrong by one is a real fault rather than a compile error — but
        // only when the value is a number, which the signature already says.
        if is_numeric_return(&sig.output)
            && let Some(Stmt::Expr(trailing, None)) = body.stmts.last()
            && !matches!(trailing, Expr::Lit(ExprLit { lit: Lit::Int(_), .. }))
        {
            self.perturb_proven(trailing);
        }

        let value_count = values.len();

        for (index, (mutator, value)) in values.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            if recursive && mutator == "fn_value.default" {
                continue;
            }

            // A site that states its own value has answered the question these guesses exist to
            // guess at, so the stated expression is emitted in their place rather than beside them.
            // Offering both would ask the same question twice at one site, and the second answer
            // would be the one the author had already said was wrong.
            if stated.is_some() {
                continue;
            }

            self.emit_shaped(mutator, span, value, index, shape);
        }

        // Emitted at its own mutator name rather than under whichever guess it displaced, so that
        // its identity is its own: a verdict cached against a guess that could not compile must not
        // be inherited by the expression written to replace it, which is the whole reason the
        // attribute was reached for.
        if let Some(range) = stated {
            let expression = self.file.text.get(range).unwrap_or_default();

            self.emit_shaped("fn_value.stated", span, expression, 0, shape);
        }

        // Caller-supplied error values, which reach the error types `Err(Default::default())`
        // cannot. Their indices continue the static list's so that adding one does not renumber
        // the mutants already generated at this site.
        if returns_result(&sig.output) && self.wants("fn_value.err_with") {
            for (offset, error) in self.errors.iter().enumerate() {
                let index = u32::try_from(value_count.saturating_add(offset)).unwrap_or(u32::MAX);
                let replacement = format_compact!("Err({error})");

                self.emit_shaped("fn_value.err_with", span, replacement, index, Shape::Block);
            }
        }
    }

    /// Emits the statement-deletion mutants for one statement.
    ///
    /// Only statements whose value is discarded are eligible. Deleting a `let` would leave every
    /// later use of the binding unresolved, which is a compile error rather than a mutant, and
    /// deleting a block's trailing expression would change the block's type.
    fn statement(&mut self, statement: &Stmt) {
        let Stmt::Expr(expression, Some(_)) = statement else {
            return;
        };

        let mutator = match expression {
            // A call whose result is thrown away is being run for its effect, which is exactly the
            // thing a test that only checks return values will not notice going missing.
            Expr::Call(_) | Expr::MethodCall(_) => in_place_reorder(expression).unwrap_or("stmt.delete_call"),
            Expr::Assign(assign) if self.initializes_deferred(&assign.left) => return,
            Expr::Assign(_) => "stmt.delete_assign",
            Expr::Binary(binary) if is_assign_op(&binary.op) => "stmt.delete_assign",

            // A `break` carrying a value decides the type of the loop it leaves, so deleting it
            // can change that type rather than the program's behaviour.
            Expr::Break(brk) if brk.expr.is_none() => "loop.delete_break",
            Expr::Continue(_) => "loop.delete_continue",

            _ => return,
        };

        self.emit_shaped(mutator, statement.span(), "", 0, Shape::Stmt);
    }

    /// Returns whether the target of an assignment is a binding that was declared without a value.
    ///
    /// Only a bare name can be the thing that first gives a deferred `let` its value; assigning
    /// through a field, an index or a dereference presupposes that the binding already holds one.
    fn initializes_deferred(&self, target: &Expr) -> bool {
        let Expr::Path(path) = target else {
            return false;
        };

        path.path
            .get_ident()
            .is_some_and(|ident| self.deferred.contains(&ident.to_string()))
    }

    /// Emits the guard mutants for one boolean condition.
    ///
    /// Shared by `if`, `while` and match arms, which ask the same question in three syntaxes and
    /// should not answer it three different ways.
    fn condition(&mut self, negate: &'static str, always_true: &'static str, always_false: &'static str, cond: &Expr) {
        // A condition that binds a pattern cannot be negated or replaced by a boolean. In a let
        // chain the binding may sit anywhere in the `&&` spine, not just at the top.
        if binds_a_pattern(cond) {
            return;
        }

        let span = cond.span();

        if self.wants(negate) {
            let negated = self.negation_of(span);

            self.emit(negate, span, negated, 0);
        }

        // A condition that is already the literal it would be replaced by yields a mutant that
        // compiles to the original program, so it can never be caught and would be scored as a
        // survivor forever.
        let literal = boolean_literal(cond);

        if literal != Some(true) {
            self.emit(always_true, span, "true", 1);
        }

        if literal != Some(false) {
            self.emit(always_false, span, "false", 2);
        }
    }

    /// Emits the mutants for the arms of one `match`.
    ///
    /// Two unrelated families meet here. An arm with a guard is a condition like any other and is
    /// mutated as one. An arm without a guard can instead be made to stop matching, but only when
    /// a later wildcard is there to receive what falls through — the compiler does not count a
    /// guarded arm towards exhaustiveness, so adding a guard to the last arm that can match a
    /// value turns the mutant into a compile error rather than a question about the tests.
    fn match_arms(&mut self, node: &ExprMatch) {
        // The first catch-all, and only an unguarded one that is actually compiled: a guarded `_`
        // catches nothing in particular and leaves the match relying on the arms above it, and one
        // behind a predicate that does not hold is not in the program at all, so relying on it
        // would produce a mutant whose match is no longer exhaustive. Suppression is deliberately
        // not consulted — a suppressed arm is still compiled and still keeps the match exhaustive.
        let wildcard = node
            .arms
            .iter()
            .position(|arm| is_catch_all(&arm.pat) && self.cfg.holds_for(&arm.attrs));

        for (index, arm) in node.arms.iter().enumerate() {
            if self.skipped(&arm.attrs) {
                continue;
            }

            if let Pat::Guard(guard) = &arm.pat {
                self.condition(
                    "match_guard.negate",
                    "match_guard.always_true",
                    "match_guard.always_false",
                    &guard.guard,
                );

                // An arm that already has a guard is disabled by forcing that guard false, which
                // `match_guard.always_false` above already offers. Emitting a second mutant that
                // does the same thing would pay twice for one question.
                continue;
            }

            if wildcard.is_some_and(|at| index < at) {
                self.emit_shaped("match_arm.never_matches", arm.pat.span(), "", 0, Shape::Arm);
            }
        }
    }

    /// Emits the field-omission mutants for one struct literal.
    ///
    /// Only a literal with a base expression is eligible, because the base is what keeps the
    /// result well formed once a field is taken out. Each mutant asks whether any test can tell
    /// the written value from the one the base would have supplied — which, for a field that is
    /// being set to its default anyway, nothing can.
    fn struct_fields(&mut self, node: &ExprStruct) {
        if !self.wants("struct_field.omit") {
            return;
        }

        // The `..` token rather than the expression after it, since the field being removed runs
        // up to the token and the whitespace between them belongs to neither.
        let Some(rest) = node.dot2_token.as_ref().map(syn::spanned::Spanned::span) else {
            return;
        };

        let whole = node.span().byte_range();

        if whole.start >= whole.end || whole.end > self.file.text.len() {
            return;
        }

        for (index, field) in node.fields.iter().enumerate() {
            // A field behind a predicate that does not hold is not in the compiled literal, so
            // omitting it changes nothing that could be observed.
            if self.skipped(&field.attrs) {
                continue;
            }

            let from = field.span().byte_range().start;
            let to = node
                .fields
                .iter()
                .nth(index.saturating_add(1))
                .map_or_else(|| rest.byte_range().start, |next| next.span().byte_range().start);

            // Everything between the two is the field, its comma and the space after it.
            if from < whole.start || to > whole.end || from >= to {
                continue;
            }

            let head = self
                .file
                .text
                .get(whole.start..from)
                .expect("field bounds were checked above and parser spans are UTF-8 boundaries");
            let tail = self
                .file
                .text
                .get(to..whole.end)
                .expect("field bounds were checked above and parser spans are UTF-8 boundaries");

            let replacement = format_compact!("{head}{tail}");
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit_shaped("struct_field.omit", node.span(), replacement, ordinal, Shape::Expr);
        }
    }

    /// Offers `+ 1` and `- 1` for one expression, in a position where a boundary is being decided.
    ///
    /// Deliberately not applied to every expression. Doing so would double the population of a
    /// large project, duplicate the literal and arithmetic families wherever they already apply,
    /// and produce type errors anywhere the expression is generic or not numeric at all. The
    /// positions it is applied to are the ones that carry a postcondition somebody could get
    /// wrong by one: what a function is handed, what it gives back, what is indexed, and where a
    /// range stops.
    /// Offers a mutant for each element of a `vec!` literal, with that element removed.
    ///
    /// The removal sweeps up the separating comma along with the element, so the list that is left
    /// is still well formed wherever the element sat in it.
    fn omit_elements(&mut self, node: &Macro, elements: &Punctuated<Expr, Comma>) {
        let spans: Vec<_> = elements.iter().map(|element| element.span().byte_range()).collect();

        let (Some(first), Some(last)) = (spans.first(), spans.last()) else {
            return;
        };

        // The site has to be the whole `vec![..]`, not just the elements inside it. A guarded
        // mutant is one arm of an `if`, and `1, 2` is a list rather than an expression, so
        // narrowing the site to the element range would emit code that does not parse at all.
        let whole = node.span().byte_range();
        let items = first.start..last.end;

        let (Some(text), Some(head), Some(tail)) = (
            self.file.text.get(items.clone()),
            self.file.text.get(whole.start..items.start),
            self.file.text.get(items.end..whole.end),
        ) else {
            return;
        };

        let (text, head, tail) = (text.to_owned(), head.to_owned(), tail.to_owned());

        for (index, span) in spans.iter().enumerate() {
            // Everything up to this element, and everything from the next one on. For the final
            // element there is no next, so the cut runs to the end and takes the preceding comma
            // with it.
            let (from, to) = spans.get(index.saturating_add(1)).map_or_else(
                || {
                    (
                        spans.get(index.wrapping_sub(1)).map_or(span.start, |previous| previous.end),
                        items.end,
                    )
                },
                |next| (span.start, next.start),
            );

            let (Some(before), Some(after)) = (
                text.get(..from.saturating_sub(items.start)),
                text.get(to.saturating_sub(items.start)..),
            ) else {
                continue;
            };

            let replacement = format_compact!("{head}{before}{after}{tail}");
            let ordinal = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit_at("collection.omit_element", whole.clone(), replacement, ordinal, Shape::Expr);
        }
    }

    /// Offers the curated same-shape renames of a standard-library method.
    ///
    /// The whole call expression is the site, not the method name, because a mutant is spliced in
    /// as `if guard { .. } else { .. }` and an `if` is not a legal method name. Rewriting the name
    /// inside a copy of the call's own text keeps any turbofish and every argument exactly as
    /// written, which reconstructing the call from its parts would not.
    fn rename_method(&mut self, node: &ExprMethodCall, method: &str) {
        let Some(swaps) = method_renames(method, node.args.len()) else {
            return;
        };

        let whole = node.span().byte_range();
        let name = node.method.span().byte_range();

        // A method call whose receiver spans a macro expansion can put the name outside the call,
        // in which case there is nothing meaningful to splice.
        if name.start < whole.start || name.end > whole.end {
            return;
        }

        let text = &self.file.text;
        let (Some(before), Some(after)) = (text.get(whole.start..name.start), text.get(name.end..whole.end)) else {
            return;
        };

        let (before, after) = (before.to_owned(), after.to_owned());

        for (index, (mutator, replacement)) in swaps.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            self.emit(mutator, node.span(), format_compact!("{before}{replacement}{after}"), index);
        }
    }

    fn perturb(&mut self, expression: &Expr) {
        // The veto outranks the proof. Both are read off the source, but only one of them can be
        // wrong in a way that costs a rollback round: nothing in the allowlist proves an expression
        // is a number so firmly that a `format!` beside it should be ignored.
        if self.is_known_numeric(expression) && !is_textual(expression) {
            self.perturb_proven(expression);
        }
    }

    /// Offers the perturbations for an expression the surrounding code has already typed.
    ///
    /// A `return` inside a function whose signature says it yields a number needs no inference:
    /// the signature settles the question, and settles it better than this file could.
    fn perturb_proven(&mut self, expression: &Expr) {
        if !self.wants("expr.increment") && !self.wants("expr.decrement") {
            return;
        }

        // A divergent expression -- `return`, a never-returning macro, a breakless `loop`, an
        // all-diverging `if` or `match` -- has type `!` and so passes the signature-only numeric
        // proof, but its value is never reached: `(return 5) + 1` returns before the `+ 1` runs, so
        // both perturbations behave exactly like the original and survive every test. See `diverges`.
        if diverges(expression) {
            return;
        }

        if is_capacity_result(expression) {
            return;
        }

        let span = expression.span();
        let text = self.text_of(span);

        if text.is_empty() {
            return;
        }

        // Parenthesised because the expression may bind more loosely than the addition, and
        // because the result is spliced into whatever position the original held.
        let incremented = format_compact!("({text}) + 1");
        let decremented = format_compact!("({text}) - 1");

        self.emit("expr.increment", span, incremented, 0);
        self.emit("expr.decrement", span, decremented, 1);
    }

    /// Returns whether the source positively says an expression is a number.
    ///
    /// Without type resolution this can only read what the source wrote down, so it is an
    /// allowlist rather than an attempt to rule out everything that is not a number. The default
    /// answer is no. That default is measured rather than assumed: guessing yes wherever nothing
    /// contradicts it makes three of every four perturbed mutants fail to compile, and those two
    /// operators alone then account for more than three quarters of the unviable population. Every
    /// withdrawal costs a share of a rollback round, which is a full rebuild of the instrumented
    /// tree.
    ///
    /// The trade is real in both directions, which is why the inference beneath this is worth its
    /// weight: a perturbation withheld on a value that was a number is a question never asked. The
    /// answer is to widen what the source is read for -- parameters, annotated and inferred
    /// locals, struct fields, casts, arithmetic, and methods whose names fix their return type --
    /// rather than to keep guessing where it says nothing at all.
    ///
    /// A literal answers no despite plainly being a number, because the literal family already
    /// perturbs it and offering both would pay twice for one question.
    fn is_known_numeric(&self, expression: &Expr) -> bool {
        match expression {
            // `x as usize` writes the type at the use site, which is as good as an annotation.
            Expr::Cast(cast) => is_numeric_binding(&cast.ty),

            // `-x` is a number. `!x` is a bool or a bitwise complement, and `*x` may be anything.
            Expr::Unary(unary) => matches!(unary.op, UnOp::Neg(_)),

            Expr::Binary(binary) => match binary.op {
                // Nothing in wide use subtracts, multiplies, divides or takes a remainder of
                // anything but a number.
                BinOp::Sub(_) | BinOp::Mul(_) | BinOp::Div(_) | BinOp::Rem(_) => true,

                // `String + &str` is addition too, so this one operator has to look at what it is
                // adding. Either side being a number settles it, because the two must agree.
                BinOp::Add(_) => self.is_known_numeric(&binary.left) || self.is_known_numeric(&binary.right),

                _ => false,
            },

            Expr::Path(path) if path.qself.is_none() => {
                // A constant is one of the most worthwhile things this family has to offer, and
                // the screaming case tells `MAX` and `DEFAULT_SIZE` apart from `PhantomData` and
                // `Ordering::Relaxed`, which are a unit struct and a variant.
                let last = path.path.segments.last().is_some_and(|segment| {
                    let name = segment.ident.to_string();

                    // What the file declares outranks how the name is spelled, in both directions:
                    // a `const LIMIT: usize` is a number however it is reached, and a
                    // `const PREFIX: &str` is not one however loudly it is spelled.
                    self.constants.get(&name).copied().unwrap_or_else(|| is_constant_case(&name))
                });

                last || path.path.get_ident().is_some_and(|ident| {
                    let name = ident.to_string();

                    self.bindings
                        .get(&name)
                        .copied()
                        .unwrap_or_else(|| self.numeric_uses.names.contains(&name))
                })
            }

            // A method whose name fixes its return type across the ecosystem says what it yields
            // more reliably than any inference here could.
            Expr::MethodCall(call) => returns_numeric(&call.method.to_string()),

            // `usize::from(..)`, `u64::try_from(..).unwrap()`: the type is written at the call
            // site, so there is nothing to guess.
            Expr::Call(call) => callee_type(&call.func).is_some_and(|name| is_numeric_type(&name)),

            // A field's type is written in the `struct` that declares it, which the pre-pass read.
            Expr::Field(field) => match &field.member {
                Member::Named(name) => {
                    let name = name.to_string();

                    self.fields
                        .get(&name)
                        .copied()
                        .unwrap_or_else(|| self.numeric_uses.fields.contains(&name))
                }
                Member::Unnamed(_) => false,
            },

            Expr::Paren(paren) => self.is_known_numeric(&paren.expr),

            _ => false,
        }
    }

    /// Offers the perturbations for every argument of a call, unless the callee is one whose
    /// arguments are a performance decision rather than a behavioural one.
    fn perturb_arguments(&mut self, callee: Option<&str>, args: &Punctuated<Expr, Comma>) {
        if callee.is_some_and(is_capacity_call) {
            return;
        }

        for argument in args {
            self.perturb(argument);
        }
    }

    /// Returns whether an item's attributes take it out of the population entirely.
    ///
    /// Two unrelated reasons land here: the item is test code, or it is behind a configuration
    /// predicate that does not hold for this build. Both mean the same thing to the collector —
    /// do not descend — so they are asked together at every place that can be entered.
    fn skipped(&self, attrs: &[Attribute]) -> bool {
        is_excluded(self.cfg, attrs) || !self.cfg.holds_for(attrs)
    }

    /// Runs `body`, treating everything it visits as inert when `constant` holds.
    ///
    /// A guard is a function call, which const contexts disallow; mutants generated there would
    /// never compile and end up withdrawn as noise instead of measuring anything.
    fn in_const<T>(&mut self, constant: bool, body: impl FnOnce(&mut Self) -> T) -> T {
        if constant { self.inert(body) } else { body(self) }
    }

    /// Runs `body` with type parameters that explicitly implement standard `Default` in scope.
    fn with_defaulted_parameters<T>(&mut self, generics: &Generics, body: impl FnOnce(&mut Self) -> T) -> T {
        let depth = self.defaulted.len();
        let names = standard_defaulted_parameters(generics, &self.default_paths);

        self.defaulted.extend(names);

        let result = body(self);

        self.defaulted.truncate(depth);
        result
    }

    /// Runs `body` with the enclosing function's return type recorded.
    ///
    /// Restored rather than cleared afterwards, because a nested function inside a body must not
    /// leave the outer one's `return` expressions looking like its own.
    fn in_function<T>(&mut self, sig: &Signature, body: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.numeric_return;
        let outer_error = self.foreign_error_return;

        self.numeric_return = is_numeric_return(&sig.output);
        self.foreign_error_return = returns_undefaultable_error(
            &sig.output,
            &Types {
                abstracts: &[],
                imports: &self.imports,
                defaults: self.defaults,
                self_type: self.impl_self_type.as_ref(),
                self_associated: Some(&self.impl_self_associated),
            },
        );

        // Saved and restored rather than cleared, because a function defined inside another one
        // cannot see the outer function's locals and must not be allowed to reason from them. The
        // maps are swapped out whole, so the undo entries the parameters and the body record refer
        // to a map that is thrown away — they are discarded on the way out rather than replayed.
        let mark = self.undo.len();
        let outer_bindings = core::mem::take(&mut self.bindings);
        let outer_deferred = core::mem::take(&mut self.deferred);

        for input in &sig.inputs {
            let FnArg::Typed(typed) = input else {
                continue;
            };

            if let Pat::Ident(ident) = &*typed.pat {
                self.bind(ident.ident.to_string(), is_numeric_binding(&typed.ty));
            }
        }

        let result = body(self);

        self.bindings = outer_bindings;
        self.deferred = outer_deferred;
        self.undo.truncate(mark);
        self.numeric_return = outer;
        self.foreign_error_return = outer_error;
        result
    }

    /// Notes whether a `let` leaves its binding empty for a later assignment to settle.
    ///
    /// Declarations are seen in source order, so re-declaring a name with a value lifts an earlier
    /// deferral, which is what shadowing a deferred binding means.
    fn record_declaration(&mut self, local: &Local) {
        let Some(name) = declared_name(&local.pat) else {
            return;
        };

        // `insert`/`remove` report whether the name was already there, which is exactly the prior
        // state the enclosing block has to be able to put back.
        let was_present = if local.init.is_none() {
            !self.deferred.insert(name.clone())
        } else {
            self.deferred.remove(&name)
        };

        self.undo.push(Undo::Deferred(name, was_present));
    }

    /// Records a name's type evidence, remembering the value it displaces so the enclosing block
    /// can put it back on the way out.
    ///
    /// The one place `bindings` is written during traversal, so routing every write through here is
    /// what keeps the block's undo log complete. `true` means the source says the name holds a
    /// number; absent and `false` are treated alike, so an overwrite of one by the other still has
    /// to be recorded to be reversible.
    fn bind(&mut self, name: String, numeric: bool) {
        let prior = self.bindings.insert(name.clone(), numeric);

        self.undo.push(Undo::Binding(name, prior));
    }

    /// Runs `body` with the binding evidence scoped to one lexical block.
    ///
    /// A block *can* see its enclosing scope's locals — inheriting them is why the maps are not
    /// cleared on entry — what it must not do is let its own outlive it. Without that a `let`
    /// inside `{ … }` overwrote the evidence for a shadowed outer name and the overwrite survived
    /// the block, so a later use of the outer binding was judged against the inner one's type. That
    /// decides whether the perturbation mutants are viable, so a wrong answer either withholds
    /// valid mutants or emits ones that cannot compile.
    ///
    /// `deferred` is scoped the same way and for the same reason. Note that it must be *inherited*:
    /// the common shape is a `let scanned;` in an outer block settled by an assignment inside an
    /// `if`, and the assignment is only passed over because the nested block can still see the
    /// deferral its parent recorded.
    ///
    /// Rather than copy both maps on entry and move them back on exit, the block marks the undo log
    /// and replays everything it recorded after that mark, in reverse. Reverse order is what makes a
    /// name touched more than once come back to the value it held on entry rather than to an
    /// intermediate one.
    fn in_scope<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        let mark = self.undo.len();

        let result = body(self);

        self.unwind(mark);
        result
    }

    /// Reverses every binding change recorded since `mark`, newest first, and forgets it.
    fn unwind(&mut self, mark: usize) {
        while self.undo.len() > mark {
            match self
                .undo
                .pop()
                .expect("the undo log length was checked before removing its final entry")
            {
                Undo::Binding(name, Some(prior)) => {
                    let _shadowed = self.bindings.insert(name, prior);
                }
                Undo::Binding(name, None) => {
                    let _dropped = self.bindings.remove(&name);
                }
                Undo::Deferred(name, true) => {
                    let _restored = self.deferred.insert(name);
                }
                Undo::Deferred(name, false) => {
                    let _dropped = self.deferred.remove(&name);
                }
            }
        }
    }

    fn inert<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        self.inert_depth += 1;

        let result = body(self);

        self.inert_depth -= 1;
        result
    }
}

/// Returns whether any attribute suppresses mutation of the whole item.
fn is_excluded(cfg: &CfgSet, attrs: &[Attribute]) -> bool {
    // `#[cfg(test)]` — and any compound gate that implies it, such as `all(test, unix)` — marks
    // code that exists only to test other code. Mutating it measures the tests' tests, which
    // nobody has. Read by the cfg subsystem's own classifier, so conditional `cfg_attr` gates are
    // evaluated under the same target as ordinary `cfg` gates.
    cfg.test_gated(attrs)
}

/// Removes trivia from a trait path without changing literal contents.
///
/// An item path feeds a stable mutant identity, so formatting `impl Trait < T > for S` must not
/// change it. A simple whitespace filter would also rewrite a literal in a const generic, so
/// literals and comments are stepped over with the source parser's lexer instead.
fn compact_path(text: &str) -> String {
    let comments = crate::parse::comment_spans(text);
    let mut comments = comments.iter().peekable();
    let mut at = 0;
    let mut compact = String::with_capacity(text.len());

    while at < text.len() {
        if let Some(comment) = comments.peek()
            && comment.start == at
        {
            at = comment.end;
            let _next = comments.next();
            continue;
        }

        if let Some(end) = crate::parse::literal_end(text, at) {
            compact.push_str(text.get(at..end).unwrap_or_default());
            at = end;
            continue;
        }

        let character = text
            .get(at..)
            .and_then(|rest| rest.chars().next())
            .expect("the loop keeps the UTF-8 boundary below the text length");
        at += character.len_utf8();

        if !character.is_whitespace() {
            compact.push(character);
        }
    }

    compact
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for Collector<'_> {
    /// Records the type of an annotated `let`, so that later uses of the name can be judged.
    ///
    /// Statements are visited in source order, so a name resolves to the most recent binding that
    /// precedes the use, which is what shadowing means. A `let` with no annotation is left off the
    /// record rather than guessed at.
    fn visit_local(&mut self, node: &'ast Local) {
        // A binding that is not in the build says nothing about the types later statements see, and
        // recording it would shadow the one that is.
        if self.skipped(&node.attrs) {
            return;
        }

        match &node.pat {
            Pat::Type(typed) => {
                if let Pat::Ident(ident) = &*typed.pat {
                    self.bind(ident.ident.to_string(), is_numeric_binding(&typed.ty));
                }
            }

            // Most locals carry no annotation, so reading only the annotated ones left the great
            // majority of names unknown. An initialiser this collector can already type answers
            // the same question the annotation would have, and answers it for `let count =
            // items.len();`, which is the shape the perturbation family meets most often.
            Pat::Ident(ident) => {
                if let Some(init) = node.init.as_ref().filter(|init| init.diverge.is_none())
                    && self.is_known_numeric(&init.expr)
                {
                    self.bind(ident.ident.to_string(), true);
                }
            }

            _ => {}
        }

        visit::visit_local(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            collector.with_defaulted_parameters(&node.sig.generics, |collector| {
                collector.function(&node.attrs, &node.sig, &node.block);
                collector.in_function(&node.sig, |collector| {
                    collector.in_const(node.sig.constness.is_some(), |collector| {
                        visit::visit_item_fn(collector, node);
                    });
                });
            });
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            collector.with_defaulted_parameters(&node.sig.generics, |collector| {
                collector.function(&node.attrs, &node.sig, &node.block);
                collector.in_function(&node.sig, |collector| {
                    collector.in_const(node.sig.constness.is_some(), |collector| {
                        visit::visit_impl_item_fn(collector, node);
                    });
                });
            });
        });
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.ident, |collector| visit::visit_item_mod(collector, node));
    }

    fn visit_item_trait(&mut self, node: &'ast ItemTrait) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.ident, |collector| {
            collector.with_defaulted_parameters(&node.generics, |collector| visit::visit_item_trait(collector, node));
        });
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if self.skipped(&node.attrs) {
            return;
        }

        let depth = self.generics.len();
        let defaulted_depth = self.defaulted.len();
        let outer = self.in_default_impl;
        let outer_self_type = self.impl_self_type.replace((*node.self_ty).clone());
        let outer_associated = core::mem::replace(
            &mut self.impl_self_associated,
            node.items
                .iter()
                .filter_map(|item| match item {
                    ImplItem::Type(associated) => Some((associated.ident.to_string(), associated.ty.clone())),
                    _ => None,
                })
                .collect(),
        );

        self.in_default_impl = node
            .trait_
            .as_ref()
            .is_some_and(|(path, _for)| self.default_paths.is_fallback_trait(path));

        self.generics.extend(undefaulted_parameters(&node.generics, &self.default_paths));
        self.defaulted
            .extend(standard_defaulted_parameters(&node.generics, &self.default_paths));
        let scope = self.impl_scope(node);

        self.scoped(scope, |collector| visit::visit_item_impl(collector, node));
        self.generics.truncate(depth);
        self.defaulted.truncate(defaulted_depth);
        self.in_default_impl = outer;
        self.impl_self_type = outer_self_type;
        self.impl_self_associated = outer_associated;
    }

    fn visit_item_const(&mut self, node: &'ast ItemConst) {
        self.inert(|collector| visit::visit_item_const(collector, node));
    }

    fn visit_item_static(&mut self, node: &'ast ItemStatic) {
        self.inert(|collector| visit::visit_item_static(collector, node));
    }

    fn visit_impl_item_const(&mut self, node: &'ast ImplItemConst) {
        self.inert(|collector| visit::visit_impl_item_const(collector, node));
    }

    fn visit_trait_item_const(&mut self, node: &'ast TraitItemConst) {
        self.inert(|collector| visit::visit_trait_item_const(collector, node));
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        // The expansion is not visible here and its spans do not map back onto the source, so
        // nothing inside a macro is traversed. `vec![a, b, c]` is the exception worth making: the
        // elements are written literally at the call site, so their spans are ordinary source
        // spans and removing one is an ordinary splice.
        //
        // `vec![value; count]` is excluded for free, because it does not parse as a comma-
        // separated list. Arrays are excluded deliberately: an array's length is part of its type,
        // so dropping an element changes the type rather than the behavior.
        if !node.path.is_ident("vec") {
            return;
        }

        let Ok(elements) = node.parse_body_with(Punctuated::<Expr, Comma>::parse_terminated) else {
            return;
        };

        // One element and the list would become empty, which is a different question — whether the
        // collection is needed at all — and one that `Vec::new()` already asks of the function.
        if elements.len() < 2 {
            return;
        }

        self.omit_elements(node, &elements);
    }

    fn visit_attribute(&mut self, _node: &'ast Attribute) {
        // Attributes are metadata, not behavior. This matters more than it sounds: a doc comment
        // is desugared into `#[doc = "..."]`, so without this every line of documentation in the
        // tree would present itself as a mutable string literal.
    }

    fn visit_pat(&mut self, _node: &'ast Pat) {
        // A pattern is matched against, not evaluated, so nothing in one can be guarded: a guard
        // is an `if` expression and no expression is legal in pattern position. This matters
        // because `syn` models a literal pattern as an `ExprLit`, so without this every `"skip"
        // =>` match arm would offer itself as a mutable literal and produce a mutant that cannot
        // compile.
    }

    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        let span = node.span();
        let assigns = is_assign_op(&node.op);
        let mut operands = None;

        // A `&&` that is part of a let-chain is not an ordinary boolean operator: turning it into
        // `||` is rejected by the parser, because a binding cannot escape one arm of an `or`. The
        // binding may be on either side, as the chain associates to the left and a `let` commonly
        // comes last.
        let chains_a_binding = matches!(node.op, BinOp::And(_)) && (binds_a_pattern(&node.left) || binds_a_pattern(&node.right));

        for (index, (mutator, operator)) in binary_replacements(&node.op).iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);

            // Most binary expressions in a tree have no selected mutator, and the operand text is
            // only needed to build a replacement, so it is read once and only on demand.
            if chains_a_binding || !self.wants(mutator) {
                continue;
            }

            let (left, right) = *operands.get_or_insert_with(|| (self.text_of(node.left.span()), self.text_of(node.right.span())));

            // The operands are parenthesized because the replacement is spliced in as a unit and
            // must not renegotiate precedence with whatever encloses it: rewriting the `*` in
            // `a + b * c` to `+` has to keep `b + c` grouped. The left side of a compound
            // assignment is left alone, since it is a place expression and reads better bare.
            let replacement = if assigns {
                format_compact!("{left} {operator} ({right})")
            } else {
                format_compact!("({left}) {operator} ({right})")
            };

            self.emit(mutator, span, replacement, index);
        }

        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_repeat(&mut self, node: &'ast ExprRepeat) {
        // The length of `[0u8; 32]` is a const expression, so it cannot hold a guard, but the
        // element expression can.
        self.visit_expr(&node.expr);
        self.inert(|collector| collector.visit_expr(&node.len));
    }

    fn visit_expr_reference(&mut self, node: &'ast ExprReference) {
        // `fn f() -> &'static [&'static str] { &["a", "b"] }` compiles only because the borrowed
        // array is a constant, which lets it be promoted to static storage. A guard is a function
        // call, so instrumenting anything inside one stops it being constant, the array becomes an
        // ordinary temporary, and the borrow no longer outlives the function. The result is a
        // borrow-check error over the whole enclosing expression rather than at the mutated site.
        self.in_const(is_promotable(&node.expr), |collector| {
            visit::visit_expr_reference(collector, node);
        });
    }

    fn visit_variant(&mut self, node: &'ast Variant) {
        // An enum discriminant is a const expression.
        self.inert(|collector| visit::visit_variant(collector, node));
    }

    fn visit_type(&mut self, node: &'ast Type) {
        // Every expression reachable from inside a type is a const expression: the length in
        // `[u8; 200]`, the argument in `Matrix<3, 3>`, and the same two nested arbitrarily deep in
        // a field, a return type or a `where` clause. `visit_expr_repeat` covers `[0u8; 32]`, the
        // *value*, and it is easy to assume that is the same thing — it is not, and the difference
        // is a mutant that cannot compile in a position the rollback rounds then have to discover
        // by building the whole tree.
        self.inert(|collector| visit::visit_type(collector, node));
    }

    fn visit_generic_argument(&mut self, node: &'ast GenericArgument) {
        // A const generic argument is a const expression wherever it stands, and `visit_type`
        // only reaches the ones a type encloses. The turbofish on a path in expression position —
        // `Foo::<{ N + 1 }>::BAR`, `g::<{ N + 1 }>()` — is not inside any type, so without this
        // it is mutated, and a guard is a function call that no const context will evaluate.
        if matches!(node, GenericArgument::Const(_)) {
            self.inert(|collector| visit::visit_generic_argument(collector, node));
            return;
        }

        visit::visit_generic_argument(self, node);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        if self.skipped(&node.attrs) {
            return;
        }

        self.scoped(&node.sig.ident, |collector| {
            collector.with_defaulted_parameters(&node.sig.generics, |collector| {
                if let Some(body) = node.default.as_ref() {
                    collector.function(&node.attrs, &node.sig, body);
                }

                collector.in_function(&node.sig, |collector| {
                    collector.in_const(node.sig.constness.is_some(), |collector| {
                        visit::visit_trait_item_fn(collector, node);
                    });
                });
            });
        });
    }

    fn visit_block(&mut self, node: &'ast Block) {
        self.in_scope(|collector| {
            for statement in &node.stmts {
                // A statement behind a predicate that does not hold is discarded after parsing, so
                // it is not in the build being measured. Mutating it emits changes to text the
                // compiler throws away: the crate builds, every test passes, and the mutant is
                // scored as a survivor — telling the reader their tests miss a line that is not in
                // their program, and inflating the denominator with mutants nothing could kill.
                if collector.skipped(stmt_attrs(statement)) {
                    continue;
                }

                collector.statement(statement);

                // Recorded as the block is judged rather than when `visit_local` later fires, because
                // every statement here is examined before the traversal descends into any of them. A
                // deferral noted during the descent would arrive after the assignment that settles it
                // had already been accepted as a candidate.
                if let Stmt::Local(local) = statement {
                    collector.record_declaration(local);
                }
            }

            // Descended into statement by statement rather than through `visit_block`, so that the
            // configured-out ones are not entered either.
            for statement in &node.stmts {
                if collector.skipped(stmt_attrs(statement)) {
                    continue;
                }

                visit::visit_stmt(collector, statement);
            }
        });
    }

    /// Leaves an expression the build does not contain unvisited.
    ///
    /// Statements are handled in `visit_block`, which never reaches this. This covers the expression
    /// positions rustc admits an attribute in, and the ones it does not admit yet, so that enabling
    /// `stmt_expr_attributes` in a crate under measurement does not silently reopen the gap.
    fn visit_expr(&mut self, node: &'ast Expr) {
        if self.skipped(expr_attrs(node)) {
            return;
        }

        visit::visit_expr(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast ExprUnary) {
        // `*` is by far the most common unary operator and has no mutant, so the operand text is
        // never read for one.
        match node.op {
            // Negating zero yields zero, so removing the negation changes nothing.
            UnOp::Neg(_) if is_integer_zero_literal(&node.expr) => {}
            UnOp::Neg(_) => self.emit("unary.remove_neg", node.span(), self.text_of(node.expr.span()), 0),
            UnOp::Not(_) => self.emit("unary.remove_not", node.span(), self.text_of(node.expr.span()), 0),
            _ => {}
        }

        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_if(&mut self, node: &'ast ExprIf) {
        self.condition("cond.negate", "cond.always_true", "cond.always_false", &node.cond);

        visit::visit_expr_if(self, node);
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        // Only the negation, because a `while` forced to `true` never terminates and one forced to
        // `false` is the loop deleted — the first costs a full timeout to reach a verdict already
        // available more cheaply, and the second is what statement deletion asks.
        if !binds_a_pattern(&node.cond) && self.wants("cond.negate") {
            let span = node.cond.span();
            let negated = self.negation_of(span);

            self.emit("cond.negate", span, negated, 0);
        }

        self.loops.push(LoopContext {
            label: node.label.as_ref().map(|label| label.name.ident.to_string()),
            produces_value: false,
        });
        visit::visit_expr_while(self, node);
        let _ = self.loops.pop();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.loops.push(LoopContext {
            label: node.label.as_ref().map(|label| label.name.ident.to_string()),
            produces_value: false,
        });
        visit::visit_expr_for_loop(self, node);
        let _ = self.loops.pop();
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.loops.push(LoopContext {
            label: node.label.as_ref().map(|label| label.name.ident.to_string()),
            produces_value: loop_produces_value(node),
        });
        visit::visit_expr_loop(self, node);
        let _ = self.loops.pop();
    }

    fn visit_expr_match(&mut self, node: &'ast ExprMatch) {
        self.match_arms(node);

        visit::visit_expr_match(self, node);
    }

    fn visit_expr_struct(&mut self, node: &'ast ExprStruct) {
        self.struct_fields(node);

        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast ExprRange) {
        // A range with no end has no boundary to move, and one with no start is still a boundary
        // worth moving, so only the end is required.
        if let Some(end) = node.end.as_ref() {
            let start = node.start.as_ref().map_or_else(CompactString::default, |start| {
                let text = self.text_of(start.span());

                format_compact!("({text})")
            });

            let end_text = self.text_of(end.span());

            // The change is expressed by moving the endpoint rather than by swapping `..` for
            // `..=`, even though swapping is what the mutation means. Every mutant here is run by
            // wrapping the site as `if guard { mutant } else { original }`, and the two arms of an
            // `if` must have the same type. `Range` and `RangeInclusive` are different types, so a
            // literal swap cannot compile — not occasionally, but every single time, which would
            // make the whole family a guaranteed build round spent to withdraw itself.
            //
            // `a..b + 1` covers exactly what `a..=b` covers and `a..=b - 1` covers exactly what
            // `a..b` covers, so the question put to the suite is unchanged. On an unsigned
            // endpoint that is already zero the subtraction overflows and the mutant is caught by
            // the panic, which is the right answer for the wrong reason but still the right answer.
            //
            // Parenthesised for the same reason every other replacement here is: the operands are
            // spliced back into an expression whose precedence we do not control.
            match node.limits {
                RangeLimits::HalfOpen(_) => {
                    self.emit(
                        "range.exclusive_to_inclusive",
                        node.span(),
                        format_compact!("{start}..(({end_text}) + 1)"),
                        0,
                    );
                }
                RangeLimits::Closed(_) => {
                    self.emit(
                        "range.inclusive_to_exclusive",
                        node.span(),
                        format_compact!("{start}..=(({end_text}) - 1)"),
                        0,
                    );
                }
            }

            self.perturb(end);
        }

        if let Some(start) = node.start.as_ref() {
            self.perturb(start);
        }

        visit::visit_expr_range(self, node);
    }

    fn visit_expr_break(&mut self, node: &'ast ExprBreak) {
        // A labelled `break` may be leaving a labelled block rather than a loop, and `continue`
        // cannot leave a block. A `break` carrying a value decides the type of its loop, which
        // `continue` cannot supply.
        if node.expr.is_none() && node.label.is_none() {
            self.emit_shaped("loop.break_to_continue", node.span(), "continue", 0, Shape::Break);
        }

        visit::visit_expr_break(self, node);
    }

    fn visit_expr_continue(&mut self, node: &'ast ExprContinue) {
        let target = node.label.as_ref().map(|label| label.ident.to_string());
        let requires_value = target.as_ref().map_or_else(
            || self.loops.last().is_some_and(|context| context.produces_value),
            |target| {
                self.loops
                    .iter()
                    .rev()
                    .find(|context| context.label.as_ref() == Some(target))
                    .is_some_and(|context| context.produces_value)
            },
        );

        if requires_value {
            visit::visit_expr_continue(self, node);
            return;
        }

        // A label on a `continue` can only name a loop, so the same label is always valid on a
        // `break`.
        let replacement = node
            .label
            .as_ref()
            .map_or_else(|| CompactString::new("break"), |label| format_compact!("break {label}"));

        self.emit_shaped("loop.continue_to_break", node.span(), replacement, 0, Shape::Continue);

        visit::visit_expr_continue(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        self.perturb_arguments(callee_name(&node.func).as_deref(), &node.args);

        // `Some(v)`, `Ok(v)` and `Err(v)` decide, at the point it is decided, whether a value is
        // present and whether an operation succeeded. Replacing a whole function can only ask that
        // question a function at a time; this asks it at the site.
        if node.args.len() == 1 {
            match callee_name(&node.func).as_deref() {
                Some("Some") => self.emit("option.some_to_none", node.span(), "None", 0),
                Some("Ok") if !self.foreign_error_return => self.emit("result.ok_to_err", node.span(), "Err(Default::default())", 0),
                Some("Err") => self.emit("result.err_to_ok", node.span(), "Ok(Default::default())", 0),
                _ => {}
            }
        }

        visit::visit_expr_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        // A bare `None` in expression position. Patterns never reach here, because `visit_pat`
        // stops the traversal before a pattern's interior is examined at all.
        if node.path.is_ident("None") {
            self.emit("option.none_to_some", node.span(), "Some(Default::default())", 0);
        }

        visit::visit_expr_path(self, node);
    }

    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        // What is assigned is what the rest of the function reads, so replacing it with the type's
        // default asks whether anything downstream depends on the value rather than the write.
        if !is_default_call(&node.right, &self.default_paths, &self.defaulted) {
            self.emit("assign_value.default", node.right.span(), "Default::default()", 0);
        }

        visit::visit_expr_assign(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();

        self.perturb_arguments(Some(method.as_str()), &node.args);
        self.rename_method(node, &method);

        // The receiver is ordinary code and is traversed; the message is not descended into at all.
        // Renaming the call itself is still offered above, because `expect` and `unwrap_or_default`
        // differ in what the program does rather than in what it says on the way down.
        if is_diagnostic_message(&method, node.args.len()) {
            self.visit_expr(&node.receiver);
            return;
        }

        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_index(&mut self, node: &'ast ExprIndex) {
        self.perturb(&node.index);

        visit::visit_expr_index(self, node);
    }

    fn visit_expr_return(&mut self, node: &'ast ExprReturn) {
        // An integer-literal return value is left to the literal family, which already offers the
        // same `+ 1` / `- 1` neighbours (and more) for it; perturbing it here as well would only
        // duplicate those mutants. A non-literal value -- a variable, a call -- is not covered
        // elsewhere, so it is still perturbed here.
        if self.numeric_return
            && let Some(value) = node.expr.as_ref()
            && !matches!(&**value, Expr::Lit(ExprLit { lit: Lit::Int(_), .. }))
        {
            self.perturb_proven(value);
        }

        visit::visit_expr_return(self, node);
    }

    fn visit_expr_lit(&mut self, node: &'ast ExprLit) {
        let span = node.span();

        match &node.lit {
            Lit::Int(value) => {
                let digits = value.base10_digits();

                // The perturbations go first so that where they collide with the value family on a
                // small literal — `0` becoming `1` is both an increment and a "to one" — the name
                // that survives is the one a reader checking a boundary is looking for.
                if let Ok(parsed) = digits.parse::<i64>() {
                    // Checked, because the literal may be `i64::MAX`: wrapping would offer
                    // `i64::MIN` as an "increment", and the unchecked form panics in a debug build.
                    if let Some(incremented) = parsed.checked_add(1) {
                        self.emit("literal.int_increment", span, incremented.to_string(), 2);
                    }

                    if !(digits == "0" && value.suffix().starts_with('u'))
                        && let Some(decremented) = parsed.checked_sub(1)
                    {
                        self.emit("literal.int_decrement", span, decremented.to_string(), 3);
                    }
                }

                if digits != "0" {
                    self.emit("literal.int_to_zero", span, "0", 0);
                }

                if digits != "1" {
                    self.emit("literal.int_to_one", span, "1", 1);
                }
            }

            Lit::Bool(value) => {
                self.emit("literal.bool_flip", span, (!value.value).to_string(), 0);
            }

            Lit::Str(value) => {
                let text = value.value();

                if !text.is_empty() {
                    self.emit("literal.str_to_empty", span, "\"\"", 0);
                }

                // Replacing `"xyzzy"` with `"xyzzy"` is the original program.
                if text != XYZZY {
                    self.emit("literal.str_to_xyzzy", span, "\"xyzzy\"", 1);
                }
            }

            _ => {}
        }

        visit::visit_expr_lit(self, node);
    }
}
