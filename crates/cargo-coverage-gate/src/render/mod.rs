// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verdict-table renderers.
//!
//! Two output flavors, sharing the same underlying [`Report`]:
//!
//! - [`text`] — fixed-width plain text suitable for terminal output.
//! - [`markdown`] — GitHub-flavored Markdown suitable for CI step
//!   summary files.
//!
//! Both renderers are deterministic: the same `Report` always
//! produces byte-identical output.
//!
//! [`Report`]: crate::verdict::Report

mod diagnostics;
pub(crate) mod markdown;
pub(crate) mod text;

pub(crate) use diagnostics::{MAX_DIAGNOSTIC_LINES, diagnostic_line_count, failure_detail, format_line_ranges};

use crate::threshold::ThresholdSource;
use crate::verdict::{PackageOutcome, Status};

const DELTA_DISPLAY_PRECISION: f64 = 0.1;
const DELTA_BOUNDARY_TOLERANCE: f64 = 1e-12;

fn is_sub_precision_delta(delta: f64) -> bool {
    delta.abs() < DELTA_DISPLAY_PRECISION - DELTA_BOUNDARY_TOLERANCE
}

/// Human-readable text for the `Lines` column.
fn format_lines(outcome: &PackageOutcome) -> String {
    match outcome.status {
        // A package asserting no coverable lines has no percentage to
        // report; show the line count instead so the failure case
        // ("found N lines") is legible.
        Status::NoCoverableLines => "(no lines)".to_owned(),
        Status::UnexpectedCoverableLines => {
            let n = outcome.totals.count;
            if n == 1 { "1 line".to_owned() } else { format!("{n} lines") }
        }
        _ if outcome.totals.count == 0 => "(no data)".to_owned(),
        _ => format_percent_for_display(outcome),
    }
}

/// Format a coverage percentage rounded down to one decimal place.
fn format_percent_for_display(outcome: &PackageOutcome) -> String {
    let tenths = u64::from(outcome.totals.covered) * 1_000 / u64::from(outcome.totals.count);
    format!("{}.{:01}%", tenths / 10, tenths % 10)
}

/// Human-readable text for the `Threshold` column.
fn format_threshold(outcome: &PackageOutcome) -> String {
    match outcome.status {
        // The "threshold" for an assertion package is the declared
        // expectation, not a percentage.
        Status::NoCoverableLines | Status::UnexpectedCoverableLines => "(no lines)".to_owned(),
        _ => format!("{:.1}%", outcome.threshold.min_lines_percent),
    }
}

/// Human-readable text for the `Δ vs threshold` column.
fn format_delta(outcome: &PackageOutcome) -> String {
    if matches!(outcome.status, Status::NoCoverableLines | Status::UnexpectedCoverableLines) {
        return "—".to_owned();
    }
    let Some(pct) = outcome.percent() else {
        return "—".to_owned();
    };
    let delta = pct - outcome.threshold.min_lines_percent;
    // A non-zero sub-precision margin retains its direction so the rendered row
    // cannot obscure why the unrounded comparison passed or failed. Keep values
    // within floating-point noise of the boundary in the ordinary 0.1pp bucket.
    let is_sub_precision = is_sub_precision_delta(delta);
    if delta > 0.0 && is_sub_precision {
        return "+<0.1pp".to_owned();
    }
    if delta < 0.0 && is_sub_precision {
        return "-<0.1pp".to_owned();
    }

    // Ordinary values use the displayed precision of one decimal place. Once
    // `delta` clears the sub-precision guard above, its rounded tenth can
    // never land back on zero (rounding a value whose magnitude is already
    // >= `DELTA_DISPLAY_PRECISION - DELTA_BOUNDARY_TOLERANCE` cannot produce
    // `0.0`), so a `rounded < 0.0` branch here is unreachable and would be an
    // equivalent mutant against `rounded <= 0.0`. Checking `is_sign_positive`
    // after handling zero explicitly avoids that redundant comparison.
    let rounded = (delta * 10.0).round() / 10.0;
    if rounded == 0.0 {
        return "0.0pp".to_owned();
    }
    if rounded.is_sign_positive() {
        format!("+{rounded:.1}pp")
    } else {
        format!("{rounded:.1}pp")
    }
}

/// Status text for the plain-text renderer.
fn format_status_text(status: Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::Fail => "FAIL",
        Status::NoData => "NO DATA",
        Status::NoCoverableLines => "EMPTY",
        Status::UnexpectedCoverableLines => "NOT EMPTY",
    }
}

/// Status text for the Markdown renderer (uses emoji for visual scan).
fn format_status_markdown(status: Status) -> &'static str {
    match status {
        Status::Ok => "✅",
        Status::Fail | Status::UnexpectedCoverableLines => "❌",
        Status::NoData => "💥",
        Status::NoCoverableLines => "➖",
    }
}

