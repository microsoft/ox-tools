// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Syntactic questions the collector asks about one node.

use syn::visit::Visit;
use syn::{
    Attribute, BinOp, Block, Expr, ExprBlock, ExprBreak, ExprForLoop, ExprGroup, ExprLit, ExprLoop, ExprParen, ExprUnsafe, ExprWhile,
    GenericArgument, Label, Lit, Macro, Pat, Path, PathArguments, ReturnType, Stmt, Type, UnOp,
};

use super::super::defaults::DefaultPaths;
use super::values::{Kind, resolve_type, strip};

/// Returns whether borrowing an expression relies on it being promoted to static storage.
///
/// Promotion is a property of const-evaluability, which cannot be decided from syntax alone. What
/// is decidable is the shape that makes it plausible — an aggregate built only from literals — and
/// that is the shape whose promotion a guard would silently take away.
pub(super) fn is_promotable(expression: &Expr) -> bool {
    match expression {
        // A path is either a constant, which is promotable, or a local, which is not a temporary
        // in the first place. Neither holds a mutation site, so the answer costs nothing either way.
        Expr::Lit(_) | Expr::Path(_) => true,

        Expr::Array(array) => array.elems.iter().all(is_promotable),
        Expr::Tuple(tuple) => tuple.elems.iter().all(is_promotable),
        Expr::Repeat(repeat) => is_promotable(&repeat.expr),
        Expr::Reference(reference) => is_promotable(&reference.expr),
        Expr::Unary(unary) => matches!(unary.op, UnOp::Neg(_)) && is_promotable(&unary.expr),
        Expr::Paren(paren) => is_promotable(&paren.expr),

        _ => false,
    }
}

/// Returns whether a condition binds a pattern, either directly or as part of a `&&` chain.
///
/// A `let` in condition position is not an expression that can be negated, replaced by a boolean,
/// or have its `&&` turned into `||`: all three are rejected by the parser or leave the bindings
/// the rest of the condition and the body depend on unbound.
pub(super) fn binds_a_pattern(condition: &Expr) -> bool {
    match condition {
        Expr::Let(_) => true,
        Expr::Binary(binary) if matches!(binary.op, BinOp::And(_)) => binds_a_pattern(&binary.left) || binds_a_pattern(&binary.right),
        Expr::Paren(paren) => binds_a_pattern(&paren.expr),
        _ => false,
    }
}

/// Returns the value of a condition that is written as a boolean literal, seeing through grouping.
///
/// Replacing `if true` with `if true` reproduces the original program, so the mutant can never be
/// killed and would sit in every report as a permanent survivor.
pub(super) fn boolean_literal(condition: &Expr) -> Option<bool> {
    match condition {
        Expr::Lit(ExprLit { lit: Lit::Bool(value), .. }) => Some(value.value),
        Expr::Paren(paren) => boolean_literal(&paren.expr),
        _ => None,
    }
}

/// Returns whether an expression is an integer literal equal to zero, seeing through grouping.
///
/// Integer zero is its own negation, so dropping the `-` from `-0` leaves the program unchanged.
/// Floating-point zero is excluded because its sign is observable.
pub(super) fn is_integer_zero_literal(expression: &Expr) -> bool {
    match expression {
        Expr::Lit(ExprLit { lit: Lit::Int(value), .. }) => value.base10_digits() == "0",
        Expr::Paren(paren) => is_integer_zero_literal(&paren.expr),
        _ => false,
    }
}

/// Returns whether a `loop` has positive evidence that it produces a value.
///
/// An unlabelled value-carrying `break` belongs to the innermost loop. A labelled one may cross
/// nested loops, so it is counted when it names this loop and no nested loop shadows the label.
/// Closures and nested items are not descended into because control flow cannot cross them.
pub(super) fn loop_produces_value(node: &ExprLoop) -> bool {
    let mut visitor = ValueBreak {
        label: node.label.as_ref(),
        nested_loops: 0,
        shadowed: 0,
        found: false,
    };

    visitor.visit_block(&node.body);
    visitor.found
}

struct ValueBreak<'a> {
    label: Option<&'a Label>,
    nested_loops: usize,
    shadowed: usize,
    found: bool,
}

