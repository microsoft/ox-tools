// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write;

use serde_json::json;

use super::{ReportableCrate, common};
use crate::Result;
use crate::expr::{ExpressionDisposition, Risk};
use crate::metrics::MetricValue;

#[expect(unused_results, reason = "HashMap::insert intentionally overwrites values")]
pub fn generate<W: Write>(crates: &[ReportableCrate], writer: &mut W) -> Result<()> {
    let mut crate_data = Vec::with_capacity(crates.len());
    let mut buf = String::new();

    for crate_info in crates {
        let mut crate_obj = serde_json::Map::new();
        crate_obj.insert("name".into(), json!(crate_info.name));
        crate_obj.insert("version".into(), json!(crate_info.version.to_string()));

        if let Some(appraisal) = &crate_info.appraisal {
            let mut eval_obj = serde_json::Map::new();
            eval_obj.insert("result".into(), json!(common::format_appraisal_status(appraisal)));
            eval_obj.insert(
                "risk".into(),
                json!(match appraisal.risk() {
                    Risk::Low => "low",
                    Risk::Medium => "medium",
                    Risk::High => "high",
                }),
            );
            eval_obj.insert("required_check_failure".into(), json!(appraisal.is_required_check_failure()));
            eval_obj.insert(
                "weighted_evaluation_failure".into(),
                json!(appraisal.is_weighted_evaluation_failure()),
            );
            let weighted_score = appraisal.weighted_score();
            eval_obj.insert("score".into(), weighted_score.into());
            let point_totals = appraisal.point_totals();
            eval_obj.insert("awarded_points".into(), point_totals.map(|points| points.0).into());
            eval_obj.insert("available_points".into(), point_totals.map(|points| points.1).into());
            eval_obj.insert(
                "reasons".into(),
                json!(
                    appraisal
                        .expression_outcomes
                        .iter()
                        .map(|outcome| match &outcome.disposition {
                            ExpressionDisposition::Failed(reason) => {
                                format!("{} (failure to evaluate: {reason})", outcome.name)
                            }
                            ExpressionDisposition::True | ExpressionDisposition::False => {
                                outcome.name.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                ),
            );
            eval_obj.insert(
                "outcomes".into(),
                json!(
                    appraisal
                        .expression_outcomes
                        .iter()
                        .map(|outcome| {
                            let (disposition, evaluation_error) = match &outcome.disposition {
                                ExpressionDisposition::True => ("passed", None),
                                ExpressionDisposition::False => ("failed", None),
                                ExpressionDisposition::Failed(reason) => ("inconclusive", Some(reason.as_str())),
                            };
                            json!({
                                "name": outcome.name,
                                "description": outcome.description,
                                "disposition": disposition,
                                "evaluation_error": evaluation_error,
                            })
                        })
                        .collect::<Vec<_>>()
                ),
            );
            crate_obj.insert("appraisal".into(), json!(eval_obj));
        }

        let mut metrics_obj = serde_json::Map::new();
        for metric in &crate_info.metrics {
            if let Some(ref value) = metric.value {
                let json_value = metric_value_to_json(value, &mut buf);
                metrics_obj.insert(metric.name().into(), json_value);
            }
        }

        crate_obj.insert("metrics".into(), json!(metrics_obj));
        crate_data.push(json!(crate_obj));
    }

    let output = json!({
        "crates": crate_data
    });

    write!(writer, "{}", serde_json::to_string_pretty(&output)?)?;
    Ok(())
}

fn metric_value_to_json(value: &MetricValue, buf: &mut String) -> serde_json::Value {
    match value {
        MetricValue::UInt(u) => json!(u),
        MetricValue::Float(f) => json!(f),
        MetricValue::Boolean(b) => json!(b),
        MetricValue::String(s) => json!(s.as_str()),
        MetricValue::DateTime(_) => {
            buf.clear();
            common::write_metric_value(buf, value);
            json!(buf.as_str())
        }
        MetricValue::List(values) => {
            json!(values.iter().map(|v| metric_value_to_json(v, buf)).collect::<Vec<_>>())
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Utc};

    use super::*;
    use crate::expr::{Appraisal, ExpressionOutcome};
    use crate::metrics::{Metric, MetricCategory, MetricDef};

    static NAME_DEF: MetricDef = MetricDef {
        name: "name",
        description: "Crate name",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static VERSION_DEF: MetricDef = MetricDef {
        name: "version",
        description: "Crate version",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    fn create_test_crate(name: &str, version: &str, evaluation: Option<Appraisal>) -> ReportableCrate {
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String(name.into())),
            Metric::with_value(&VERSION_DEF, MetricValue::String(version.into())),
        ];
        ReportableCrate::new(name.into(), Arc::new(version.parse().unwrap()), metrics, evaluation)
    }

    #[test]
    fn test_metric_value_to_json_float() {
        let value = MetricValue::Float(1.234);
        let json = metric_value_to_json(&value, &mut String::new());
        assert_eq!(json, json!(1.234));
    }

    #[test]
    fn test_metric_value_to_json_boolean() {
        let value = MetricValue::Boolean(true);
        let json = metric_value_to_json(&value, &mut String::new());
        assert_eq!(json, json!(true));
    }

    #[test]
    fn test_metric_value_to_json_text() {
        let value = MetricValue::String("hello".into());
        let json = metric_value_to_json(&value, &mut String::new());
        assert_eq!(json, json!("hello"));
    }

    #[test]
    fn test_metric_value_to_json_datetime() {
        let dt = DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z").unwrap();
        let dt_utc: DateTime<Utc> = dt.into();
        let value = MetricValue::DateTime(dt_utc);
        let json = metric_value_to_json(&value, &mut String::new());
        assert!(json.as_str().unwrap().contains("2024-01-15"));
    }

    #[test]
    fn test_generate_empty_crates() {
        let crates: Vec<ReportableCrate> = vec![];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(parsed["crates"].is_array());
        assert_eq!(parsed["crates"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_generate_single_crate_no_evaluation() {
        let crates = vec![create_test_crate("test_crate", "1.2.3", None)];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["crates"][0]["name"], "test_crate");
        assert_eq!(parsed["crates"][0]["version"], "1.2.3");
        // Should not have evaluation
        assert!(parsed["crates"][0]["evaluation"].is_null());
    }

    #[test]
    fn test_generate_single_crate_with_evaluation() {
        let eval = Appraisal::new(
            Risk::Low,
            vec![ExpressionOutcome::new("good".into(), "Good".into(), ExpressionDisposition::True)],
            1,
            1,
            100.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            parsed["crates"][0]["appraisal"]["result"],
            "LOW RISK (score = 100, awarded points = 1, available points = 1)"
        );
        assert_eq!(parsed["crates"][0]["appraisal"]["reasons"][0], "good");
        assert_eq!(parsed["crates"][0]["appraisal"]["risk"], "low");
        assert_eq!(parsed["crates"][0]["appraisal"]["score"], 100.0);
        assert_eq!(parsed["crates"][0]["appraisal"]["outcomes"][0]["disposition"], "passed");
    }

    #[test]
    fn test_generate_multiple_crates() {
        let crates = vec![
            create_test_crate("crate_a", "1.0.0", None),
            create_test_crate("crate_b", "2.0.0", None),
        ];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["crates"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["crates"][0]["name"], "crate_a");
        assert_eq!(parsed["crates"][1]["name"], "crate_b");
    }

    #[test]
    fn test_generate_denied_status() {
        let eval = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "security".into(),
                "Security issue".into(),
                ExpressionDisposition::False,
            )],
            1,
            0,
            0.0,
        );
        let crates = vec![create_test_crate("bad_crate", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            parsed["crates"][0]["appraisal"]["result"],
            "HIGH RISK (score = 0, awarded points = 0, available points = 1)"
        );
        assert_eq!(parsed["crates"][0]["appraisal"]["outcomes"][0]["description"], "Security issue");
        assert_eq!(parsed["crates"][0]["appraisal"]["outcomes"][0]["disposition"], "failed");
    }

    #[test]
    fn test_generate_required_check_failure_has_null_score_and_evaluation_error() {
        let eval = Appraisal::required_check_failure(vec![ExpressionOutcome::new(
            "facts".into(),
            "Facts must be available.".into(),
            ExpressionDisposition::Failed("service unavailable".into()),
        )]);
        let crates = vec![create_test_crate("bad_crate", "1.0.0", Some(eval))];
        let mut output = String::new();

        generate(&crates, &mut output).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let appraisal = &parsed["crates"][0]["appraisal"];
        assert_eq!(appraisal["required_check_failure"], true);
        assert_eq!(appraisal["weighted_evaluation_failure"], false);
        assert!(appraisal["score"].is_null());
        assert!(appraisal["awarded_points"].is_null());
        assert!(appraisal["available_points"].is_null());
        assert_eq!(appraisal["reasons"][0], "facts (failure to evaluate: service unavailable)");
        assert_eq!(appraisal["outcomes"][0]["disposition"], "inconclusive");
        assert_eq!(appraisal["outcomes"][0]["evaluation_error"], "service unavailable");
    }

    #[test]
    fn test_generate_weighted_evaluation_failure_has_distinct_state_and_null_score() {
        let eval = Appraisal::weighted_evaluation_failure(vec![ExpressionOutcome::new(
            "facts".into(),
            "Facts must be available.".into(),
            ExpressionDisposition::Failed("service unavailable".into()),
        )]);
        let crates = vec![create_test_crate("bad_crate", "1.0.0", Some(eval))];
        let mut output = String::new();

        generate(&crates, &mut output).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let appraisal = &parsed["crates"][0]["appraisal"];
        assert_eq!(appraisal["required_check_failure"], false);
        assert_eq!(appraisal["weighted_evaluation_failure"], true);
        assert!(appraisal["score"].is_null());
        assert!(appraisal["awarded_points"].is_null());
        assert!(appraisal["available_points"].is_null());
    }

    #[test]
    fn test_generate_pretty_formatting() {
        let crates = vec![create_test_crate("test", "1.0.0", None)];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Pretty-printed JSON should have newlines and indentation
        assert!(output.contains('\n'));
        assert!(output.contains("  "));
    }
}
