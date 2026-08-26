// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that ensures every `[workspace.dependencies]` entry is
//! inherited by at least one workspace member.
#![doc(
    html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-ensure-no-unused-workspace-deps/logo.png"
)]
#![doc(
    html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-ensure-no-unused-workspace-deps/favicon.ico"
)]
//!
//! A workspace root declares a dependency catalog that members draw from with
//! `dep = { workspace = true }`. Nothing requires an entry to be drawn from, so
//! an entry nobody inherits stays in the manifest forever: it never enters the
//! dependency graph, and no build fails because of it. It still carries a
//! version requirement, so it keeps attracting dependency-bump traffic and keeps
//! misleading readers about what the workspace depends on.
//!
//! Unused-dependency tools resolve the crate graph and ask which *declared*
//! dependencies go unused, so an entry that no member declares is invisible to
//! them. This tool answers the prior question -- is the entry inherited at all?
//! -- from the manifests alone, which makes it free of false positives and cheap
//! enough to run on every pull request.
//!
//! # Usage
//!
//! Run in a cargo workspace:
//!
//! ```bash
//! cargo ensure-no-unused-workspace-deps
//! ```
//!
//! Remove what it finds:
//!
//! ```bash
//! cargo ensure-no-unused-workspace-deps --fix
//! ```
//!
//! `--manifest-path` points at an explicit workspace root, defaulting to the
//! `Cargo.toml` in the current directory. A manifest with no `[workspace]` table
//! declares no catalog and passes with a note; `--require-workspace` turns that
//! into an error for callers that know they are pointing at a workspace root.
//!
//! # Configuration
//!
//! An entry kept on purpose is exempted in the workspace manifest:
//!
//! ```toml
//! [workspace.metadata.ensure-no-unused-workspace-deps]
//! allowed = ["kept-on-purpose"]
//! ```
//!
//! An `allowed` name that suppresses nothing is reported as stale, on stderr,
//! without failing the run.
//!
//! # Installation
//!
//! ```bash
//! cargo install cargo-ensure-no-unused-workspace-deps
//! ```
//!
//! # Example output
//!
//! ```text
//! Found 2 unused workspace dependencies in Cargo.toml:
//!
//!   - once_cell
//!   - smallvec
//!
//! Re-run with --fix to remove them.
//! ```

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod detect;
mod fix;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use cargo_metadata::MetadataCommand;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};

use crate::detect::{Catalog, WorkspaceCatalog};

const CLAP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Cargo subcommand to ensure every workspace dependency is inherited.
#[derive(Parser, Debug)]
#[command(bin_name = "cargo", version, about, author)]
#[command(styles = CLAP_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Ensure every `[workspace.dependencies]` entry is inherited by a member
    #[command(version, display_name = "cargo-ensure-no-unused-workspace-deps")]
    EnsureNoUnusedWorkspaceDeps {
        /// Path to the workspace root Cargo.toml
        #[arg(long, default_value = "Cargo.toml", value_name = "PATH")]
        manifest_path: PathBuf,

        /// Remove the unused entries instead of only reporting them
        #[arg(long)]
        fix: bool,

        /// Treat a manifest with no [workspace] table as an error
        #[arg(long)]
        require_workspace: bool,
    },
}

/// Main entry point for the library, called from the binary crate.
///
/// Returns [`ExitCode::SUCCESS`] when every catalog entry is inherited by at
/// least one member -- or, under `--fix`, once the entries that were not have
/// been removed -- and [`ExitCode::FAILURE`] otherwise. Returning an exit code
/// (rather than calling `std::process::exit`) lets `main` unwind normally so the
/// process terminates through the standard runtime path -- important under
/// coverage instrumentation, where an abrupt `process::exit` skips the profile
/// flush on some platforms (notably Windows).
///
/// # Errors
///
/// Returns an error if a manifest cannot be read or parsed, if the workspace
/// members cannot be enumerated, or if a fixed manifest cannot be written back.
pub fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let Commands::EnsureNoUnusedWorkspaceDeps {
        manifest_path,
        fix,
        require_workspace,
    } = cli.command;

    check(&manifest_path, fix, require_workspace)
}

