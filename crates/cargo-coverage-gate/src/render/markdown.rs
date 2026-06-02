// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub-flavored Markdown verdict table.
//!
//! Output is suitable for `$GITHUB_STEP_SUMMARY` (GitHub Actions),
//! `##vso[task.uploadsummary]` (Azure DevOps), and any other CI
//! integration that renders GFM-style tables.

use std::io;

use crate::render::{
    count_failures, count_no_data, files, format_delta, format_lines, format_source, format_status_markdown, format_threshold, packages,
};
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

    let failures = count_failures(&report.outcomes);
    let no_data = count_no_data(&report.outcomes);
    match (failures, no_data) {
        (0, 0) => writeln!(out, "**Result:** all packages meet their threshold.")?,
        (n, 0) => writeln!(out, "**Result:** {} below threshold.", packages(n))?,
        (0, n) => writeln!(out, "**Result:** {} with no attributed coverage data.", packages(n))?,
        (f, d) => writeln!(out, "**Result:** {} below threshold, {d} with no attributed data.", packages(f))?,
    }
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
}
