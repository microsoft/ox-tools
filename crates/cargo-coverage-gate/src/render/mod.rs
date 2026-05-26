// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verdict-table renderers.
//!
//! Two output flavours, sharing the same underlying [`Report`]:
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
use crate::verdict::{CrateOutcome, Status};

/// Human-readable text for the `Lines` column.
fn format_lines(outcome: &CrateOutcome) -> String {
    outcome
        .percent()
        .map_or_else(|| "(no data)".to_owned(), |p| format!("{p:.1}%"))
}

/// Human-readable text for the `Threshold` column.
fn format_threshold(outcome: &CrateOutcome) -> String {
    format!("{:.1}%", outcome.threshold.min_lines)
}

/// Human-readable text for the `Δ vs threshold` column.
fn format_delta(outcome: &CrateOutcome) -> String {
    let Some(pct) = outcome.percent() else {
        return "—".to_owned();
    };
    let delta = pct - outcome.threshold.min_lines;
    if delta >= 0.0 {
        format!("+{delta:.1}pp")
    } else {
        format!("{delta:.1}pp")
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
        Status::NoData => "⚠️",
    }
}

/// Source-column label.
fn format_source(source: ThresholdSource) -> &'static str {
    source.label()
}

/// Number of crates below threshold (`Fail` only — `NoData` is a
/// configuration error and is summarised separately).
fn count_failures(outcomes: &[CrateOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| o.status == Status::Fail)
        .count()
}

/// Number of crates with no attributed coverage data.
fn count_no_data(outcomes: &[CrateOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|o| o.status == Status::NoData)
        .count()
}
