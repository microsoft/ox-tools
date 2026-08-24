// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common utilities shared across report generators.

use core::fmt;

use crate::expr::{Appraisal, ExpressionDisposition, ExpressionOutcome, Risk};
use crate::metrics::{Metric, MetricCategory, MetricValue};
use crate::{HashMap, HashSet};

/// Format a metric value as a string using consistent formatting rules.
///
/// `DateTime` values are formatted as date-only (YYYY-MM-DD) for readability.
/// `List` values are formatted as comma-separated strings.
pub fn format_metric_value(value: &MetricValue) -> String {
    let mut buf = String::new();
    write_metric_value(&mut buf, value);
    buf
}

/// Write a metric value into the given buffer.
pub fn write_metric_value(buf: &mut String, value: &MetricValue) {
    use core::fmt::Write;
    match value {
        MetricValue::UInt(u) => {
            let _ = write!(buf, "{u}");
        }
        MetricValue::Float(f) => {
            let _ = write!(buf, "{f:.2}");
        }
        MetricValue::Boolean(b) => {
            let _ = write!(buf, "{b}");
        }
        MetricValue::String(s) => buf.push_str(s),
        MetricValue::DateTime(dt) => {
            let _ = write!(buf, "{}", dt.format("%Y-%m-%d"));
        }
        MetricValue::List(values) => {
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    buf.push_str(", ");
                }
                write_metric_value(buf, value);
            }
        }
    }
}

/// Check if a metric name is the crate name metric.
pub fn is_crate_name_metric(metric_name: &str) -> bool {
    metric_name == "crate.name"
}

/// Check if a string is a URL (starts with http:// or https://).
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Check if a metric name represents keywords.
pub fn is_keywords_metric(metric_name: &str) -> bool {
    metric_name.to_lowercase().contains("keyword")
}

/// Check if a metric name represents categories.
pub fn is_categories_metric(metric_name: &str) -> bool {
    metric_name.to_lowercase().contains("categor")
}

/// Format keywords or categories with # prefix for each item.
///
/// Takes a comma-separated string and returns a formatted string with # prefix for each item.
/// Example: "rust, cli, tool" becomes "#rust, #cli, #tool"
/// Returns an empty string if the input is empty.
pub fn format_keywords_or_categories_with_prefix(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut result = String::new();
    for (i, item) in value.split(',').enumerate() {
        if i > 0 {
            result.push_str(", ");
        }
        result.push('#');
        result.push_str(item.trim());
    }
    result
}

/// Join an iterator of displayable items with a separator, without collecting into a Vec.
pub fn join_with<I, D>(iter: I, sep: &str) -> String
where
    I: IntoIterator<Item = D>,
    D: fmt::Display,
{
    use core::fmt::Write;
    let mut result = String::new();
    for (i, item) in iter.into_iter().enumerate() {
        if i > 0 {
            result.push_str(sep);
        }
        let _ = write!(result, "{item}");
    }
    result
}

/// Format a risk level as a consistent string.
pub const fn format_risk_status(risk: Risk) -> &'static str {
    match risk {
        Risk::Low => "LOW RISK",
        Risk::Medium => "MEDIUM RISK",
        Risk::High => "HIGH RISK",
    }
}

/// Return policy-failure and inconclusive counts when weighted scoring was skipped.
fn required_check_counts(appraisal: &Appraisal) -> (usize, usize) {
    if !appraisal.is_required_check_failure() {
        return (0, 0);
    }

    appraisal
        .expression_outcomes
        .iter()
        .fold((0, 0), |(failed, inconclusive), outcome| match &outcome.disposition {
            ExpressionDisposition::False => (failed + 1, inconclusive),
            ExpressionDisposition::Failed(_) => (failed, inconclusive + 1),
            ExpressionDisposition::True => (failed, inconclusive),
        })
}

/// Summarize how many required checks failed and how many could not be evaluated.
///
/// `(0, 0)` cannot arise from an appraisal — a required-check failure always carries at least one
/// non-passing outcome — but the wording is defined anyway so the summary is never empty.
fn required_check_summary(failed: usize, inconclusive: usize) -> String {
    match (failed, inconclusive) {
        (0, 0) => "required check failed (details unavailable)".to_string(),
        (0, 1) => "1 required check inconclusive".to_string(),
        (0, count) => format!("{count} required checks inconclusive"),
        (1, 0) => "1 required check failed".to_string(),
        (count, 0) => format!("{count} required checks failed"),
        (1, inconclusive) => format!("1 required check failed, {inconclusive} inconclusive"),
        (failed, inconclusive) => format!("{failed} required checks failed, {inconclusive} inconclusive"),
    }
}

