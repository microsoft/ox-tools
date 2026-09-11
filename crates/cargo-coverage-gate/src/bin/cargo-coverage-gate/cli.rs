// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-coverage-gate`.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Cargo sub-command entry point.
///
/// Handles the `cargo coverage-gate` invocation pattern where cargo
/// passes `coverage-gate` as the first argument to the
/// `cargo-coverage-gate` binary. The single-variant enum is the
/// standard clap pattern for cargo subcommands without nested
/// sub-subcommands.
#[derive(Parser, Debug)]
#[command(name = "cargo", bin_name = "cargo")]
pub(crate) enum CargoCli {
    /// Gate pull requests on per-package line coverage.
    CoverageGate(CoverageGateArgs),
}

/// Arguments for the `cargo coverage-gate` command.
#[derive(Args, Debug, Clone)]
#[command(version, about = "Gate pull requests on per-package line coverage")]
pub(crate) struct CoverageGateArgs {
    /// Path(s) to the cargo-llvm-cov lcov tracefile(s).
    ///
    /// May be repeated (`--lcov a.info --lcov b.info`); the tracefiles are
    /// merged at the line level before gating, so multiple feature-config
    /// exports can be evaluated together. Defaults to a single
    /// `target/coverage/lcov.info` when omitted, matching the recommended
    /// `cargo llvm-cov report --lcov --output-path` invocation.
    #[arg(long = "lcov", value_name = "PATH", global = true)]
    pub(crate) lcov: Vec<PathBuf>,

    /// Restrict the operation to one or more package selectors.
    ///
    /// Accepts the same `-p` / `--package` idiom as `cargo build`:
    /// repeat the flag (`-p foo -p bar`) and/or use Unix glob patterns
    /// (`-p 'tokio-*'`, `-p 'ohno*'`). When unset, every workspace
    /// member is in scope. CI integrations typically pass the
    /// impacted-package list from their test-impact step.
    #[arg(long = "package", short = 'p', value_name = "SPEC", global = true)]
    pub(crate) packages: Vec<String>,

    /// Rust target triple whose coverage policy should be evaluated.
    ///
    /// Defaults to the rustc host target.
    #[arg(long, value_name = "TRIPLE", global = true)]
    pub(crate) target: Option<String>,

    /// Write the Markdown verdict table to this file.
    ///
    /// When unset, the tool falls back to `$GITHUB_STEP_SUMMARY` and then
    /// `$COVERAGE_GATE_SUMMARY` (in that order) before giving up.
    #[arg(long, value_name = "PATH", global = true)]
    pub(crate) summary_file: Option<PathBuf>,

    /// Suppress stdout output (the summary file, if any, is still written).
    #[arg(long, global = true)]
    pub(crate) quiet: bool,

    /// Optional collection command. When omitted, evaluate existing LCOV files.
    #[command(subcommand)]
    pub(crate) command: Option<CoverageGateCommand>,
}

/// Commands that extend the backward-compatible evaluation mode.
#[derive(Subcommand, Debug, Clone)]
pub(crate) enum CoverageGateCommand {
    /// Collect coverage with cargo-llvm-cov and nextest, then evaluate it.
    Run(CollectionArgs),
}

/// Options specific to portable coverage collection.
#[derive(Args, Debug, Clone)]
pub(crate) struct CollectionArgs {
    /// Read exact `name@version` workspace package specs from this file.
    ///
    /// Each nonempty UTF-8 line is one package. The file's packages are
    /// unioned with repeated `--package` selectors. A present empty file with
    /// no `--package` selectors is an explicit successful no-op.
    #[arg(long, value_name = "PATH")]
    pub(crate) package_file: Option<PathBuf>,

    /// Feature configuration to collect.
    ///
    /// May be repeated. Defaults to both supported configurations.
    #[arg(long = "configuration", value_enum, value_name = "CONFIGURATION")]
    pub(crate) configurations: Vec<FeatureConfiguration>,

    /// Directory for generated LCOV files.
    #[arg(long, value_name = "PATH", default_value = "target/coverage")]
    pub(crate) coverage_dir: PathBuf,

    /// Concurrency forwarded to nextest for both build and test execution.
    #[arg(long, value_name = "N")]
    pub(crate) jobs: Option<NonZeroUsize>,
}

/// Supported feature configurations for an instrumented test run.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FeatureConfiguration {
    /// Enable every declared Cargo feature.
    AllFeatures,
    /// Disable each package's default Cargo features.
    NoDefaultFeatures,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_well_formed() {
        CargoCli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_retains_evaluation_options() {
        let CargoCli::CoverageGate(args) =
            CargoCli::try_parse_from(["cargo", "coverage-gate", "--lcov", "one.info", "-p", "alpha", "--quiet"])
                .expect("legacy invocation must parse");

        assert!(args.command.is_none());
        assert_eq!(args.lcov, [PathBuf::from("one.info")]);
        assert_eq!(args.packages, ["alpha"]);
        assert!(args.quiet);
    }

    #[test]
    fn run_accepts_collection_and_global_evaluation_options() {
        let CargoCli::CoverageGate(args) = CargoCli::try_parse_from([
            "cargo",
            "coverage-gate",
            "run",
            "--package-file",
            "packages.txt",
            "--configuration",
            "all-features",
            "--jobs",
            "4",
            "--coverage-dir",
            "coverage",
            "--package",
            "alpha",
            "--target",
            "x86_64-unknown-linux-gnu",
        ])
        .expect("run invocation must parse");

        let CoverageGateCommand::Run(run) = args.command.expect("run subcommand must be selected");
        assert_eq!(run.package_file, Some(PathBuf::from("packages.txt")));
        assert_eq!(run.configurations, [FeatureConfiguration::AllFeatures]);
        assert_eq!(run.jobs.map(NonZeroUsize::get), Some(4));
        assert_eq!(run.coverage_dir, PathBuf::from("coverage"));
        assert_eq!(args.packages, ["alpha"]);
        assert_eq!(args.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }
}
