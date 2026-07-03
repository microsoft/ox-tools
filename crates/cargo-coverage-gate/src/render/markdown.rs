// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub-flavored Markdown verdict table.
//!
//! Output is suitable for `$GITHUB_STEP_SUMMARY` (GitHub Actions),
//! `##vso[task.uploadsummary]` (Azure DevOps), and any other CI
//! integration that renders GFM-style tables. The format matches
//! section 6.5 of the design doc.

use std::io;

use crate::render::{files, format_delta, format_lines, format_source, format_status_markdown, format_threshold, result_summary};
use crate::verdict::Report;

/// Render `report` as a GFM table to `out`.
pub(crate) fn render(out: &mut dyn io::Write, report: &Report) -> io::Result<()> {
    writeln!(out, "### coverage-gate")?;
    writeln!(out)?;
    writeln!(out, "| Package | Lines | Threshold | Δ vs threshold | Status | Source |")?;
    writeln!(out, "|-------|------:|----------:|---------------:|:------:|:-------|")?;
    for o in &report.outcomes {
        writeln!(
            out,
            "| {name} | {lines} | {threshold} | {delta} | {status} | {source} |",
            name = o.name,
            lines = format_lines(o),
            threshold = format_threshold(o),
            delta = format_delta(o),
            status = format_status_markdown(o.status),
            source = format_source(o.threshold.source),
        )?;
    }
    writeln!(out)?;

    writeln!(out, "**Result:** {}", result_summary(&report.outcomes))?;
    if report.unattributed > 0 {
        writeln!(
            out,
            "_Note: {} had paths outside any workspace member and were not attributed._",
            files(report.unattributed),
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::aggregate::LineTotals;
    use crate::threshold::{Threshold, ThresholdSource};
    use crate::verdict::{PackageOutcome, Status};

    fn outcome(name: &str, count: u32, covered: u32, threshold: f64, source: ThresholdSource, status: Status) -> PackageOutcome {
        PackageOutcome {
            name: name.to_owned(),
            threshold: Threshold {
                min_lines_percent: threshold,
                source,
            },
            totals: LineTotals { count, covered },
            status,
        }
    }

    fn render_to_string(report: &Report) -> String {
        let mut buf: Vec<u8> = Vec::new();
        render(&mut buf, report).expect("render to Vec never fails");
        String::from_utf8(buf).expect("renderer emits UTF-8")
    }

    #[test]
    fn renders_gfm_table_header() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.starts_with("### coverage-gate"));
        assert!(s.contains("| Package | Lines |"));
        assert!(s.contains("|-------|------:|"));
    }

    #[test]
    fn uses_check_emoji_for_pass_and_cross_for_fail() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok),
                outcome("beta", 100, 50, 80.0, ThresholdSource::Workspace, Status::Fail),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| ✅ |"));
        assert!(s.contains("| ❌ |"));
        assert!(s.contains("1 package below threshold"));
    }

    #[test]
    fn renders_no_data_with_warning_emoji() {
        let report = Report {
            outcomes: vec![outcome("gamma", 0, 0, 100.0, ThresholdSource::Default, Status::NoData)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| (no data) |"));
        assert!(s.contains("| 💥 |"));
        assert!(s.contains("no attributed coverage data"));
    }

    #[test]
    fn renders_unattributed_warning_when_present() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 2,
        };
        let s = render_to_string(&report);
        assert!(s.contains("_Note:"), "expected italicized unattributed warning, got:\n{s}");
        assert!(s.contains("2 files"));
    }

    #[test]
    fn omits_unattributed_warning_when_zero() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(!s.contains("_Note:"));
    }

    #[test]
    fn renders_combined_fail_and_no_data_summary() {
        // Exercises the multi-clause summary: a report with both a
        // below-threshold package and a no-data package.
        let report = Report {
            outcomes: vec![
                outcome("beta", 100, 50, 80.0, ThresholdSource::Workspace, Status::Fail),
                outcome("gamma", 0, 0, 100.0, ThresholdSource::Default, Status::NoData),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(
            s.contains("below threshold") && s.contains("no attributed coverage data"),
            "expected combined summary, got:\n{s}"
        );
    }

    #[test]
    fn renders_no_coverable_lines_with_dash_emoji() {
        let report = Report {
            outcomes: vec![outcome("alpha", 0, 0, 0.0, ThresholdSource::Package, Status::NoCoverableLines)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| ➖ |"));
        assert!(s.contains("| (no lines) |"));
        assert!(s.contains("all packages meet their threshold"));
    }

    #[test]
    fn renders_unexpected_coverable_lines_with_cross_and_summary() {
        let report = Report {
            outcomes: vec![outcome(
                "alpha",
                1,
                0,
                0.0,
                ThresholdSource::Package,
                Status::UnexpectedCoverableLines,
            )],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| ❌ |"));
        assert!(s.contains("| 1 line |"));
        assert!(s.contains("1 package with unexpected coverable lines"));
    }

    /// Writer that returns an error the first time a write contains
    /// `needle`, used to exercise the `?` error-propagation branches on
    /// the multi-line `writeln!` calls. Earlier writes (which don't
    /// contain the needle) succeed, so the failure lands on a specific
    /// statement rather than the first write.
    struct FailOnNeedle {
        needle: &'static [u8],
    }

    impl io::Write for FailOnNeedle {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.windows(self.needle.len()).any(|w| w == self.needle) {
                return Err(io::Error::other("injected write failure"));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn propagates_error_from_per_row_write() {
        // The package name "ROWFAIL" is emitted only by the per-row
        // `writeln!`, so failing on it covers that statement's `?`.
        let report = Report {
            outcomes: vec![outcome("ROWFAIL", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let mut w = FailOnNeedle { needle: b"ROWFAIL" };
        assert!(render(&mut w, &report).is_err());
    }

    #[test]
    fn propagates_error_from_unattributed_note_write() {
        // "had paths" appears only in the unattributed-note `writeln!`,
        // so failing on it covers that statement's `?` after the rest of
        // the table has been written successfully.
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 2,
        };
        let mut w = FailOnNeedle { needle: b"had paths" };
        assert!(render(&mut w, &report).is_err());
    }
}