/// Format the details of an appraisal without its risk label.
fn format_appraisal_details(appraisal: &Appraisal) -> String {
    format_appraisal_details_with_separator(appraisal, "; ")
}

/// Format appraisal details using the requested separator before skipped-score text.
pub(super) fn format_appraisal_details_with_separator(appraisal: &Appraisal, separator: &str) -> String {
    if appraisal.is_required_check_failure() {
        let (failed, inconclusive) = required_check_counts(appraisal);
        let summary = required_check_summary(failed, inconclusive);
        return format!("{summary}{separator}weighted score not calculated");
    }

    if appraisal.is_weighted_evaluation_failure() {
        let inconclusive = appraisal
            .expression_outcomes
            .iter()
            .filter(|outcome| matches!(outcome.disposition, ExpressionDisposition::Failed(_)))
            .count();
        let summary = if inconclusive == 1 {
            "1 weighted check inconclusive".to_string()
        } else {
            format!("{inconclusive} weighted checks inconclusive")
        };
        return format!("{summary}{separator}weighted score not calculated");
    }

    let (awarded_points, available_points) = appraisal.point_totals().expect("scored appraisals have point totals");
    format!(
        "score = {:.0}, awarded points = {}, available points = {}",
        appraisal.weighted_score().expect("scored appraisals have a weighted score"),
        awarded_points,
        available_points,
    )
}

/// Format an appraisal as a detailed status string.
pub fn format_appraisal_status(appraisal: &Appraisal) -> String {
    format!("{} ({})", format_risk_status(appraisal.risk()), format_appraisal_details(appraisal))
}

/// Returns the pass/fail icon for an expression outcome.
pub const fn outcome_icon(outcome: &ExpressionOutcome) -> &'static str {
    match outcome.disposition {
        ExpressionDisposition::True => "✔️",
        ExpressionDisposition::False => "❌",
        ExpressionDisposition::Failed(_) => "➖",
    }
}

/// Returns a displayable outcome: passing checks show icon and name, policy
/// failures add the expected condition, and inconclusive checks add the
/// expected condition and evaluation error.
pub const fn outcome_icon_name(outcome: &ExpressionOutcome) -> IconName<'_> {
    IconName(outcome)
}

/// A zero-allocation wrapper for the disposition-specific outcome text.
#[derive(Debug)]
pub struct IconName<'a>(&'a ExpressionOutcome);

impl fmt::Display for IconName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", outcome_icon(self.0), self.0.name)?;
        match &self.0.disposition {
            ExpressionDisposition::True => {}
            ExpressionDisposition::False => write!(f, ": {}", self.0.description)?,
            ExpressionDisposition::Failed(reason) => {
                write!(f, ": {} (failure to evaluate: {reason})", self.0.description)?;
            }
        }
        Ok(())
    }
}

/// Group metrics by category.
///
/// Returns a `HashMap` mapping each category to a vector of metric names.
pub fn group_metrics_by_category<'a>(metrics: &'a [Metric]) -> HashMap<MetricCategory, Vec<&'a str>> {
    let mut metrics_by_category: HashMap<MetricCategory, Vec<&'a str>> = crate::hash_map_with_capacity(metrics.len().min(16));

    for metric in metrics {
        metrics_by_category.entry(metric.category()).or_default().push(metric.name());
    }

    metrics_by_category
}

/// Pre-computed data shared across report generators.
///
/// Computing metric groupings and lookup maps is `O(crates × metrics)`.
/// When generating multiple report formats, building this once avoids
/// redundant work.
pub struct ReportContext<'a> {
    /// Metrics grouped by category across all crates (union of metric names).
    pub metrics_by_category: HashMap<MetricCategory, Vec<&'static str>>,
    /// Per-crate metric lookup maps for O(1) access by metric name.
    pub crate_metric_maps: Vec<HashMap<&'a str, &'a Metric>>,
}

impl<'a> ReportContext<'a> {
    /// Build a report context from a slice of reportable crates.
    pub fn new(crates: &'a [super::ReportableCrate]) -> Self {
        Self {
            metrics_by_category: group_all_metrics_by_category(crates.iter().map(|c| c.metrics.as_slice())),
            crate_metric_maps: build_metric_lookup_maps(crates),
        }
    }
}

