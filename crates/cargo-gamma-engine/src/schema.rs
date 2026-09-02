// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Encoding a whole population of mutants into a single compilable source tree.
//!
//! This is the idea the tool is built on. The conventional way to test a mutant is to edit the
//! source, compile, run the suite, and revert — which means one full build per mutant, and a build
//! is far and away the most expensive thing in the loop. Instead, every mutant is compiled into
//! the tree *at once*, each one behind a runtime guard, and a single environment variable picks
//! which one is live. The build happens once; testing a mutant costs a process launch.
//!
//! The construction that makes this possible is called a *mutant schema*, after Untch, Offutt and
//! Harrold, who introduced it in 1993 for Fortran. A guarded site looks like this:
//!
//! ```text
//! original:     a < b
//! instrumented: (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b })
//! ```
//!
//! # Why only the default branch carries nested guards
//!
//! A mutation site can contain others: in `a + b < c` the `<` site contains the `+` site. The
//! obvious encoding instruments both arms of every guard, which makes the output grow as
//! `2^depth` and duplicates whole subtrees.
//!
//! It is also unnecessary. Exactly one mutant is active in a process, so if the `<` mutant is
//! live then no `+` mutant can be, and the `<` arm can hold the plain original text of its
//! operands. Only the `else` arm — the one taken when this site is not the active mutant — needs
//! instrumented children. That makes the encoding linear in the size of the source.
//!
//! The alternative, binding operands to temporaries and sharing them between arms, was rejected:
//! it would defeat the short-circuit of `&&` and `||`, move values that were only borrowed, and
//! change when temporaries are dropped. Duplicating the operand text costs compile time; changing
//! any of those three would change what the tests prove.

use core::fmt::Write as _;
use core::num::NonZeroU32;
use core::ops::Range;

use crate::error::Error;
#[cfg(test)]
use crate::model::MutantDefinition;
use crate::ops::collect::Shape;
use crate::{HashMap, Result};

/// The crate path of the guard predicate, as it appears in instrumented source.
pub const GUARD_PATH: &str = "::gamma_rt::a";

/// The crate path of the two-variant iterator wrapper, as it appears in instrumented source.
///
/// Used only by [`Shape::IterBlock`], which is the one shape whose two arms cannot be made to
/// agree on a type without it.
pub const EITHER_PATH: &str = "::gamma_rt::Either";

/// Which of a guard's up to four positions one resolved offset belongs to.
#[derive(Clone, Copy)]
enum Slot {
    SiteStart,
    SiteEnd,
    MutatedStart,
    MutatedEnd,
}

/// A guard's positions as they are filled in during the sweep in [`positions`], before every field
/// that the mutant's shape requires is known to be present.
#[derive(Clone, Copy, Default)]
struct Slots {
    site_start: Option<Position>,
    site_end: Option<Position>,
    mutated_start: Option<Position>,
    mutated_end: Option<Position>,
}

/// Maps each mutant ordinal to where its guard landed in the instrumented text.
///
/// A guard emits both the mutated text and the original, so a multi-line site grows and every
/// later line shifts. Instrumented text therefore does not line up with its source, and anything
/// attributing a compiler diagnostic to a mutant has to use these positions rather than the
/// mutant's source line.
///
/// Every guard needs up to four offsets resolved (`site.start`, `site.end`, and, unless the mutant
/// is a deletion, `mutated.start`/`mutated.end`), and a naive resolver recounts characters from the
/// start of the enclosing line for each one independently — worst case, work proportional to
/// mutant count times line length on a single generated long line. Resolving every offset from
/// every guard in one ascending sweep instead makes the total character-counting work proportional
/// to the text once, with the sweep never re-reading a byte it has already counted.
fn positions(text: &str, spans: &HashMap<u32, (Range<usize>, Range<usize>)>) -> HashMap<u32, Guard> {
    let mut starts: Vec<usize> = Vec::with_capacity(text.len() / 32);

    starts.push(0);
    starts.extend(text.match_indices('\n').map(|(at, _matched)| at + 1));

    let mut requests: Vec<(usize, u32, Slot)> = Vec::with_capacity(spans.len() * 4);

    for (ordinal, (site, mutated)) in spans {
        requests.push((site.start, *ordinal, Slot::SiteStart));
        requests.push((site.end, *ordinal, Slot::SiteEnd));

        if !mutated.is_empty() {
            requests.push((mutated.start, *ordinal, Slot::MutatedStart));
            requests.push((mutated.end, *ordinal, Slot::MutatedEnd));
        }
    }

    // Ascending order lets the sweep below only ever move forward: to a later line, or to a later
    // byte within the line it is already on.
    requests.sort_by_key(|(offset, ..)| *offset);

    let mut resolved: HashMap<u32, Slots> = HashMap::default();
    let mut line = 0_usize;
    let mut cursor = 0_usize;
    let mut column = 0_usize;

    for (offset, ordinal, slot) in requests {
        while starts.get(line + 1).is_some_and(|next_start| *next_start <= offset) {
            line += 1;
            cursor = starts[line];
            column = 0;
        }

        // `offset` never precedes `cursor`: requests are sorted ascending, so each one is no
        // earlier than the last, and the line advance above only ever resets the cursor to a line
        // start no later than the offset that triggered it. Both are byte offsets into valid UTF-8
        // spans built by `render`, so both land on character boundaries.
        let prefix = text
            .get(cursor..offset)
            .expect("cursor never exceeds offset and both are char-boundary offsets into this text");

        column += prefix.chars().count();
        cursor = offset;

        let position = Position::from_zero_based(line, column);

        let slots = resolved.entry(ordinal).or_default();

        match slot {
            Slot::SiteStart => slots.site_start = Some(position),
            Slot::SiteEnd => slots.site_end = Some(position),
            Slot::MutatedStart => slots.mutated_start = Some(position),
            Slot::MutatedEnd => slots.mutated_end = Some(position),
        }
    }

    spans
        .keys()
        .map(|ordinal| {
            let slots = resolved.remove(ordinal).unwrap_or_default();

            let guard = Guard {
                site: slots.site_start.expect("every span above requests its own site.start")
                    ..slots.site_end.expect("every span above requests its own site.end"),
                mutated: slots.mutated_start.zip(slots.mutated_end).map(|(start, end)| start..end),
            };

            (*ordinal, guard)
        })
        .collect()
}

