// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common processing logic shared between crates and deps commands.

use super::ProgressReporter;
use super::config::Config;
use crate::Result;
use crate::expr::{ExpressionDisposition, ExpressionOutcome, Risk, evaluate};
use crate::facts::{Collector, CrateFacts, CrateRef, ProviderResult};
use crate::metrics::flatten;
use crate::reports::ReportableCrate;
use crate::reports::{ConsoleOutputMode, generate_console, generate_csv, generate_html, generate_json, generate_xlsx};
use camino::Utf8PathBuf;
use cargo_metadata::MetadataCommand;
use chrono::Local;
use clap::Args;
use clap::ValueEnum;
use core::fmt::Write as _;
use core::time::Duration;
use directories::BaseDirs;
use ohno::IntoAppError;
use std::fs;
use std::io::Write;
use std::sync::Arc;

/// Color mode configuration for output
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Always use colors
    Always,

    /// Never use colors
    Never,

    /// Use colors if the output is a terminal, otherwise don't use colors
    Auto,
}

/// Log level for diagnostic output
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LogLevel {
    /// No logging output
    None,

    /// Only error messages
    Error,

    /// Warning and error messages
    Warn,

    /// Info, warning, and error messages
    Info,

    /// Debug, info, warning, and error messages
    Debug,

    /// Trace, debug, info, warning, and error messages
    Trace,
}

/// Individual sections that can be shown in console output
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConsoleSection {
    /// Show the appraisal risk level
    Appraisal,

    /// Show the reasons to justify the appraisal
    Reasons,

    /// Show individual metrics
    Metrics,
}

/// Common arguments shared between crates and deps commands
#[derive(Args, Debug)]
pub struct CommonArgs {
    /// GitHub personal access token
    #[arg(long, value_name = "TOKEN", env = "GITHUB_TOKEN")]
    pub github_token: Option<String>,

    /// Codeberg personal access token
    #[arg(long, value_name = "TOKEN", env = "CODEBERG_TOKEN")]
    pub codeberg_token: Option<String>,

    /// Path to Cargo.toml file
    #[arg(long, default_value = "Cargo.toml", value_name = "PATH")]
    pub manifest_path: Utf8PathBuf,

    /// Path to configuration file (default is `aprz.toml`)
    #[arg(long, short = 'c', value_name = "PATH")]
    pub config: Option<Utf8PathBuf>,

    /// Control when to use colored output
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub color: ColorMode,

    /// Directory where crate facts are cached
    #[arg(long, value_name = "PATH")]
    pub cache_dir: Option<Utf8PathBuf>,

    /// Set the logging level for diagnostic output
    #[arg(long, value_name = "LEVEL", default_value = "none", global = true)]
    pub log_level: LogLevel,

    /// Output crate information to an Excel spreadsheet file
    #[arg(long, value_name = "PATH", help_heading = "Report Output")]
    pub excel: Option<Utf8PathBuf>,

    /// Output crate information to an HTML file
    #[arg(long, value_name = "PATH", help_heading = "Report Output")]
    pub html: Option<Utf8PathBuf>,

    /// Output crate information to a CSV file instead of to the terminal
    #[arg(long, value_name = "PATH", help_heading = "Report Output")]
    pub csv: Option<Utf8PathBuf>,

    /// Output crate information to a JSON file
    #[arg(long, value_name = "PATH", help_heading = "Report Output")]
    pub json: Option<Utf8PathBuf>,

    /// Output crate information to the console, showing the specified sections.
    /// Defaults to showing all sections. If omitted entirely, console output is shown only when no other reports are generated.
    #[arg(long, value_name = "SECTIONS", value_delimiter = ',', default_missing_value = "appraisal,reasons,metrics", num_args = 0..=1, help_heading = "Report Output")]
    pub console: Option<Vec<ConsoleSection>>,

    /// Exit with status code 1 if any crate is appraised as high risk
    #[arg(long)]
    pub error_if_high_risk: bool,

    /// Exit with status code 1 if any crate is appraised as medium or high risk
    #[arg(long)]
    pub error_if_medium_risk: bool,

    /// Ignore cached data and fetch everything fresh
    #[arg(long)]
    pub ignore_cached: bool,
}

