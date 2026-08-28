// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;
use std::sync::Arc;

use compact_str::CompactString;

use super::Shape;

/// One mutation opportunity, before it is given a run-wide ordinal.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Registry name of the mutator.
    pub mutator: &'static str,

    /// Byte range of the construct being replaced.
    pub span: Range<usize>,

    /// The text that replaces the *whole* span.
    ///
    /// Whole-span, not just the changed token: every mutant is then a uniform (range, text) pair,
    /// so instrumentation never has to know what family produced it, and the human-readable form
    /// shows the real before and after rather than a bare operator with no context.
    pub replacement: CompactString,

    /// Index among the replacements this mutator offers at this site.
    pub replacement_index: u32,

    /// Path of the enclosing item.
    ///
    /// Shared rather than owned: a file opens a few hundred scopes and emits tens of thousands of
    /// candidates, so one allocation per scope and a pointer per candidate is the whole difference.
    pub item_path: Arc<str>,

    /// How the site must be guarded.
    pub shape: Shape,
}
