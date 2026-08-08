// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that ensures every dependency in a `Cargo.toml` file is declared
//! with `default-features = false`.
//!
//! Enabling default features by accident pulls in code you never asked for, which inflates
//! build times, binary size, and the dependency surface that must be audited. This tool
//! makes that mistake a build break instead of a silent regression.
//!
//! If both `[workspace.dependencies]` and `[dependencies]` are present in the same
//! manifest, both sections are checked. Dependencies that use `workspace = true` are
//! skipped, since they inherit their settings from the workspace.
//!
//! # Usage
//!
//! Run this command in a cargo workspace or crate directory:
//!
//! ```bash
//! cargo ensure-no-default-features
//! ```
//!
//! The `--manifest-path` option lets you specify an explicit `Cargo.toml` file to check.
//! Without this option, it defaults to the `Cargo.toml` in the current directory.
//!
//! The `--exceptions` (`-e`) option lets you specify a comma-separated list of dependencies
//! to exclude from the `default-features` check. This is useful for dependencies that you
//! explicitly want to have default features enabled.
//!
//! ```bash
//! cargo ensure-no-default-features --manifest-path path/to/Cargo.toml --exceptions serde,tokio
//! ```
//!
//! # Installation
//!
//! ```bash
//! cargo install cargo-ensure-no-default-features
//! ```
//!
//! # Example Output
//!
//! When offending dependencies are found:
//!
//! ```text
//! Found 1 dependencies without default-features = false:
//!
//!   - 'serde': missing default-features = false
//! ```
//!
//! When everything checks out:
//!
//! ```text
//! All required dependencies have default-features = false
//! ```
//!
//! The tool exits with code 0 if all dependencies are well-formed, or code 1 otherwise.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod validation;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use validation::validate_dependencies;

const CLAP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Cargo subcommand to ensure dependencies have `default-features = false`
#[derive(Parser, Debug)]
#[command(bin_name = "cargo", version, about, author)]
#[command(styles = CLAP_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Ensure all dependencies have `default-features = false`
    #[command(version, display_name = "cargo-ensure-no-default-features")]
    EnsureNoDefaultFeatures {
        /// Path to Cargo.toml
        #[arg(long, default_value = "Cargo.toml", value_name = "PATH")]
        manifest_path: PathBuf,

        /// List of dependencies to exclude from default-features check
        #[arg(long, short = 'e', value_delimiter = ',')]
        exceptions: Option<Vec<String>>,
    },
}

/// Main entry point for the library, called from the binary crate.
///
/// Returns [`ExitCode::SUCCESS`] when every dependency is declared with
/// `default-features = false` and [`ExitCode::FAILURE`] otherwise. Returning an
/// exit code (rather than calling `std::process::exit`) lets `main` unwind
/// normally so the process terminates through the standard runtime path --
/// important under coverage instrumentation, where an abrupt `process::exit`
/// skips the profile flush on some platforms (notably Windows).
///
/// # Errors
///
/// Returns an error if the manifest cannot be read or parsed, or if it contains
/// no dependency section to check.
pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let Commands::EnsureNoDefaultFeatures { manifest_path, exceptions } = cli.command;

    let content = std::fs::read_to_string(&manifest_path).with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let exceptions = exceptions.unwrap_or_default();

    let (errors, found_deps, checked_sections) = validate_dependencies(&content, &exceptions)?;
    if !errors.is_empty() {
        eprintln!("❌ Found {} dependencies without default-features = false:\n", errors.len());
        for error in &errors {
            eprintln!("{error}");
        }

        return Ok(ExitCode::FAILURE);
    }

    // Warn if any exception was not found in the dependencies
    let sections_label = checked_sections.join(" or ");
    for exception in &exceptions {
        if !found_deps.contains(exception) {
            eprintln!("⚠️ Warning: exception '{exception}' was not found in {sections_label}");
        }
    }

    println!("✅ All required dependencies have default-features = false");

    Ok(ExitCode::SUCCESS)
}
