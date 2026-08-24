// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write;
use std::borrow::Cow;

use strum::IntoEnumIterator;

use super::{ReportableCrate, common};
use crate::Result;
use crate::metrics::{MetricCategory, MetricValue};
use crate::reports::common::ReportContext;

pub fn generate<W: Write>(crates: &[ReportableCrate], writer: &mut W) -> Result<()> {
    let ctx = ReportContext::new(crates);
    generate_with_context(crates, &ctx, writer)
}

fn generate_with_context<W: Write>(crates: &[ReportableCrate], ctx: &ReportContext<'_>, writer: &mut W) -> Result<()> {
    // Write header row
    write!(writer, "Metric")?;
    for crate_info in crates {
        write!(
            writer,
            ",{}",
            escape_csv_untrusted(&format!("{} v{}", crate_info.name, crate_info.version))
        )?;
    }
    writeln!(writer)?;

    // Write appraisal rows if any crate has an appraisal
    let has_appraisals = crates.iter().any(|c| c.appraisal.is_some());
    if has_appraisals {
        write!(writer, "Appraisals")?;
        for crate_info in crates {
            if let Some(eval) = &crate_info.appraisal {
                let status_str = common::format_appraisal_status(eval);
                write!(writer, ",{}", escape_csv(&status_str))?;
            } else {
                write!(writer, ",")?;
            }
        }
        writeln!(writer)?;

        write!(writer, "Reasons")?;
        for crate_info in crates {
            if let Some(appraisal) = &crate_info.appraisal {
                let reasons = common::join_with(appraisal.expression_outcomes.iter().map(common::outcome_icon_name), "; ");
                write!(writer, ",{}", escape_csv(&reasons))?;
            } else {
                write!(writer, ",")?;
            }
        }
        writeln!(writer)?;
    }

    // Write metrics grouped by category
    let mut metric_buf = String::new();
    for category in MetricCategory::iter() {
        if let Some(category_metrics) = ctx.metrics_by_category.get(&category) {
            // Write each metric in this category
            for metric_name in category_metrics {
                write!(writer, "{}", escape_csv_untrusted(metric_name))?;

                // Write values for each crate
                for metric_map in &ctx.crate_metric_maps {
                    if let Some(metric) = metric_map.get(metric_name)
                        && let Some(ref value) = metric.value
                    {
                        metric_buf.clear();
                        common::write_metric_value(&mut metric_buf, value);
                        let escaped = if is_textual_metric_value(value) {
                            escape_csv_untrusted(&metric_buf)
                        } else {
                            escape_csv(&metric_buf)
                        };
                        write!(writer, ",{escaped}")?;
                    } else {
                        write!(writer, ",")?;
                    }
                }
                writeln!(writer)?;
            }
        }
    }

    Ok(())
}

fn is_textual_metric_value(value: &MetricValue) -> bool {
    // Mixed and nested lists are textual if any element is textual, because
    // the complete rendered list occupies one spreadsheet cell and must cross
    // the same trust boundary as a standalone string.
    match value {
        MetricValue::String(_) => true,
        MetricValue::List(values) => values.iter().any(is_textual_metric_value),
        MetricValue::UInt(_) | MetricValue::Float(_) | MetricValue::Boolean(_) | MetricValue::DateTime(_) => false,
    }
}

/// Prevent spreadsheet software from interpreting untrusted text as a formula.
///
/// Spreadsheet engines can ignore leading whitespace before `=`, `+`, `-`, or
/// `@`, so inspect the first non-whitespace character and prefix an apostrophe
/// before applying ordinary CSV escaping. Numeric-only metrics intentionally
/// bypass this helper to preserve numeric cell types. See docs/DESIGN.md and
/// OWASP's CSV Injection guidance.
fn escape_csv_untrusted(s: &str) -> Cow<'_, str> {
    if s.trim_start().starts_with(['=', '+', '-', '@']) {
        let mut neutralized = String::with_capacity(s.len() + 1);
        neutralized.push('\'');
        neutralized.push_str(s);
        Cow::Owned(escape_csv(&neutralized).into_owned())
    } else {
        escape_csv(s)
    }
}