pub struct Common<'a, H: super::Host> {
    pub collector: Collector,
    pub config: Config,
    pub metadata_cmd: MetadataCommand,
    host: &'a mut H,
    color: ColorMode,
    error_if_high_risk: bool,
    error_if_medium_risk: bool,
    console: Option<ConsoleOutputMode>,
    html: Option<Utf8PathBuf>,
    excel: Option<Utf8PathBuf>,
    csv: Option<Utf8PathBuf>,
    json: Option<Utf8PathBuf>,
}

impl<'a, H: super::Host> Common<'a, H> {
    /// Create a new Common processor with logger, collector, and config
    ///
    /// # Errors
    ///
    /// Returns an error if the collector or config cannot be initialized
    pub async fn new(host: &'a mut H, args: &CommonArgs) -> Result<Self> {
        Self::init_logging(args.log_level);

        // Create metadata command for workspace operations
        let mut metadata_cmd = MetadataCommand::new();
        let _ = metadata_cmd.manifest_path(&args.manifest_path);

        // Execute metadata command once and use it for both cache and config paths
        let metadata = metadata_cmd.exec().into_app_err("retrieving workspace metadata")?;

        // Use workspace_root for config base path
        let config_base_path = metadata.workspace_root;

        // Load config from the determined base path first (we need the cache TTL)
        let config = Config::load(&config_base_path, args.config.as_ref())?;

        // Determine cache directory: use provided path or default cache directory for the platform
        let cache_dir = if let Some(cache_path) = &args.cache_dir {
            cache_path.as_std_path().to_path_buf()
        } else {
            BaseDirs::new()
                .into_app_err("could not determine cache directory")?
                .cache_dir()
                .join("cargo-aprz")
        };

        let delay = if args.log_level == LogLevel::None {
            Duration::from_millis(300)
        } else {
            Duration::from_hours(365 * 24)
        };

        let use_colors_for_progress = match args.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                use std::io::{IsTerminal, stderr};
                stderr().is_terminal()
            }
        };

        let progress_reporter = ProgressReporter::new(delay, use_colors_for_progress);

        let collector = Collector::new(
            args.github_token.as_deref(),
            args.codeberg_token.as_deref(),
            &cache_dir,
            config.crates_cache_ttl,
            config.hosting_cache_ttl,
            config.codebase_cache_ttl,
            config.coverage_cache_ttl,
            config.advisories_cache_ttl,
            args.ignore_cached,
            config.bug_label_matcher()?.into(),
            progress_reporter,
        )
        .await?;

        // Create a fresh metadata command for the caller to use
        let mut metadata_cmd = MetadataCommand::new();
        let _ = metadata_cmd.manifest_path(&args.manifest_path);

        let console = args.console.as_ref().map(|sections| ConsoleOutputMode {
            appraisal: sections.contains(&ConsoleSection::Appraisal),
            reasons: sections.contains(&ConsoleSection::Reasons),
            metrics: sections.contains(&ConsoleSection::Metrics),
        });

        Ok(Self {
            collector,
            config,
            metadata_cmd,
            host,
            color: args.color,
            error_if_high_risk: args.error_if_high_risk,
            error_if_medium_risk: args.error_if_medium_risk,
            console,
            html: args.html.clone(),
            excel: args.excel.clone(),
            csv: args.csv.clone(),
            json: args.json.clone(),
        })
    }

    /// Initialize logger based on log level
    fn init_logging(log_level: LogLevel) {
        let level = match log_level {
            LogLevel::None => return,
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        };

        let env = env_logger::Env::default().filter_or("RUST_LOG", level);

        env_logger::Builder::from_env(env)
            .format_timestamp(None)
            .format_module_path(false)
            .format_target(matches!(log_level, LogLevel::Debug | LogLevel::Trace))
            .init();
    }

    pub async fn process_crates(&self, crates: &[CrateRef], suggestions: bool) -> Result<Vec<CrateFacts>> {
        let results = self.collector.collect(crates, suggestions).await;

        match results {
            Ok(facts_iter) => Ok(facts_iter.collect()),
            Err(e) => {
                eprintln!("{e:#}");
                Err(e)
            }
        }
    }

    #[expect(clippy::too_many_lines, reason = "Function handles multiple report formats and evaluation logic")]
    pub fn report(&mut self, processed_crates: impl IntoIterator<Item = CrateFacts>) -> Result<()> {
        // Filter out crates with missing core data (can't be reported)
        let (analyzable_crates, failed_crates): (Vec<_>, Vec<_>) =
            processed_crates.into_iter().partition(|facts| facts.crates_data.is_found());

        // Log crates that couldn't be analyzed
        if !failed_crates.is_empty() {
            let _ = writeln!(self.host.error(), "\nUnable to analyze {} crate(s)", failed_crates.len());
            for facts in &failed_crates {
                match &facts.crates_data {
                    ProviderResult::CrateNotFound(suggestions) => {
                        if suggestions.is_empty() {
                            let _ = writeln!(
                                self.host.error(),
                                "  Could not find information on crate '{}'",
                                facts.crate_spec.name()
                            );
                        } else {
                            let suggestion_text = match suggestions.as_ref() {
                                [single] => format!("Did you mean '{single}'?"),
                                [first, second] => format!("Did you mean '{first}' or '{second}'?"),
                                [all_but_last @ .., last] => {
                                    let quoted_suggestions = all_but_last.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
                                    format!("Did you mean {quoted_suggestions}, or '{last}'?")
                                }
                                [] => unreachable!("checked above that suggestions is not empty"),
                            };
                            let _ = writeln!(
                                self.host.error(),
                                "  Could not find information on crate '{}'. {}",
                                facts.crate_spec.name(),
                                suggestion_text
                            );
                        }
                    }
                    ProviderResult::VersionNotFound => {
                        let _ = writeln!(
                            self.host.error(),
                            "  Could not find information on version {} of crate `{}`",
                            facts.crate_spec.version(),
                            facts.crate_spec.name()
                        );
                    }
                    ProviderResult::Error(err) => {
                        let _ = writeln!(
                            self.host.error(),
                            "  Could not gather information for crate '{}': {err:#}",
                            facts.crate_spec
                        );
                    }
                    ProviderResult::Found(_) | ProviderResult::Unavailable(_) => {}
                }
            }
        }

        // Flatten crate facts into metrics and optionally evaluate, creating ReportableCrate instances
        let has_expressions =
            !self.config.high_risk.is_empty() || !self.config.eval.is_empty();
        let should_eval = has_expressions || self.error_if_high_risk || self.error_if_medium_risk;

        // A single instant is used for every crate: expressions such as `now - crate.updated_at`
        // must compare each crate against the same baseline, and it avoids a clock lookup per crate.
        let now = Local::now();

        let mut reportable_crates: Vec<ReportableCrate> = if should_eval {
            analyzable_crates
                .into_iter()
                .map(|facts| {
                    let metrics: Vec<_> = flatten(&facts).collect();
                    let evaluation = evaluate(
                        &self.config.high_risk,
                        &self.config.eval,
                        &metrics,
                        now,
                        self.config.medium_risk_threshold,
                        self.config.low_risk_threshold,
                    );

                    ReportableCrate::new(
                        Arc::clone(facts.crate_spec.name_arc()),
                        Arc::clone(facts.crate_spec.version_arc()),
                        metrics,
                        Some(evaluation),
                    )
                })
                .collect()
        } else {
            analyzable_crates
                .into_iter()
                .map(|facts| {
                    let metrics: Vec<_> = flatten(&facts).collect();
                    ReportableCrate::new(
                        Arc::clone(facts.crate_spec.name_arc()),
                        Arc::clone(facts.crate_spec.version_arc()),
                        metrics,
                        None,
                    )
                })
                .collect()
        };

        // Sort crates by name and version for consistent ordering
        reportable_crates.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()).then_with(|| a.version.cmp(&b.version)));

        let generating_reports = self.html.is_some() || self.excel.is_some() || self.csv.is_some() || self.json.is_some();

        // Show console output if:
        // - --console flag is explicitly set, OR
        // - No reports are being generated AND no --error-if flag is set
        let error_if = self.error_if_high_risk || self.error_if_medium_risk;
        let default_mode = ConsoleOutputMode::full();
        let console_mode = match &self.console {
            Some(mode) => Some(mode),
            None if !generating_reports && !error_if => Some(&default_mode),
            None => None,
        };

        if let Some(mode) = console_mode && !reportable_crates.is_empty() {
            let mut console_output = String::new();
            let use_colors = match self.color {
                ColorMode::Always => true,
                ColorMode::Never => false,
                ColorMode::Auto => {
                    use std::io::{IsTerminal, stdout};
                    stdout().is_terminal()
                }
            };
            _ = generate_console(&reportable_crates, use_colors, mode, &mut console_output);
            let _ = write!(self.host.output(), "{console_output}");
        }

        if let Some(filename) = &self.html {
            let mut html = String::new();
            generate_html(&reportable_crates, Local::now(), &mut html)?;
            fs::write(filename, html)?;
        }

        if let Some(filename) = &self.excel {
            let mut file = fs::File::create(filename)?;
            generate_xlsx(&reportable_crates, &mut file)?;
        }

        if let Some(filename) = &self.csv {
            let mut csv_output = String::new();
            generate_csv(&reportable_crates, &mut csv_output)?;
            fs::write(filename, csv_output)?;
        }

        if let Some(filename) = &self.json {
            let mut json_output = String::new();
            generate_json(&reportable_crates, &mut json_output)?;
            fs::write(filename, json_output)?;
        }

        // If --error-if-medium-risk flag is set, return error if any non-allowed crate is medium or high risk
        // If --error-if-high-risk flag is set, return error if any non-allowed crate is high risk
        check_risk_errors(
            &reportable_crates,
            &self.config,
            self.error_if_medium_risk,
            self.error_if_high_risk,
            should_include_rejection_details(console_mode),
        )?;

        Ok(())
    }
}

