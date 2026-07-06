// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-width plain-text verdict table.

use std::io;

use crate::render::{files, format_delta, format_lines, format_source, format_status_text, format_threshold, result_summary};
use crate::verdict::Report;

const HEADERS: [&str; 6] = ["Package", "Lines", "Threshold", "Δ vs threshold", "Status", "Source"];

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

    writeln!(out, "coverage-gate")?;
    writeln!(out)?;
    write_row(out, &HEADERS.map(str::to_owned), &widths)?;
    write_separator(out, &widths)?;
    for row in &rows {
        write_row(out, row, &widths)?;
    }
    write_separator(out, &widths)?;

    writeln!(out, "Result: {}", result_summary(&report.outcomes))?;
    if report.unattributed > 0 {
        writeln!(
            out,
            "Note: {} had paths outside any workspace member and were not attributed.",
            files(report.unattributed),
        )?;
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
    fn renders_pass_table() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("coverage-gate"));
        assert!(s.contains("alpha"));
        assert!(s.contains("95.0%"));
        assert!(s.contains("80.0%"));
        assert!(s.contains("+15.0pp"));
        assert!(s.contains("OK"));
        assert!(s.contains("package"));
        assert!(s.contains("all packages meet their threshold"));
    }

    #[test]
    fn renders_two_separator_rules() {
        // The text renderer wraps the data rows with ─-bar separators
        // above and below; both must appear.
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        let bar_lines = s.lines().filter(|l| l.contains('─')).count();
        assert_eq!(bar_lines, 2, "expected two ─ separator lines, got:\n{s}");
    }

    #[test]
    fn renders_fail_with_negative_delta() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok),
                outcome("beta", 100, 60, 80.0, ThresholdSource::Workspace, Status::Fail),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("beta"));
        assert!(s.contains("-20.0pp"));
        assert!(s.contains("FAIL"));
        assert!(s.contains("workspace"));
        assert!(s.contains("1 package below threshold"));
    }

    #[test]
    fn renders_no_data_row_and_summary() {
        let report = Report {
            outcomes: vec![outcome("gamma", 0, 0, 100.0, ThresholdSource::Default, Status::NoData)],
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
                outcome("alpha", 100, 60, 80.0, ThresholdSource::Package, Status::Fail),
                outcome("beta", 0, 0, 100.0, ThresholdSource::Default, Status::NoData),
            ],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("1 package below threshold, 1 package with no attributed coverage data"));
    }

    #[test]
    fn renders_no_coverable_lines_row() {
        // A package asserting (and having) no coverable lines renders as a
        // passing EMPTY row with dashes in the numeric columns.
        let report = Report {
            outcomes: vec![outcome("alpha", 0, 0, 0.0, ThresholdSource::Package, Status::NoCoverableLines)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("EMPTY"));
        assert!(s.contains("(no lines)"));
        assert!(s.contains("all packages meet their threshold"));
    }

    #[test]
    fn renders_unexpected_coverable_lines_row_and_summary() {
        let report = Report {
            outcomes: vec![outcome(
                "alpha",
                7,
                0,
                0.0,
                ThresholdSource::Package,
                Status::UnexpectedCoverableLines,
            )],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(s.contains("NOT EMPTY"));
        assert!(s.contains("7 lines"));
        assert!(s.contains("1 package with unexpected coverable lines"));
    }

    #[test]
    fn output_is_deterministic() {
        let report = Report {
            outcomes: vec![
                outcome("alpha", 10, 10, 50.0, ThresholdSource::Package, Status::Ok),
                outcome("beta", 10, 5, 80.0, ThresholdSource::Workspace, Status::Fail),
            ],
            unattributed: 0,
        };
        assert_eq!(render_to_string(&report), render_to_string(&report));
    }

    #[test]
    fn renders_unattributed_warning_when_present() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 3,
        };
        let s = render_to_string(&report);
        assert!(s.contains("Note:"), "expected aggregated unattributed warning, got:\n{s}");
        assert!(s.contains("3 files"), "expected warning to include count, got:\n{s}");
    }

    #[test]
    fn omits_unattributed_warning_when_zero() {
        let report = Report {
            outcomes: vec![outcome("alpha", 100, 95, 80.0, ThresholdSource::Package, Status::Ok)],
            unattributed: 0,
        };
        let s = render_to_string(&report);
        assert!(!s.contains("Note:"), "did not expect unattributed warning, got:\n{s}");
    }

    /// Writer that errors the first time a write contains `needle`,
    /// used to exercise the `?` error-propagation branch on the
    /// multi-line unattributed-note `writeln!`.
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