/// Group metrics by category across multiple crates, producing the union of all metric names.
///
/// Each metric name appears at most once per category, in the order first encountered.
pub fn group_all_metrics_by_category<'a>(
    crate_metrics: impl IntoIterator<Item = &'a [Metric]>,
) -> HashMap<MetricCategory, Vec<&'static str>> {
    let mut seen: HashSet<&'static str> = crate::hash_set_with_capacity(128);
    let mut metrics_by_category: HashMap<MetricCategory, Vec<&'static str>> = crate::hash_map_with_capacity(16);

    for metrics in crate_metrics {
        for metric in metrics {
            if seen.insert(metric.name()) {
                metrics_by_category.entry(metric.category()).or_default().push(metric.name());
            }
        }
    }

    metrics_by_category
}

/// Build per-crate metric lookup maps for O(1) access by metric name.
pub fn build_metric_lookup_maps(crates: &[super::ReportableCrate]) -> Vec<HashMap<&str, &Metric>> {
    crates.iter().map(|c| c.metrics.iter().map(|m| (m.name(), m)).collect()).collect()
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::metrics::MetricDef;

    static METRIC1_DEF: MetricDef = MetricDef {
        name: "metric1",
        description: "desc1",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static METRIC2_DEF: MetricDef = MetricDef {
        name: "metric2",
        description: "desc2",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static METADATA_METRIC_DEF: MetricDef = MetricDef {
        name: "metadata_metric",
        description: "desc",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static STABILITY_METRIC_DEF: MetricDef = MetricDef {
        name: "stability_metric",
        description: "desc",
        category: MetricCategory::Stability,
        extractor: |_| None,
        default_value: || None,
    };

    #[test]
    fn test_format_metric_value_unsigned_integer() {
        assert_eq!(format_metric_value(&MetricValue::UInt(100)), "100");
        assert_eq!(format_metric_value(&MetricValue::UInt(0)), "0");
    }

    #[test]
    fn test_format_metric_value_float() {
        assert_eq!(format_metric_value(&MetricValue::Float(1.2345)), "1.23");
        assert_eq!(format_metric_value(&MetricValue::Float(0.0)), "0.00");
        assert_eq!(format_metric_value(&MetricValue::Float(99.999)), "100.00");
    }

    #[test]
    fn test_format_metric_value_boolean() {
        assert_eq!(format_metric_value(&MetricValue::Boolean(true)), "true");
        assert_eq!(format_metric_value(&MetricValue::Boolean(false)), "false");
    }

    #[test]
    fn test_format_metric_value_text() {
        assert_eq!(format_metric_value(&MetricValue::String("hello".into())), "hello");
        assert_eq!(format_metric_value(&MetricValue::String("".into())), "");
    }

    #[test]
    fn test_format_metric_value_datetime() {
        let dt = DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z").unwrap();
        let dt_utc: DateTime<Utc> = dt.into();

        // All datetime values show only the date
        let formatted = format_metric_value(&MetricValue::DateTime(dt_utc));
        assert_eq!(formatted, "2024-01-15");
    }

    #[test]
    fn test_is_url() {
        assert!(is_url("http://example.com"));
        assert!(is_url("https://example.com"));
        assert!(is_url("https://github.com/user/repo"));
        assert!(!is_url("example.com"));
        assert!(!is_url("ftp://example.com"));
        assert!(!is_url(""));
    }

    #[test]
    fn test_is_keywords_metric() {
        assert!(is_keywords_metric("keywords"));
        assert!(is_keywords_metric("Keywords"));
        assert!(is_keywords_metric("KEYWORDS"));
        assert!(is_keywords_metric("crate_keywords"));
        assert!(!is_keywords_metric("keys"));
        assert!(!is_keywords_metric(""));
    }

    #[test]
    fn test_is_categories_metric() {
        assert!(is_categories_metric("categories"));
        assert!(is_categories_metric("Categories"));
        assert!(is_categories_metric("CATEGORIES"));
        assert!(is_categories_metric("crate_categories"));
        assert!(is_categories_metric("category"));
        assert!(!is_categories_metric("cats"));
        assert!(!is_categories_metric(""));
    }

    #[test]
    fn test_format_keywords_or_categories_with_prefix() {
        assert_eq!(format_keywords_or_categories_with_prefix("rust"), "#rust");
        assert_eq!(format_keywords_or_categories_with_prefix("rust, cli, tool"), "#rust, #cli, #tool");
        assert_eq!(format_keywords_or_categories_with_prefix("rust,cli,tool"), "#rust, #cli, #tool");
        assert_eq!(format_keywords_or_categories_with_prefix("  rust  ,  cli  "), "#rust, #cli");
    }

    #[test]
    fn test_format_keywords_or_categories_with_prefix_empty_input() {
        assert_eq!(format_keywords_or_categories_with_prefix(""), "");
    }

    #[test]
    fn test_format_risk_status() {
        assert_eq!(format_risk_status(Risk::Low), "LOW RISK");
        assert_eq!(format_risk_status(Risk::Medium), "MEDIUM RISK");
        assert_eq!(format_risk_status(Risk::High), "HIGH RISK");
    }

    #[test]
    fn test_format_appraisal_status_explains_required_check_failure() {
        let appraisal = Appraisal::required_check_failure(vec![ExpressionOutcome::new(
            "Sound Crate".into(),
            "RustSec reports zero unsound advisories for this crate version.".into(),
            ExpressionDisposition::False,
        )]);

        assert_eq!(
            format_appraisal_status(&appraisal),
            "HIGH RISK (1 required check failed; weighted score not calculated)"
        );
    }

    #[test]
    fn test_required_check_counts_distinguish_failures_and_inconclusive_outcomes() {
        let appraisal = Appraisal::required_check_failure(vec![
            ExpressionOutcome::new("Pass".into(), "Passes.".into(), ExpressionDisposition::True),
            ExpressionOutcome::new("Fail".into(), "Fails.".into(), ExpressionDisposition::False),
            ExpressionOutcome::new(
                "Error".into(),
                "Cannot evaluate.".into(),
                ExpressionDisposition::Failed("missing fact".into()),
            ),
        ]);

        assert_eq!(required_check_counts(&appraisal), (1, 1));
        assert_eq!(
            format_appraisal_details(&appraisal),
            "1 required check failed, 1 inconclusive; weighted score not calculated"
        );
    }

    #[test]
    fn test_required_check_counts_ignore_weighted_outcomes() {
        let appraisal = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "Weighted".into(),
                "Did not earn points.".into(),
                ExpressionDisposition::False,
            )],
            10,
            2,
            20.0,
        );

        assert_eq!(required_check_counts(&appraisal), (0, 0));
        assert_eq!(
            format_appraisal_details(&appraisal),
            "score = 20, awarded points = 2, available points = 10"
        );
    }

    #[test]
    fn test_format_appraisal_details_supports_custom_separator() {
        let appraisal = Appraisal::required_check_failure(vec![ExpressionOutcome::new(
            "Required".into(),
            "Required policy".into(),
            ExpressionDisposition::False,
        )]);

        assert_eq!(
            format_appraisal_details_with_separator(&appraisal, " · "),
            "1 required check failed · weighted score not calculated"
        );
    }

    #[test]
    fn test_format_appraisal_details_explains_total_weighted_evaluation_failure() {
        let appraisal = Appraisal::weighted_evaluation_failure(vec![
            ExpressionOutcome::new(
                "Weighted 1".into(),
                "Weighted policy 1".into(),
                ExpressionDisposition::Failed("unavailable".into()),
            ),
            ExpressionOutcome::new(
                "Weighted 2".into(),
                "Weighted policy 2".into(),
                ExpressionDisposition::Failed("unavailable".into()),
            ),
        ]);

        assert_eq!(
            format_appraisal_status(&appraisal),
            "HIGH RISK (2 weighted checks inconclusive; weighted score not calculated)"
        );
    }

    #[test]
    fn test_outcome_icon_name_includes_description() {
        let outcome = ExpressionOutcome::new(
            "Sound Crate".into(),
            "RustSec reports zero unsound advisories for this crate version.".into(),
            ExpressionDisposition::False,
        );

        assert_eq!(
            outcome_icon_name(&outcome).to_string(),
            "❌ Sound Crate: RustSec reports zero unsound advisories for this crate version."
        );
    }

    #[test]
    fn test_outcome_icon_name_omits_description_for_passing_check() {
        let outcome = ExpressionOutcome::new(
            "Sound Crate".into(),
            "RustSec reports zero unsound advisories for this crate version.".into(),
            ExpressionDisposition::True,
        );

        assert_eq!(outcome_icon_name(&outcome).to_string(), "✔️ Sound Crate");
    }

    #[test]
    fn test_group_metrics_by_category_empty() {
        let metrics: Vec<Metric> = vec![];
        let grouped = group_metrics_by_category(&metrics);
        assert!(grouped.is_empty());
    }

    #[test]
    fn test_group_metrics_by_category_single_category() {
        let metrics = vec![
            Metric::with_value(&METRIC1_DEF, MetricValue::UInt(1)),
            Metric::with_value(&METRIC2_DEF, MetricValue::UInt(2)),
        ];
        let grouped = group_metrics_by_category(&metrics);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[&MetricCategory::Metadata].len(), 2);
    }

    #[test]
    fn test_group_metrics_by_category_multiple_categories() {
        let metrics = vec![
            Metric::with_value(&METADATA_METRIC_DEF, MetricValue::UInt(1)),
            Metric::with_value(&STABILITY_METRIC_DEF, MetricValue::UInt(2)),
        ];
        let grouped = group_metrics_by_category(&metrics);
        assert_eq!(grouped.len(), 2);
        assert!(grouped.contains_key(&MetricCategory::Metadata));
        assert!(grouped.contains_key(&MetricCategory::Stability));
    }

    /// A `fmt::Write` that accepts `budget` writes and then fails.
    struct FailAfter {
        budget: usize,
        writes: usize,
    }

    impl fmt::Write for FailAfter {
        fn write_str(&mut self, _s: &str) -> fmt::Result {
            if self.writes >= self.budget {
                return Err(fmt::Error);
            }
            self.writes += 1;
            Ok(())
        }
    }

    fn required_check_failure(failed: usize, inconclusive: usize) -> Appraisal {
        let mut outcomes: Vec<_> = (0..failed)
            .map(|i| ExpressionOutcome::new(format!("Fail {i}").into(), "Fails.".into(), ExpressionDisposition::False))
            .collect();

        outcomes.extend((0..inconclusive).map(|i| {
            ExpressionOutcome::new(
                format!("Error {i}").into(),
                "Cannot evaluate.".into(),
                ExpressionDisposition::Failed("missing fact".into()),
            )
        }));

        Appraisal::required_check_failure(outcomes)
    }

    #[test]
    fn test_required_check_summary_counts_are_pluralized() {
        assert_eq!(
            format_appraisal_details(&required_check_failure(0, 1)),
            "1 required check inconclusive; weighted score not calculated"
        );
        assert_eq!(
            format_appraisal_details(&required_check_failure(0, 2)),
            "2 required checks inconclusive; weighted score not calculated"
        );
        assert_eq!(
            format_appraisal_details(&required_check_failure(1, 0)),
            "1 required check failed; weighted score not calculated"
        );
        assert_eq!(
            format_appraisal_details(&required_check_failure(3, 0)),
            "3 required checks failed; weighted score not calculated"
        );
        assert_eq!(
            format_appraisal_details(&required_check_failure(1, 2)),
            "1 required check failed, 2 inconclusive; weighted score not calculated"
        );
        assert_eq!(
            format_appraisal_details(&required_check_failure(2, 3)),
            "2 required checks failed, 3 inconclusive; weighted score not calculated"
        );
    }

    #[test]
    fn test_outcome_icon_name_propagates_writer_errors() {
        use core::fmt::Write as _;

        let outcome = ExpressionOutcome::new(
            "Error".into(),
            "Cannot evaluate.".into(),
            ExpressionDisposition::Failed("missing fact".into()),
        );

        // Every write the formatter makes must be able to fail, including the one that renders
        // the reason the outcome could not be evaluated.
        let mut counter = FailAfter {
            budget: usize::MAX,
            writes: 0,
        };
        write!(counter, "{}", outcome_icon_name(&outcome)).expect("a writer that never fails must succeed");
        assert!(counter.writes > 1, "the formatter must write more than once");

        for budget in 0..counter.writes {
            let mut writer = FailAfter { budget, writes: 0 };
            assert!(
                write!(writer, "{}", outcome_icon_name(&outcome)).is_err(),
                "the writer failed on write {budget}, so formatting must fail too"
            );
        }
    }

    #[test]
    fn test_required_check_summary_describes_a_countless_failure() {
        // Defensive wording for a state an appraisal cannot reach.
        assert_eq!(required_check_summary(0, 0), "required check failed (details unavailable)");
    }
}
