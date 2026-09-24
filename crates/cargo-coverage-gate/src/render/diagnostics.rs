// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared failure-detail formatting for terminal and Markdown output.

use crate::verdict::{PackageOutcome, Status};

/// Maximum source locations shown for one package.
pub(crate) const MAX_DIAGNOSTIC_LINES: usize = 100;

pub(crate) fn failure_detail(outcome: &PackageOutcome) -> Option<String> {
    match outcome.status {
        Status::Fail => {
            let uncovered = outcome.totals.count.saturating_sub(outcome.totals.covered);
            Some(format!(
                "{}/{} lines covered; {} uncovered.",
                outcome.totals.covered, outcome.totals.count, uncovered
            ))
        }
        Status::NoData => Some("no coverage records were attributed to this package.".to_owned()),
        Status::UnexpectedCoverableLines => Some(format!(
            "expected no coverable lines; found {}.",
            super::plural(outcome.totals.count as usize, "line", "lines")
        )),
        Status::Ok | Status::NoCoverableLines => None,
    }
}

pub(crate) fn diagnostic_line_count(outcome: &PackageOutcome) -> usize {
    outcome.diagnostics.iter().map(|diagnostic| diagnostic.lines.len()).sum()
}

pub(crate) fn displayed_diagnostics(outcome: &PackageOutcome) -> Vec<(&std::path::Path, &[u32])> {
    let mut remaining = MAX_DIAGNOSTIC_LINES;
    let mut displayed_diagnostics = Vec::new();

    for diagnostic in &outcome.diagnostics {
        if remaining == 0 {
            break;
        }

        let displayed = diagnostic.lines.len().min(remaining);
        if displayed == 0 {
            continue;
        }

        remaining -= displayed;
        displayed_diagnostics.push((diagnostic.path.as_path(), &diagnostic.lines[..displayed]));
    }

    displayed_diagnostics
}

pub(crate) fn format_line_ranges(lines: &[u32]) -> String {
    let mut ranges = Vec::new();
    let Some((&first, rest)) = lines.split_first() else {
        return String::new();
    };
    let mut start = first;
    let mut end = first;
    for &line in rest {
        if line != end.saturating_add(1) {
            push_line_range(&mut ranges, start, end);
            start = line;
        }
        end = line;
    }
    push_line_range(&mut ranges, start, end);
    ranges.join(", ")
}

fn push_line_range(ranges: &mut Vec<String>, start: u32, end: u32) {
    if start == end {
        ranges.push(start.to_string());
    } else {
        ranges.push(format!("{start}-{end}"));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::aggregate::LineTotals;
    use crate::threshold::{Threshold, ThresholdSource};
    use crate::verdict::LineDiagnostic;

    fn outcome(status: Status) -> PackageOutcome {
        PackageOutcome {
            name: "alpha".to_owned(),
            threshold: Threshold {
                min_lines_percent: 80.0,
                source: ThresholdSource::Package,
            },
            totals: LineTotals { count: 10, covered: 10 },
            status,
            diagnostics: Vec::<LineDiagnostic>::new(),
        }
    }

    #[test]
    fn line_ranges_compress_only_adjacent_lines() {
        assert_eq!(format_line_ranges(&[]), "");
        assert_eq!(format_line_ranges(&[7]), "7");
        assert_eq!(format_line_ranges(&[1, 2, 3, 5, 7, 8]), "1-3, 5, 7-8");
    }

    #[test]
    fn passing_statuses_have_no_failure_detail() {
        assert_eq!(failure_detail(&outcome(Status::Ok)), None);
        assert_eq!(failure_detail(&outcome(Status::NoCoverableLines)), None);
    }

    #[test]
    fn displayed_diagnostics_stop_exactly_at_the_limit() {
        let mut outcome = outcome(Status::Fail);
        outcome.diagnostics = vec![
            LineDiagnostic {
                path: "first.rs".into(),
                lines: (1..=100).collect(),
            },
            LineDiagnostic {
                path: "second.rs".into(),
                lines: vec![101],
            },
        ];
        let displayed = displayed_diagnostics(&outcome);
        assert_eq!(displayed.len(), 1);
        assert_eq!(displayed[0].0, std::path::Path::new("first.rs"));
        assert_eq!(displayed[0].1, (1..=100).collect::<Vec<_>>());
    }

    #[test]
    fn empty_diagnostics_do_not_hide_later_diagnostics() {
        let mut outcome = outcome(Status::Fail);
        outcome.diagnostics = vec![
            LineDiagnostic {
                path: "empty.rs".into(),
                lines: Vec::new(),
            },
            LineDiagnostic {
                path: "visible.rs".into(),
                lines: vec![7],
            },
        ];

        let displayed = displayed_diagnostics(&outcome);

        assert_eq!(displayed.len(), 1);
        assert_eq!(displayed[0].0, std::path::Path::new("visible.rs"));
        assert_eq!(displayed[0].1, [7]);
    }
}
