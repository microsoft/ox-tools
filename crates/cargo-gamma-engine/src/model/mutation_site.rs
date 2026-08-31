// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;

use compact_str::CompactString;

/// Per-span data shared by every replacement that targets the same source construct.
///
/// Multiple mutators (or multiple replacements from one mutator) can target the same byte range.
/// Storing the location and original text once per span, rather than once per definition, removes
/// repeated allocations proportional to the replacement count at each site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationSite {
    /// Byte range of the construct in the original file.
    pub span: Range<usize>,

    /// One-based line of the start of the span.
    pub line: usize,

    /// One-based line of the last source line the span covers.
    pub end_line: usize,

    /// One-based column of the start of the span.
    pub column: usize,

    /// The original source text of the construct.
    pub original: CompactString,
}