/// The check itself, split from [`run`] so tests can drive it without a process
/// boundary or a parsed command line.
fn check(manifest_path: &Path, fix: bool, require_workspace: bool) -> Result<ExitCode> {
    let mut manifest = detect::read_manifest(manifest_path)?;

    let catalog = match detect::catalog(&manifest) {
        Catalog::Workspace(catalog) => catalog,
        Catalog::NotAWorkspace => {
            if require_workspace {
                eprintln!("❌ {} has no [workspace] table.", manifest_path.display());
                return Ok(ExitCode::FAILURE);
            }

            eprintln!(
                "ℹ️ {} has no [workspace] table; there is no dependency catalog to check.",
                manifest_path.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
    };

    if catalog.declared.is_empty() {
        println!("✅ {} declares no workspace dependencies.", manifest_path.display());
        return Ok(ExitCode::SUCCESS);
    }

    let members = members_of(manifest_path)?;
    let used = detect::inherited(&members)?;
    let (unused, stale) = detect::partition(&catalog, &used);

    report_stale(&stale);

    if unused.is_empty() {
        report_clean(manifest_path, &catalog, &used, members.len());
        return Ok(ExitCode::SUCCESS);
    }

    if !fix {
        report_unused(manifest_path, &unused);
        return Ok(ExitCode::FAILURE);
    }

    let removed = fix::remove(&mut manifest, &unused);
    // Formatted eagerly rather than in a `with_context` closure: the closure
    // only runs when the write fails, which no test can force portably.
    let failure = format!("failed to write {}", manifest_path.display());
    std::fs::write(manifest_path, manifest.to_string()).context(failure)?;

    println!(
        "🧹 Removed {removed} unused workspace {} from {}.",
        entries(removed),
        manifest_path.display()
    );

    Ok(ExitCode::SUCCESS)
}

/// Manifest paths of every workspace member, as Cargo resolves them.
///
/// Deferring to `cargo metadata` rather than re-deriving `members`, its globs
/// and `exclude` keeps this in step with Cargo itself; a member missed here
/// would look like an entry nobody inherits.
fn members_of(manifest_path: &Path) -> Result<Vec<PathBuf>> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()
        .with_context(|| format!("failed to enumerate the workspace members of {}", manifest_path.display()))?;

    Ok(metadata
        .workspace_packages()
        .into_iter()
        .map(|package| package.manifest_path.clone().into_std_path_buf())
        .collect())
}

/// Report allow-list entries that suppressed nothing.
fn report_stale(stale: &[String]) {
    for name in stale {
        eprintln!("⚠️ '{name}' is allowed but is inherited or not declared; the allow-list entry can be removed.");
    }
}

/// Report a catalog in which every entry is inherited or allowed.
fn report_clean(manifest_path: &Path, catalog: &WorkspaceCatalog, used: &BTreeSet<String>, members: usize) {
    let declared = catalog.declared.len();
    let inherited = catalog.declared.iter().filter(|name| used.contains(name.as_str())).count();

    if declared == inherited {
        println!(
            "✅ All {declared} workspace {} in {} are inherited by one of {members} members.",
            entries(declared),
            manifest_path.display()
        );
    } else {
        println!(
            "✅ All {declared} workspace {} in {} are inherited by one of {members} members or explicitly allowed.",
            entries(declared),
            manifest_path.display()
        );
    }
}

/// Report the entries no member inherits.
fn report_unused(manifest_path: &Path, unused: &[String]) {
    eprintln!(
        "❌ Found {} unused workspace {} in {}:\n",
        unused.len(),
        entries(unused.len()),
        manifest_path.display()
    );
    for name in unused {
        eprintln!("  - {name}");
    }
    eprintln!("\nRe-run with --fix to remove them.");
}

/// Pluralize `dependency` for `count`.
fn entries(count: usize) -> &'static str {
    if count == 1 { "dependency" } else { "dependencies" }
}