/// Source-column label.
fn format_source(source: ThresholdSource) -> &'static str {
    source.as_str()
}

/// Number of packages below threshold (`Fail` only — `NoData` and
/// `UnexpectedCoverableLines` are summarized separately).
fn count_failures(outcomes: &[PackageOutcome]) -> usize {
    outcomes.iter().filter(|o| o.status == Status::Fail).count()
}

/// Number of packages with no attributed coverage data.
fn count_no_data(outcomes: &[PackageOutcome]) -> usize {
    outcomes.iter().filter(|o| o.status == Status::NoData).count()
}

/// Number of packages that declared `expect-no-coverable-lines` but had
/// coverable lines.
fn count_unexpected_coverable_lines(outcomes: &[PackageOutcome]) -> usize {
    outcomes.iter().filter(|o| o.status == Status::UnexpectedCoverableLines).count()
}

/// Build the result sentence (without the renderer-specific `Result:`
/// prefix) summarizing the failing categories. Returns
/// `"all packages meet their threshold."` when nothing failed; otherwise
/// joins one clause per non-empty failure category.
fn result_summary(outcomes: &[PackageOutcome]) -> String {
    let mut clauses: Vec<String> = Vec::new();
    let failures = count_failures(outcomes);
    if failures > 0 {
        clauses.push(format!("{} below threshold", packages(failures)));
    }
    let unexpected = count_unexpected_coverable_lines(outcomes);
    if unexpected > 0 {
        clauses.push(format!("{} with unexpected coverable lines", packages(unexpected)));
    }
    let no_data = count_no_data(outcomes);
    if no_data > 0 {
        clauses.push(format!("{} with no attributed coverage data", packages(no_data)));
    }
    if clauses.is_empty() {
        "all packages meet their threshold.".to_owned()
    } else {
        format!("{}.", clauses.join(", "))
    }
}

