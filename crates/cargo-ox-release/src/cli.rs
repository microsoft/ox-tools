// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface for `cargo-ox-release`.
//!
//! The binary is invoked as `cargo ox-release <command>`. Cargo passes
//! `ox-release` as the first argument; it is stripped before parsing.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ohno::{AppError, app_err};

use crate::model::{Facts, Request};

/// Parsed top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-ox-release",
    bin_name = "cargo ox-release",
    about = "Deterministic release planner for Oxidizer-style Cargo workspaces",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The available subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Resolve a deterministic release plan from facts and a classified
    /// request, printing the canonical plan JSON to stdout.
    Resolve(ResolveArgs),
}

/// Arguments for `resolve`.
#[derive(Debug, clap::Args)]
struct ResolveArgs {
    /// Path to the facts JSON emitted by fact-gathering.
    #[arg(long, value_name = "FILE")]
    facts: PathBuf,

    /// Path to the request JSON (mode, tokens, selection decisions,
    /// classifications, macro contracts).
    #[arg(long, value_name = "FILE")]
    request: PathBuf,
}

impl Cli {
    /// Parses the CLI from cargo's argv, dropping the leading `ox-release`
    /// token cargo injects for a subcommand invocation.
    fn parse_from_cargo_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut iter = args.into_iter().map(Into::<std::ffi::OsString>::into);
        let exe = iter.next();
        let mut rest: Vec<std::ffi::OsString> = iter.collect();
        if rest.first().is_some_and(|a| a == "ox-release") {
            rest.remove(0);
        }
        Self::try_parse_from(exe.into_iter().chain(rest))
    }
}

/// Reads and deserializes a JSON document, attributing failures to `label`.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path, label: &str) -> Result<T, AppError> {
    let text = std::fs::read_to_string(path).map_err(|e| app_err!("failed to read {label} '{}': {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| app_err!("failed to parse {label} '{}': {e}", path.display()))
}

/// Executes a parsed CLI.
fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Command::Resolve(args) => {
            let facts: Facts = read_json(&args.facts, "facts")?;
            let request: Request = read_json(&args.request, "request")?;
            let plan = crate::resolve(&facts, &request)?;
            let rendered = serde_json::to_string_pretty(&plan).map_err(|e| app_err!("failed to serialize the resolved plan: {e}"))?;
            println!("{rendered}");
            Ok(())
        }
    }
}

/// Entry point mapping the CLI outcome to a process exit code.
///
/// Parses `std::env::args_os` and runs. Clap parse errors (including
/// `--help`/`--version`) are printed by clap and mapped to their conventional
/// exit code.
///
/// This is the binary's adapter, not part of the `facts + request → plan`
/// library API.
#[doc(hidden)]
#[must_use]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run_main() -> ExitCode {
    let cli = match Cli::parse_from_cargo_args(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            // Clap already rendered the message; honor its intended exit code.
            let _ = err.print();
            return if err.use_stderr() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_subcommand_token() {
        let cli = Cli::parse_from_cargo_args([
            "cargo-ox-release",
            "ox-release",
            "resolve",
            "--facts",
            "f.json",
            "--request",
            "r.json",
        ])
        .unwrap();
        let Command::Resolve(args) = cli.command;
        assert_eq!(args.facts, PathBuf::from("f.json"));
        assert_eq!(args.request, PathBuf::from("r.json"));
    }

    #[test]
    fn parses_without_subcommand_token() {
        let cli = Cli::parse_from_cargo_args(["cargo-ox-release", "resolve", "--facts", "f.json", "--request", "r.json"]).unwrap();
        let Command::Resolve(args) = cli.command;
        assert_eq!(args.request, PathBuf::from("r.json"));
    }

    #[test]
    fn missing_required_argument_is_an_error() {
        let err = Cli::parse_from_cargo_args(["cargo-ox-release", "resolve", "--facts", "f.json"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
