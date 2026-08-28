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
//! # Fixing
//!
//! `--fix` replaces the manifest atomically -- a temporary file in the same
//! directory, renamed over the original, carrying the permissions of the
//! manifest it replaces and following a symlinked manifest to its target -- and
//! refuses to write at all if the file changed after it was read, so a
//! concurrent edit is never clobbered.
//!
//! Comments on a removed entry are carried to the next surviving entry, which
//! keeps a group header attached to the group it introduces. A note about one
//! specific dependency is indistinguishable from such a header, so every move
//! is reported on stderr: check that carried text still describes the entry it
//! landed on. Comments that cannot be placed -- the removal emptied the table,
//! or left a trailing survivor with nothing to append to -- are reported as
//! dropped.
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use cargo_metadata::MetadataCommand;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use tempfile::NamedTempFile;

use crate::detect::{Catalog, WorkspaceCatalog};
use crate::fix::Carry;

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
    let original = detect::read_manifest_text(manifest_path)?;
    let mut manifest = detect::parse_manifest(&original, manifest_path)?;

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
        // An empty catalog is the boundary where *every* allowed name
        // suppresses nothing, so the stale report is due here too.
        let (_, stale) = detect::partition(&catalog, &BTreeSet::new());
        report_stale(&stale);

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

    let outcome = fix::remove(&mut manifest, &unused);
    write_back(manifest_path, &original, &manifest.to_string())?;

    println!(
        "🧹 Removed {} unused workspace {} from {}.",
        outcome.removed,
        entries(outcome.removed),
        manifest_path.display()
    );
    report_carries(&outcome.carries);

    Ok(ExitCode::SUCCESS)
}

/// Replace `manifest_path` with `contents`, atomically and only if the file
/// still holds what was read.
///
/// The workspace root manifest is the one file whose loss breaks every other
/// tool in the repository, so it is never truncated in place: the replacement
/// is written to a temporary file in the same directory and renamed over the
/// original, which is atomic on one filesystem. `cargo metadata` runs between
/// the read and the write, and it is a child process, so that window is wide
/// enough for an editor to save into it -- hence the unchanged-input guard.
///
/// Replacing a file by rename brings the temporary file's identity with it, so
/// two properties an in-place write would have kept are restored deliberately:
///
/// - **Permissions.** A temporary file is created owner-only, and a rename
///   carries its mode rather than inheriting the target's, so the manifest's
///   own permissions are read first and applied to the replacement. Without
///   that, a world-readable manifest silently comes back owner-only, which git
///   does not track and the next differently-owned reader discovers the hard
///   way.
/// - **Symlinks.** A symlinked manifest is resolved first, so the rename lands
///   on the file the link points at and the indirection survives. Replacing the
///   link itself would quietly turn it into a regular file.
fn write_back(manifest_path: &Path, original: &str, contents: &str) -> Result<()> {
    // Eagerly formatted rather than built in `with_context` closures: those
    // closures only run on failures no test can force portably.
    let resolve_failure = format!("failed to resolve {}", manifest_path.display());
    let read_failure = format!("failed to re-read {} before writing it", manifest_path.display());
    let metadata_failure = format!("failed to read the permissions of {}", manifest_path.display());
    let write_failure = format!("failed to write {}", manifest_path.display());
    let permissions_failure = format!("failed to apply the permissions of {} to its replacement", manifest_path.display());
    let persist_failure = format!("failed to replace {}", manifest_path.display());

    // Follow a symlinked manifest through to its target, the way an in-place
    // write would have.
    let target = std::fs::canonicalize(manifest_path).context(resolve_failure)?;

    let current = std::fs::read_to_string(&target).context(read_failure)?;
    ensure!(
        current == original,
        "{} changed on disk while the check was running; not writing",
        manifest_path.display()
    );

    let permissions = std::fs::metadata(&target).context(metadata_failure)?.permissions();
    let directory = target
        .parent()
        .expect("a canonicalized file path always names a file, so it always has a parent directory");

    // Same directory as the manifest, so the rename stays on one filesystem.
    let mut staged = NamedTempFile::new_in(directory).context(write_failure.clone())?;
    staged.write_all(contents.as_bytes()).context(write_failure)?;
    staged.as_file().set_permissions(permissions).context(permissions_failure)?;
    staged.persist(&target).context(persist_failure)?;

    Ok(())
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

/// Report comments that moved off a removed entry.
///
/// A group header and a note about one specific dependency have identical
/// decor, so carrying the note onto the next entry makes it read as if it were
/// about that one. Naming what moved and where puts the reviewer of the `--fix`
/// diff on the right lines.
fn report_carries(carries: &[Carry]) {
    for carry in carries {
        let sources = carry.from.join("', '");
        match carry.onto.as_ref() {
            Some(onto) => eprintln!(
                "⚠️ Carried {} comment {} from '{sources}' onto '{onto}'; check that the text still describes '{onto}'.",
                carry.lines,
                lines(carry.lines)
            ),
            None => eprintln!(
                "⚠️ Dropped {} comment {} from '{sources}': no surviving entry could carry them.",
                carry.lines,
                lines(carry.lines)
            ),
        }
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

/// Pluralize `line` for `count`.
fn lines(count: usize) -> &'static str {
    if count == 1 { "line" } else { "lines" }
}

#[cfg(test)]
// Miri runs with filesystem isolation, and these tests need real files in a
// real temp directory.
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::write_back;

    /// The unchanged-input guard cannot be driven from an integration test: the
    /// window it protects is between the read and the write of a single run, so
    /// forcing a change inside it would mean racing a child process. Exercised
    /// directly instead.
    #[test]
    fn write_back_replaces_a_manifest_that_is_unchanged() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "original").expect("failed to seed the manifest");

        write_back(&path, "original", "replacement").expect("an unchanged manifest is replaced");

        assert_eq!(fs::read_to_string(&path).expect("failed to read back"), "replacement");
    }

    #[test]
    fn write_back_refuses_a_manifest_that_changed_under_it() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "edited by someone else").expect("failed to seed the manifest");

        let error = write_back(&path, "original", "replacement").expect_err("a changed manifest is refused");

        assert!(
            error.to_string().contains("changed on disk while the check was running"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("failed to read back"),
            "edited by someone else",
            "the competing edit must survive"
        );
    }
}
