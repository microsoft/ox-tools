// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;
use std::sync::Arc;

use camino::Utf8Path;
use cargo_gamma_engine::model::MutantDefinition;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use super::identity::MutantId;
use super::outcome::Outcome;
use super::suppression::Suppression;
use crate::ops::collect::Shape;

/// What a directive says a mutant's fate should be, and where the claim was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectation {
    /// Whether the mutant is expected to be killed or to survive.
    pub killed: bool,

    /// One-based line of the directive that made the claim.
    pub line: usize,

    /// The stated reason, if any.
    pub reason: Option<String>,
}

/// One mutant: a single change to a single site, with a stable identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mutant {
    /// Content-addressed stable identity: [`MUTANT_ID_HEX_LEN`] lowercase hex characters.
    ///
    /// [`MUTANT_ID_HEX_LEN`]: crate::model::MUTANT_ID_HEX_LEN
    pub id: MutantId,

    /// One-based ordinal within this run, used as the value of `GAMMA_ACTIVE`.
    ///
    /// Unlike [`Mutant::id`] this is *not* stable across runs; it is a compact selector.
    pub ordinal: u32,

    /// Path relative to the workspace root, with forward slashes.
    ///
    /// Shared rather than owned per mutant: a file with four hundred mutants would otherwise hold
    /// four hundred copies of its own path. See [`super::Interner`].
    pub file: Arc<Utf8Path>,

    /// The package the file belongs to.
    ///
    /// Shared: a workspace has a handful of package names and a run has hundreds of thousands of
    /// mutants.
    pub package: Arc<str>,

    /// Byte range of the mutated construct in the original file.
    pub span: Range<usize>,

    /// One-based line of the start of the span.
    pub line: usize,

    /// One-based line of the last source line the span covers.
    ///
    /// Equal to `line` for a site on a single line; greater for one that spans several, such as a
    /// multi-line call, match, or binary expression. Carrying the end line lets `--in-diff` match a
    /// site by its whole extent, so a diff that edited only an interior line of the site still
    /// selects its mutants rather than dropping them.
    pub end_line: usize,

    /// One-based column of the start of the span.
    pub column: usize,

    /// The registry name of the mutator that produced this mutant.
    ///
    /// Shared, and the sharpest case of it: the name starts life as a `&'static str` in the
    /// registry and is drawn from a set of a few dozen.
    pub mutator: Arc<str>,

    /// Path of the enclosing item, such as `parser::Lexer::next_token`.
    ///
    /// Shared: one item contributes as many mutants as it has sites.
    pub item_path: Arc<str>,

    /// Index among identical normalized sites within the enclosing item.
    pub occurrence: u32,

    /// Index among the replacements this mutator offers at this site.
    pub replacement_index: u32,

    /// The original source text of the construct, for display.
    pub original: CompactString,

    /// The replacement source text, for display and for splicing.
    pub replacement: CompactString,

    /// How the site must be guarded when the schema is instrumented.
    pub shape: Shape,

    /// The verdict.
    pub outcome: Outcome,

    /// Why it was suppressed, when it was.
    pub suppression: Option<Suppression>,

    /// What an `expect_survived` or `expect_killed` directive says this mutant's fate should be.
    ///
    /// Recorded at discovery, checked once the run has a verdict for it. An expectation is a claim
    /// about the test suite that the author wants held to — "this site is deliberately untested" or
    /// "this site must stay covered" — so it is worth failing the run when reality diverges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expectation: Option<Expectation>,

    /// Specific test timeout multiplier override for this mutant, if specified by a directive or attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout_multiplier: Option<f64>,

    /// Wall time spent deciding this mutant, in milliseconds.
    pub elapsed_ms: u64,

    /// The name of the test that killed it, when one did.
    pub killed_by: Option<String>,

    /// Anything else worth saying about the verdict.
    ///
    /// Separate from `killed_by` because that field means one specific thing — the test whose
    /// failure detected the mutant, and the report publishes it under that name.
    pub note: Option<String>,
}

impl Mutant {
    pub(crate) fn from_definition(definition: MutantDefinition, package: Arc<str>) -> Self {
        Self {
            id: definition.id,
            ordinal: 0,
            file: definition.file,
            package,
            span: definition.site.span.clone(),
            line: definition.site.line,
            end_line: definition.site.end_line,
            column: definition.site.column,
            mutator: definition.mutator,
            item_path: definition.item_path,
            occurrence: definition.occurrence,
            replacement_index: definition.replacement_index,
            original: definition.site.original.clone(),
            replacement: definition.replacement,
            shape: definition.shape,
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }

    /// Renders a one-line human description, in the form used by `list` and the console reporter.
    ///
    /// A mutated construct can span many lines. Emitting it verbatim would break the one-line
    /// contract that makes this output greppable, so it is flattened and elided in the middle: the
    /// two ends are what identify the construct, and the middle is what is least informative.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{}:{}:{}: {} [{}]", self.file, self.line, self.column, self.summary(), self.mutator)
    }

    /// How long the run spent deciding this mutant.
    ///
    /// The stored figure is milliseconds because that is what a report holds; every reader of it
    /// wants a `Duration`, and converting at each of them invites one of them to divide instead.
    #[must_use]
    pub const fn elapsed(&self) -> core::time::Duration {
        core::time::Duration::from_millis(self.elapsed_ms)
    }

