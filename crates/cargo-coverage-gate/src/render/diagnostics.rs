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

    #[test]
    fn line_ranges_compress_only_adjacent_lines() {
        assert_eq!(format_line_ranges(&[]), "");
        assert_eq!(format_line_ranges(&[7]), "7");
        assert_eq!(format_line_ranges(&[1, 2, 3, 5, 7, 8]), "1-3, 5, 7-8");
    }
}
