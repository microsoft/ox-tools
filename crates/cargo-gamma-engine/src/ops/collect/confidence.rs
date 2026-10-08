// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Candidate confidence records how strongly source-visible evidence supports compilation.
//!
//! Proven candidates have positive syntactic evidence, optimistic candidates depend on semantic
//! facts the collector cannot establish, and explicit candidates come directly from user input.
//! The stable spellings are part of diagnostics and selection telemetry. Confidence deliberately
//! does not participate in mutant identity, so improving inference does not invalidate persisted
//! outcomes or suppressions for an otherwise unchanged mutant.

/// How strongly source-visible evidence supports a candidate compiling.
///
/// ```
/// use cargo_gamma_engine::ops::collect::Confidence;
///
/// assert_eq!(Confidence::Optimistic.to_string(), "optimistic");
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// The source provides enough evidence for the replacement.
    #[default]
    Proven,

    /// The replacement depends on a semantic fact the source does not establish.
    Optimistic,

    /// The user explicitly requested the replacement value.
    Explicit,
}

impl Confidence {
    /// Stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proven => "proven",
            Self::Optimistic => "optimistic",
            Self::Explicit => "explicit",
        }
    }
}

impl core::fmt::Display for Confidence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn display_uses_stable_diagnostic_spellings() {
        assert_eq!(Confidence::Proven.to_string(), "proven");
        assert_eq!(Confidence::Optimistic.to_string(), "optimistic");
        assert_eq!(Confidence::Explicit.to_string(), "explicit");
    }
}
