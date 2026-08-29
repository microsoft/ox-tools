// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use super::scoring::Scoring;

/// The verdict for a single mutant.
///
/// The names follow the `mutation-testing-elements` schema so that our reports and the standard
/// report UI agree on vocabulary without a translation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    /// Never judged: not yet run, or run and the machine refused to perform it.
    Pending,

    /// A test failed while this mutant was active: the test suite detected the change.
    Killed,

    /// Every test passed while this mutant was active: the change went unnoticed.
    Survived,

    /// The test run exceeded its budget.
    ///
    /// Counted as undetected because no assertion established that the suite rejected the change.
    Timeout,

    /// The mutant's test run passed the memory ceiling derived from that binary's own baseline.
    ///
    /// Counted as undetected: the ceiling established that the mutant changed resource use, but no
    /// assertion established that the suite rejected the change. It remains distinct from
    /// [`Self::Survived`] because its remedy is a resource bound or terminating assertion rather
    /// than an assertion over a completed result.
    OutOfMemory,

    /// The mutant could not be compiled. Not a test-suite failing.
    CompileError,

    /// A test failed both with the mutant active and with no mutant active.
    ///
    /// The suite noticed something, but not the mutant, so crediting the mutant with a kill would
    /// let one unreliable test manufacture a detection against every mutant it happened to be run
    /// against. Recording it as a survivor is just as wrong in the other direction: a survivor is a
    /// claim that the tests have a gap, and it sends the reader to write an assertion for code an
    /// assertion already covers. The useful report is neither verdict but the name of the test that
    /// failed both ways, which is what this outcome carries a note for.
    ///
    /// Excluded from the score on the same reasoning as [`Self::CompileError`]: nothing was
    /// established about the tests, so counting it either way would be a lie.
    Flaky,

    /// Suppressed by a directive, an attribute, or configuration.
    Ignored,

    /// No test reaches this mutant's site.
    NoCoverage,

    /// The build never compiled this mutant's source file, so it was never a candidate.
    ///
    /// Conditional compilation is the reason: a module behind `#[cfg(feature = "serde")]` is real
    /// source that a run without that feature never builds. Mutants are found by reading files, so
    /// they are generated there anyway, and the instrumented tree compiles perfectly well because
    /// the code holding them is not part of it.
    ///
    /// Reporting such a mutant as a survivor is the worst available answer: no test can fail for
    /// code that was never built, so it reads as a test-suite gap that nobody could ever close. On a
    /// crate whose `serde` support is not a default feature that is most of the survivor list, and
    /// thirty points of score.
    ///
    /// Excluded from the score for the same reason `CompileError` is — a mutant that never ran is
    /// not evidence about the tests — and named separately so the run can say the feature set is
    /// the reason and the reader can decide whether to widen it.
    NotBuilt,
}

impl Outcome {
    /// Every outcome, detected first, then the rest of the denominator, then what stays out of it.
    ///
    /// A breakdown is only trustworthy if its rows add up to the population it sits under, and a
    /// reporter that spells its own list of outcomes out by hand is a list nobody has to extend: the
    /// run-table that omitted three counters rendered a perfect score over an empty breakdown, and
    /// nothing in the compiler had an opinion about it. Reporters walk this instead, so a run's
    /// verdicts are enumerated in one place, in one order, and the test below refuses an array that
    /// has fallen behind the enum.
    pub const ALL: [Self; 10] = [
        Self::Killed,
        Self::Timeout,
        Self::OutOfMemory,
        Self::Survived,
        Self::NoCoverage,
        Self::Flaky,
        Self::CompileError,
        Self::Ignored,
        Self::NotBuilt,
        Self::Pending,
    ];

    /// Where this outcome lands in the score's fraction.
    ///
    /// One `match` rather than a pair of independent predicates, so the question is asked once per
    /// variant and the compiler asks it. Two predicates each answering "no" by falling off the end
    /// of their own list is how a new outcome silently becomes an undetected mutant — counted
    /// against the score by the very code that meant to leave it out.
    ///
    /// Compile errors, suppressions, flakes and mutants the build never compiled are excluded: a
    /// mutant that never ran, or whose run established nothing, is not evidence about the test
    /// suite, and counting it either way would be a lie.
    #[must_use]
    pub const fn scoring(self) -> Scoring {
        match self {
            Self::Killed => Scoring::Detected,
            Self::Survived | Self::Timeout | Self::OutOfMemory | Self::NoCoverage => Scoring::Undetected,
            Self::Flaky | Self::CompileError | Self::Ignored | Self::NotBuilt | Self::Pending => Scoring::Excluded,
        }
    }

    /// Returns whether this outcome counts as the suite having detected the mutant.
    #[must_use]
    pub const fn is_detected(self) -> bool {
        self.scoring().is_detected()
    }

    /// Returns whether this outcome contributes to the mutation score at all.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.scoring().is_valid()
    }

    /// Returns the short lowercase name used in output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Killed => "killed",
            Self::Survived => "survived",
            Self::Timeout => "timeout",
            Self::OutOfMemory => "outofmem",
            Self::Flaky => "flaky",
            Self::CompileError => "unviable",
            Self::Ignored => "ignored",
            Self::NoCoverage => "uncovered",
            Self::NotBuilt => "notbuilt",
        }
    }
}