/// A one-based line and column in instrumented text, ordered as the text reads.
///
/// The coordinates are private because zero is not a position. Everything that consumes one — a
/// compiler diagnostic, an editor, the blame that decides which mutant a build error belongs to —
/// counts from one, so a zero would compare as strictly before the first character of the file and
/// silently widen every containment test that reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    line: NonZeroU32,
    column: NonZeroU32,
}

impl Position {
    /// Builds a position from one-based coordinates, rejecting a zero in either.
    ///
    /// Returns `None` rather than clamping, because the callers that reach this hold numbers
    /// copied out of someone else's JSON: a zero there means the producer did not mean a position
    /// at all, and turning it into line one would attribute a diagnostic to real code.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Option<Self> {
        match (NonZeroU32::new(line), NonZeroU32::new(column)) {
            (Some(line), Some(column)) => Some(Self { line, column }),
            _other => None,
        }
    }

    /// Builds a position from zero-based coordinates, as an offset sweep counts them.
    ///
    /// Total by construction: adding one to a count that starts at zero cannot produce zero, and a
    /// count too large for `u32` saturates to the largest representable position rather than
    /// wrapping into a small one.
    #[must_use]
    pub fn from_zero_based(line: usize, column: usize) -> Self {
        let one_based = |count: usize| {
            let widened = u32::try_from(count).unwrap_or(u32::MAX - 1);

            NonZeroU32::new(widened.saturating_add(1)).expect("a saturating increment of an unsigned count is never zero")
        };

        Self {
            line: one_based(line),
            column: one_based(column),
        }
    }

    /// The one-based line.
    #[must_use]
    pub const fn line(self) -> u32 {
        self.line.get()
    }

    /// The one-based column.
    #[must_use]
    pub const fn column(self) -> u32 {
        self.column.get()
    }
}

/// Where one mutant's guard landed in instrumented text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guard {
    /// The whole guarded site: the `if`, the mutated branch, and the original text.
    pub site: Range<Position>,

    /// Just the mutated branch — the only text in the tree that is not a copy of the original.
    ///
    /// Nested guards go exclusively in the `else` branch, so these ranges never overlap between
    /// mutants, not even for two mutants of the same site. A compiler diagnostic landing inside
    /// one therefore names its cause exactly. A deletion mutant has no replacement text at all,
    /// and so has nothing here.
    pub mutated: Option<Range<Position>>,
}

/// A node in the containment tree of mutation sites within one file.
#[derive(Debug)]
struct Node<'a> {
    span: Range<usize>,
    shape: Shape,

    /// Every mutant sharing exactly this span, in ordinal order.
    mutants: Vec<(u32, &'a str)>,

    /// Sites strictly contained within this one.
    children: Vec<Self>,
}

/// Which guard, of all the guards in one instrumented tree, a mutant is selected by.
///
/// A newtype because the number is one of several `u32` counts that travel together and mean
/// entirely different things: an occurrence counts repeats of a site within an item, a replacement
/// index counts a mutator's alternatives for one site, and this counts mutants across the whole
/// run. They are interchangeable to the compiler and to the eye, and swapping two of them produces
/// a tree that builds, runs, and attributes every verdict to the wrong mutant.
///
/// Zero is not a guard. A mutant an earlier run already settled, or that a shard left out, keeps
/// zero because it was never scheduled, and nothing in the tree ever tests for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ordinal(u32);

impl Ordinal {
    /// Names the guard this ordinal selects.
    #[must_use]
    pub const fn new(ordinal: u32) -> Self {
        Self(ordinal)
    }

    /// The number as the instrumented tree spells it.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A source-level mutant definition paired with the run-local ordinal that selects its guard.
#[derive(Clone, Copy, Debug)]
pub struct AssignedMutant<'a> {
    ordinal: Ordinal,
    span: &'a Range<usize>,
    replacement: &'a str,
    shape: Shape,
}

