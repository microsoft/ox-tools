// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! Portable setup helpers for cargo-spellcheck.
//!
//! Prepare a repository word list in Hunspell's dictionary format and check
//! the native prerequisites needed to build cargo-spellcheck from source,
//! without PowerShell. The helper does not run the spellchecker or install
//! system packages, and does not itself require libclang.
//!
//! # Installation
//!
//! ```text
//! cargo install cargo-spellcheck-helper
//! ```
//!
//! # Usage
//!
//! ```text
//! cargo spellcheck-helper prepare-dictionary --input .spelling --output target/spelling.dic
//! cargo spellcheck-helper check-source-deps
//! ```
//!
//! Direct invocation as `cargo-spellcheck-helper` also works.
//!
//! Dictionary preparation uses `hunspell-dictgen`: missing input produces an
//! empty dictionary, empty and ASCII-decimal-only lines are removed, entries
//! are sorted with duplicates retained, and UTF-8/LF output is published
//! atomically.
//!
//! The prerequisite check models native default-feature cargo-spellcheck
//! 0.15.7 installation. It follows clang-sys's link-time libclang discovery:
//! an authoritative file or directory in `LIBCLANG_PATH`, LLVM/Xcode discovery,
//! `LD_LIBRARY_PATH`, and platform search patterns. ELF/PE architecture,
//! Windows MSVC import libraries and DLL `PATH`, and dynamic loadability are
//! checked. Linux also checks the selected library's ELF SONAME (or filename
//! when absent) through native runtime lookup before loading its absolute path.
//! C/C++ compiler and archiver availability use `cc` discovery.
//! Missing prerequisites produce installation guidance; the actual dependency
//! build remains authoritative for headers, SDKs, and final linking.
//!
//! Success exits with code 0 and produces no stdout. Operational failures
//! exit with code 1 and a diagnostic on stderr; argument errors exit with
//! code 2.

mod source_deps;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ohno::{AppError, IntoAppError};

#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, tag = "native-boundary", reason = "Only wires process arguments, native operations and exit reporting; in-process tests cover argument adaptation and command dispatch without spawning the CLI.")]
fn main() -> ExitCode {
    let cli = Cli::parse_from(command_args(std::env::args_os()));
    match run_with(&cli, hunspell_dictgen::generate, source_deps::check) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn command_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args: Vec<_> = args.into_iter().collect();
    if args.get(1).is_some_and(|arg| arg == "spellcheck-helper") {
        args.remove(1);
    }
    args
}

fn run_with<E: std::error::Error + Send + Sync + 'static>(
    cli: &Cli,
    generate: impl FnOnce(&Path, &Path) -> Result<(), E>,
    check: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    match &cli.command {
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes contextual prose; the test checks the preserved typed generator failure.")]
        HelperCommand::PrepareDictionary { input, output } => generate(input, output).into_app_err("failed to prepare Hunspell dictionary"),
        HelperCommand::CheckSourceDeps => check(),
    }
}

/// Portable dictionary preparation and source prerequisite checks for cargo-spellcheck.
#[derive(Debug, Parser)]
#[command(name = "cargo-spellcheck-helper", version)]
struct Cli {
    #[command(subcommand)]
    command: HelperCommand,
}

#[derive(Debug, Subcommand)]
enum HelperCommand {
    /// Generate a Hunspell dictionary from a UTF-8 word list.
    PrepareDictionary {
        /// Path to the human-maintained word list; missing input is allowed.
        #[arg(long, value_name = "PATH")]
        input: PathBuf,

        /// Destination dictionary path.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
    },
    /// Check libclang source-build prerequisites without installing system packages.
    CheckSourceDeps,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::io;

    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(command_args(args.iter().map(OsString::from))).expect("valid command arguments should parse")
    }

    #[test]
    fn cargo_and_direct_invocations_dispatch_dictionary_paths() {
        for args in [
            vec!["cargo-spellcheck-helper", "prepare-dictionary"],
            vec!["cargo-spellcheck-helper", "spellcheck-helper", "prepare-dictionary"],
        ] {
            let mut args = args;
            args.extend(["--input", "words with spaces.txt", "--output", "output with spaces.dic"]);
            let cli = parse(&args);
            run_with(
                &cli,
                |input, output| {
                    assert_eq!(input, Path::new("words with spaces.txt"));
                    assert_eq!(output, Path::new("output with spaces.dic"));
                    Ok::<(), io::Error>(())
                },
                || panic!("dictionary command must not probe source dependencies"),
            )
            .unwrap();
        }
    }

    #[test]
    fn cargo_argument_adaptation_preserves_executable_and_remaining_arguments() {
        let original = ["helper executable", "spellcheck-helper", "check-source-deps", "--help"].map(OsString::from);
        assert_eq!(
            command_args(original),
            ["helper executable", "check-source-deps", "--help"].map(OsString::from)
        );
        assert_eq!(
            command_args([OsString::from("helper executable")]),
            [OsString::from("helper executable")]
        );
        assert_eq!(command_args([]), Vec::<OsString>::new());
    }

    #[test]
    fn source_check_dispatch_preserves_failure() {
        let cli = parse(&["cargo-spellcheck-helper", "spellcheck-helper", "check-source-deps"]);
        let error = run_with(
            &cli,
            |_, _| -> Result<(), io::Error> { panic!("source check must not generate a dictionary") },
            || Err(AppError::new("install LLVM")),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "install LLVM");
    }

    #[test]
    fn source_check_dispatch_preserves_success() {
        let cli = parse(&["cargo-spellcheck-helper", "check-source-deps"]);
        run_with(
            &cli,
            |_, _| -> Result<(), io::Error> { panic!("source check must not generate a dictionary") },
            || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn dictionary_failure_retains_underlying_cause() {
        let cli = parse(&[
            "cargo-spellcheck-helper",
            "prepare-dictionary",
            "--input",
            ".spelling",
            "--output",
            "spelling.dic",
        ]);
        let error = run_with(
            &cli,
            |_, _| Err(io::Error::new(io::ErrorKind::PermissionDenied, "dictionary output denied")),
            || panic!("dictionary command must not probe source dependencies"),
        )
        .unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn dictionary_paths_and_subcommand_are_required() {
        for args in [
            vec!["cargo-spellcheck-helper"],
            vec!["cargo-spellcheck-helper", "prepare-dictionary", "--input", ".spelling"],
            vec!["cargo-spellcheck-helper", "prepare-dictionary", "--output", "spelling.dic"],
            vec!["cargo-spellcheck-helper", "unknown"],
        ] {
            let error = Cli::command().term_width(80).try_get_matches_from(args).unwrap_err();
            assert!(matches!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
                    | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | clap::error::ErrorKind::InvalidSubcommand
            ));
        }
    }
}