impl Display for Outcome {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_classification() {
        assert!(Outcome::Killed.is_detected());
        assert!(!Outcome::Timeout.is_detected());
        assert!(!Outcome::OutOfMemory.is_detected());
        assert!(!Outcome::Survived.is_detected());

        assert!(Outcome::Timeout.is_valid());
        assert!(Outcome::OutOfMemory.is_valid());
        assert!(Outcome::Survived.is_valid());
        assert!(Outcome::NoCoverage.is_valid());
        assert!(!Outcome::CompileError.is_valid());
        assert!(!Outcome::Ignored.is_valid());
        assert!(!Outcome::Pending.is_valid());
    }

    /// A run stopped by the clock or the memory ceiling scores against the suite, not for it.
    ///
    /// Both outcomes mean the same thing about the tests: the mutant changed how the code behaves
    /// and no assertion said so. The suite noticed nothing — a budget did — so neither may be
    /// credited as a detection. Pinned here because the reasoning is inverted easily and the
    /// consequence is silent: reading a timeout as a kill turns a suite too slow for its own budget
    /// into a near-perfect score made of mutants nothing ever exercised, and reading a memory limit
    /// as a kill does the same for a mutant that merely allocated. Both remain in the denominator,
    /// because the mutant did run and something about it was observed.
    #[test]
    fn a_timeout_and_a_memory_limit_score_as_undetected_mutants() {
        assert_eq!(Outcome::Timeout.scoring(), Scoring::Undetected);
        assert_eq!(Outcome::OutOfMemory.scoring(), Scoring::Undetected);

        for outcome in [Outcome::Timeout, Outcome::OutOfMemory] {
            assert!(!outcome.is_detected(), "{outcome} must not be credited as a detection");
            assert!(outcome.is_valid(), "{outcome} must stay in the denominator");
        }

        // They are undetected in exactly the way a survivor is, and are not the excluded case that
        // a flake or an unviable mutant takes.
        assert_eq!(Outcome::Timeout.scoring(), Outcome::Survived.scoring());
        assert_eq!(Outcome::OutOfMemory.scoring(), Outcome::Survived.scoring());
        assert_ne!(Outcome::Timeout.scoring(), Outcome::Killed.scoring());
        assert_ne!(Outcome::OutOfMemory.scoring(), Outcome::Flaky.scoring());
    }

    /// A flake is neither a detection nor a gap in the tests, and scores as neither.
    ///
    /// Counting it as detected would let one unreliable test manufacture a kill against every
    /// mutant it was run against; counting it as valid but undetected would charge the score for a
    /// run that established nothing. Both directions are wrong, so it stays out of the fraction
    /// entirely, as an unviable mutant does.
    #[test]
    fn a_flake_neither_detects_nor_counts() {
        assert!(!Outcome::Flaky.is_detected());
        assert!(!Outcome::Flaky.is_valid());
    }

    /// The enumeration reporters walk must hold every outcome, exactly once.
    ///
    /// A breakdown built from [`Outcome::ALL`] prints one row per entry, so an outcome missing from
    /// the array is a category that never appears while its mutants stay in the total — the reader
    /// sees rows that do not add up and has no way to tell an unlisted category from a lost mutant.
    /// `position` is a `match`, so a new variant fails to compile here first; the index it is then
    /// given is out of the array's range until `ALL` carries it too, which is what makes this pin
    /// the array rather than merely the enum.
    #[test]
    fn every_outcome_appears_in_the_enumeration_exactly_once() {
        const fn position(outcome: Outcome) -> usize {
            match outcome {
                Outcome::Killed => 0,
                Outcome::Timeout => 1,
                Outcome::OutOfMemory => 2,
                Outcome::Survived => 3,
                Outcome::NoCoverage => 4,
                Outcome::Flaky => 5,
                Outcome::CompileError => 6,
                Outcome::Ignored => 7,
                Outcome::NotBuilt => 8,
                Outcome::Pending => 9,
            }
        }

        let mut seen = [false; Outcome::ALL.len()];

        for outcome in Outcome::ALL {
            let slot = &mut seen[position(outcome)];

            assert!(!*slot, "{outcome} is listed twice");
            *slot = true;
        }

        assert!(seen.iter().all(|listed| *listed), "an outcome is missing from `Outcome::ALL`");
    }

    /// The score is a fraction of at most one, so nothing detected can be outside the denominator.
    #[test]
    fn every_detected_outcome_is_also_counted() {
        for outcome in Outcome::ALL {
            assert!(!outcome.is_detected() || outcome.is_valid(), "{outcome}");
        }
    }

    #[test]
    fn every_outcome_has_the_short_name_used_in_reports() {
        // These strings are user-facing and serialized in text reports, so changing one is a
        // compatibility break rather than a cosmetic edit.
        assert_eq!(Outcome::Pending.as_str(), "pending");
        assert_eq!(Outcome::Killed.as_str(), "killed");
        assert_eq!(Outcome::Survived.as_str(), "survived");
        assert_eq!(Outcome::Timeout.as_str(), "timeout");
        assert_eq!(Outcome::OutOfMemory.as_str(), "outofmem");
        assert_eq!(Outcome::Flaky.as_str(), "flaky");
        assert_eq!(Outcome::CompileError.as_str(), "unviable");
        assert_eq!(Outcome::Ignored.as_str(), "ignored");
        assert_eq!(Outcome::NoCoverage.as_str(), "uncovered");
        assert_eq!(Outcome::NotBuilt.as_str(), "notbuilt");
    }
}
