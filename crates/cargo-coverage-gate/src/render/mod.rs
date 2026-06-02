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

pub(crate) mod markdown;
pub(crate) mod text;

use crate::threshold::ThresholdSource;
use crate::verdict::{PackageOutcome, Status};

/// Human-readable text for the `Lines` column.
fn format_lines(outcome: &PackageOutcome) -> String {
    outcome.percent().map_or_else(|| "(no data)".to_owned(), |p| format!("{p:.1}%"))
}

/// Human-readable text for the `Threshold` column.
fn format_threshold(outcome: &PackageOutcome) -> String {
    format!("{:.1}%", outcome.threshold.min_lines_percent)
}

/// Human-readable text for the `Δ vs threshold` column.
fn format_delta(outcome: &PackageOutcome) -> String {
    let Some(pct) = outcome.percent() else {
        return "—".to_owned();
    };
    let delta = pct - outcome.threshold.min_lines_percent;
    // Round to the displayed precision (one decimal place) before choosing a sign so
    // sub-precision floating-point noise doesn't render as a misleading "-0.0pp" or
    // "+0.0pp". Verdict classification uses an epsilon; the rendered value should agree.
    let rounded = (delta * 10.0).round() / 10.0;
    if rounded > 0.0 {
        format!("+{rounded:.1}pp")
    } else if rounded < 0.0 {
        format!("{rounded:.1}pp")
    } else {
        "0.0pp".to_owned()
    }
}

/// Status text for the plain-text renderer.
fn format_status_text(status: Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::Fail => "FAIL",
        Status::NoData => "NO DATA",
    }
}

/// Status text for the Markdown renderer (uses emoji for visual scan).
fn format_status_markdown(status: Status) -> &'static str {
    match status {
        Status::Ok => "✅",
        Status::Fail => "❌",
        Status::NoData => "💥",
    }
}

/// Source-column label.
fn format_source(source: ThresholdSource) -> &'static str {
    source.as_str()
}

/// Number of packages below threshold (`Fail` only — `NoData` is a
/// configuration error and is summarized separately).
fn count_failures(outcomes: &[PackageOutcome]) -> usize {
    outcomes.iter().filter(|o| o.status == Status::Fail).count()
}

/// Number of packages with no attributed coverage data.
fn count_no_data(outcomes: &[PackageOutcome]) -> usize {
    outcomes.iter().filter(|o| o.status == Status::NoData).count()
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
    fn format_delta_collapses_sub_precision_noise_to_unsigned_zero() {
        // 82/100 - 82.0 is exactly zero algebraically but the recomputed
        // percentage can drift by ~1e-13 due to f64 representation.
        // We must not render "-0.0pp" or "+0.0pp" — just "0.0pp".
        let o = outcome(100, 82, 82.0);
        assert_eq!(format_delta(&o), "0.0pp");

        // Tiny positive drift below the displayed precision rounds to zero too.
        let mut o = outcome(100, 82, 82.0);
        o.totals = LineTotals {
            count: 100_000_000,
            covered: 82_000_001,
        };
        // 82.000001 - 82.0 = 1e-6 -> rounds to 0.0pp.
        assert_eq!(format_delta(&o), "0.0pp");

        // Tiny negative drift below the displayed precision must also render as
        // unsigned "0.0pp", not "-0.0pp". `f64::round` on a sub-precision negative
        // value yields `-0.0`, so the `< 0.0` branch must reject it (and not be
        // weakened to `<= 0.0`, which would print "-0.0pp").
        let mut o = outcome(100, 82, 82.0);
        o.totals = LineTotals {
            count: 100_000_000,
            covered: 81_999_999,
        };
        // 81.999999 - 82.0 = -1e-6 -> rounds to -0.0 -> must render as "0.0pp".
        assert_eq!(format_delta(&o), "0.0pp");
    }

    #[test]
    fn format_delta_returns_dash_for_no_data() {
        let o = outcome(0, 0, 80.0);
        assert_eq!(format_delta(&o), "—");
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
}