    /// Renders just the change, with no location.
    ///
    /// Reports that carry the location in a field of their own would otherwise repeat it in the
    /// prose beside it.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.shape {
            Shape::Stmt => format!("delete {}", one_line(&self.original, 56)),
            // The replacement is a guard the reader never sees, so describing it would say
            // nothing. What changed is that the arm stops matching.
            Shape::Arm => format!("stop the arm matching {} from matching", one_line(&self.original, 48)),
            // The `Either` wrapper an `IterBlock` splices around both arms is scaffolding the
            // reader never wrote and cannot act on, so the change is reported as the replacement
            // alone, exactly as for any other body.
            Shape::Expr | Shape::Block | Shape::IterBlock | Shape::Continue | Shape::Break => {
                format!("replace {} with {}", one_line(&self.original, 40), one_line(&self.replacement, 32))
            }
        }
    }
}

/// Collapses text to a single line of at most `width` characters.
#[must_use]
pub fn one_line(text: &str, width: usize) -> String {
    let flattened: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    let length = flattened.chars().count();

    if length <= width {
        return flattened;
    }

    let head = width.saturating_sub(3) / 2;
    let tail = width.saturating_sub(3) - head;

    flattened
        .chars()
        .take(head)
        .chain("...".chars())
        .chain(flattened.chars().skip(length - tail))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    /// A minimal mutant, with only the fields `summary` reads set to something meaningful.
    ///
    /// Every other field is a placeholder, because `summary` never touches them; keeping the
    /// builder narrow means a future field added to `Mutant` cannot silently make this test
    /// obsolete by requiring a value that was never a documented contract of the method under
    /// test.
    fn mutant(shape: Shape, original: &str, replacement: &str) -> Mutant {
        Mutant {
            id: "deadbeefcafe".to_owned().into(),
            file: Arc::from(Utf8Path::new("a.rs")),
            mutator: ("m".to_owned()).into(),
            original: original.to_owned().into(),
            replacement: replacement.to_owned().into(),
            shape,
            outcome: Outcome::Pending,
            ..fixtures::mutant()
        }
    }

    #[test]
    fn a_statement_shaped_mutant_is_summarized_as_a_deletion() {
        // `Shape::Stmt` mutants work by deleting the statement outright, so the summary must say
        // "delete", not "replace" — a reader deciding whether a survivor matters needs to know
        // immediately whether the mutant removed code or swapped it for something else.
        let mutant = mutant(Shape::Stmt, "counter += 1;", "");

        assert_eq!(mutant.summary(), "delete counter += 1;");
    }

    #[test]
    fn an_arm_shaped_mutant_never_shows_its_guard_replacement() {
        // A match arm mutant is made unreachable by a guard the reader never sees in the source,
        // so printing `self.replacement` here would show meaningless generated text. If this
        // regressed to include the replacement, every arm survivor report would confuse users
        // with guard internals instead of naming the pattern that stopped matching.
        let mutant = mutant(Shape::Arm, "Some(value)", "if GAMMA_GUARD { unreachable!() }");

        assert_eq!(mutant.summary(), "stop the arm matching Some(value) from matching");
    }

    #[test]
    fn expr_and_block_shaped_mutants_are_summarized_as_a_replacement() {
        // Both shapes swap the original text for the replacement text, so both must render
        // identically through `summary` even though they are guarded differently at
        // instrumentation time — the shape only changes how the guard is wrapped, not how the
        // change is described to a human.
        let expr = mutant(Shape::Expr, "a + b", "a - b");
        let block = mutant(Shape::Block, "{ a() }", "{ b() }");

        assert_eq!(expr.summary(), "replace a + b with a - b");
        assert_eq!(block.summary(), "replace { a() } with { b() }");
    }

    #[test]
    fn short_source_text_is_inline_and_long_source_text_remains_intact() {
        let short = mutant(Shape::Expr, "a + b", "a - b");

        assert!(!short.id.is_heap_allocated());
        assert!(!short.original.is_heap_allocated());
        assert!(!short.replacement.is_heap_allocated());

        let original = "a".repeat(25);
        let replacement = "b".repeat(25);
        let long = mutant(Shape::Expr, &original, &replacement);

        assert!(long.original.is_heap_allocated());
        assert!(long.replacement.is_heap_allocated());
        assert_eq!(long.original, original);
        assert_eq!(long.replacement, replacement);
    }

    #[test]
    fn a_description_is_always_one_line() {
        let text = "a\n  +\n  b";

        assert_eq!(one_line(text, 48), "a + b");
    }

    #[test]
    fn a_long_construct_is_elided_in_the_middle() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india";
        let short = one_line(text, 20);

        assert_eq!(short.chars().count(), 20);
        assert!(short.contains("..."), "{short}");
        assert!(short.starts_with("alpha"), "{short}");
        assert!(short.ends_with("india"), "{short}");
    }

    #[test]
    fn a_short_construct_is_left_alone() {
        assert_eq!(one_line("a + b", 48), "a + b");
    }

    #[test]
    fn eliding_counts_characters_not_bytes() {
        let text = "ééééééééééééééééééééééééé";

        assert_eq!(one_line(text, 10).chars().count(), 10);
    }
}