/// Escape a value for RFC compliant CSV output.
///
/// Wraps the value in double quotes if it contains commas, newlines, or double quotes.
/// Internal double quotes are doubled per the RFC.
fn escape_csv(s: &str) -> Cow<'_, str> {
    let mut needs_quoting = false;
    let mut has_quote = false;
    for &b in s.as_bytes() {
        if b == b'"' {
            has_quote = true;
            needs_quoting = true;
        } else if matches!(b, b',' | b'\n' | b'\r') {
            needs_quoting = true;
        }
    }

    if has_quote {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else if needs_quoting {
        Cow::Owned(format!("\"{s}\""))
    } else {
        Cow::Borrowed(s)
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::expr::{Appraisal, ExpressionDisposition, ExpressionOutcome, Risk};
    use crate::metrics::{Metric, MetricDef};

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

    static TEXT_DEF: MetricDef = MetricDef {
        name: "text",
        description: "Untrusted text",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    static FORMULA_NAME_DEF: MetricDef = MetricDef {
        name: "=formula-name",
        description: "Formula-like metric name",
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
    fn test_escape_csv_no_special_chars() {
        let result = escape_csv("hello world");
        assert_eq!(result, "hello world");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_escape_csv_with_quotes() {
        let result = escape_csv("hello \"world\"");
        assert_eq!(result, "\"hello \"\"world\"\"\"");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_escape_csv_with_comma() {
        let result = escape_csv("hello,world");
        assert_eq!(result, "\"hello,world\"");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_escape_csv_with_newline() {
        let result = escape_csv("hello\nworld");
        assert_eq!(result, "\"hello\nworld\"");
        assert!(matches!(result, Cow::Owned(_)));
    }

    #[test]
    fn test_escape_csv_empty() {
        let result = escape_csv("");
        assert_eq!(result, "");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_escape_csv_untrusted_neutralizes_formula_prefixes() {
        for value in ["=1+1", "+cmd", "-cmd", "@SUM(A1:A2)"] {
            assert_eq!(escape_csv_untrusted(value), format!("'{value}"));
        }
    }

    #[test]
    fn test_escape_csv_untrusted_neutralizes_whitespace_prefixed_formulas() {
        for value in [" =1+1", "\t+cmd", "\r\n-cmd", "\n@SUM(A1:A2)"] {
            assert_eq!(escape_csv_untrusted(value), escape_csv(&format!("'{value}")));
        }
    }

    #[test]
    fn test_escape_csv_untrusted_preserves_non_formula_whitespace() {
        assert_eq!(escape_csv_untrusted(" normal text"), " normal text");
        assert_eq!(escape_csv_untrusted("\ttext"), "\ttext");
    }

    #[test]
    fn test_escape_csv_untrusted_quotes_neutralized_formula() {
        assert_eq!(escape_csv_untrusted("=SUM(1,1)"), "\"'=SUM(1,1)\"");
    }

    #[test]
    fn test_numeric_metric_values_remain_numeric() {
        assert!(!is_textual_metric_value(&MetricValue::Float(-1.0)));
        assert!(is_textual_metric_value(&MetricValue::String("-1".into())));
    }

    #[test]
    fn test_list_metric_is_textual_when_any_value_is_textual() {
        assert!(is_textual_metric_value(&MetricValue::List(vec![
            MetricValue::UInt(1),
            MetricValue::String("=formula".into()),
        ])));
        assert!(!is_textual_metric_value(&MetricValue::List(vec![])));
        assert!(!is_textual_metric_value(&MetricValue::List(vec![
            MetricValue::UInt(1),
            MetricValue::Float(2.0),
        ])));
    }

    #[test]
    fn test_generate_empty_crates() {
        let crates: Vec<ReportableCrate> = vec![];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Should only have header
        assert_eq!(output, "Metric\n");
    }

    #[test]
    fn test_generate_single_crate_no_evaluation() {
        let crates = vec![create_test_crate("test_crate", "1.2.3", None)];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Should have header with crate name and version
        assert!(output.starts_with("Metric,test_crate v1.2.3"));
        // Should not have Status or Reasons rows
        assert!(!output.contains("Status,"));
        assert!(!output.contains("Reasons,"));
    }

    #[test]
    fn test_generate_neutralizes_formula_in_textual_metric() {
        let crate_info = ReportableCrate::new(
            "test_crate".into(),
            Arc::new("1.0.0".parse().unwrap()),
            vec![Metric::with_value(
                &TEXT_DEF,
                MetricValue::String("=HYPERLINK(\"https://example.invalid\")".into()),
            )],
            None,
        );
        let mut output = String::new();

        generate(&[crate_info], &mut output).unwrap();

        assert!(output.contains("text,\"'=HYPERLINK(\"\"https://example.invalid\"\")\""));
    }

    #[test]
    fn test_generate_neutralizes_formula_like_header_and_metric_name() {
        let crate_info = ReportableCrate::new(
            "=formula-crate".into(),
            Arc::new("1.0.0".parse().unwrap()),
            vec![Metric::with_value(&FORMULA_NAME_DEF, MetricValue::UInt(1))],
            None,
        );
        let mut output = String::new();

        generate(&[crate_info], &mut output).unwrap();

        assert!(output.starts_with("Metric,'=formula-crate v1.0.0\n"));
        assert!(output.contains("\n'=formula-name,1\n"));
    }

    #[test]
    fn test_generate_single_crate_with_evaluation() {
        let eval = Appraisal::new(
            Risk::Low,
            vec![
                ExpressionOutcome::new("good".into(), "Good".into(), ExpressionDisposition::True),
                ExpressionOutcome::new("quality".into(), "Quality".into(), ExpressionDisposition::True),
            ],
            2,
            2,
            100.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // The status contains commas, so the cell must be quoted to stay one column.
        assert!(output.contains("Appraisals,\"LOW RISK (score = 100"), "unexpected output: {output}");
        assert!(output.contains("Reasons,✔\u{fe0f} good; ✔\u{fe0f} quality"));
    }

    #[test]
    fn test_generate_escapes_expression_description_as_regular_csv_text() {
        let eval = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "Policy".into(),
                "=HYPERLINK(\"https://example.invalid\")".into(),
                ExpressionDisposition::False,
            )],
            1,
            0,
            0.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = String::new();

        generate(&crates, &mut output).unwrap();

        assert!(output.contains("Reasons,\"❌ Policy: =HYPERLINK(\"\"https://example.invalid\"\")\""));
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
        // Should have both crates in header
        assert!(output.contains("crate_a v1.0.0"));
        assert!(output.contains("crate_b v2.0.0"));
    }

    #[test]
    fn test_generate_with_special_characters() {
        let eval = Appraisal::new(
            Risk::Low,
            vec![ExpressionOutcome::new(
                "quotes".into(),
                "Reason with \"quotes\"".into(),
                ExpressionDisposition::True,
            )],
            1,
            1,
            100.0,
        );
        let crates = vec![create_test_crate("test,\"crate\"", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, &mut output);
        result.unwrap();
        // Name with quotes in crate name should be escaped
        assert!(output.contains("test,"));
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
        assert!(output.contains("Appraisals,\"HIGH RISK (score = 0"), "unexpected output: {output}");
    }

    /// A writer that accepts `budget` writes and then fails.
    struct FailAfter {
        budget: usize,
        writes: usize,
    }

    impl Write for FailAfter {
        fn write_str(&mut self, _s: &str) -> core::fmt::Result {
            if self.writes >= self.budget {
                return Err(core::fmt::Error);
            }
            self.writes += 1;
            Ok(())
        }
    }

    #[test]
    fn test_generate_propagates_writer_errors_from_every_write() {
        let outcomes = vec![
            ExpressionOutcome::new("Passing".into(), "Passes.".into(), ExpressionDisposition::True),
            ExpressionOutcome::new("Failing".into(), "Fails.".into(), ExpressionDisposition::False),
            ExpressionOutcome::new(
                "Inconclusive".into(),
                "Cannot evaluate.".into(),
                ExpressionDisposition::Failed("service unavailable".into()),
            ),
        ];
        let crates = vec![
            create_test_crate("scored", "1.0.0", Some(Appraisal::new(Risk::Low, outcomes.clone(), 4, 4, 100.0))),
            create_test_crate("unscored", "1.0.0", Some(Appraisal::required_check_failure(outcomes))),
            create_test_crate("unevaluated", "1.0.0", None),
        ];

        let mut counter = FailAfter {
            budget: usize::MAX,
            writes: 0,
        };
        generate(&crates, &mut counter).expect("a writer that never fails must succeed");
        let total = counter.writes;
        assert!(total > 0, "the generator must write something");

        for budget in 0..total {
            let mut writer = FailAfter { budget, writes: 0 };
            assert!(
                generate(&crates, &mut writer).is_err(),
                "the writer failed on write {budget} of {total}, so generation must fail too"
            );
        }
    }
}