fn check_risk_errors(
    reportable_crates: &[ReportableCrate],
    config: &Config,
    error_if_medium_risk: bool,
    error_if_high_risk: bool,
    include_check_details: bool,
) -> Result<()> {
    // Keep the final error within a practical CI-log footprint while showing
    // enough independent failures to reveal whether rejection is widespread.
    const MAX_REJECTED_CRATES: usize = 20;
    // Per-crate detail is capped separately so one dependency cannot crowd out
    // diagnostics for the other blocking crates. See docs/DESIGN.md.
    const MAX_OUTCOMES_PER_CRATE: usize = 10;

    let rejected_risks: fn(Risk) -> bool = if error_if_medium_risk {
        |risk| matches!(risk, Risk::Medium | Risk::High)
    } else if error_if_high_risk {
        |risk| risk == Risk::High
    } else {
        return Ok(());
    };

    let rejected: Vec<_> = reportable_crates
        .iter()
        .filter(|crate_info| {
            crate_info
                .appraisal
                .as_ref()
                .is_some_and(|appraisal| rejected_risks(appraisal.risk()))
                && !config.is_allowed(&crate_info.name, &crate_info.version)
        })
        .collect();

    if rejected.is_empty() {
        return Ok(());
    }

    let threshold = if error_if_medium_risk {
        "medium or high risk"
    } else {
        "high risk"
    };
    let mut message = format!(
        "{} {} {} appraised as {threshold} and caused rejection:",
        rejected.len(),
        if rejected.len() == 1 { "crate" } else { "crates" },
        if rejected.len() == 1 { "was" } else { "were" }
    );

    let mut details_were_capped = rejected.len() > MAX_REJECTED_CRATES;
    for crate_info in rejected.iter().take(MAX_REJECTED_CRATES) {
        let appraisal = crate_info.appraisal.as_ref().expect("rejected crates have an appraisal");
        let _ = write!(message, "\n- {} v{}", crate_info.name, crate_info.version);

        if appraisal.is_required_check_failure() {
            let _ = write!(
                message,
                ": {} (weighted score not calculated)",
                appraisal.risk()
            );
            details_were_capped |= append_non_passing_outcomes(
                &mut message,
                &appraisal.expression_outcomes,
                MAX_OUTCOMES_PER_CRATE,
                include_check_details,
                "required check",
                "required checks",
            );
        } else if let Some(score) = appraisal.weighted_score() {
            let _ = write!(message, ": {} (score {score:.0})", appraisal.risk());
            if include_check_details {
                details_were_capped |= append_non_passing_outcomes(
                    &mut message,
                    &appraisal.expression_outcomes,
                    MAX_OUTCOMES_PER_CRATE,
                    true,
                    "non-passing outcome",
                    "non-passing outcomes",
                );
            }
        } else {
            let _ = write!(
                message,
                ": {} (weighted score not calculated)",
                appraisal.risk()
            );
            if include_check_details {
                details_were_capped |= append_non_passing_outcomes(
                    &mut message,
                    &appraisal.expression_outcomes,
                    MAX_OUTCOMES_PER_CRATE,
                    true,
                    "weighted check",
                    "weighted checks",
                );
            }
        }
    }
    if rejected.len() > MAX_REJECTED_CRATES {
        let omitted = rejected.len() - MAX_REJECTED_CRATES;
        let _ = write!(
            message,
            "\n- ... and {omitted} more rejected {}",
            if omitted == 1 { "crate" } else { "crates" }
        );
    }
    // The pointer is unnecessary only when the console already rendered both
    // the appraisal and its reasons immediately before this concise error.
    if include_check_details && details_were_capped {
        message.push_str(
            "\nRun with --console appraisal,reasons or write --json <path> for complete appraisal details.",
        );
    }

    message.push_str(
        "\nReview the non-passing checks and remediate, upgrade, or replace the affected dependencies. \
         To acknowledge a temporary exception, add the exact crate name and version to [[allow_list]] in aprz.toml.",
    );

    Err(ohno::AppError::new(message))
}

