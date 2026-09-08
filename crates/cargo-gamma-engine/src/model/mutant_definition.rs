// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;
use std::sync::Arc;

use camino::Utf8Path;
use compact_str::CompactString;

use super::{MutantId, MutationSite};
use crate::ops::collect::Shape;

/// One source-level mutation, before Cargo/run policy and execution state are attached.
#[derive(Debug, Clone)]
pub struct MutantDefinition {
    pub id: MutantId,
    pub file: Arc<Utf8Path>,
    /// Shared site data (span, location, original text) for all replacements at this span.
    pub site: Arc<MutationSite>,
    pub mutator: Arc<str>,
    pub item_path: Arc<str>,
    /// Terminal identifier of the enclosing implemented trait, for selection policy.
    ///
    /// This is the final written path segment, not a qualified trait path or an identity component.
    pub trait_impl: Option<Arc<str>>,
    pub occurrence: u32,
    pub replacement_index: u32,
    pub replacement: CompactString,
    pub shape: Shape,
}

impl MutantDefinition {
    /// Byte range of the construct in the original file.
    #[inline]
    #[must_use]
    pub fn span(&self) -> &Range<usize> {
        &self.site.span
    }

    /// One-based start line.
    #[inline]
    #[must_use]
    pub fn line(&self) -> usize {
        self.site.line
    }

    /// One-based end line.
    #[inline]
    #[must_use]
    pub fn end_line(&self) -> usize {
        self.site.end_line
    }

    /// One-based start column.
    #[inline]
    #[must_use]
    pub fn column(&self) -> usize {
        self.site.column
    }

    /// The original source text of the construct.
    #[inline]
    #[must_use]
    pub fn original(&self) -> &CompactString {
        &self.site.original
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a definition whose accessors are each expected to return a distinguishable value,
    /// so a mistake such as reading the wrong field cannot pass unnoticed.
    fn sample() -> MutantDefinition {
        MutantDefinition {
            id: MutantId::new("deadbeefcafe"),
            file: Arc::from(Utf8Path::new("src/lib.rs")),
            site: Arc::new(MutationSite {
                span: 12..19,
                line: 3,
                end_line: 4,
                column: 5,
                original: "1 + 1".to_owned().into(),
            }),
            mutator: Arc::from("arith.add_to_sub"),
            item_path: Arc::from("subject::f"),
            trait_impl: None,
            occurrence: 0,
            replacement_index: 0,
            replacement: "1 - 1".to_owned().into(),
            shape: Shape::Expr,
        }
    }

    /// Every accessor reads through to the field on the shared `MutationSite` it delegates to,
    /// rather than to some other field or a stale copy.
    #[test]
    fn accessors_read_through_to_the_shared_site() {
        let definition = sample();

        assert_eq!(definition.span(), &(12..19));
        assert_eq!(definition.line(), 3);
        assert_eq!(definition.end_line(), 4);
        assert_eq!(definition.column(), 5);
        assert_eq!(definition.original(), "1 + 1");
    }
}