impl<'a> AssignedMutant<'a> {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(ordinal: Ordinal, definition: &'a MutantDefinition) -> Self {
        Self::from_parts(ordinal, definition.span(), &definition.replacement, definition.shape)
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn from_parts(ordinal: Ordinal, span: &'a Range<usize>, replacement: &'a str, shape: Shape) -> Self {
        Self {
            ordinal,
            span,
            replacement,
            shape,
        }
    }
}

/// Rewrites one file so that it encodes every one of the given mutants.
///
/// Any mutant whose span is not a region of `text` that can be replaced is skipped rather than
/// spliced at a wrong offset, because a bad offset would silently corrupt unrelated code and the
/// resulting failures would be blamed on the test suite. A valid span is nonempty, in bounds, and
/// starts and ends on UTF-8 character boundaries.
///
/// # Errors
///
/// Returns an error if two mutants have spans that overlap without nesting, which would make the
/// rewrite ambiguous.
pub fn instrument(text: &str, mutants: &[AssignedMutant<'_>]) -> Result<String> {
    instrument_with_guards(text, mutants).map(|(out, _guards)| out)
}

/// Rewrites one file as [`instrument`] does, also reporting where each guard landed.
///
/// The positions are one-based and refer to the returned text, not to `text`.
///
/// # Errors
///
/// Returns an error if two mutants have spans that overlap without nesting.
pub fn instrument_with_guards(text: &str, mutants: &[AssignedMutant<'_>]) -> Result<(String, HashMap<u32, Guard>)> {
    let mut sites: Vec<&AssignedMutant<'_>> = mutants.iter().filter(|mutant| spliceable(text, mutant.span)).collect();

    // Outermost first, and within one span the lowest ordinal first, so that the tree builds by a
    // single stack walk and guard order is deterministic.
    sites.sort_by(|left, right| {
        left.span
            .start
            .cmp(&right.span.start)
            .then_with(|| right.span.end.cmp(&left.span.end))
            .then_with(|| left.ordinal.cmp(&right.ordinal))
    });

    let roots = build_tree(&sites)?;
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut spans = HashMap::default();
    let mut cursor = 0;

    for root in &roots {
        out.push_str(copy(text, cursor..root.span.start));
        render(text, root, &mut out, &mut spans);
        cursor = root.span.end;
    }

    out.push_str(copy(text, cursor..text.len()));

    let guards = positions(&out, &spans);

    Ok((out, guards))
}

/// Whether a span names a region of `text` that can be replaced.
///
/// Length is not the whole of it. A span whose endpoint falls inside a character is within the
/// text and still cannot be sliced, and the failure that follows is the worst of the three
/// available: not an error, not a splice at a wrong offset, but the silent disappearance of every
/// region the bad endpoint bounds. Nothing downstream can catch that — the guard the mutant asked
/// for is emitted correctly, so the invariant check over the guards passes, and only the code
/// around them is gone.
///
/// Within one parse this cannot happen, because every span is a token span and token spans fall on
/// character boundaries. The spans and the text come from two independent reads of the file, so
/// "within one parse" is an assumption rather than a fact.
const fn spliceable(text: &str, span: &Range<usize>) -> bool {
    span.start < span.end && span.end <= text.len() && text.is_char_boundary(span.start) && text.is_char_boundary(span.end)
}

/// The text a range covers, for a range built from endpoints that have already been vetted.
///
/// Every caller composes its range from the endpoints of spans that passed [`spliceable`], in an
/// order the containment tree fixes: a parent's start precedes its first child's, siblings do not
/// overlap, and each range ends no later than the enclosing node. So the slice exists, and saying
/// so out loud is the point — the alternative spelling substitutes an empty string for a range
/// that does not, which is how a region of the file goes missing without anyone being told.
fn copy(text: &str, range: Range<usize>) -> &str {
    text.get(range)
        .expect("every range here is built from vetted endpoints in containment order")
}

/// Groups sites into a forest ordered by containment.
fn build_tree<'a>(sites: &[&'a AssignedMutant<'a>]) -> Result<Vec<Node<'a>>> {
    // Neither can ever hold more than one node per site: `roots` gains at most one entry per site
    // that closes out to the top level, and `stack` holds at most one open node per site.
    let mut roots: Vec<Node<'a>> = Vec::with_capacity(sites.len());
    let mut stack: Vec<Node<'a>> = Vec::with_capacity(sites.len());

    for mutant in sites {
        // Close every open node this site is not inside.
        while let Some(top) = stack.last() {
            if mutant.span.start < top.span.end {
                break;
            }

            let Some(finished) = stack.pop() else { break };

            attach(finished, &mut stack, &mut roots);
        }

        if let Some(top) = stack.last_mut() {
            // Two mutants share a node only when they mutate the same text in the same way. Equal
            // spans with different shapes would be spliced with the wrong wrapper — an expression
            // guard around a statement, say — so they are kept apart and nested instead.
            if top.span == *mutant.span && top.shape == mutant.shape {
                top.mutants.push((mutant.ordinal.get(), mutant.replacement));
                continue;
            }

            // Partial overlap. A single parse cannot produce this, so it means spans from two
            // different parses were mixed, and splicing either one would corrupt the other.
            if mutant.span.end > top.span.end {
                return Err(Error::new(format!(
                    "mutation sites {:?} and {:?} overlap without nesting",
                    top.span, mutant.span
                )));
            }
        }

        stack.push(Node {
            span: (*mutant.span).clone(),
            shape: mutant.shape,
            mutants: vec![(mutant.ordinal.get(), mutant.replacement)],
            children: Vec::new(),
        });
    }

    while let Some(finished) = stack.pop() {
        attach(finished, &mut stack, &mut roots);
    }

    roots.sort_by_key(|node| node.span.start);

    Ok(roots)
}

/// Files a finished node under its parent, or under the roots when it has none.
fn attach<'a>(node: Node<'a>, stack: &mut [Node<'a>], roots: &mut Vec<Node<'a>>) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else {
        roots.push(node);
    }
}

/// Writes the instrumented form of one node, recording the output each guard covers.
fn render(text: &str, node: &Node<'_>, out: &mut String, spans: &mut HashMap<u32, (Range<usize>, Range<usize>)>) {
    let start = out.len();
    let mut mutated = Vec::with_capacity(node.mutants.len());

    for (ordinal, replacement) in &node.mutants {
        match node.shape {
            Shape::Expr | Shape::Block | Shape::IterBlock => {
                let opening = if node.shape == Shape::Expr { "(" } else { "{ " };

                let _ = write!(out, "{opening}if {GUARD_PATH}({ordinal}u32) {{ ");

                // The mutant is the left variant and the original the right, which is what makes
                // the two arms one type. See `Shape::IterBlock`.
                if node.shape == Shape::IterBlock {
                    let _ = write!(out, "{EITHER_PATH}::L(");
                }

                let from = out.len();

                out.push_str(replacement);
                mutated.push((*ordinal, from..out.len()));

                if node.shape == Shape::IterBlock {
                    out.push(')');
                }

                out.push_str(" } else { ");

                // Every `else` arm is wrapped, not just the one holding the original text. An
                // outer arm holds the next guard down, whose type is an `Either` of its own, and
                // the two arms only agree once that is the right-hand side of this one.
                if node.shape == Shape::IterBlock {
                    let _ = write!(out, "{EITHER_PATH}::R(");
                }
            }
            Shape::Continue | Shape::Break => {
                let _ = write!(out, "{{ if {GUARD_PATH}({ordinal}u32) {{ ");
                let from = out.len();

                out.push_str(replacement);
                mutated.push((*ordinal, from..out.len()));
                out.push_str("; } ");
            }
            Shape::Stmt => {
                let _ = write!(out, "if !{GUARD_PATH}({ordinal}u32) {{ ");
                mutated.push((*ordinal, out.len()..out.len()));
            }
            // Written after the pattern rather than before it, so nothing is emitted here.
            Shape::Arm => {}
        }
    }

    // The innermost thing is the original text with this node's children instrumented in place.
    let mut cursor = node.span.start;

    for child in &node.children {
        out.push_str(copy(text, cursor..child.span.start));
        render(text, child, out, spans);
        cursor = child.span.end;
    }

    out.push_str(copy(text, cursor..node.span.end));

    // An arm is disabled by a guard trailing its pattern. Several mutants on one arm chain with
    // `&&`, which is correct however many there are, though only one can ever be active at once.
    if node.shape == Shape::Arm {
        for (index, (ordinal, _replacement)) in node.mutants.iter().enumerate() {
            let joiner = if index == 0 { " if" } else { " &&" };

            let _ = write!(out, "{joiner} !{GUARD_PATH}({ordinal}u32)");
            mutated.push((*ordinal, out.len()..out.len()));
        }
    }

    let close = match node.shape {
        Shape::Expr => " })",
        Shape::Block => " } }",
        // The extra `)` closes the `Either::R` this arm was opened with.
        Shape::IterBlock => ") } }",
        Shape::Continue | Shape::Break | Shape::Stmt => " }",
        // The guard is the whole of the change, and it is already written.
        Shape::Arm => "",
    };

    for _ in 0..node.mutants.len() {
        out.push_str(close);
    }

    let end = out.len();

    for (ordinal, region) in mutated {
        let _ = spans.insert(ordinal, (start..end, region));
    }
}

#[cfg(test)]
mod tests {
    use core::ops::Range;

    use super::{AssignedMutant, Guard, Ordinal, Position, Shape};
    use crate::{HashMap, Result};

    #[derive(Debug)]
    struct Mutant {
        ordinal: u32,
        span: Range<usize>,
        replacement: String,
        shape: Shape,
    }

    /// The one-based position a test states, which is never zero.
    fn at(line: u32, column: u32) -> Position {
        Position::new(line, column).expect("a position written into a test is one-based by construction")
    }

    fn instrument(text: &str, mutants: &[&Mutant]) -> Result<String> {
        let assigned = assigned_mutants(mutants);

        super::instrument(text, &assigned)
    }

    fn instrument_with_guards(text: &str, mutants: &[&Mutant]) -> Result<(String, HashMap<u32, Guard>)> {
        let assigned = assigned_mutants(mutants);

        super::instrument_with_guards(text, &assigned)
    }

    fn assigned_mutants<'a>(mutants: &[&'a Mutant]) -> Vec<AssignedMutant<'a>> {
        mutants
            .iter()
            .map(|mutant| AssignedMutant::from_parts(Ordinal::new(mutant.ordinal), &mutant.span, &mutant.replacement, mutant.shape))
            .collect()
    }

    /// Every `else` arm of an `IterBlock` must be wrapped, not only the one holding the original.
    ///
    /// This is the invariant that makes the shape work, and it is not obvious. Wrapping just the
    /// innermost arm looks right and compiles for a single mutant, but a second mutant on the same
    /// site nests a whole `Either` into the outer `else`, and the outer `if` then has
    /// `Either<Empty<_>, _>` against `Either<Once<_>, _>` — two types, so the build fails and both
    /// mutants are withdrawn as unviable. Nothing else in the suite would notice, because the
    /// single-mutant case stays green.
    #[test]
    fn every_else_arm_of_an_iterator_body_is_wrapped_so_that_two_mutants_still_agree_on_a_type() {
        let text = "fn f() -> impl Iterator<Item = u32> { 0..10 }\n";
        let empty = mutant(36..45, 1, "core::iter::empty()", Shape::IterBlock);
        let once = mutant(36..45, 2, "core::iter::once(0)", Shape::IterBlock);

        let out = instrument(text, &[&empty, &once]).expect("instrumented");

        assert_eq!(
            out.matches("::gamma_rt::Either::R(").count(),
            2,
            "one wrapper per guard, not one for the whole site: {out}"
        );
        assert_eq!(out.matches("::gamma_rt::Either::L(").count(), 2, "{out}");

        // The original body is the innermost thing, still intact inside the wrappers.
        assert!(out.contains("::gamma_rt::Either::R({ 0..10 })"), "{out}");
    }

    /// A body wrapped this way has to parse, and braces are easy to get wrong by one.
    #[test]
    fn an_instrumented_iterator_body_is_still_balanced_rust() {
        let text = "fn f() -> impl Iterator<Item = u32> { 0..10 }\n";
        let only = mutant(36..45, 1, "core::iter::empty()", Shape::IterBlock);

        let out = instrument(text, &[&only]).expect("instrumented");

        let _parsed = parse_file(&out).expect("the instrumented form must parse");
    }

    #[test]
    fn guards_report_the_lines_they_actually_landed_on() {
        let text = "fn a() {}\nlet x = 1;\nv.push(1);\n";
        let first = mutant(18..19, 3, "0", Shape::Expr);
        let second = mutant(21..31, 9, "", Shape::Stmt);

        let (_out, guards) = instrument_with_guards(text, &[&first, &second]).expect("instrumented");

        assert_eq!(guards.get(&3).map(|guard| guard.site.start.line()), Some(2));
        assert_eq!(guards.get(&9).map(|guard| guard.site.start.line()), Some(3));
        assert_eq!(guards.len(), 2);
    }

    /// A byte-counting regression would report the wrong column for anything past a multibyte
    /// character, and a missing one-based `+ 1` would report one column short — both silent while
    /// every ASCII-only test in this file stays green. `é` (2 bytes) sits before the mutation site
    /// and inside it, on the same line, so both the leading-prefix count and the site's own
    /// character count are exercised; the expected positions are hand-derived from the exact
    /// guard template `render` emits (`(if `, `GUARD_PATH`, `(1u32) { `, the replacement, `
    /// } else { `, the original text, ` })`), not from the code under test.
    #[test]
    fn guard_positions_count_unicode_characters_not_bytes() {
        let text = "fn f() -> i32 {\n    let x = 1;\n    é_café + 1\n}\n";
        let site = span_of(text, "café + 1");
        let only = mutant(site, 1, "0", Shape::Expr);

        let (_out, guards) = instrument_with_guards(text, &[&only]).expect("instrumented");
        let guard = guards.get(&1).expect("recorded").clone();

        // "    é_" precedes the site: 6 characters, so the site starts at column 7.
        // The whole rendered site is `(if ::gamma_rt::a(1u32) { 0 } else { café + 1 })`, which is
        // 48 characters long, so the site ends at column 7 + 48 = 55.
        assert_eq!(guard.site, at(3, 7)..at(3, 55));

        // The replacement `0` begins right after the 26-character `(if ::gamma_rt::a(1u32) { `
        // prefix, so at column 7 + 26 = 33, and is 1 character long, ending at column 34.
        let mutated = guard.mutated.expect("a replacement was recorded");

        assert_eq!(mutated, at(3, 33)..at(3, 34));
    }

    /// Zero is not a position, and the constructor that takes one-based numbers has to say so
    /// rather than accept a value that would compare as before the first character of the file.
    #[test]
    fn a_one_based_position_rejects_a_zero_coordinate() {
        assert_eq!(Position::new(0, 1), None);
        assert_eq!(Position::new(1, 0), None);
        assert_eq!(Position::new(0, 0), None);

        let first = Position::new(1, 1).expect("line one column one is a position");

        assert_eq!(first.line(), 1);
        assert_eq!(first.column(), 1);
    }

    /// The sweep counts from zero, so the conversion adds one — and a count too large for `u32`
    /// has to saturate at the top rather than wrap around to the start of the file.
    #[test]
    fn a_zero_based_count_becomes_the_position_after_it() {
        let origin = Position::from_zero_based(0, 0);

        assert_eq!((origin.line(), origin.column()), (1, 1));

        let inside = Position::from_zero_based(4, 11);

        assert_eq!((inside.line(), inside.column()), (5, 12));

        let beyond = Position::from_zero_based(usize::MAX, usize::MAX);

        assert_eq!((beyond.line(), beyond.column()), (u32::MAX, u32::MAX));
    }

    /// Positions order as the text reads: by line first, and only then by column, which is what
    /// every containment test in the blame pass relies on.
    #[test]
    fn positions_order_by_line_before_column() {
        assert!(at(1, 99) < at(2, 1));
        assert!(at(2, 1) < at(2, 2));
        assert_eq!(at(7, 3), at(7, 3));
    }

    #[test]
    fn a_guard_reports_the_whole_range_it_spans() {
        let text = "fn f(a: i32, b: i32) -> bool {\n    a\n        < b\n}\n";
        let only = mutant(35..48, 1, "(a)\n        <= (b)", Shape::Expr);

        let (out, guards) = instrument_with_guards(text, &[&only]).expect("instrumented");
        let span = guards.get(&1).expect("recorded").clone();

        assert!(out.lines().count() > text.lines().count(), "the site should have grown");
        assert!(
            span.site.end.line() > span.site.start.line(),
            "a multi-line site should span multiple lines"
        );
    }

    #[test]
    fn a_nested_guard_lies_outside_its_enclosing_guards_mutated_branch() {
        // This is what lets a compile error be attributed exactly. An enclosing mutant that
        // replaces a whole function body is far likelier to break the build than the literal
        // nested inside it, so the two must be distinguishable: the enclosing mutant's own text
        // has to be disjoint from where the innocent nested guard sits.
        let text = "fn f() -> i32 {\n    0\n}\n";
        let outer = mutant(14..23, 1, "Default::default()", Shape::Block);
        let inner = mutant(20..21, 2, "1", Shape::Expr);

        let (_out, guards) = instrument_with_guards(text, &[&outer, &inner]).expect("instrumented");
        let outer = guards.get(&1).expect("outer recorded").clone();
        let inner = guards.get(&2).expect("inner recorded").clone();

        let replacement = outer.mutated.expect("the outer mutant has replacement text");

        assert!(
            outer.site.start <= inner.site.start && outer.site.end >= inner.site.end,
            "the outer site should enclose"
        );
        assert!(
            replacement.end <= inner.site.start || replacement.start >= inner.site.end,
            "the outer replacement must not overlap the nested guard"
        );
    }

    #[test]
    fn a_guarded_site_that_grows_shifts_the_guards_below_it() {
        // The first site emits both the mutated text and the original, so the file gets longer and
        // the second guard sits well below the line its mutant was written on.
        let text = "fn f(a: i32, b: i32) -> bool {\n    a\n        < b\n}\nfn g() -> i32 { 1 }\n";
        let first = mutant(35..48, 1, "(a)\n        <= (b)", Shape::Expr);
        let second = mutant(67..68, 2, "0", Shape::Expr);

        let (out, guards) = instrument_with_guards(text, &[&first, &second]).expect("instrumented");
        let found = out
            .lines()
            .position(|line| line.contains("a(2u32)"))
            .and_then(|at| u32::try_from(at + 1).ok());

        assert_eq!(guards.get(&2).map(|guard| guard.site.start.line()), found);
        assert_ne!(
            guards.get(&2).map(|guard| guard.site.start.line()),
            Some(5),
            "the guard should not be on its source line"
        );
    }
    use syn::parse_file;

    fn mutant(span: Range<usize>, ordinal: u32, replacement: &str, shape: Shape) -> Mutant {
        Mutant {
            ordinal,
            span,
            replacement: replacement.to_owned(),
            shape,
        }
    }

    #[test]
    fn continue_to_break_keeps_continue_as_the_tail_expression() {
        let text = "fn f() -> i32 { loop { let x = if true { continue } else { 1 }; return x } }\n";
        let site = span_of(text, "continue");
        let out = apply(text, &[mutant(site, 7, "break", Shape::Continue)]);

        assert!(out.contains("{ if ::gamma_rt::a(7u32) { break; } continue }"), "{out}");
        let _parsed = parse_file(&out).expect("the specialized guard must parse");
    }

    #[test]
    fn break_to_continue_keeps_break_as_the_tail_expression() {
        let text = "fn f() { loop { if true { break } } }\n";
        let site = span_of(text, "break");
        let out = apply(text, &[mutant(site, 8, "continue", Shape::Break)]);

        assert!(out.contains("{ if ::gamma_rt::a(8u32) { continue; } break }"), "{out}");
        let _parsed = parse_file(&out).expect("the specialized guard must parse");
    }

    fn apply(text: &str, mutants: &[Mutant]) -> String {
        let refs: Vec<&Mutant> = mutants.iter().collect();

        instrument(text, &refs).unwrap()
    }

    fn span_of(text: &str, needle: &str) -> Range<usize> {
        let start = text.find(needle).unwrap();

        start..start + needle.len()
    }

    #[test]
    fn no_mutants_leaves_the_text_untouched() {
        assert_eq!(apply("fn f() {}", &[]), "fn f() {}");
    }

    #[test]
    fn an_arm_site_becomes_a_guard_trailing_the_pattern() {
        let text = "fn f(v: Option<i32>) -> i32 { match v { Some(n) => n, _ => 0 } }";
        let out = apply(text, &[mutant(span_of(text, "Some(n)"), 4, "", Shape::Arm)]);

        // The arm has to keep its pattern: the body still binds `n`, so replacing the pattern
        // rather than qualifying it would not compile.
        assert!(out.contains("Some(n) if !::gamma_rt::a(4u32) => n"), "{out}");
    }

    #[test]
    fn a_guarded_arm_still_parses_as_rust() {
        let text = "fn f(v: Option<i32>) -> i32 { match v { Some(n) => n, _ => 0 } }";
        let out = apply(text, &[mutant(span_of(text, "Some(n)"), 4, "", Shape::Arm)]);

        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn two_arm_mutants_on_one_pattern_chain_with_and() {
        let text = "fn f(v: Option<i32>) -> i32 { match v { Some(n) => n, _ => 0 } }";
        let span = span_of(text, "Some(n)");
        let out = apply(text, &[mutant(span.clone(), 4, "", Shape::Arm), mutant(span, 5, "", Shape::Arm)]);

        // Repeating `if` would not parse. Only one mutant is ever active, so the conjunction is
        // never actually deciding between them, but it has to be syntactically well formed.
        assert!(out.contains("Some(n) if !::gamma_rt::a(4u32) && !::gamma_rt::a(5u32) =>"), "{out}");
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn an_arm_guard_nests_inside_an_enclosing_expression_site() {
        let text = "fn f(v: Option<i32>) -> i32 { match v { Some(n) => n, _ => 0 } }";
        let whole = span_of(text, "match v { Some(n) => n, _ => 0 }");
        let arm = span_of(text, "Some(n)");
        let out = apply(text, &[mutant(whole, 1, "0", Shape::Expr), mutant(arm, 2, "", Shape::Arm)]);

        _ = parse_file(&out).expect("the instrumented source does not parse");
        assert!(out.contains("if ::gamma_rt::a(1u32) { 0 }"), "{out}");
        assert!(out.contains("Some(n) if !::gamma_rt::a(2u32) =>"), "{out}");
    }

    #[test]
    fn an_expression_site_becomes_a_parenthesized_guard() {
        let text = "fn f(a: i32, b: i32) -> bool { a < b }";
        let out = apply(text, &[mutant(span_of(text, "a < b"), 7, "(a) <= (b)", Shape::Expr)]);

        assert_eq!(
            out,
            "fn f(a: i32, b: i32) -> bool { (if ::gamma_rt::a(7u32) { (a) <= (b) } else { a < b }) }"
        );
    }

    #[test]
    fn the_guarded_expression_still_parses_as_rust() {
        let text = "fn f(a: i32, b: i32) -> bool { a < b }";
        let out = apply(text, &[mutant(span_of(text, "a < b"), 7, "(a) <= (b)", Shape::Expr)]);

        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn a_guard_in_condition_position_parses() {
        // Without the parentheses this would be `if { .. } { .. }`, which Rust rejects.
        let text = "fn f(a: i32, b: i32) { if a < b { g(); } }";
        let out = apply(text, &[mutant(span_of(text, "a < b"), 1, "(a) <= (b)", Shape::Expr)]);

        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn two_mutants_on_one_span_nest_rather_than_collide() {
        let text = "fn f(a: i32, b: i32) -> bool { a < b }";
        let site = span_of(text, "a < b");
        let out = apply(
            text,
            &[
                mutant(site.clone(), 1, "(a) <= (b)", Shape::Expr),
                mutant(site, 2, "(a) > (b)", Shape::Expr),
            ],
        );

        assert!(out.contains("::gamma_rt::a(1u32)"));
        assert!(out.contains("::gamma_rt::a(2u32)"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn a_nested_site_is_instrumented_only_in_the_default_branch() {
        // `a + b < c`: the `<` site contains the `+` site. Exactly one mutant is live per process,
        // so the `<` replacement can use plain operands; only the fall-through arm needs children.
        let text = "fn f(a: i32, b: i32, c: i32) -> bool { a + b < c }";
        let out = apply(
            text,
            &[
                mutant(span_of(text, "a + b < c"), 1, "(a + b) <= (c)", Shape::Expr),
                mutant(span_of(text, "a + b"), 2, "(a) - (b)", Shape::Expr),
            ],
        );

        assert_eq!(out.matches("::gamma_rt::a(2u32)").count(), 1);

        let then_arm = out.split("else").next().unwrap();

        assert!(!then_arm.contains("a(2u32)"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn nesting_does_not_grow_exponentially() {
        // Three nested sites encode three guards, not eight.
        let text = "fn f(a: i32, b: i32, c: i32, d: i32) -> bool { a + b * c < d }";
        let out = apply(
            text,
            &[
                mutant(span_of(text, "a + b * c < d"), 1, "(a + b * c) <= (d)", Shape::Expr),
                mutant(span_of(text, "a + b * c"), 2, "(a) - (b * c)", Shape::Expr),
                mutant(span_of(text, "b * c"), 3, "(b) / (c)", Shape::Expr),
            ],
        );

        assert_eq!(out.matches("::gamma_rt::a(").count(), 3);
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn a_block_site_stays_a_block() {
        let text = "fn f() -> i32 { compute() }";
        let out = apply(text, &[mutant(span_of(text, "{ compute() }"), 4, "0", Shape::Block)]);

        assert!(out.contains("{ if ::gamma_rt::a(4u32) { 0 } else {"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn a_block_site_containing_an_expression_site_nests() {
        let text = "fn f(a: i32) -> i32 { a + 1 }";
        let out = apply(
            text,
            &[
                mutant(span_of(text, "{ a + 1 }"), 1, "0", Shape::Block),
                mutant(span_of(text, "a + 1"), 2, "(a) - (1)", Shape::Expr),
            ],
        );

        assert_eq!(out.matches("::gamma_rt::a(").count(), 2);
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn a_statement_site_is_skipped_when_active() {
        let text = "fn f(v: &mut Vec<i32>) { v.push(1); }";
        let out = apply(text, &[mutant(span_of(text, "v.push(1);"), 9, "", Shape::Stmt)]);

        assert!(out.contains("if !::gamma_rt::a(9u32) { v.push(1); }"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn sites_are_spliced_at_the_right_offsets_when_there_are_several() {
        let text = "fn f(a: i32, b: i32) -> i32 { let x = a + b; let y = a - b; x * y }";
        let out = apply(
            text,
            &[
                mutant(span_of(text, "a + b"), 1, "(a) - (b)", Shape::Expr),
                mutant(span_of(text, "a - b"), 2, "(a) + (b)", Shape::Expr),
            ],
        );

        assert!(out.contains("let x = (if ::gamma_rt::a(1u32)"));
        assert!(out.contains("let y = (if ::gamma_rt::a(2u32)"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn text_outside_every_site_is_preserved_exactly() {
        let text = "// a comment\nfn f(a: i32) -> i32 { a + 1 }\n// trailing\n";
        let out = apply(text, &[mutant(span_of(text, "a + 1"), 1, "(a) - (1)", Shape::Expr)]);

        assert!(out.starts_with("// a comment\n"));
        assert!(out.ends_with("// trailing\n"));
    }

    #[test]
    fn a_span_beyond_the_text_is_skipped_rather_than_spliced() {
        let text = "fn f() {}";

        assert_eq!(apply(text, &[mutant(100..200, 1, "0", Shape::Expr)]), text);
    }

    #[test]
    fn an_empty_span_is_skipped() {
        let text = "fn f() {}";

        assert_eq!(apply(text, &[mutant(3..3, 1, "0", Shape::Expr)]), text);
    }

    /// A span landing inside a character is not a span this file can splice.
    ///
    /// Every offset is within the text, so a length check alone lets it through — and then every
    /// slice taken from it is `None`, which as an empty string erases the region instead of
    /// splicing it. That is worse than a wrong-offset splice: whole functions vanish from the
    /// instrumented copy, the build fails somewhere unrelated, and nothing in the pipeline can
    /// attribute it, because the guard the invariant check looks for was written correctly.
    #[test]
    fn a_span_bisecting_a_character_is_skipped_rather_than_deleting_the_region() {
        let text = "fn f() -> usize { let s = \"ππ\"; s.len() }";
        let start = text.find('π').expect("the fixture holds a multi-byte character");

        assert!(!text.is_char_boundary(start + 1), "the premise is an endpoint inside a character");

        assert_eq!(apply(text, &[mutant(start..start + 1, 1, "0", Shape::Expr)]), text);
        assert_eq!(apply(text, &[mutant(start + 1..text.len(), 2, "0", Shape::Expr)]), text);
    }

    #[test]
    fn overlapping_sites_are_rejected() {
        let text = "fn f(a: i32, b: i32, c: i32) -> i32 { a + b + c }";
        let left = span_of(text, "a + b");
        let right = span_of(text, "b + c");
        let result = instrument(text, &[&mutant(left, 1, "x", Shape::Expr), &mutant(right, 2, "y", Shape::Expr)]);

        _ = result.expect_err("the instrumentation was expected to fail");
    }

    #[test]
    fn multibyte_text_is_spliced_on_byte_boundaries() {
        let text = "fn f(a: i32) -> i32 { /* π ≈ 3 */ a + 1 }";
        let out = apply(text, &[mutant(span_of(text, "a + 1"), 1, "(a) - (1)", Shape::Expr)]);

        assert!(out.contains("π ≈ 3"));
        _ = parse_file(&out).expect("the instrumented source does not parse");
    }

    #[test]
    fn ordinals_are_emitted_in_order_within_a_span() {
        let text = "fn f(a: i32, b: i32) -> bool { a < b }";
        let site = span_of(text, "a < b");
        let out = apply(
            text,
            &[
                mutant(site.clone(), 5, "(a) > (b)", Shape::Expr),
                mutant(site, 3, "(a) <= (b)", Shape::Expr),
            ],
        );

        assert!(out.find("a(3u32)").unwrap() < out.find("a(5u32)").unwrap());
    }
}
