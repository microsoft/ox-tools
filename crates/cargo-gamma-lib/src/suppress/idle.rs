// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Naming the skip directives that suppressed nothing, and could have.

use camino::{Utf8Path, Utf8PathBuf};

use super::{Directive, Intent};
use crate::model::Mutant;
use crate::ops::registry::Selection;

/// One skip directive that suppressed nothing, located well enough to go and look at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Idle {
    /// The file it sits in, relative to the workspace root.
    pub file: Utf8PathBuf,

    /// One-based line the directive appears on.
    pub line: usize,

    /// The selector text as written, so the report says what the author said.
    pub selectors: String,

    /// The stated reason, if any. Usually the most informative thing about a stale directive.
    pub reason: Option<String>,
}

/// Names the `skip` directives in a file that suppressed nothing, and could have.
///
/// A skip directive earns its place by suppressing a mutant that would otherwise be reported. Once
/// the code beneath it changes — the expression is rewritten, the operator stops firing there, a
/// test is written that would now kill it — the directive keeps applying to nothing. Nothing says
/// so, so it stays, and the next reader takes it as evidence that something there is genuinely
/// untestable. This is the one part of a mutation report nobody can audit from outside: a directive
/// that suppresses nothing is indistinguishable from one doing real work, and both read as a
/// deliberate decision.
///
/// # What is deliberately not reported
///
/// *Could have* is the whole difficulty. A directive that named mutators this run never offered has
/// not stopped earning its place — the run simply never visited it — and condemning it would make
/// `--mutators relational` report every `skip(arith)` in the tree as dead wood. So a directive is only
/// named when the run had something to suppress it with: its own selection has to intersect the
/// run's, or there was never a mutant for it to govern in the first place.
///
/// The narrower filters need no such care. A file outside `--package`, a shard that did not hold
/// it, or a `--in-diff` that skipped it means the file was never scanned, so its directives are
/// never collected and cannot be reported.
///
/// Only `skip` is considered. `expect_killed` and `expect_survived` are claims about a verdict
/// rather than suppressions, and the run already fails when one of those is not met.
#[must_use]
pub fn idle(file: &Utf8Path, mutants: &[Mutant], found: &[Directive], offered: &Selection) -> Vec<Idle> {
    found
        .iter()
        .filter(|directive| directive.intent == Some(Intent::Skip))
        .filter(|directive| directive.selection.sorted().iter().any(|name| offered.contains(name)))
        .filter(|directive| !mutants.iter().any(|mutant| directive.governs(mutant)))
        .map(|directive| Idle {
            file: file.to_owned(),
            line: directive.line,
            selectors: directive.selectors.clone(),
            reason: directive.reason.clone(),
        })
        .collect()
}
