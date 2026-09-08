// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Result;
use crate::error::error;
use crate::model::Outcome;

/// A verdict that may be suppressed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eligible {
    /// The mutant exceeded its time budget.
    ///
    /// Eligible by default, and still the second-best answer: a timeout that is cached keeps the
    /// mutant in the score and costs nothing on re-runs, whereas suppressing it removes it from the
    /// denominator. This is for sites that are permanently un-mutatable — a hand-written spin loop,
    /// a driver poll, a reactor — where the team wants that recorded where the next reader sees it.
    Timeout,

    /// The mutant's test run passed its memory ceiling.
    ///
    /// Eligible by default for the same reason as [`Self::Timeout`], and it is the same defect
    /// wearing different clothes: a mutant that turns a bounded loop into an unbounded one is
    /// stopped by whichever ceiling it reaches first, and which one that is depends on the machine
    /// rather than on the code. Making one suppressible and not the other would mean a directive
    /// that works on the maintainer's laptop and not in CI.
    OutOfMemory,

    /// The mutant did not compile.
    Unviable,
}

impl Eligible {
    /// Parses the `--eligible` list.
    ///
    /// `missed` and `survived` are named explicitly so the refusal can explain itself. Falling
    /// through to "unknown verdict" would read like a typo, and the person who typed it would try
    /// harder rather than reconsidering.
    pub fn parse(list: &str) -> Result<Vec<Self>> {
        let mut out = Vec::new();

        for entry in list.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
            match entry {
                "timeout" => out.push(Self::Timeout),
                "outofmem" | "oom" | "out-of-memory" => out.push(Self::OutOfMemory),
                "unviable" | "compile-error" => out.push(Self::Unviable),

                "missed" | "survived" | "survivor" => {
                    return Err(error!(
                        "`{entry}` is not eligible for `suppress`, and cannot be made eligible: a surviving mutant is a gap in the test suite, and suppressing it would remove that gap from the score rather than from the code"
                    )
                    .usage());
                }

                other => {
                    return Err(error!("unknown verdict `{other}`; --eligible accepts `timeout`, `outofmem` and `unviable`").usage());
                }
            }
        }

        out.sort_unstable();
        out.dedup();

        Ok(out)
    }

    /// The tag written into generated directives.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::OutOfMemory => "outofmem",
            Self::Unviable => "unviable",
        }
    }

    /// Returns the verdict this covers.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::Timeout => Outcome::Timeout,
            Self::OutOfMemory => Outcome::OutOfMemory,
            Self::Unviable => Outcome::CompileError,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_survivor_cannot_be_made_eligible() {
        // The single most important test in the module. If this ever passes, every mutation score
        // the tool reports becomes a number that can be improved by editing comments.
        let cause = Eligible::parse("missed").expect_err("survivors must be refused");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("gap in the test suite"), "{cause}");
    }

    #[test]
    fn eligible_lists_are_trimmed_sorted_and_deduplicated() {
        let parsed = Eligible::parse(" unviable, timeout, compile-error, timeout ").unwrap();

        assert_eq!(parsed, vec![Eligible::Timeout, Eligible::Unviable]);
    }

    /// Both ceilings a runaway mutant can hit are suppressible, under every spelling.
    ///
    /// `outofmem` is the canonical form because it is what [`Outcome::as_str`] prints and therefore
    /// what a reader sees in the report they are acting on; `oom` and `out-of-memory` are what they
    /// are liable to type instead.
    #[test]
    fn out_of_memory_is_eligible_under_each_of_its_spellings() {
        for spelling in ["outofmem", "oom", "out-of-memory"] {
            let parsed = Eligible::parse(spelling).unwrap_or_else(|_| panic!("`{spelling}` must parse"));

            assert_eq!(parsed, vec![Eligible::OutOfMemory], "{spelling}");
            assert_eq!(parsed[0].outcome(), Outcome::OutOfMemory, "{spelling}");
            assert_eq!(parsed[0].tag(), "outofmem", "every spelling writes one tag");
        }
    }

    /// Every eligible verdict maps to a distinct outcome and a distinct tag.
    ///
    /// Two variants sharing an outcome would make `plan`'s first-match lookup pick arbitrarily, and
    /// two sharing a tag would merge suppressions that were written for different reasons.
    #[test]
    fn each_eligible_verdict_has_its_own_outcome_and_tag() {
        let all = [Eligible::Timeout, Eligible::OutOfMemory, Eligible::Unviable];

        for (index, entry) in all.iter().enumerate() {
            for other in &all[index + 1..] {
                assert_ne!(entry.outcome(), other.outcome(), "{entry:?} and {other:?} share an outcome");
                assert_ne!(entry.tag(), other.tag(), "{entry:?} and {other:?} share a tag");
            }
        }
    }

    #[test]
    fn unknown_eligible_verdicts_are_errors() {
        let cause = Eligible::parse("killed").expect_err("unknown verdict should be rejected");

        assert!(cause.is_usage(), "{cause}");
        assert!(cause.to_string().contains("timeout"), "{cause}");
        assert!(
            cause.to_string().contains("outofmem"),
            "the message must name every accepted verdict: {cause}"
        );
    }
}
