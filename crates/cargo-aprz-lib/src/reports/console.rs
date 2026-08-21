// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write;
use std::borrow::Cow;

use owo_colors::OwoColorize;
use strum::IntoEnumIterator;
use terminal_size::{Width, terminal_size};

use super::{ReportableCrate, common};
use crate::expr::Risk;
use crate::metrics::{Metric, MetricCategory};
use crate::{HashMap, Result};

/// Controls which sections are included in console output.
#[derive(Debug, Clone)]
pub struct ConsoleOutputMode {
    /// Show the appraisal risk level
    pub appraisal: bool,
    /// Show expression outcome reasons
    pub reasons: bool,
    /// Show individual metrics
    pub metrics: bool,
}

impl ConsoleOutputMode {
    /// All sections enabled.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            appraisal: true,
            reasons: true,
            metrics: true,
        }
    }
}

pub fn generate<W: Write>(crates: &[ReportableCrate], use_colors: bool, mode: &ConsoleOutputMode, writer: &mut W) -> Result<()> {
    for (index, crate_info) in crates.iter().enumerate() {
        if index > 0 && (mode.metrics || mode.reasons) {
            writeln!(writer)?;
            writeln!(writer, "═══════════════════════════════════════")?;
            writeln!(writer)?;
        }

        // Show appraisal if one is available
        if mode.appraisal {
            if let Some(eval) = &crate_info.appraisal {
                let status_str = common::format_appraisal_status(eval);
                let colored_status: Cow<'_, str> = if use_colors {
                    match eval.risk() {
                        Risk::Low => status_str.green().bold().to_string().into(),
                        Risk::Medium => status_str.yellow().bold().to_string().into(),
                        Risk::High => status_str.red().bold().to_string().into(),
                    }
                } else {
                    Cow::Owned(status_str)
                };
                writeln!(
                    writer,
                    "{} v{} is appraised as {colored_status}",
                    crate_info.name, crate_info.version
                )?;

                if mode.reasons {
                    for outcome in &eval.expression_outcomes {
                        writeln!(writer, "  {}", common::outcome_icon_name(outcome))?;
                    }
                }
            } else {
                writeln!(writer, "{} v{} was not appraised", crate_info.name, crate_info.version)?;
            }
        }

        if !mode.metrics {
            continue;
        }

        // Build per-crate lookup map for O(1) metric access
        let metric_map: HashMap<&str, &Metric> = crate_info.metrics.iter().map(|m| (m.name(), m)).collect();

        // Use common grouping function to get metric names by category
        let metrics_by_category = common::group_metrics_by_category(&crate_info.metrics);

        // Display metrics grouped by category
        for category in MetricCategory::iter() {
            if let Some(metric_names) = metrics_by_category.get(&category) {
                writeln!(writer)?;
                if use_colors {
                    let category_str = category.to_string();
                    writeln!(writer, "{}", category_str.bold())?;
                } else {
                    writeln!(writer, "{category}")?;
                }

                // Compute max metric name length for alignment
                let max_name_len = metric_names.iter().map(|name| name.len()).max().unwrap_or(0);

                // Get terminal width and calculate available space for values
                let term_width = get_terminal_width();
                // Indent for metric lines: "  " (2) + metric_name + " : " (3)
                let value_indent = 2 + max_name_len + 3;

                for &metric_name in metric_names {
                    let &metric = metric_map
                        .get(metric_name)
                        .expect("metric_names was grouped from the very metrics that metric_map indexes, so every name resolves");
                    let formatted_value: Cow<'_, str> = metric
                        .value
                        .as_ref()
                        .map_or(Cow::Borrowed("n/a"), |v| Cow::Owned(common::format_metric_value(v)));

                    // Wrap the value text
                    let wrapped_lines = wrap_text(&formatted_value, term_width, value_indent);
                    let first_line = wrapped_lines
                        .first()
                        .expect("wrap_text always emits at least one line, even for empty input");

                    // Write first line with metric name
                    writeln!(writer, "  {:<width$} : {}", metric.name(), first_line, width = max_name_len)?;

                    // Write continuation lines
                    for line in wrapped_lines.iter().skip(1) {
                        writeln!(writer, "{line}")?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Get the terminal width, defaulting to 80 if not detectable
fn get_terminal_width() -> usize {
    terminal_size().map_or(80, |(Width(w), _)| w as usize)
}

/// Word-wrap text to fit within a given width, with indentation for continuation lines
fn wrap_text(text: &str, width: usize, indent: usize) -> Vec<String> {
    if width <= indent {
        // Not enough space, return single line
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut is_first_line = true;

    for word in text.split_whitespace() {
        let word_len = word.len();

        // Check if adding this word would exceed the width
        let separator_len = usize::from(!current_line.is_empty()); // space before word
        let line_width = if is_first_line {
            current_line.len()
        } else {
            indent + current_line.len()
        };

        if !current_line.is_empty() && line_width + separator_len + word_len > width {
            // Start a new line
            if is_first_line {
                lines.push(current_line);
                is_first_line = false;
            } else {
                lines.push(format!("{:indent$}{}", "", current_line, indent = indent));
            }
            current_line = word.to_string();
        } else {
            // Add word to current line
            if !current_line.is_empty() {
                current_line.push(' ');
            }
            current_line.push_str(word);
        }
    }

    // Add the last line
    if !current_line.is_empty() {
        if is_first_line {
            lines.push(current_line);
        } else {
            lines.push(format!("{:indent$}{}", "", current_line, indent = indent));
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::expr::{Appraisal, ExpressionDisposition, ExpressionOutcome};
    use crate::metrics::{MetricDef, MetricValue};

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

    static DESCRIPTION_DEF: MetricDef = MetricDef {
        name: "description",
        description: "Crate description",
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
    fn test_generate_empty_crates() {
        let crates: Vec<ReportableCrate> = vec![];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_generate_single_crate_no_evaluation() {
        let crates = vec![create_test_crate("test_crate", "1.0.0", None)];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        // Output should contain crate information but no evaluation
        assert!(!output.contains("Evaluation Result"));
        assert!(!output.contains("RISK"));
    }

    #[test]
    fn test_generate_single_crate_with_evaluation_accepted() {
        let eval = Appraisal::new(
            Risk::Low,
            vec![ExpressionOutcome::new(
                "quality".into(),
                "Good quality".into(),
                ExpressionDisposition::True,
            )],
            1,
            1,
            100.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        assert!(output.contains("appraised as"));
        assert!(output.contains("LOW RISK"));
    }

    #[test]
    fn test_generate_single_crate_with_evaluation_denied() {
        let eval = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "security".into(),
                "Security issues".into(),
                ExpressionDisposition::False,
            )],
            1,
            0,
            0.0,
        );
        let crates = vec![create_test_crate("test_crate", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        assert!(output.contains("appraised as"));
        assert!(output.contains("HIGH RISK"));
    }

    #[test]
    fn test_generate_multiple_crates() {
        let crates = vec![create_test_crate("zebra", "1.0.0", None), create_test_crate("alpha", "2.0.0", None)];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        // Should have separator between crates
        assert!(output.contains("═══════════════════════════════════════"));
    }

    #[test]
    fn test_generate_color_mode_never() {
        let eval = Appraisal::new(Risk::Low, vec![], 0, 0, 100.0);
        let crates = vec![create_test_crate("test", "1.0.0", Some(eval))];
        let mut output = String::new();
        let result = generate(&crates, false, &ConsoleOutputMode::full(), &mut output);
        result.unwrap();
        // Should not contain ANSI color codes
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn test_wrap_text_short() {
        let text = "short text";
        let lines = wrap_text(text, 80, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "short text");
    }

    #[test]
    fn test_wrap_text_long() {
        let text = "This is a very long text that should be wrapped at word boundaries when it exceeds the specified width";
        let lines = wrap_text(text, 40, 10);
        assert!(lines.len() > 1);
        // First line should not be indented
        assert!(!lines[0].starts_with(' '));
        // Continuation lines should be indented
        assert!(lines[1].starts_with("          ")); // 10 spaces
    }

    #[test]
    fn test_wrap_text_exact_fit() {
        let text = "word1 word2 word3";
        let lines = wrap_text(text, 17, 5);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_wrap_text_empty() {
        let text = "";
        let lines = wrap_text(text, 80, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "");
    }

    static SUMMARY_DEF: MetricDef = MetricDef {
        name: "summary",
        description: "Crate summary",
        category: MetricCategory::Metadata,
        extractor: |_| None,
        default_value: || None,
    };

    #[test]
    fn colored_output_covers_every_risk_level() {
        for risk in [Risk::Low, Risk::Medium, Risk::High] {
            let eval = Appraisal::new(risk, vec![], 0, 0, 100.0);
            let crates = vec![create_test_crate("test", "1.0.0", Some(eval))];
            let mut output = String::new();
            generate(&crates, true, &ConsoleOutputMode::full(), &mut output).unwrap();

            assert!(output.contains("\x1b["), "expected ANSI styling for {risk:?}");
            assert!(output.contains("appraised as"));
        }
    }

    #[test]
    fn metrics_section_is_skipped_when_not_requested() {
        let eval = Appraisal::new(Risk::Low, vec![], 0, 0, 100.0);
        let crates = vec![create_test_crate("test", "1.0.0", Some(eval))];
        let mode = ConsoleOutputMode {
            appraisal: true,
            reasons: false,
            metrics: false,
        };

        let mut output = String::new();
        generate(&crates, false, &mode, &mut output).unwrap();

        assert!(output.contains("appraised as"));
        assert!(!output.contains("name  "), "metrics must not be rendered: {output}");
    }

    #[test]
    fn reasons_only_output_still_separates_multiple_crates() {
        let crates = vec![
            create_test_crate("first", "1.0.0", Some(Appraisal::new(Risk::Low, vec![], 0, 0, 100.0))),
            create_test_crate("second", "1.0.0", Some(Appraisal::new(Risk::Low, vec![], 0, 0, 100.0))),
        ];
        let mode = ConsoleOutputMode {
            appraisal: false,
            reasons: true,
            metrics: false,
        };
        let mut output = String::new();

        generate(&crates, false, &mode, &mut output).unwrap();

        assert_eq!(output, "\n═══════════════════════════════════════\n\n");
    }

    #[test]
    fn long_metric_values_are_wrapped_onto_continuation_lines() {
        let long_value = "word ".repeat(60);
        let metrics = vec![Metric::with_value(&SUMMARY_DEF, MetricValue::String(long_value.into()))];
        let crates = vec![ReportableCrate::new(
            "test".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];

        let mut output = String::new();
        generate(&crates, false, &ConsoleOutputMode::full(), &mut output).unwrap();

        let continuation_lines = output.lines().filter(|line| line.starts_with("           ")).count();
        assert!(continuation_lines > 0, "expected wrapped continuation lines in:\n{output}");
    }

    #[test]
    fn generated_wrapped_metric_uses_exact_continuation_indent() {
        static EXACT_WIDTH_DEF: MetricDef = MetricDef {
            name: "abcde",
            description: "Five-character metric name",
            category: MetricCategory::Metadata,
            extractor: |_| None,
            default_value: || None,
        };
        let metrics = vec![Metric::with_value(
            &EXACT_WIDTH_DEF,
            MetricValue::String("word ".repeat(200).into()),
        )];
        let crates = vec![ReportableCrate::new(
            "test".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];
        let mode = ConsoleOutputMode {
            appraisal: false,
            reasons: false,
            metrics: true,
        };
        let mut output = String::new();

        generate(&crates, false, &mode, &mut output).unwrap();

        assert!(
            output.lines().any(|line| line.starts_with("          word")),
            "continuation indent is exactly 2 + metric name length + 3: {output}"
        );
        assert!(
            !output.lines().any(|line| line.starts_with("             word"))
                && !output.lines().any(|line| line.starts_with("                     word")),
            "continuation indent must not use multiplied terms: {output}"
        );
    }

    #[test]
    fn wrap_text_wraps_at_width_boundary_and_keeps_continuation_words_together() {
        let lines = wrap_text("aaaaa aaaa bbbb", 14, 4);

        assert_eq!(lines, vec!["aaaaa aaaa".to_owned(), "    bbbb".to_owned()]);

        let continuation_lines = wrap_text("aaaaa aaaaa bbbb c", 14, 4);

        assert_eq!(continuation_lines, vec!["aaaaa aaaaa".to_owned(), "    bbbb c".to_owned()]);
    }

    #[test]
    fn test_wrap_text_when_indent_exceeds_width() {
        let lines = wrap_text("some text", 5, 10);
        assert_eq!(lines, vec!["some text".to_owned()]);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetModuleFileNameW")]
    fn test_generate_without_the_appraisal_section() {
        let crates = vec![create_test_crate(
            "serde",
            "1.0.0",
            Some(Appraisal::new(Risk::Low, vec![], 1, 1, 100.0)),
        )];
        let mode = ConsoleOutputMode {
            appraisal: false,
            reasons: false,
            metrics: true,
        };
        let mut output = String::new();

        generate(&crates, false, &mode, &mut output).unwrap();

        assert!(!output.contains("appraised as"), "the appraisal section is off: {output}");
        assert!(output.contains("name"), "the metrics section is on: {output}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetModuleFileNameW")]
    fn test_generate_wraps_long_metric_values_onto_continuation_lines() {
        let long_value = "word ".repeat(200);
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String("serde".into())),
            Metric::with_value(&VERSION_DEF, MetricValue::String("1.0.0".into())),
            Metric::with_value(&DESCRIPTION_DEF, MetricValue::String(long_value.into())),
        ];
        let crates = vec![ReportableCrate::new(
            "serde".into(),
            Arc::new("1.0.0".parse().unwrap()),
            metrics,
            None,
        )];
        let mode = ConsoleOutputMode {
            appraisal: false,
            reasons: false,
            metrics: true,
        };
        let mut output = String::new();

        generate(&crates, false, &mode, &mut output).unwrap();

        let continuation_lines = output
            .lines()
            .filter(|line| line.starts_with("       ") && line.contains("word"))
            .count();
        assert!(continuation_lines > 0, "the long value must wrap: {output}");
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
    #[cfg_attr(miri, ignore = "Miri cannot call GetModuleFileNameW")]
    fn test_generate_propagates_writer_errors_from_every_write() {
        let long_value = "word ".repeat(200);
        let metrics = vec![
            Metric::with_value(&NAME_DEF, MetricValue::String("serde".into())),
            Metric::with_value(&DESCRIPTION_DEF, MetricValue::String(long_value.into())),
        ];
        let crates = vec![
            ReportableCrate::new("serde".into(), Arc::new("1.0.0".parse().unwrap()), metrics, None),
            create_test_crate(
                "scored",
                "1.0.0",
                Some(Appraisal::new(
                    Risk::Low,
                    vec![ExpressionOutcome::new(
                        "Check".into(),
                        "Passes.".into(),
                        ExpressionDisposition::True,
                    )],
                    1,
                    1,
                    100.0,
                )),
            ),
        ];
        let mode = ConsoleOutputMode {
            appraisal: true,
            reasons: true,
            metrics: true,
        };

        let mut counter = FailAfter {
            budget: usize::MAX,
            writes: 0,
        };
        generate(&crates, true, &mode, &mut counter).expect("a writer that never fails must succeed");
        let total = counter.writes;
        assert!(total > 0, "the generator must write something");

        for budget in 0..total {
            let mut writer = FailAfter { budget, writes: 0 };
            assert!(
                generate(&crates, true, &mode, &mut writer).is_err(),
                "the writer failed on write {budget} of {total}, so generation must fail too"
            );
        }
    }
}
