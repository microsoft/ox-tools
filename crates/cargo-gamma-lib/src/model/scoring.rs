// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a verdict does to the mutation score.

/// Where a verdict lands in the score's fraction.
///
/// The three cases exhaust it: a mutant is in the numerator and the denominator, in the denominator
/// alone, or in neither. Saying that once, as a type, is what stops the answer drifting. A pair of
/// independent predicates — "did the suite notice?" and "does it count?" — can be given a fourth,
/// incoherent answer by accident, and each reporter that consults the pair inherits whatever the
/// pair happened to decide; every reporter that consults this decides the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scoring {
    /// In the numerator and the denominator: the suite noticed the mutant.
    Detected,

    /// In the denominator only: the mutant was judged and nothing noticed it.
    Undetected,

    /// In neither: the run established nothing about the tests, so counting it either way would be
    /// a claim the run did not make.
    Excluded,
}

impl Scoring {
    /// Whether this counts toward the denominator.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        !matches!(self, Self::Excluded)
    }

    /// Whether this counts toward the numerator.
    #[must_use]
    pub const fn is_detected(self) -> bool {
        matches!(self, Self::Detected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numerator is a subset of the denominator, which is what makes the score a fraction of at
    /// most one. A classification that detected something it did not count would score above 100%.
    #[test]
    fn everything_detected_is_also_counted() {
        for scoring in [Scoring::Detected, Scoring::Undetected, Scoring::Excluded] {
            assert!(!scoring.is_detected() || scoring.is_valid(), "{scoring:?}");
        }
    }
}
