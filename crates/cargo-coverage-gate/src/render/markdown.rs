// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub-flavored Markdown verdict table.
//!
//! Output is suitable for `$GITHUB_STEP_SUMMARY` (GitHub Actions),
//! `##vso[task.uploadsummary]` (Azure DevOps), and any other CI
//! integration that renders GFM-style tables. The format matches
//! §6.5 of the design doc.

use std::io;

use crate::render::{
    count_failures, count_no_data, format_delta, format_lines, format_source,
    format_status_markdown, format_threshold,
};
use crate::verdict::Report;

/// Render `report` as a GFM table to `out`.
pub(crate) fn render(out: &mut dyn io::Write, report: &Report) -> io::Result<()> {
    writeln!(out, "### ox coverage-gate")?;
    writeln!(out)?;
    writeln!(
        out,
        "| Crate | Lines | Threshold | Δ vs threshold | Status | Source |"
    )?;
    writeln!(
        out,
        "|-------|------:|----------:|---------------:|:------:|:-------|"
    )?;
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
        (0, 0) => writeln!(out, "**Result:** all crates meet their threshold.")?,
        (n, 0) => writeln!(out, "**Result:** {n} crate(s) below threshold.")?,
        (0, n) => writeln!(
            out,
            "**Result:** {n} crate(s) with no attributed coverage data."
        )?,
        (f, d) => writeln!(
            out,
            "**Result:** {f} crate(s) below threshold, {d} with no attributed data."
        )?,
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::aggregate::LineTotals;
    use crate::threshold::{Threshold, ThresholdSource};
    use crate::verdict::{CrateOutcome, Status};

    fn outcome(
        name: &str,
        count: u64,
        covered: u64,
        threshold: f64,
        source: ThresholdSource,
        status: Status,
    ) -> CrateOutcome {
        CrateOutcome {
            name: name.to_owned(),
            threshold: Threshold {
                min_lines: threshold,
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
            outcomes: vec![outcome(
                "alpha",
                100,
                95,
                80.0,
                ThresholdSource::Crate,
                Status::Ok,
            )],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.starts_with("### ox coverage-gate"));
        assert!(s.contains("| Crate | Lines |"));
        assert!(s.contains("|-------|------:|"));
    }

    #[test]
    fn uses_check_emoji_for_pass_and_cross_for_fail() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 100, 95, 80.0, ThresholdSource::Crate, Status::Ok),
                outcome(
                    "beta",
                    100,
                    50,
                    80.0,
                    ThresholdSource::Workspace,
                    Status::Fail,
                ),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| ✅ |"));
        assert!(s.contains("| ❌ |"));
        assert!(s.contains("1 crate(s) below threshold"));
    }

    #[test]
    fn renders_no_data_with_warning_emoji() {
        let report = Report {
            outcomes: vec![outcome(
                "gamma",
                0,
                0,
                100.0,
                ThresholdSource::Default,
                Status::NoData,
            )],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("| (no data) |"));
        assert!(s.contains("| ⚠️ |"));
        assert!(s.contains("no attributed coverage data"));
    }
}
