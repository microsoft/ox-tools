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
    pub(crate) count: u64,
    /// Executable lines that ran at least once.
    pub(crate) covered: u64,
}

impl LineTotals {
    /// Returns the coverage percentage, or `None` when `count == 0`
    /// (no measurable lines).
    pub(crate) fn percent(self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            // Cast is intentional: u64 → f64 is lossy only beyond
            // ~2^53, which we will never see for line counts.
            #[expect(clippy::cast_precision_loss, reason = "line counts will never exceed f64 mantissa width")]
            let pct = 100.0 * self.covered as f64 / self.count as f64;
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

    fn entry(path: &str, count: u64, covered: u64) -> FileEntry {
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
