// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-coverage-gate`.

use std::path::PathBuf;

use clap::{Args, Parser};

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
    /// Path to the cargo-llvm-cov JSON report.
    ///
    /// Defaults to `target/coverage/coverage.json`, matching the
    /// recommended `cargo llvm-cov report --json --output-path` invocation.
    #[arg(long = "llvm-cov-json", value_name = "PATH")]
    pub(crate) llvm_cov_json: Option<PathBuf>,

    /// Restrict the operation to a comma-separated list of package names.
    ///
    /// When unset, every workspace member is in scope. CI integrations
    /// typically pass the impacted-package list from their test-impact step.
    #[arg(long = "packages", value_name = "NAME,NAME,...", value_delimiter = ',')]
    pub(crate) packages: Vec<String>,

    /// Write the Markdown verdict table to this file.
    ///
    /// When unset, the tool falls back to `$GITHUB_STEP_SUMMARY` and then
    /// `$COVERAGE_GATE_SUMMARY` (in that order) before giving up.
    #[arg(long, value_name = "PATH")]
    pub(crate) summary_file: Option<PathBuf>,

    /// Suppress stdout output (the summary file, if any, is still written).
    #[arg(long)]
    pub(crate) quiet: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_well_formed() {
        CargoCli::command().debug_assert();
    }
}
