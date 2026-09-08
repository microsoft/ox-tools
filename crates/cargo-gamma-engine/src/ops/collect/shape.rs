// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use serde::{Deserialize, Serialize};

/// What kind of construct a mutation site is.
///
/// This decides how a guard can be wrapped around it: the three cases are not stylistic. Rust will
/// not accept the same guard text in all three positions, so the shape has to travel with the
/// mutant all the way to instrumentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// An expression. Guarded by a parenthesized `if`/`else` yielding one of two values.
    ///
    /// The parentheses matter: a bare block or `if` in condition position (`if { .. } { .. }`)
    /// is rejected, and without them the guard would also rebind precedence against whatever
    /// operator encloses the site.
    Expr,

    /// A block that must stay a block, such as a function body. Guarded by an `if`/`else` whose
    /// `else` arm is the original block, wrapped in braces so the result is still a block.
    Block,

    /// A function body returning `impl Iterator`, where both arms are wrapped in a variant of
    /// `gamma_rt::Either` so that they share one type.
    ///
    /// This is [`Shape::Block`] with one extra step, and the extra step is forced. The two arms of
    /// an `if` must agree on a type, but `impl Iterator` is a single concrete type chosen by the
    /// body, so a synthesized iterator and the original are never the same type. Naming a shared
    /// type is not possible either: the item type may be unwritten, or itself opaque. Wrapping
    /// each arm in a different variant of one two-parameter enum sidesteps both problems, because
    /// the compiler infers both parameters and carries `Send`, `Sync` and `Clone` across for free.
    IterBlock,

    /// A `continue` replaced with `break`.
    ///
    /// A generic expression guard makes the containing block appear to return `()`, even when the
    /// original `continue` occupied a diverging tail position. Instrumentation keeps the original
    /// `continue` as the block's tail and conditionally executes the replacement before it.
    Continue,

    /// A `break` replaced with `continue`.
    ///
    /// Symmetric with [`Shape::Continue`]: the original `break` remains the tail expression so the
    /// schema has the same type as the source.
    Break,

    /// A whole statement, which the mutant deletes. Guarded by a negated `if` that runs the
    /// original only when the mutant is inactive.
    Stmt,

    /// A match arm's pattern, which the mutant stops from matching.
    ///
    /// Deleting an arm outright is not something a runtime guard can do, because which arms exist
    /// is fixed when the code is compiled. Adding a guard achieves the same behaviour: an arm
    /// whose guard is false does not match, and control falls through to whatever follows. This is
    /// why the collector only offers the mutant when a later wildcard arm is there to catch it.
    ///
    /// It also costs a constant amount of text. The obvious alternative — replacing the whole
    /// `match` with a copy that lacks the arm — grows with the square of the arm count, so a
    /// hundred-arm dispatch would emit a hundred copies of itself and price the family out of any
    /// codebase large enough to want it.
    Arm,
}
