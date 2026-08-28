// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The interchange schema's verdict names, and what each one does to the merged score.

use crate::model::Scoring;

/// A verdict that means the mutant was never actually run.
pub(super) const NEVER_RUN: &str = "Pending";

/// Where a status lands in the merged score, or `None` when the schema does not define it.
///
/// One classification rather than a pair of predicates, because the pair disagreed: written as an
/// allow-list for the numerator and a deny-list for the denominator, a status neither of them had
/// heard of landed in the denominator as an *undetected* mutant — the worst of the three available
/// defaults, since it is the one that moves the score.
///
/// `merge` is the one command that reads documents it did not write, so the closed status set is
/// the schema's and not this tool's. Gamma deliberately classifies `Timeout` as undetected: a
/// resource limit observed the mutant, but no test assertion rejected it. `RuntimeError` remains
/// excluded because the run established neither detection nor survival.
///
/// A status outside the set is not classified at all. [`read`](super::read) refuses a document
/// carrying one, so nothing here has to guess what a misspelling meant, and a `Merged` assembled by
/// hand from an unchecked document leaves it out of the fraction rather than charging the score for
/// a word nobody can interpret.
pub(super) fn scoring(status: &str) -> Option<Scoring> {
    match status {
        "Killed" => Some(Scoring::Detected),
        "Survived" | "Timeout" | "NoCoverage" => Some(Scoring::Undetected),
        "CompileError" | "Ignored" | "RuntimeError" | NEVER_RUN => Some(Scoring::Excluded),
        _undefined => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema's status set, and what each one is worth.
    ///
    /// Spelled out here rather than derived, because the point of the table is that it is the
    /// *schema's* list: a status this crate stopped writing is still a status another producer may
    /// send, and dropping one from the classification silently moves a merged score.
    #[test]
    fn every_status_the_schema_defines_is_classified() {
        for (status, expected) in [
            ("Killed", Scoring::Detected),
            ("Timeout", Scoring::Undetected),
            ("Survived", Scoring::Undetected),
            ("NoCoverage", Scoring::Undetected),
            ("CompileError", Scoring::Excluded),
            ("RuntimeError", Scoring::Excluded),
            ("Ignored", Scoring::Excluded),
            ("Pending", Scoring::Excluded),
        ] {
            assert_eq!(scoring(status), Some(expected), "{status}");
        }
    }

    /// Nothing outside the schema is classified, including the near misses a corrupt or
    /// hand-edited document produces: a merge that guessed at `Kiled` would be guessing with the
    /// score.
    #[test]
    fn a_status_outside_the_schema_is_not_classified() {
        for status in ["Kiled", "killed", "", "OutOfMemory", "NotBuilt"] {
            assert_eq!(scoring(status), None, "{status}");
        }
    }
}