fn append_non_passing_outcomes(
    message: &mut String,
    outcomes: &[ExpressionOutcome],
    limit: usize,
    include_details: bool,
    singular_tail_noun: &str,
    plural_tail_noun: &str,
) -> bool {
    let relevant_outcomes: Vec<_> = outcomes
        .iter()
        .filter(|outcome| !matches!(outcome.disposition, ExpressionDisposition::True))
        .collect();

    for outcome in relevant_outcomes.iter().take(limit) {
        match &outcome.disposition {
            ExpressionDisposition::False => {
                let _ = write!(message, "\n    - FAILED: {}", outcome.name);
                if include_details {
                    let _ = write!(message, "; expected: {}", outcome.description);
                }
            }
            ExpressionDisposition::Failed(reason) => {
                let _ = write!(message, "\n    - INCONCLUSIVE: {}", outcome.name);
                if include_details {
                    let _ = write!(
                        message,
                        "; could not evaluate requirement: {} (error: {reason})",
                        outcome.description
                    );
                }
            }
            ExpressionDisposition::True => {}
        }
    }

    let omitted = relevant_outcomes.len().saturating_sub(limit);
    if omitted > 0 {
        let tail_noun =
            if omitted == 1 { singular_tail_noun } else { plural_tail_noun };
        let _ = write!(message, "\n    - ... and {omitted} more {tail_noun}");
        true
    } else {
        false
    }
}

