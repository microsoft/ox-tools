// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-width plain-text verdict table.

use std::io;

use crate::render::{
    count_failures, count_no_data, format_delta, format_lines, format_source, format_status_text,
    format_threshold,
};
use crate::verdict::Report;

const HEADERS: [&str; 6] = [
    "Crate",
    "Lines",
    "Threshold",
    "Δ vs threshold",
    "Status",
    "Source",
];

/// Render `report` as a plain-text table to `out`.
pub(crate) fn render(out: &mut dyn io::Write, report: &Report) -> io::Result<()> {
    let rows: Vec<[String; 6]> = report
        .outcomes
        .iter()
        .map(|o| {
            [
                o.name.clone(),
                format_lines(o),
                format_threshold(o),
                format_delta(o),
                format_status_text(o.status).to_owned(),
                format_source(o.threshold.source).to_owned(),
            ]
        })
        .collect();

    // Column widths: header vs widest row, whichever is wider.
    let mut widths = [0_usize; 6];
    for (i, h) in HEADERS.iter().enumerate() {
        widths[i] = h.chars().count();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    writeln!(out, "ox coverage-gate")?;
    writeln!(out)?;
    write_row(out, &HEADERS.map(str::to_owned), &widths)?;
    write_separator(out, &widths)?;
    for row in &rows {
        write_row(out, row, &widths)?;
    }
    write_separator(out, &widths)?;

    let failures = count_failures(&report.outcomes);
    let no_data = count_no_data(&report.outcomes);
    match (failures, no_data) {
        (0, 0) => writeln!(out, "Result: all crates meet their threshold.")?,
        (n, 0) => writeln!(out, "Result: {n} crate(s) below threshold.")?,
        (0, n) => writeln!(
            out,
            "Result: {n} crate(s) with no attributed coverage data."
        )?,
        (f, d) => writeln!(
            out,
            "Result: {f} crate(s) below threshold, {d} with no attributed data."
        )?,
    }
    Ok(())
}

fn write_row(out: &mut dyn io::Write, row: &[String; 6], widths: &[usize; 6]) -> io::Result<()> {
    writeln!(
        out,
        "  {:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:<w4$}  {:<w5$}",
        row[0],
        row[1],
        row[2],
        row[3],
        row[4],
        row[5],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
    )
}

fn write_separator(out: &mut dyn io::Write, widths: &[usize; 6]) -> io::Result<()> {
    let bars: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    writeln!(
        out,
        "  {}  {}  {}  {}  {}  {}",
        bars[0], bars[1], bars[2], bars[3], bars[4], bars[5]
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

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
        let _ = PathBuf::new();
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
    fn renders_pass_table() {
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
        assert!(s.contains("ox coverage-gate"));
        assert!(s.contains("alpha"));
        assert!(s.contains("95.0%"));
        assert!(s.contains("80.0%"));
        assert!(s.contains("+15.0pp"));
        assert!(s.contains("OK"));
        assert!(s.contains("crate"));
        assert!(s.contains("all crates meet their threshold"));
    }

    #[test]
    fn renders_fail_with_negative_delta() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 100, 95, 80.0, ThresholdSource::Crate, Status::Ok),
                outcome("beta", 100, 60, 80.0, ThresholdSource::Workspace, Status::Fail),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("beta"));
        assert!(s.contains("-20.0pp"));
        assert!(s.contains("FAIL"));
        assert!(s.contains("workspace"));
        assert!(s.contains("1 crate(s) below threshold"));
    }

    #[test]
    fn renders_no_data_row_and_summary() {
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
        assert!(s.contains("(no data)"));
        assert!(s.contains("NO DATA"));
        assert!(s.contains("default"));
        assert!(s.contains("—"));
        assert!(s.contains("no attributed coverage data"));
    }

    #[test]
    fn renders_combined_fail_and_no_data_summary() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 100, 60, 80.0, ThresholdSource::Crate, Status::Fail),
                outcome("beta", 0, 0, 100.0, ThresholdSource::Default, Status::NoData),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("1 crate(s) below threshold, 1 with no attributed data"));
    }

    #[test]
    fn output_is_deterministic() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 10, 10, 50.0, ThresholdSource::Crate, Status::Ok),
                outcome("beta", 10, 5, 80.0, ThresholdSource::Workspace, Status::Fail),
            ],
            unattributed: 0,
        };
        assert_eq!(render_to_string(&report), render_to_string(&report));
    }
}
