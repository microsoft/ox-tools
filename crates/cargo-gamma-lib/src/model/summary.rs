// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use serde::{Deserialize, Serialize};

use super::mutant::Mutant;
use super::outcome::Outcome;

/// Aggregate counts and scores for a set of mutants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub killed: u32,
    pub survived: u32,
    pub timeout: u32,
    pub out_of_memory: u32,

    /// Mutants whose judging test failed with no mutant active as well as with one.
    pub flaky: u32,

    pub unviable: u32,
    pub ignored: u32,
    pub uncovered: u32,
    pub not_built: u32,
    pub pending: u32,
}

impl Summary {
    /// Tallies a set of mutants.
    #[must_use]
    pub fn of(mutants: &[Mutant]) -> Self {
        let mut summary = Self::default();

        for mutant in mutants {
            let counter = match mutant.outcome {
                Outcome::Killed => &mut summary.killed,
                Outcome::Survived => &mut summary.survived,
                Outcome::Timeout => &mut summary.timeout,
                Outcome::OutOfMemory => &mut summary.out_of_memory,
                Outcome::Flaky => &mut summary.flaky,
                Outcome::CompileError => &mut summary.unviable,
                Outcome::Ignored => &mut summary.ignored,
                Outcome::NoCoverage => &mut summary.uncovered,
                Outcome::NotBuilt => &mut summary.not_built,
                Outcome::Pending => &mut summary.pending,
            };

            *counter += 1;
        }

        summary
    }

    /// The count recorded for one outcome.
    ///
    /// A reporter that lists the population walks [`Outcome::ALL`] and reads its counts through
    /// here, rather than naming the fields it happens to remember. That is what makes a breakdown
    /// total: adding an outcome fails to compile in this `match`, where reaching for a field
    /// directly leaves the new category unprinted and the rows no longer summing to the run.
    #[must_use]
    pub const fn count(self, outcome: Outcome) -> u32 {
        match outcome {
            Outcome::Killed => self.killed,
            Outcome::Survived => self.survived,
            Outcome::Timeout => self.timeout,
            Outcome::OutOfMemory => self.out_of_memory,
            Outcome::Flaky => self.flaky,
            Outcome::CompileError => self.unviable,
            Outcome::Ignored => self.ignored,
            Outcome::NoCoverage => self.uncovered,
            Outcome::NotBuilt => self.not_built,
            Outcome::Pending => self.pending,
        }
    }

    /// Number of mutants that count toward the score.
    #[must_use]
    pub const fn valid(self) -> u32 {
        self.killed + self.survived + self.timeout + self.out_of_memory + self.uncovered
    }

    /// Number of mutants a failing test assertion detected.
    #[must_use]
    pub const fn detected(self) -> u32 {
        self.killed
    }

    /// The mutation score: detected over valid, as a percentage.
    ///
    /// Uncovered mutants are in the denominator and never in the numerator, so they count against
    /// the score exactly as survivors do. That is deliberate, and not in tension with reporting
    /// them separately: the two facts answer different questions. *Which* mutants went undetected,
    /// and why, is a diagnosis — and "no test links this code" is a different problem from "the
    /// tests that run it did not notice", so the two are never merged in a report. *How much* of
    /// the code is defended is a single number, and code no test reaches is undefended.
    ///
    /// Timeout and out-of-memory verdicts remain in the denominator because no assertion rejected
    /// them. Reports export those outcomes as schema `Survived` with a reason preserving the
    /// distinction, so the standard report UI computes the same score.
    #[must_use]
    pub fn score(self) -> f64 {
        let valid = self.valid();

        if valid == 0 {
            return 100.0;
        }

        f64::from(self.detected()) * 100.0 / f64::from(valid)
    }

    /// The mutation score, or `None` when nothing was scored.
    ///
    /// [`Self::score`] answers 100% for an empty population, which is the only sensible thing to
    /// print — a run that caught everything it tested did catch everything it tested — and a
    /// catastrophic thing to hand a threshold. `--min-score 100` against an empty population is not
    /// a gate that passed; it is a gate that never ran, and the two have to be distinguishable at
    /// the one place the difference decides an exit code.
    #[must_use]
    pub fn scored(self) -> Option<f64> {
        (self.valid() > 0).then(|| self.score())
    }
}
