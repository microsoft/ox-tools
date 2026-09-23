// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that fails when two workspace packages own targets which
//! uplift to the same build artifact.
#![doc(
    html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-unique-target-names/logo.png"
)]
#![doc(
    html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-unique-target-names/favicon.ico"
)]
//!
//! Cargo compiles each unit into `target/<profile>/deps/` under a
//! metadata-hashed name, then uplifts the root unit of most target kinds to
//! `target/<profile>/` (examples to `target/<profile>/examples/`) under a
//! plain name carrying no hash. Two workspace packages whose targets uplift to
//! same file therefore write the same file. Cargo reports this as `output
//! filename collision`, warns that it may become a hard error, and keeps
//! building -- so the workspace stays green while the last writer wins and
//! concurrent jobs race for one path. This tool turns that warning into a
//! failure before anything is compiled.
//!
//! The most likely way to hit it needs no configuration at all: Cargo
//! normalizes `-` to `_` in the default library target name, so packages
//! `foo-bar` and `foo_bar` in one workspace both uplift to `libfoo_bar.rlib`.
//!
//! # Usage
//!
//! Run this command in a cargo workspace or crate directory:
//!
//! ```bash
//! cargo unique-target-names
//! ```
//!
//! The `--manifest-path` option lets you point at an explicit `Cargo.toml`.
//! Without it, the manifest is discovered from the current directory.
//!
//! # Installation
//!
//! ```bash
//! cargo install cargo-unique-target-names
//! ```
//!
//! # Example Output
//!
//! When two packages contend for one file:
//!
//! ```text
//! cargo-unique-target-names: target 'basic' is declared by 2 targets: metabench (example 'basic'), observed (example 'basic')
//!   they uplift to the same files: target/<profile>/examples/basic.pdb, target/<profile>/examples/basic[.exe]
//!
//! Rename the reported targets so each one uplifts to its own path.
//! ```
//!
//! When everything checks out:
//!
//! ```text
//! All workspace targets uplift to their own path
//! ```
//!
//! The tool exits with code 0 when every uplifted file has one owner, code 1
//! when at least one is contended, and code 2 when the workspace could not be
//! read at all -- so a broken workspace is distinguishable from a finding.
//!
//! # What is and is not reported
//!
//! Targets are keyed by *every* file they uplift, because the relationship
//! between crate type and file runs both ways:
//!
//! - `cdylib`, `dylib`, and `proc-macro` all emit one platform shared library,
//!   so a collision crosses those crate types;
//! - an executable and a shared library emit different primary files
//!   (`tool.exe`, `tool.dll`) but both write `tool.pdb`;
//! - an `rlib` and a `staticlib` emit no uplifted debug-info file, so they
//!   coexist with a binary of the same name.
//!
//! Test and benchmark binaries and build scripts keep their metadata hash in
//! `deps/` and are exempt. Owners are keyed per target rather than per package,
//! so a package that contends with itself — Cargo permits a `[lib]` and a
//! `[[bin]]` of one name, and they share a debug-info file — is reported like
//! any other pair.
//!
//! The Windows debug-info file is considered on every platform, so a
//! Windows-only collision still fails a Linux run and every leg of a build
//! matrix agrees.
//!
//! Two hazards Cargo itself does not warn about are deliberately not reported:
//! dep-info files, which collapse by file stem, and target names differing only
//! in case on a case-insensitive filesystem. Both are recorded in the crate's
//! design document.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod collisions;

use std::path::PathBuf;
use std::process::ExitCode;

use cargo_metadata::MetadataCommand;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};

/// Exit code for a workspace whose metadata could not be read, kept distinct
/// from the collision code so a caller can tell "broken" from "contended".
pub const EXIT_UNREADABLE_WORKSPACE: u8 = 2;

const CLAP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Cargo subcommand that keeps uplifted target names unique across a workspace.
#[derive(Parser, Debug)]
#[command(bin_name = "cargo", version, about, author)]
#[command(styles = CLAP_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Check that no two workspace targets uplift to the same build artifact.
    #[command(version, display_name = "cargo-unique-target-names")]
    UniqueTargetNames(Args),
}

#[derive(Parser, Debug)]
struct Args {
    /// Path to the `Cargo.toml` to inspect.
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,
}

/// Runs the check and returns the process exit code.
///
/// Returns [`ExitCode::SUCCESS`] when every uplifted file has one owner,
/// [`ExitCode::FAILURE`] when at least one is contended, and
/// [`EXIT_UNREADABLE_WORKSPACE`] when the workspace could not be read at all --
/// a broken workspace must not be reportable as clean, and must be
/// distinguishable from a genuine finding.
#[must_use]
pub fn run() -> ExitCode {
    let Commands::UniqueTargetNames(args) = Cli::parse().command;

    let mut command = MetadataCommand::new();
    command.no_deps();
    if let Some(path) = args.manifest_path {
        command.manifest_path(path);
    }
    let metadata = match command.exec() {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("cargo-unique-target-names: failed to read workspace metadata from cargo: {error}");
            return ExitCode::from(EXIT_UNREADABLE_WORKSPACE);
        }
    };

    let collisions = collisions::find(&metadata);
    if collisions.is_empty() {
        println!("All workspace targets uplift to their own path");
        return ExitCode::SUCCESS;
    }

    for collision in &collisions {
        eprintln!("cargo-unique-target-names: {}", collision.render());
    }
    eprintln!("\nRename the reported targets so each one uplifts to its own path.");
    ExitCode::FAILURE
}