impl ValueBreak<'_> {
    fn enters(&mut self, label: Option<&Label>, body: impl FnOnce(&mut Self)) {
        let shadows = self.label.is_some_and(|root| label.is_some_and(|nested| nested.name == root.name));

        self.nested_loops += 1;
        self.shadowed += usize::from(shadows);
        body(self);
        self.shadowed -= usize::from(shadows);
        self.nested_loops -= 1;
    }
}

#[expect(
    clippy::renamed_function_params,
    reason = "syn names every visitor parameter `i`, which says nothing about what it is"
)]
impl<'ast> Visit<'ast> for ValueBreak<'_> {
    fn visit_expr_break(&mut self, node: &'ast ExprBreak) {
        if node.expr.is_some()
            && (node.label.is_none() && self.nested_loops == 0
                || self.shadowed == 0
                    && self
                        .label
                        .is_some_and(|root| node.label.as_ref().is_some_and(|label| label.ident == root.name.ident)))
        {
            self.found = true;
        }
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.enters(node.label.as_ref(), |visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        self.enters(node.label.as_ref(), |visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        self.enters(node.label.as_ref(), |visitor| visitor.visit_block(&node.body));
    }

    fn visit_expr_closure(&mut self, _node: &'ast syn::ExprClosure) {}

    fn visit_item(&mut self, _node: &'ast syn::Item) {}
}

/// Returns whether the source positively says an expression is text rather than a number.
///
/// The mirror of `is_known_numeric`, and it exists because that one has to answer from evidence
/// that is sometimes circumstantial — a screaming-case name, a field name some other struct
/// declared, a name used as an index once elsewhere in the file. Text is the case where the source
/// says so outright, and `+ 1` against it is `E0369` every time: a build that is thrown away, a
/// share of a rollback round paid, and nothing measured.
///
/// An allowlist again, and deliberately a short one. A plain path whose type is a caller's struct
/// is not decidable here and is not attempted; what is decidable is a value the source constructed
/// as text on the spot.
pub(super) fn is_textual(expression: &Expr) -> bool {
    match expression {
        Expr::Lit(literal) => matches!(literal.lit, Lit::Str(_) | Lit::ByteStr(_) | Lit::Char(_) | Lit::Byte(_)),

        // `format!`, `concat!` and `stringify!` have exactly one result type between them.
        Expr::Macro(macro_call) => macro_call
            .mac
            .path
            .segments
            .last()
            .is_some_and(|segment| matches!(segment.ident.to_string().as_str(), "format" | "concat" | "stringify")),

        // `min`, `max` and `clamp` are `Ord` methods, so they return whatever the receiver was —
        // which is the one way a method call reaches here still holding text after
        // `is_known_numeric` accepted it for the name alone.
        Expr::MethodCall(call) => {
            let method = call.method.to_string();

            returns_text(&method) || (matches!(method.as_str(), "min" | "max" | "clamp") && is_textual(&call.receiver))
        }

        // `String::from(..)`, `str::to_owned(..)`: the type is written at the call site.
        Expr::Call(call) => callee_type(&call.func).is_some_and(|name| matches!(name.as_str(), "String" | "str" | "OsString" | "PathBuf")),

        // `String + &str` is addition, so one textual side makes the whole sum textual — which is
        // the case `is_known_numeric` cannot see, since it accepts a sum either side of which
        // looked like a number.
        Expr::Binary(binary) => matches!(binary.op, BinOp::Add(_)) && (is_textual(&binary.left) || is_textual(&binary.right)),

        Expr::Paren(paren) => is_textual(&paren.expr),
        Expr::Reference(reference) => is_textual(&reference.expr),

        _ => false,
    }
}

/// Returns whether a method's name fixes its return type as text across the ecosystem.
///
/// `to_string` is the one that matters and the rest are its neighbours: each is a method whose name
/// is so strongly associated with producing a string that a receiver of some other type would be a
/// surprise. Nothing here is a method a number has.
pub(super) fn returns_text(method: &str) -> bool {
    matches!(
        method,
        "to_string"
            | "to_owned"
            | "to_uppercase"
            | "to_lowercase"
            | "to_ascii_uppercase"
            | "to_ascii_lowercase"
            | "to_string_lossy"
            | "into_string"
            | "concat"
            | "join"
            | "repeat"
            | "escape_debug"
            | "escape_default"
    )
}

/// Returns whether an expression is an integer literal, which fixes the type of whatever it meets.
pub(super) fn is_int_literal(expression: &Expr) -> bool {
    match expression {
        Expr::Lit(literal) => matches!(literal.lit, Lit::Int(_)),
        Expr::Paren(paren) => is_int_literal(&paren.expr),
        _ => false,
    }
}

/// Returns whether a method exists only on numbers, so that having a receiver proves it is one.
///
/// `min`, `max` and `clamp` are deliberately absent even though they return a number: they are
/// `Ord` methods, so any two comparable values have them and a receiver proves nothing.
pub(super) fn is_numeric_receiver(method: &str) -> bool {
    matches!(
        method,
        "saturating_add"
            | "saturating_sub"
            | "saturating_mul"
            | "wrapping_add"
            | "wrapping_sub"
            | "wrapping_mul"
            | "checked_add"
            | "checked_sub"
            | "checked_mul"
            | "checked_div"
            | "abs"
            | "signum"
            | "pow"
            | "rem_euclid"
            | "div_euclid"
            | "count_ones"
            | "count_zeros"
            | "leading_zeros"
            | "trailing_zeros"
            | "is_power_of_two"
            | "next_power_of_two"
            | "to_le_bytes"
            | "to_be_bytes"
    )
}

pub(super) fn is_constant_case(name: &str) -> bool {
    name.chars().any(|character| character.is_ascii_uppercase()) && !name.chars().any(char::is_lowercase)
}

/// Returns whether a type's name says its associated functions produce a number.
///
/// Written out rather than derived, because these are the only names for which `usize::from(..)`
/// and its kind can be read off the call site without resolving anything.
pub(super) fn is_numeric_type(name: &str) -> bool {
    matches!(
        name,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
            | "NonZeroU8"
            | "NonZeroU16"
            | "NonZeroU32"
            | "NonZeroU64"
            | "NonZeroUsize"
    )
}

/// Returns whether a method's name is enough on its own to know it yields a number.
///
/// Only names whose meaning is fixed across the ecosystem are listed, and only ones that yield a
/// bare number rather than an `Option` or a `Result` wrapping one -- `checked_add` is absent for
/// that reason, and `clone` because what it yields depends entirely on its receiver.
pub(super) fn returns_numeric(method: &str) -> bool {
    matches!(
        method,
        "len"
            | "count"
            | "capacity"
            | "abs"
            | "signum"
            | "pow"
            | "min"
            | "max"
            | "clamp"
            | "saturating_add"
            | "saturating_sub"
            | "saturating_mul"
            | "wrapping_add"
            | "wrapping_sub"
            | "wrapping_mul"
            | "as_millis"
            | "as_micros"
            | "as_nanos"
            | "as_secs"
            | "subsec_millis"
            | "subsec_nanos"
            | "elapsed_secs"
            | "leading_zeros"
            | "trailing_zeros"
            | "count_ones"
            | "count_zeros"
    )
}

/// Returns whether a callee's arguments describe how much room to set aside rather than what the
/// program should do.
///
/// Perturbing one of these produces a mutant that changes only an allocation strategy. A test
/// suite that caught it would be a test suite pinning an implementation detail, so reporting it as
/// a survivor accuses the tests of a gap they should not be asked to fill.
pub(super) fn is_capacity_call(name: &str) -> bool {
    matches!(
        name,
        "with_capacity" | "with_capacity_in" | "reserve" | "reserve_exact" | "try_reserve" | "try_reserve_exact" | "shrink_to"
    )
}

/// Returns whether a call's argument is a message a person reads after the program has failed.
///
/// `expect` and `expect_err` take one argument and it is never behavior: it is what the panic says,
/// read only once the program has already given up. Rewriting it changes what a crash prints, not
/// what the program does, so the mutant is unkillable by any test worth writing — killing one means
/// asserting the exact wording of a panic message, which pins phrasing that should stay free to
/// improve and turns a typo fix into a failing suite.
///
/// The panicking macros need no such rule: nothing inside a macro is traversed at all, because the
/// expansion's spans do not map back onto the source. So `assert!(x, "...")` and `panic!("...")`
/// are already exempt, and this is what is left.
///
/// Everything the argument is built from is exempt with it. A message assembled by a `format!` or
/// a helper call is still a message, and the reasoning does not change with its shape.
pub(super) fn is_diagnostic_message(name: &str, arity: usize) -> bool {
    arity == 1 && matches!(name, "expect" | "expect_err")
}

/// Returns whether a pattern accepts every value the arms above it left over.
///
/// `_` is the obvious spelling. A bare binding — `other => ...` — is the other one: it names the
/// value instead of discarding it, but it accepts every value just the same, so a match ending in
/// one stays exhaustive when an earlier arm is stopped from matching.
///
/// A binding is only recognised when it is spelled the way a binding is: one identifier in value
/// case, with no subpattern. `None`, `MAX` and every other unit variant or constant parse as the
/// same node and match exactly one value, so taking those for catch-alls would leave the match
/// non-exhaustive and the mutant unable to compile.
pub(super) fn is_catch_all(pat: &Pat) -> bool {
    match pat {
        Pat::Wild(_) => true,
        Pat::Ident(binding) => binding.subpat.is_none() && is_binding_case(&binding.ident.to_string()),
        Pat::Paren(paren) => is_catch_all(&paren.pat),
        _ => false,
    }
}

/// Returns whether an identifier is spelled the way a binding is.
///
/// Bindings are `snake_case` and constants and unit variants are not, so the leading character
/// tells them apart. Anything that does not start lower — `None`, `MAX`, `Ordering` — is taken for
/// a pattern that matches one value rather than all of them, which is the safe way to be wrong:
/// the mutant is withheld instead of failing to compile.
pub(super) fn is_binding_case(name: &str) -> bool {
    name.starts_with(|first: char| first == '_' || first.is_lowercase())
}

/// Returns whether an expression is already a standard `Default::default()` call.
///
/// Replacing it with itself would be a mutant no test could ever detect, which would be reported
/// as a survivor and read as an accusation against the suite for something it cannot do. A final
/// segment named `default` is not enough: inherent methods and custom traits can use that name,
/// and their calls are real behavior a default-value mutant must still test.
pub(super) fn is_default_call(expression: &Expr, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    match expression {
        Expr::Call(call) if call.args.is_empty() => {
            callee_path(&call.func).is_some_and(|path| is_standard_default_callee(path, defaults, defaulted_types))
        }
        Expr::Paren(paren) => is_default_call(&paren.expr, defaults, defaulted_types),
        _ => false,
    }
}

/// Returns the path directly called by an expression, seeing through redundant parentheses.
fn callee_path(callee: &Expr) -> Option<&Path> {
    match callee {
        Expr::Path(path) => Some(&path.path),
        Expr::Paren(paren) => callee_path(&paren.expr),
        _ => None,
    }
}

/// Returns whether a `default` method path selects the standard trait.
fn is_standard_default_callee(path: &Path, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    let Some(method) = path.segments.last() else {
        return false;
    };

    if method.ident != "default" || path.segments.len() < 2 {
        return false;
    }

    if defaults.is_standard_default_callee(path) {
        return true;
    }

    if path.segments.len() == 2 {
        let qualifier = path.segments.first().expect("the path length was checked to contain two segments");
        let qualifier = qualifier.ident.to_string();

        return defaulted_types.iter().any(|name| name == &qualifier);
    }

    false
}

/// Returns whether an expression is a call to one of those functions.
///
/// Their arguments are excluded because perturbing them says nothing; their *results* are excluded
/// because a call that reserves room returns the collection, never a number.
pub(super) fn is_capacity_result(expression: &Expr) -> bool {
    match expression {
        Expr::Call(call) => callee_name(&call.func).is_some_and(|name| is_capacity_call(&name)),
        Expr::MethodCall(call) => is_capacity_call(&call.method.to_string()),
        Expr::Paren(paren) => is_capacity_result(&paren.expr),
        _ => false,
    }
}

/// Returns whether a signature promises a number, which is the only thing worth perturbing by one.
pub(super) fn is_numeric_return(output: &ReturnType) -> bool {
    let ReturnType::Type(_arrow, ty) = output else {
        return false;
    };

    matches!(resolve_type(ty), Kind::Signed | Kind::Unsigned | Kind::Float)
}

/// Returns whether an expression's value is unreachable because control leaves before it is used.
///
/// A divergent expression has type `!`, which coerces to the numeric return type and so slips past
/// the signature-only proof that a tail is a number. Perturbing it is pointless: `(return 5) + 1`
/// returns before the `+ 1` ever runs, so the mutant behaves exactly like the original and survives
/// every test. Only the cases the source states outright are reported. This never tries to prove a
/// value *is* reachable, so anything it is unsure about answers `false` and is perturbed as before.
pub(super) fn diverges(expression: &Expr) -> bool {
    match expression {
        // Control leaves the function, or the enclosing loop, outright.
        Expr::Return(_) | Expr::Break(_) | Expr::Continue(_) => true,

        // The standard never-returning macros: a `panic!`, `unreachable!`, `todo!` or
        // `unimplemented!` in tail position is the body's value only in the type system's bookkeeping.
        Expr::Macro(node) => is_diverging_macro(&node.mac),

        // A `loop` with no `break` runs forever; one with a `break` may yield a value.
        Expr::Loop(node) => !has_break(&node.body),

        // Wrappers that add no control flow carry the divergence of what they hold.
        Expr::Paren(ExprParen { expr, .. }) | Expr::Group(ExprGroup { expr, .. }) => diverges(expr),
        Expr::Block(ExprBlock { block, .. }) | Expr::Unsafe(ExprUnsafe { block, .. }) => block_diverges(block),

        // An `if` diverges only when it cannot fall through: it needs an `else`, and both the `then`
        // block and the `else` branch must diverge in turn.
        Expr::If(node) => node
            .else_branch
            .as_ref()
            .is_some_and(|(_else, otherwise)| block_diverges(&node.then_branch) && diverges(otherwise)),

        // A `match` diverges when it has arms and every one of them does. A match on an empty type
        // has no arm to reach here as a numeric tail, so the emptiness check only guards the logic.
        Expr::Match(node) => !node.arms.is_empty() && node.arms.iter().all(|arm| diverges(&arm.body)),

        _ => false,
    }
}

/// Returns whether a block's value is unreachable because its final statement diverges.
fn block_diverges(block: &Block) -> bool {
    match block.stmts.last() {
        Some(Stmt::Expr(expr, _semi)) => diverges(expr),
        Some(Stmt::Macro(node)) => is_diverging_macro(&node.mac),
        _ => false,
    }
}

/// Returns whether a macro is one of the standard never-returning ones.
fn is_diverging_macro(mac: &Macro) -> bool {
    mac.path.segments.last().is_some_and(|segment| {
        matches!(
            segment.ident.to_string().as_str(),
            "panic" | "unreachable" | "todo" | "unimplemented"
        )
    })
}

/// Returns whether a loop body contains any `break`, anywhere within it.
///
/// Deliberately conservative: a `break` that belongs to a nested loop still counts, so a loop is
/// only called breakless when it truly has none. Erring this way keeps the divergence test from
/// ever rejecting a loop that might produce a value.
fn has_break(block: &Block) -> bool {
    let mut finder = BreakFinder { found: false };

    finder.visit_block(block);
    finder.found
}

/// Records whether any `break` was seen while walking a loop body.
struct BreakFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for BreakFinder {
    fn visit_expr_break(&mut self, _node: &'ast ExprBreak) {
        self.found = true;
    }
}

/// Returns a type's `index`th generic argument, so `Result<T, E>` can be asked for either side.
pub(super) fn payload(ty: &Type, index: usize) -> Option<&Type> {
    let Type::Path(path) = strip(ty) else {
        return None;
    };

    let PathArguments::AngleBracketed(args) = &path.path.segments.last()?.arguments else {
        return None;
    };

    args.args
        .iter()
        .filter_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .nth(index)
}

/// Returns the single name a `let` pattern introduces, if it introduces exactly one.
///
/// A `let` with no initialiser can only bind a bare name, optionally typed — a tuple or struct
/// pattern has nothing to destructure until a value arrives, so the compiler rejects it. Looking
/// through `Pat::Type` is therefore enough, and anything more elaborate is not a deferred binding.
pub(super) fn declared_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Type(typed) => declared_name(&typed.pat),
        Pat::Ident(ident) if ident.subpat.is_none() => Some(ident.ident.to_string()),
        _ => None,
    }
}

/// Returns whether a written-down type is one that `+ 1` applies to.
///
/// References are peeled first: `&usize + 1` compiles, so treating a reference as a non-number
/// would throw away a mutant that builds and runs.
pub(super) fn is_numeric_binding(ty: &Type) -> bool {
    match ty {
        Type::Reference(reference) => is_numeric_binding(&reference.elem),

        _ => matches!(resolve_type(ty), Kind::Signed | Kind::Unsigned | Kind::Float),
    }
}

/// Returns the type a called path is qualified by, which is the segment before the function name.
///
/// `Vec::new` is qualified by `Vec` and `usize::from` by `usize`; a bare `helper()` is qualified by
/// nothing, and there is nothing to read off it.
pub(super) fn callee_type(callee: &Expr) -> Option<String> {
    match callee {
        Expr::Path(path) => {
            let mut segments = path.path.segments.iter().rev();
            let _last = segments.next()?;

            segments.next().map(|segment| segment.ident.to_string())
        }

        Expr::Paren(paren) => callee_type(&paren.expr),
        _ => None,
    }
}

/// Returns the final path segment of a called expression, which is the function's own name.
///
/// `Vec::with_capacity` and a bare `with_capacity` should be recognised as the same thing, and the
/// path in between says nothing the skip list needs.
pub(super) fn callee_name(callee: &Expr) -> Option<String> {
    match callee {
        Expr::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
        Expr::Paren(paren) => callee_name(&paren.expr),
        _ => None,
    }
}

/// Returns whether an operator is a compound assignment, whose left side is a place expression.
pub(super) const fn is_assign_op(op: &BinOp) -> bool {
    matches!(
        op,
        BinOp::AddAssign(_)
            | BinOp::SubAssign(_)
            | BinOp::MulAssign(_)
            | BinOp::DivAssign(_)
            | BinOp::RemAssign(_)
            | BinOp::BitAndAssign(_)
            | BinOp::BitOrAssign(_)
            | BinOp::BitXorAssign(_)
            | BinOp::ShlAssign(_)
            | BinOp::ShrAssign(_)
    )
}

/// Returns whether a function's return type is syntactically a `Result`.
pub(super) fn returns_result(output: &ReturnType) -> bool {
    matches!(output, ReturnType::Type(_, ty) if resolve_type(ty) == Kind::Result)
}

/// The attributes written on one expression.
///
/// `syn` gives every expression variant an `attrs` field but no way to reach it without naming the
/// variant, and the collector has to ask the question of an `Expr` it has not matched on. The
/// fallthrough is the empty slice, which reads as "nothing configured it out" — the fail-open answer
/// every other predicate here takes for a shape it cannot identify.
pub(super) fn expr_attrs(expression: &Expr) -> &[Attribute] {
    macro_rules! attrs_of {
        ($($variant:ident),+ $(,)?) => {
            match expression {
                $(Expr::$variant(node) => &node.attrs,)+
                _ => &[],
            }
        };
    }

    attrs_of!(
        Array, Assign, Async, Await, Binary, Block, Break, Call, Cast, Closure, Const, Continue, Field, ForLoop, Group, If, Index, Infer,
        Let, Lit, Loop, Macro, Match, MethodCall, Paren, Path, Range, Reference, Repeat, Return, Struct, Try, TryBlock, Tuple, Unary,
        Unsafe, While, Yield,
    )
}

/// The attributes written on one statement.
///
/// A statement's attributes live on whatever it wraps, so `#[cfg(unix)] f();` and `#[cfg(unix)] let
/// x = 1;` put them in different places. Both mean the same thing about the statement.
pub(super) fn stmt_attrs(statement: &Stmt) -> &[Attribute] {
    match statement {
        Stmt::Local(local) => &local.attrs,
        Stmt::Macro(mac) => &mac.attrs,
        Stmt::Expr(expression, _semi) => expr_attrs(expression),

        // An item's own visitor asks the question already, and asking it here as well would need
        // the same match over every item kind to no additional effect.
        Stmt::Item(_) => &[],
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    fn default_paths(source: &str) -> DefaultPaths {
        DefaultPaths::of(&syn::parse_file(source).expect("test source should parse"))
    }

    /// The value-break visitor must walk nested loop forms but ignore closures and nested items,
    /// because only the loop forms can carry a `break` back to the loop being classified.
    #[test]
    fn nested_loop_forms_are_visited_when_deciding_whether_a_loop_yields_a_value() {
        let node: ExprLoop = parse_quote!('outer: loop {
            loop {
                break;
            }

            while false {}

            for _ in 0..1 {
                break 'outer 7;
            }

            let _ignored = || 1;

            fn helper() {}
        });

        assert!(loop_produces_value(&node));
    }

    /// Textuality is decided only from syntax, so every syntax form that states "this is text"
    /// needs a direct unit test rather than relying on wider collector behavior to reach it.
    #[test]
    fn literal_macro_method_and_reference_forms_can_all_be_textual() {
        let literal: Expr = parse_quote!("hello");
        let macro_call: Expr = parse_quote!(format!("hello {}", 1));
        let text_method: Expr = parse_quote!("hello".to_string());
        let ord_method: Expr = parse_quote!(String::from("a").min(String::from("b")));
        let reference: Expr = parse_quote!(&"hello");

        assert!(is_textual(&literal));
        assert!(is_textual(&macro_call));
        assert!(is_textual(&text_method));
        assert!(is_textual(&ord_method));
        assert!(is_textual(&reference));
    }

    #[test]
    fn only_whitelisted_method_names_are_treated_as_returning_text() {
        assert!(returns_text("escape_default"));
        assert!(!returns_text("checked_add"));
    }

    /// Standard `Default::default` detection must fail closed for malformed and non-default paths,
    /// while still accepting explicit type-qualified defaults for the caller's own type parameter.
    #[test]
    fn default_callee_detection_rejects_non_paths_and_non_default_methods() {
        let defaults = default_paths("use std::default::Default as StdDefault;");
        let defaulted_types = vec!["Thing".to_owned()];
        let wrong_callee: Expr = parse_quote!(value + 1);
        let wrong_method: Path = parse_quote!(Thing::new);
        let bare_defaulted: Path = parse_quote!(Thing::default);
        let aliased_standard: Path = parse_quote!(StdDefault::default);
        let empty = Path {
            leading_colon: None,
            segments: syn::punctuated::Punctuated::default(),
        };

        assert!(callee_path(&parse_quote!((StdDefault::default))).is_some());
        assert!(callee_path(&wrong_callee).is_none());
        assert!(!is_standard_default_callee(&empty, &defaults, &defaulted_types));
        assert!(!is_standard_default_callee(&wrong_method, &defaults, &defaulted_types));
        assert!(is_standard_default_callee(&aliased_standard, &defaults, &defaulted_types));
        assert!(is_standard_default_callee(&bare_defaulted, &defaults, &defaulted_types));
    }

    #[test]
    fn capacity_and_callee_helpers_fall_back_for_non_call_expressions() {
        let associated_call: Expr = parse_quote!(Vec::with_capacity(8));
        let method_call: Expr = parse_quote!(buffer.reserve(8));
        let path: Expr = parse_quote!(Vec::with_capacity);
        let non_path: Expr = parse_quote!(buffer + 1);

        assert!(is_capacity_result(&associated_call));
        assert!(is_capacity_result(&method_call));
        assert_eq!(callee_type(&path), Some("Vec".to_owned()));
        assert_eq!(callee_type(&non_path), None);
        assert_eq!(callee_name(&path), Some("with_capacity".to_owned()));
        assert_eq!(callee_name(&non_path), None);
    }

    /// Divergence depends on the *last* statement only, so a macro statement tail and a non-tail
    /// expression-less block must be distinguished directly.
    #[test]
    fn macro_statement_tails_diverge_but_non_expression_tails_do_not() {
        let macro_block: Block = parse_quote!({
            panic!();
        });
        let local_tail: Block = parse_quote!({
            let _value = 1;
        });

        assert!(block_diverges(&macro_block));
        assert!(!block_diverges(&local_tail));
    }

    #[test]
    fn payload_requires_a_path_type() {
        let reference: Type = parse_quote!(&Result<u8, Error>);

        assert!(payload(&reference, 0).is_none());
    }
}