fn should_include_rejection_details(console_mode: Option<&ConsoleOutputMode>) -> bool {
    console_mode.is_none_or(|mode| !(mode.appraisal && mode.reasons))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::config::AllowListEntry;
    use crate::expr::{Appraisal, ExpressionDisposition, ExpressionOutcome, Risk};
    use semver::{Version, VersionReq};

    fn make_crate(name: &str, version: Version, risk: Risk) -> ReportableCrate {
        ReportableCrate::new(
            Arc::from(name),
            Arc::new(version),
            vec![],
            Some(Appraisal::new(risk, vec![], 0, 0, 0.0)),
        )
    }

    fn make_crate_with_failure(name: &str, version: Version) -> ReportableCrate {
        ReportableCrate::new(
            Arc::from(name),
            Arc::new(version),
            vec![],
            Some(Appraisal::required_check_failure(vec![
                ExpressionOutcome::new(
                    "Sound Crate".into(),
                    "RustSec reports zero unsound advisories for this crate version.".into(),
                    ExpressionDisposition::False,
                ),
            ])),
        )
    }

    #[test]
    fn test_check_risk_errors_no_flags() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::High)];
        let config = Config::default();
        check_risk_errors(&crates, &config, false, false, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_high_risk_flag_rejects() {
        let crates = vec![make_crate_with_failure("foo", Version::new(1, 0, 0))];
        let config = Config::default();
        let error = check_risk_errors(&crates, &config, false, true, false).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("1 crate was appraised as high risk and caused rejection"));
        assert!(message.contains("- foo v1.0.0: HIGH RISK (weighted score not calculated)"));
        assert!(message.contains("    - FAILED: Sound Crate"));
        assert!(!message.contains("RustSec reports zero unsound advisories for this crate version."));
        assert!(message.contains("[[allow_list]]"));
    }

    #[test]
    fn test_check_risk_errors_includes_details_when_console_is_suppressed() {
        let appraisal = Appraisal::required_check_failure(vec![
            ExpressionOutcome::new(
                "Policy Failure".into(),
                "The policy was not satisfied.".into(),
                ExpressionDisposition::False,
            ),
            ExpressionOutcome::new(
                "Unavailable Facts".into(),
                "The policy could not be evaluated.".into(),
                ExpressionDisposition::Failed("service unavailable".into()),
            ),
        ]);
        let crates = vec![ReportableCrate::new(
            "foo".into(),
            Arc::new(Version::new(1, 0, 0)),
            vec![],
            Some(appraisal),
        )];

        let error =
            check_risk_errors(&crates, &Config::default(), false, true, true).unwrap_err();
        let message = error.to_string();

        assert!(message.contains(
            "- foo v1.0.0: HIGH RISK (weighted score not calculated)\n    - FAILED: Policy Failure; \
             expected: The policy was not satisfied.\n    - INCONCLUSIVE: \
             Unavailable Facts; could not evaluate requirement: The policy could not be \
             evaluated. (error: service unavailable)"
        ));
    }

    #[test]
    fn test_partial_console_modes_include_rejection_details() {
        let metrics_only =
            ConsoleOutputMode { appraisal: false, reasons: false, metrics: true };
        let full = ConsoleOutputMode::full();

        assert!(should_include_rejection_details(None));
        assert!(should_include_rejection_details(Some(&metrics_only)));
        assert!(!should_include_rejection_details(Some(&full)));
    }

    #[test]
    fn test_check_risk_errors_includes_weighted_outcomes_when_details_are_needed() {
        let appraisal = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "Maintained".into(),
                "The crate was recently maintained.".into(),
                ExpressionDisposition::False,
            )],
            10,
            2,
            20.0,
        );
        let crates = vec![ReportableCrate::new(
            "foo".into(),
            Arc::new(Version::new(1, 0, 0)),
            vec![],
            Some(appraisal),
        )];

        let error =
            check_risk_errors(&crates, &Config::default(), false, true, true).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(
                    "FAILED: Maintained; expected: The crate was recently maintained."
                )
        );
    }

    #[test]
    fn test_check_risk_errors_high_risk_flag_allows_medium() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::Medium)];
        let config = Config::default();
        check_risk_errors(&crates, &config, false, true, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_medium_risk_flag_rejects_medium() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::Medium)];
        let config = Config::default();
        let error = check_risk_errors(&crates, &config, true, false, false).unwrap_err();
        assert!(error.to_string().contains("- foo v1.0.0: MEDIUM RISK (score 0)"));
    }

    #[test]
    fn test_check_risk_errors_medium_risk_flag_rejects_high() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::High)];
        let config = Config::default();
        let _ = check_risk_errors(&crates, &config, true, false, false).unwrap_err();
    }

    #[test]
    fn test_check_risk_errors_medium_risk_flag_explains_required_failure() {
        let crates = vec![make_crate_with_failure("foo", Version::new(1, 0, 0))];
        let config = Config::default();
        let error = check_risk_errors(&crates, &config, true, false, false).unwrap_err();
        let message = error.to_string();

        assert!(message.contains(
            "- foo v1.0.0: HIGH RISK (weighted score not calculated)\n    - FAILED: Sound Crate"
        ));
        assert!(!message.contains("score 0"));
    }

    #[test]
    fn test_check_risk_errors_medium_risk_flag_allows_low() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::Low)];
        let config = Config::default();
        check_risk_errors(&crates, &config, true, false, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_allow_list_bypasses_high_risk() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::High)];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("^1.0").unwrap(),
        });
        check_risk_errors(&crates, &config, false, true, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_allow_list_bypasses_medium_risk() {
        let crates = vec![make_crate("foo", Version::new(1, 0, 0), Risk::Medium)];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("*").unwrap(),
        });
        check_risk_errors(&crates, &config, true, false, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_allow_list_wrong_version_still_rejects() {
        let crates = vec![make_crate("foo", Version::new(2, 0, 0), Risk::High)];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("^1.0").unwrap(),
        });
        let _ = check_risk_errors(&crates, &config, false, true, false).unwrap_err();
    }

    #[test]
    fn test_check_risk_errors_allow_list_wrong_name_still_rejects() {
        let crates = vec![make_crate("bar", Version::new(1, 0, 0), Risk::High)];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("*").unwrap(),
        });
        let _ = check_risk_errors(&crates, &config, false, true, false).unwrap_err();
    }

    #[test]
    fn test_check_risk_errors_mixed_crates_one_allowed() {
        let crates = vec![
            make_crate("foo", Version::new(1, 0, 0), Risk::High),
            make_crate_with_failure("bar", Version::new(1, 0, 0)),
        ];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("*").unwrap(),
        });
        // bar is still high risk and not allowed
        let error = check_risk_errors(&crates, &config, false, true, false).unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("foo v1.0.0"));
        assert!(message.contains("bar v1.0.0"));
    }

    #[test]
    fn test_check_risk_errors_mixed_crates_all_allowed() {
        let crates = vec![
            make_crate("foo", Version::new(1, 0, 0), Risk::High),
            make_crate("bar", Version::new(1, 0, 0), Risk::Medium),
        ];
        let mut config = Config::default();
        config.allow_list.push(AllowListEntry {
            name: "foo".to_string(),
            version: VersionReq::parse("*").unwrap(),
        });
        config.allow_list.push(AllowListEntry {
            name: "bar".to_string(),
            version: VersionReq::parse("*").unwrap(),
        });
        check_risk_errors(&crates, &config, true, true, false).unwrap();
    }

    #[test]
    fn test_check_risk_errors_bounds_crates_and_required_checks() {
        let outcomes: Vec<_> = (0..11)
            .map(|index| {
                ExpressionOutcome::new(
                    format!("Check {index}").into(),
                    "Required policy.".into(),
                    ExpressionDisposition::False,
                )
            })
            .collect();
        let crates: Vec<_> = (0..21)
            .map(|index| {
                ReportableCrate::new(
                    format!("crate-{index}").into(),
                    Arc::new(Version::new(1, 0, 0)),
                    vec![],
                    Some(Appraisal::required_check_failure(outcomes.clone())),
                )
            })
            .collect();

        let error =
            check_risk_errors(&crates, &Config::default(), false, true, true).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("Check 9"));
        assert!(!message.contains("Check 10"));
        assert!(message.contains("... and 1 more required check\n"));
        assert!(!message.contains("1 more required checks"));
        assert!(message.contains("- crate-19 v1.0.0"));
        assert!(!message.contains("- crate-20 v1.0.0"));
        assert!(message.contains("... and 1 more rejected crate\n"));
        assert!(!message.contains("1 more rejected crates"));
        assert!(message.contains(
            "Run with --console appraisal,reasons or write --json <path> for complete appraisal details."
        ));
    }

    #[test]
    fn test_check_risk_errors_pluralizes_required_and_weighted_outcome_tails() {
        let outcomes: Vec<_> = (0..12)
            .map(|index| {
                ExpressionOutcome::new(
                    format!("Check {index}").into(),
                    "Policy.".into(),
                    ExpressionDisposition::Failed("unavailable".into()),
                )
            })
            .collect();
        let crates = vec![
            ReportableCrate::new(
                "required".into(),
                Arc::new(Version::new(1, 0, 0)),
                vec![],
                Some(Appraisal::required_check_failure(outcomes.clone())),
            ),
            ReportableCrate::new(
                "weighted".into(),
                Arc::new(Version::new(1, 0, 0)),
                vec![],
                Some(Appraisal::new(Risk::High, outcomes, 12, 0, 0.0)),
            ),
        ];

        let error =
            check_risk_errors(&crates, &Config::default(), false, true, true).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("... and 2 more required checks\n"));
        assert!(message.contains("... and 2 more non-passing outcomes\n"));
    }

    #[test]
    fn test_check_risk_errors_explains_total_weighted_evaluation_failure() {
        let appraisal = Appraisal::weighted_evaluation_failure(vec![ExpressionOutcome::new(
            "Advisory facts".into(),
            "Advisory facts must be available.".into(),
            ExpressionDisposition::Failed("service unavailable".into()),
        )]);
        let crates = vec![ReportableCrate::new(
            "foo".into(),
            Arc::new(Version::new(1, 0, 0)),
            vec![],
            Some(appraisal),
        )];

        let error =
            check_risk_errors(&crates, &Config::default(), false, true, true).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("- foo v1.0.0: HIGH RISK (weighted score not calculated)"));
        assert!(message.contains(
            "INCONCLUSIVE: Advisory facts; could not evaluate requirement: Advisory facts must be \
             available. (error: service unavailable)"
        ));
    }
}
