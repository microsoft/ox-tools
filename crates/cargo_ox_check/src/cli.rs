// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface for `cargo-ox-check`.
//!
//! The entry binary is invoked as `cargo ox-check …`. Cargo passes `ox-check`
//! as the first argument; we strip it and parse the remainder.

use clap::{Args, Parser, Subcommand};

/// Parsed top-level CLI.
///
/// Constructed via [`Cli::parse_from_cargo_args`], which strips the leading
/// `ox-check` token that Cargo injects when the binary is invoked as a
/// subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-ox-check",
    bin_name = "cargo ox-check",
    about = "Opinionated, unified Rust build/CI scaffolding",
    version,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
///
/// Only `update` exists by design — see [design.md §5.2](../docs/design/design.md).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Update local recipes, CI building blocks, and managed regions.
    Update(UpdateArgs),
}

/// Arguments for the `update` subcommand.
#[derive(Debug, Args, Clone, Default)]
pub struct UpdateArgs {
    /// CI backend(s) to emit. Repeatable. Valid values: `github`, `ado`.
    ///
    /// If omitted and `--no-backends` is not set, the backend is autodetected
    /// from the `origin` git remote.
    #[arg(long = "backend", value_name = "NAME")]
    pub backends: Vec<String>,

    /// Emit only local files; skip every CI backend.
    ///
    /// Mutually exclusive with `--backend`.
    #[arg(long, conflicts_with = "backends")]
    pub no_backends: bool,

    /// Analyze and report without writing any files.
    ///
    /// Exits with code 1 if anything would be written or proposed.
    #[arg(long)]
    pub dry_run: bool,
}

impl Cli {
    /// Parse the CLI from the raw `std::env::args_os` iterator that cargo
    /// passes to its subcommand binaries.
    ///
    /// Cargo invokes `cargo-ox-check ox-check <args…>` when the user types
    /// `cargo ox-check <args…>`. We drop the `ox-check` token if present so
    /// that clap sees a normal argv.
    ///
    /// # Errors
    ///
    /// Returns clap's parse error (typically with an exit code already
    /// encoded) on invalid input.
    pub fn parse_from_cargo_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut iter = args.into_iter().map(Into::<std::ffi::OsString>::into);
        let exe = iter.next();
        let mut rest: Vec<std::ffi::OsString> = iter.collect();
        if rest.first().is_some_and(|a| a == "ox-check") {
            rest.remove(0);
        }
        let argv_iter = exe.into_iter().chain(rest);
        Self::try_parse_from(argv_iter)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn parse_update_no_args() {
        let cli = Cli::parse_from(["cargo-ox-check", "update"]);
        let Command::Update(args) = cli.command;
        assert!(args.backends.is_empty());
        assert!(!args.no_backends);
        assert!(!args.dry_run);
    }

    #[test]
    fn parse_update_dry_run() {
        let cli = Cli::parse_from(["cargo-ox-check", "update", "--dry-run"]);
        let Command::Update(args) = cli.command;
        assert!(args.dry_run);
    }

    #[test]
    fn parse_update_single_backend() {
        let cli = Cli::parse_from(["cargo-ox-check", "update", "--backend", "github"]);
        let Command::Update(args) = cli.command;
        assert_eq!(args.backends, vec!["github"]);
    }

    #[test]
    fn parse_update_multiple_backends() {
        let cli = Cli::parse_from([
            "cargo-ox-check",
            "update",
            "--backend",
            "github",
            "--backend",
            "ado",
        ]);
        let Command::Update(args) = cli.command;
        assert_eq!(args.backends, vec!["github", "ado"]);
    }

    #[test]
    fn parse_update_no_backends() {
        let cli = Cli::parse_from(["cargo-ox-check", "update", "--no-backends"]);
        let Command::Update(args) = cli.command;
        assert!(args.no_backends);
    }

    #[test]
    fn backend_and_no_backends_conflict() {
        let err = Cli::try_parse_from([
            "cargo-ox-check",
            "update",
            "--backend",
            "github",
            "--no-backends",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parse_from_cargo_args_strips_ox_check_token() {
        let cli =
            Cli::parse_from_cargo_args(["cargo-ox-check", "ox-check", "update", "--dry-run"]).unwrap();
        let Command::Update(args) = cli.command;
        assert!(args.dry_run);
    }

    #[test]
    fn parse_from_cargo_args_works_without_ox_check_token() {
        let cli = Cli::parse_from_cargo_args(["cargo-ox-check", "update", "--dry-run"]).unwrap();
        let Command::Update(args) = cli.command;
        assert!(args.dry_run);
    }

    #[test]
    fn missing_subcommand_fails() {
        let err = Cli::try_parse_from(["cargo-ox-check"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn unknown_backend_value_accepted_at_parse_time() {
        // Validation of backend names is the resolver's job, not clap's.
        let cli = Cli::parse_from(["cargo-ox-check", "update", "--backend", "weird"]);
        let Command::Update(args) = cli.command;
        assert_eq!(args.backends, vec!["weird"]);
    }
}