/// Format `n` followed by `singular` when `n == 1`, else `plural`.
fn plural(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// "1 package" / "N packages".
fn packages(n: usize) -> String {
    plural(n, "package", "packages")
}

/// "1 file" / "N files".
fn files(n: usize) -> String {
    plural(n, "file", "files")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::aggregate::LineTotals;
    use crate::threshold::Threshold;

    fn outcome(count: u32, covered: u32, threshold: f64) -> PackageOutcome {
        PackageOutcome {
            name: "x".to_owned(),
            threshold: Threshold {
                min_lines_percent: threshold,
                source: ThresholdSource::Package,
            },
            totals: LineTotals { count, covered },
            status: Status::Ok,
            diagnostics: Vec::new(),
        }
    }

    fn outcome_with_status(count: u32, covered: u32, status: Status) -> PackageOutcome {
        PackageOutcome {
            name: "x".to_owned(),
            threshold: Threshold {
                min_lines_percent: 0.0,
                source: ThresholdSource::Package,
            },
            totals: LineTotals { count, covered },
            status,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn format_delta_renders_positive_with_sign() {
        assert_eq!(format_delta(&outcome(100, 95, 80.0)), "+15.0pp");
    }

    #[test]
    fn format_delta_renders_negative_with_sign() {
        assert_eq!(format_delta(&outcome(100, 60, 80.0)), "-20.0pp");
    }

    #[test]
    fn format_delta_preserves_sub_precision_direction() {
        assert_eq!(format_delta(&outcome(10_000, 8_196, 81.95)), "+<0.1pp");
        assert_eq!(format_delta(&outcome(2_000, 1_999, 100.0)), "-<0.1pp");
        assert_eq!(format_delta(&outcome(10_000, 8_200, 81.91)), "+<0.1pp");
        assert_eq!(format_delta(&outcome(10_000, 8_200, 82.09)), "-<0.1pp");
    }

    #[test]
    fn format_delta_renders_exact_tenth_boundaries_normally() {
        assert_eq!(format_delta(&outcome(1_000, 821, 82.0)), "+0.1pp");
        assert_eq!(format_delta(&outcome(1_000, 820, 82.1)), "-0.1pp");
        assert_eq!(format_delta(&outcome(5_000, 4_107, 82.04)), "+0.1pp");
        assert_eq!(format_delta(&outcome(5_000, 4_102, 82.14)), "-0.1pp");
        assert_eq!(format_delta(&outcome(125, 23, 18.3)), "+0.1pp");
        assert_eq!(format_delta(&outcome(25, 1, 4.1)), "-0.1pp");
    }

    #[test]
    fn sub_precision_delta_excludes_the_tolerance_boundary() {
        let boundary = DELTA_DISPLAY_PRECISION - DELTA_BOUNDARY_TOLERANCE;
        assert!(!is_sub_precision_delta(boundary));
        assert!(!is_sub_precision_delta(-boundary));
        assert!(is_sub_precision_delta(
            2.0f64.mul_add(-DELTA_BOUNDARY_TOLERANCE, DELTA_DISPLAY_PRECISION)
        ));
    }

    #[test]
    fn format_delta_renders_exact_match_as_zero() {
        assert_eq!(format_delta(&outcome(100, 82, 82.0)), "0.0pp");
    }

    #[test]
    fn format_delta_returns_dash_for_no_data() {
        let o = outcome(0, 0, 80.0);
        assert_eq!(format_delta(&o), "—");
    }

    #[test]
    fn measured_percentages_round_down_for_display() {
        assert_eq!(format_lines(&outcome(2_000, 1_999, 100.0)), "99.9%");
        assert_eq!(format_lines(&outcome(10_000, 8_195, 82.0)), "81.9%");
        assert_eq!(format_lines(&outcome(1_000, 821, 82.1)), "82.1%");
        assert_eq!(format_lines(&outcome(100, 100, 100.0)), "100.0%");
    }

    #[test]
    fn format_percent_for_display_uses_exact_line_counts() {
        assert_eq!(format_percent_for_display(&outcome(2_000, 1_999, 100.0)), "99.9%");
        assert_eq!(format_percent_for_display(&outcome(1_000, 821, 82.1)), "82.1%");
        assert_eq!(format_percent_for_display(&outcome(100, 100, 100.0)), "100.0%");
    }

    #[test]
    fn plural_helpers_pick_singular_only_for_one() {
        assert_eq!(packages(0), "0 packages");
        assert_eq!(packages(1), "1 package");
        assert_eq!(packages(2), "2 packages");
        assert_eq!(files(0), "0 files");
        assert_eq!(files(1), "1 file");
        assert_eq!(files(2), "2 files");
    }

    #[test]
    fn format_lines_for_assertion_statuses() {
        assert_eq!(format_lines(&outcome_with_status(0, 0, Status::NoCoverableLines)), "(no lines)");
        assert_eq!(format_lines(&outcome_with_status(1, 0, Status::UnexpectedCoverableLines)), "1 line");
        assert_eq!(
            format_lines(&outcome_with_status(7, 0, Status::UnexpectedCoverableLines)),
            "7 lines"
        );
    }

    #[test]
    fn format_threshold_and_delta_for_assertion_statuses() {
        for status in [Status::NoCoverableLines, Status::UnexpectedCoverableLines] {
            let o = outcome_with_status(3, 1, status);
            assert_eq!(format_threshold(&o), "(no lines)");
            assert_eq!(format_delta(&o), "—");
        }
    }

    #[test]
    fn status_labels_cover_assertion_variants() {
        assert_eq!(format_status_text(Status::NoCoverableLines), "EMPTY");
        assert_eq!(format_status_text(Status::UnexpectedCoverableLines), "NOT EMPTY");
        assert_eq!(format_status_markdown(Status::NoCoverableLines), "➖");
        assert_eq!(format_status_markdown(Status::UnexpectedCoverableLines), "❌");
    }

    #[test]
    fn result_summary_reports_each_category() {
        assert_eq!(
            result_summary(&[outcome_with_status(10, 10, Status::Ok)]),
            "all packages meet their threshold."
        );
        assert_eq!(
            result_summary(&[outcome_with_status(10, 1, Status::Fail)]),
            "1 package below threshold."
        );
        assert_eq!(
            result_summary(&[outcome_with_status(3, 0, Status::UnexpectedCoverableLines)]),
            "1 package with unexpected coverable lines."
        );
        assert_eq!(
            result_summary(&[outcome_with_status(0, 0, Status::NoData)]),
            "1 package with no attributed coverage data."
        );
        // NoCoverableLines is a passing outcome and contributes no clause.
        assert_eq!(
            result_summary(&[outcome_with_status(0, 0, Status::NoCoverableLines)]),
            "all packages meet their threshold."
        );
    }

    #[test]
    fn result_summary_joins_all_failing_categories() {
        let outcomes = vec![
            outcome_with_status(10, 1, Status::Fail),
            outcome_with_status(3, 0, Status::UnexpectedCoverableLines),
            outcome_with_status(0, 0, Status::NoData),
        ];
        assert_eq!(
            result_summary(&outcomes),
            "1 package below threshold, 1 package with unexpected coverable lines, 1 package with no attributed coverage data."
        );
    }

    #[test]
    fn count_helpers_select_their_status() {
        let outcomes = vec![
            outcome_with_status(10, 1, Status::Fail),
            outcome_with_status(3, 0, Status::UnexpectedCoverableLines),
            outcome_with_status(0, 0, Status::NoData),
            outcome_with_status(0, 0, Status::NoCoverableLines),
        ];
        assert_eq!(count_failures(&outcomes), 1);
        assert_eq!(count_unexpected_coverable_lines(&outcomes), 1);
        assert_eq!(count_no_data(&outcomes), 1);
    }
}
