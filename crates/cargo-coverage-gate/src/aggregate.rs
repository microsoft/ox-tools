// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-crate line-coverage aggregation.
//!
//! Sums `lines.count` and `lines.covered` across every file attributed
//! to a crate. The result is order-independent because integer addition
//! is commutative and associative, so two runs over the same data
//! always produce byte-identical counters.

use crate::llvm_cov::FileEntry;

/// Aggregated line totals for a single crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LineTotals {
    /// Total executable lines.
    pub(crate) count: u32,
    /// Executable lines that ran at least once.
    pub(crate) covered: u32,
}

impl LineTotals {
    /// Returns the coverage percentage, or `None` when `count == 0`
    /// (no measurable lines).
    pub(crate) fn percent(self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            // u32 → f64 is lossless: 32 bits fits inside the 53-bit
            // f64 mantissa with room to spare.
            let pct = 100.0 * f64::from(self.covered) / f64::from(self.count);
            Some(pct)
        }
    }
}

/// Sum line counters across `files`.
///
/// Order-independent: integer addition is commutative and associative,
/// so two calls with the same files in any permutation produce the same
/// totals.
pub(crate) fn aggregate(files: &[&FileEntry]) -> LineTotals {
    let mut totals = LineTotals::default();
    for f in files {
        totals.count += f.summary.lines.count;
        totals.covered += f.summary.lines.covered;
    }
    totals
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::llvm_cov::{LineCounters, SummaryBlock};

    fn entry(path: &str, count: u32, covered: u32) -> FileEntry {
        FileEntry {
            filename: PathBuf::from(path),
            summary: SummaryBlock {
                lines: LineCounters { count, covered },
            },
        }
    }

    #[test]
    fn sums_counters() {
        let a = entry("/repo/a.rs", 10, 5);
        let b = entry("/repo/b.rs", 20, 18);
        let files = [&a, &b];
        let totals = aggregate(&files);
        assert_eq!(totals.count, 30);
        assert_eq!(totals.covered, 23);
    }

    #[test]
    fn percent_handles_zero_lines() {
        let totals = LineTotals { count: 0, covered: 0 };
        assert!(totals.percent().is_none());
    }

    #[test]
    fn percent_computes_correctly() {
        let totals = LineTotals { count: 100, covered: 82 };
        let pct = totals.percent().expect("non-empty totals");
        assert!((pct - 82.0).abs() < f64::EPSILON);
    }

    #[test]
    fn deterministic_across_input_order() {
        // Sum should not depend on input ordering.
        let a = entry("/repo/aaa.rs", 7, 3);
        let b = entry("/repo/bbb.rs", 11, 9);
        let c = entry("/repo/ccc.rs", 13, 12);
        let abc = aggregate(&[&a, &b, &c]);
        let cba = aggregate(&[&c, &b, &a]);
        assert_eq!(abc, cba);
    }

    #[test]
    fn empty_input_is_zero_totals() {
        let totals = aggregate(&[]);
        assert_eq!(totals, LineTotals::default());
    }
}
