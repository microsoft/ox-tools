// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface for `cargo-anvil`.
//!
//! The entry binary is invoked as `cargo anvil …`. Cargo passes
//! `anvil` as the first argument; we strip it and parse the
//! remainder.

use clap::{CommandFactory, FromArgMatches, Parser};

use crate::catalog::Catalog;

/// Parsed top-level CLI.
///
/// Constructed via [`Cli::parse_from_cargo_args`], which strips the leading
/// `anvil` token that Cargo injects when the binary is invoked as a
/// subcommand.
///
/// The tool intentionally has a single action (update local recipes,
/// cloud-workflow building blocks, and managed regions), so the flags live at the top
/// level rather than under a subcommand.
#[derive(Debug, Parser, Clone, Default)]
#[command(
    name = "cargo-anvil",
    bin_name = "cargo anvil",
    about = "Update local recipes, cloud-workflow building blocks, and managed regions for the anvil unified build setup",
    version,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// cloud-workflow backend(s) to emit. Repeatable. Valid values: `github`, `ado`.
    ///
    /// If omitted and `--no-backends` is not set, the backend is autodetected
    /// from the `origin` git remote.
    #[arg(long = "backend", value_name = "NAME")]
    pub backends: Vec<String>,

    /// Emit only local files; skip every cloud-workflow backend.
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
    /// passes to its subcommand binaries, rendering the command's name,
    /// `about`, and version from the catalog's [`crate::CliMeta`].
    ///
    /// Cargo invokes `cargo-<sub> <sub> <args…>` when the user types
    /// `cargo <sub> <args…>`. We drop the leading `<sub>` token (the
    /// catalog's `subcommand`) if present so that clap sees a normal argv.
    ///
    /// # Errors
    ///
    /// Returns clap's parse error (typically with an exit code already
    /// encoded) on invalid input.
    pub fn parse_from_cargo_args<I, T>(catalog: &Catalog, args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let meta = catalog.cli();
        let mut iter = args.into_iter().map(Into::<std::ffi::OsString>::into);
        let exe = iter.next();
        let mut rest: Vec<std::ffi::OsString> = iter.collect();
        if rest.first().is_some_and(|a| a == meta.subcommand.as_str()) {
            rest.remove(0);
        }
        let argv_iter = exe.into_iter().chain(rest);

        // clap interns command metadata as `&'static str`. The catalog
        // outlives the single parse in `run_app`, so leaking these few
        // small strings is acceptable and keeps the metadata catalog-driven.
        let bin_name: &'static str = String::leak(meta.bin_name.clone());
        let usage_name: &'static str = String::leak(format!("cargo {}", meta.subcommand));
        let about: &'static str = String::leak(meta.about.clone());
        let version: &'static str = String::leak(meta.version.clone());

        let command = Self::command()
            .name(bin_name)
            .bin_name(usage_name)
            .about(about)
            .long_about(about)
            .version(version);
        let matches = command.try_get_matches_from(argv_iter)?;
        Self::from_arg_matches(&matches)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[test]
    fn parse_no_args() {
        let cli = Cli::parse_from(["cargo-anvil"]);
        assert!(cli.backends.is_empty());
        assert!(!cli.no_backends);
        assert!(!cli.dry_run);
    }

    #[test]
    fn parse_dry_run() {
        let cli = Cli::parse_from(["cargo-anvil", "--dry-run"]);
        assert!(cli.dry_run);
    }

    #[test]
    fn parse_single_backend() {
        let cli = Cli::parse_from(["cargo-anvil", "--backend", "github"]);
        assert_eq!(cli.backends, vec!["github"]);
    }

    #[test]
    fn parse_multiple_backends() {
        let cli = Cli::parse_from(["cargo-anvil", "--backend", "github", "--backend", "ado"]);
        assert_eq!(cli.backends, vec!["github", "ado"]);
    }

    #[test]
    fn parse_no_backends() {
        let cli = Cli::parse_from(["cargo-anvil", "--no-backends"]);
        assert!(cli.no_backends);
    }

    #[test]
    fn backend_and_no_backends_conflict() {
        let err = Cli::try_parse_from(["cargo-anvil", "--backend", "github", "--no-backends"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parse_from_cargo_args_strips_subcommand_token() {
        let catalog = crate::catalog::Catalog::anvil();
        let cli = Cli::parse_from_cargo_args(&catalog, ["cargo-anvil", "anvil", "--dry-run"]).unwrap();
        assert!(cli.dry_run);
    }

    #[test]
    fn parse_from_cargo_args_works_without_subcommand_token() {
        let catalog = crate::catalog::Catalog::anvil();
        let cli = Cli::parse_from_cargo_args(&catalog, ["cargo-anvil", "--dry-run"]).unwrap();
        assert!(cli.dry_run);
    }

    #[test]
    fn unknown_backend_value_accepted_at_parse_time() {
        // Validation of backend names is the resolver's job, not clap's.
        let cli = Cli::parse_from(["cargo-anvil", "--backend", "weird"]);
        assert_eq!(cli.backends, vec!["weird"]);
    }
}
