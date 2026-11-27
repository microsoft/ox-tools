// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Turning source text into a syntax tree with byte-accurate spans and comment trivia.
//!
//! Two things make this module more than a thin wrapper around `syn::parse_file`.
//!
//! The first is byte accuracy. Instrumentation is a text splice, not a token-tree rewrite, because
//! re-emitting a `syn` tree through `quote` would reformat every file it touches and destroy the
//! line numbers that every report, every suppression and every diff depends on. That requires
//! exact byte offsets for each node, which `proc-macro2` provides only with the `span-locations`
//! feature enabled.
//!
//! The second is comments. Comments are trivia: they are not in the syntax tree at all, so a
//! suppression written as a comment is invisible to `syn`. This module scans the raw text for
//! them, which means it needs a small but honest lexer that knows about raw strings, escapes and
//! the lifetime-versus-character-literal ambiguity.
//!
//! # The one file this module refuses
//!
//! Source under audit is untrusted input, and every stage that reads it — the parser here, the
//! collector's visitor, the scope walk, the render that splices guards back in — descends by
//! recursion. A file nested past [`nesting::NESTING_LIMIT`] levels is therefore refused with a
//! diagnostic naming it, rather than parsed into a stack overflow that names nothing.

mod comment;
mod extent;
// `pub` (within this private module) so the crate's proc-macro agreement test can reach
// `nesting::NESTING_LIMIT` and `nesting::CHAIN_FACTOR` through the `internals` facade.
pub mod nesting;
mod source_file;

#[doc(inline)]
pub use comment::{Comment, CommentKind};
pub(crate) use comment::{comment_spans, literal_end};
pub(crate) use extent::Extent;
pub(crate) use source_file::shebang_as_whitespace;
#[doc(hidden)]
pub use source_file::{BOM, strip_bom};
#[doc(inline)]
pub use source_file::{SourceFile, exceeds_nesting_limit, without_bom};

/// Configurable nesting budgets for small integration-test inputs.
#[cfg(feature = "test-util")]
pub mod test_util {
    use camino::Utf8PathBuf;

    use super::{SourceFile, source_file};
    use crate::Result;

    /// Checks the normal source precheck against an explicit test budget.
    #[must_use]
    pub fn exceeds_nesting_limit(text: &str, limit: usize) -> bool {
        source_file::exceeds_nesting_limit_with_limit(text, limit)
    }

    /// Parses through the normal source pipeline with an explicit test budget.
    ///
    /// # Errors
    ///
    /// Returns the ordinary parse or nesting diagnostic for this input and budget.
    pub fn parse(path: impl Into<Utf8PathBuf>, text: String, limit: usize) -> Result<SourceFile> {
        SourceFile::parse_with_limit(path.into(), text, limit)
    }
}
