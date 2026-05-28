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
    /// Path to the cargo-llvm-cov lcov tracefile.
    ///
    /// Defaults to `target/coverage/lcov.info`, matching the recommended
    /// `cargo llvm-cov report --lcov --output-path` invocation.
    #[arg(long = "lcov", value_name = "PATH")]
    pub(crate) lcov: Option<PathBuf>,

    /// Restrict the operation to one or more package selectors.
    ///
    /// Accepts the same `-p` / `--package` idiom as `cargo build`:
    /// repeat the flag (`-p foo -p bar`) and/or use Unix glob patterns
    /// (`-p 'tokio-*'`, `-p 'ohno*'`). The legacy comma-separated form
    /// `--package foo,bar` is also accepted for back-compat. When unset,
    /// every workspace member is in scope. CI integrations typically
    /// pass the impacted-package list from their test-impact step.
    #[arg(
        long = "package",
        short = 'p',
        alias = "packages",
        value_name = "SPEC",
        value_delimiter = ','
    )]
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
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_well_formed() {
        CargoCli::command().debug_assert();
    }
}
