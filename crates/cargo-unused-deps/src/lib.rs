// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that finds unused dependencies.
#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-unused-deps/logo.png")]
#![doc(
    html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-unused-deps/favicon.ico"
)]
//!
//! It answers three questions that usually take two other tools and still leave
//! a gap:
//!
//! - **Catalog.** Which `[workspace.dependencies]` entries does no member
//!   inherit? Inheritance is written in the manifest, so this needs no compiler
//!   and cannot produce a false positive.
//! - **Unused.** Which declared dependencies did no compiled unit load?
//! - **Misplaced.** Which `[dependencies]` entries only development units load,
//!   and therefore belong in `[dev-dependencies]`?
//!
//! The last two are answered by rustc itself, through the
//! `unused_crate_dependencies` lint, aggregated across every unit of a package:
//! a dependency is unused only when every unit that had it in scope said so.
//! Doctests are included, which no other tool manages -- rustdoc discards the
//! compiler's output for them, so this binary stands in for the compiler rustdoc uses
//! and keeps a copy.
//!
//! # Requirements
//!
//! A nightly toolchain, because compiling doctests without running them is
//! unstable. Run it as `cargo +nightly unused-deps`.
//!
//! # Usage
//!
//! Run in a Cargo workspace:
//!
//! ```bash
//! cargo +nightly unused-deps
//! ```
//!
//! Restrict the compiled evidence the way cargo does, which is what lets an
//! impact-scoped pipeline pass its own package list straight through:
//!
//! ```bash
//! cargo +nightly unused-deps --package my-crate --package other-crate
//! ```
//!
//! Run one check only:
//!
//! ```bash
//! cargo +nightly unused-deps --check catalog
//! ```
//!
//! Remove the catalog entries nobody inherits:
//!
//! ```bash
//! cargo +nightly unused-deps --fix
//! ```
//!
//! `--manifest-path` points at an explicit workspace root, defaulting to the
//! `Cargo.toml` in the current directory. A manifest with no `[workspace]` table
//! declares no catalog, so that check passes with a note while the rest still
//! run; `--require-workspace` turns it into an error instead.
//!
//! Package selection scopes the compiled evidence only. The catalog check always
//! reads every member, because "no member inherits this entry" is only true if
//! every member was consulted.
//!
//! # Configuration
//!
//! A dependency kept on purpose is exempted in the workspace manifest:
//!
//! ```toml
//! [workspace.metadata.unused-deps]
//! allowed = ["kept-on-purpose"]
//! ```
//!
//! An `allowed` name that suppresses nothing is reported as stale, on stderr,
//! without failing the run. The list is also the answer for a dependency linked
//! for its side effects and never named -- an allocator, a `-sys` shim -- where
//! "unused" is literally true and operationally wrong.
//!
//! # Fixing
//!
//! `--fix` covers the catalog only. It replaces the manifest atomically -- a
//! temporary file in the same directory, renamed over the original, carrying the
//! permissions of the manifest it replaces and following a symlinked manifest to
//! its target -- and refuses to write at all if the file changed after it was
//! read, so a concurrent edit is never clobbered.
//!
//! Comments on a removed entry are carried to the next surviving entry, which
//! keeps a group header attached to the group it introduces. A note about one
//! specific dependency is indistinguishable from such a header, so every move
//! is reported on stderr: check that carried text still describes the entry it
//! landed on. Comments that cannot be placed -- the removal emptied the table,
//! or left a trailing survivor with nothing to append to -- are reported as
//! dropped.
//!
//! Removing a dependency a crate declares is not automated: the evidence is
//! strong enough to fail a build and ask a human, not strong enough to edit code
//! paths nobody compiled.
//!
//! # Installation
//!
//! ```bash
//! cargo install cargo-unused-deps
//! ```
//!
//! # Example output
//!
//! ```text
//! ✅ All 70 workspace dependencies in Cargo.toml are inherited by one of 10 members.
//! ❌ Found 1 dependency problem:
//!
//!   my-crate [dependencies] once_cell: no compiled unit loaded it.
//!       remove it, or gate the declaration to where it is used.
//! ```

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod detect;
mod doctests;
mod evidence;
mod fix;
mod verdict;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use cargo_metadata::MetadataCommand;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use tempfile::NamedTempFile;

use crate::detect::{Catalog, ManifestInput, Section, WorkspaceCatalog};
use crate::fix::Carry;
use crate::verdict::Verdict;

// Deliberately identical to the palette of the repository's other styled Cargo
// subcommands, so help output looks the same whichever one the user reaches for.
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

/// The subcommand token Cargo passes through, and the options that follow it.
///
/// Cargo invokes a custom subcommand as `cargo-unused-deps unused-deps ...`, so
/// the tool parses the repeated name as a nested subcommand and takes the check's
/// options from that level rather than from the top-level parser.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Find unused dependencies: uninherited catalog entries, dead declarations
    /// and misplaced ones
    #[command(version, display_name = "cargo-unused-deps")]
    UnusedDeps {
        /// Path to the workspace root Cargo.toml
        #[arg(long, default_value = "Cargo.toml", value_name = "PATH")]
        manifest_path: PathBuf,

        /// Package to gather compile evidence for. Repeatable
        #[arg(short = 'p', long = "package", value_name = "SPEC")]
        packages: Vec<String>,

        /// Gather compile evidence for every workspace member
        #[arg(long)]
        workspace: bool,

        /// Exclude a member from a --workspace run. Repeatable
        #[arg(long, value_name = "SPEC")]
        exclude: Vec<String>,

        /// Run only the named checks
        #[arg(long = "check", value_name = "NAME", value_enum)]
        checks: Vec<Check>,

        /// Remove the unused entries instead of only reporting them
        #[arg(long)]
        fix: bool,

        /// Treat a manifest with no [workspace] table as an error
        #[arg(long)]
        require_workspace: bool,
    },
}

/// The checks a run can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Check {
    /// `[workspace.dependencies]` entries no member inherits.
    Catalog,

    /// Declared dependencies no unit loaded.
    Unused,

    /// Normal dependencies only development units use.
    Misplaced,
}

impl Check {
    /// Whether `selected` asks for this check. An empty selection means all.
    fn wanted(self, selected: &[Self]) -> bool {
        selected.is_empty() || selected.contains(&self)
    }

    /// Whether any selected check needs the compiler.
    fn any_needs_evidence(selected: &[Self]) -> bool {
        Self::Unused.wanted(selected) || Self::Misplaced.wanted(selected)
    }
}

/// Entry point: either the check, or the compiler shim rustdoc invokes.
///
/// rustdoc calls a `--test-builder` as a bare rustc, with no flag of ours to
/// key on, so shim mode is signaled by the capture-file variable this tool
/// sets on the doctest build it starts.
///
/// # Errors
///
/// Returns whatever the selected mode returns.
pub fn dispatch(args: &[OsString]) -> Result<ExitCode> {
    if let Some(capture) = std::env::var_os(doctests::CAPTURE_VAR) {
        let subcommand = args.get(1).map(OsString::as_os_str);
        if subcommand != Some(SUBCOMMAND.as_ref()) {
            return doctests::shim(&args[1..], Path::new(&capture));
        }
    }

    run()
}

/// The cargo subcommand this binary answers to.
const SUBCOMMAND: &str = "unused-deps";

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
    let Commands::UnusedDeps {
        manifest_path,
        packages,
        workspace,
        exclude,
        checks,
        fix,
        require_workspace,
    } = cli.command;

    let selection = selection_flags(&packages, workspace, &exclude);
    let mut failed = false;

    if Check::Catalog.wanted(&checks) {
        failed |= catalog_check(&manifest_path, fix, require_workspace)? != ExitCode::SUCCESS;
    }

    if Check::any_needs_evidence(&checks) {
        failed |= source_checks(&manifest_path, &selection, &checks)?;
    }

    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Cargo's own package-selection flags, forwarded verbatim to the child builds.
fn selection_flags(packages: &[String], workspace: bool, exclude: &[String]) -> Vec<OsString> {
    let mut flags: Vec<OsString> = Vec::new();

    for package in packages {
        flags.push(OsString::from("--package"));
        flags.push(OsString::from(package));
    }

    if workspace || packages.is_empty() {
        flags.push(OsString::from("--workspace"));
    }

    for excluded in exclude {
        flags.push(OsString::from("--exclude"));
        flags.push(OsString::from(excluded));
    }

    flags
}

/// The checks that read compile evidence: unused declarations and misplaced ones.
///
/// Returns whether anything was found.
fn source_checks(manifest_path: &Path, selection: &[OsString], checks: &[Check]) -> Result<bool> {
    let workspace = workspace_of(manifest_path)?;
    // Two passes: default targets first, where a report can only have come from
    // a target's single plain unit, then everything.
    let plain = evidence::gather(manifest_path, selection, &workspace.evidence_target_dir, false)?;
    let all = evidence::gather(manifest_path, selection, &workspace.evidence_target_dir, true)?;

    let shim = std::env::current_exe().context("failed to locate this executable to use as the doctest shim")?;
    let mut doctests = doctests::DoctestEvidence::default();
    for package in &workspace.packages {
        // Only a library target can have doctests; asking cargo for the
        // doctests of a bin-only package is an error, not an empty answer.
        if package.has_library && all.saw_package(&package.manifest_path) {
            let found = doctests::gather_package(manifest_path, &package.name, &workspace.evidence_target_dir, &shim)?;
            doctests.insert(package.name.clone(), found);
        }
    }

    let findings = verdict::judge(&workspace.packages, &plain, &all, &doctests, &workspace.allowed);

    let wanted: Vec<&verdict::Finding> = findings
        .iter()
        .filter(|finding| match finding.verdict {
            Verdict::Unused => Check::Unused.wanted(checks),
            Verdict::Misplaced => Check::Misplaced.wanted(checks),
        })
        .collect();

    if wanted.is_empty() {
        println!(
            "✅ Every declared dependency in {} packages is used where it is declared.",
            workspace.packages.len()
        );
        return Ok(false);
    }

    report_findings(&wanted);

    Ok(true)
}

/// Report every source-level finding, grouped the way a reader fixes them.
fn report_findings(findings: &[&verdict::Finding]) {
    eprintln!(
        "❌ Found {} dependency {}:\n",
        findings.len(),
        if findings.len() == 1 { "problem" } else { "problems" }
    );

    for finding in findings {
        let table = match finding.section {
            Section::Normal => "dependencies",
            Section::Development => "dev-dependencies",
            Section::Build => "build-dependencies",
        };

        match finding.verdict {
            Verdict::Unused => {
                eprintln!("  {} [{table}] {}: no compiled unit loaded it.", finding.package, finding.name);
                eprintln!("      remove it, or gate the declaration to where it is used.");
            }
            Verdict::Misplaced => {
                eprintln!("  {} [{table}] {}: only development units load it.", finding.package, finding.name);
                eprintln!("      move it to [dev-dependencies].");
            }
        }

        eprintln!("      {}", finding.manifest_path.display());
    }
}

/// The catalog check: `[workspace.dependencies]` entries no member inherits.
fn catalog_check(manifest_path: &Path, fix: bool, require_workspace: bool) -> Result<ExitCode> {
    let original = detect::read_manifest_text(manifest_path)?;
    let mut manifest = detect::parse_manifest(&original, manifest_path)?;

    let catalog = match detect::catalog(&manifest)? {
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
    let inheritance = detect::inherited(&members)?;
    let (unused, stale) = detect::partition(&catalog, &inheritance.keys);

    report_stale(&stale);

    if unused.is_empty() {
        report_clean(manifest_path, &catalog, &inheritance.keys, members.len());
        return Ok(ExitCode::SUCCESS);
    }

    if !fix {
        report_unused(manifest_path, &unused);
        return Ok(ExitCode::FAILURE);
    }

    let outcome = fix::remove(&mut manifest, &unused);
    verify_members_unchanged(manifest_path, &members)?;
    write_back(manifest_path, &original, &inheritance.inputs, &manifest.to_string())?;

    println!(
        "🧹 Removed {} unused workspace {} from {}.",
        outcome.removed,
        entries(outcome.removed),
        manifest_path.display()
    );
    report_carries(&outcome.carries);

    Ok(ExitCode::SUCCESS)
}

/// Replace `manifest_path` with `contents`.
///
/// The replacement is atomic, and happens only if every manifest input still
/// holds what detection read.
///
/// The workspace root manifest is the one file whose loss breaks every other
/// tool in the repository, so it is never truncated in place: the replacement
/// is written to a temporary file in the same directory and renamed over the
/// original, which is atomic on one filesystem. `cargo metadata` and member
/// scanning run between the reads and the write, so that window is wide enough
/// for an editor to save into either the root or a member manifest -- hence the
/// unchanged-input guards.
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
fn write_back(manifest_path: &Path, original: &str, member_inputs: &[ManifestInput], contents: &str) -> Result<()> {
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
    let target = fs::canonicalize(manifest_path).context(resolve_failure)?;

    let current = fs::read_to_string(&target).context(read_failure)?;
    ensure!(
        current == original,
        "{} changed on disk while the check was running; not writing",
        manifest_path.display()
    );

    for input in member_inputs {
        let current =
            fs::read_to_string(&input.path).with_context(|| format!("failed to re-read {} before writing", input.path.display()))?;
        ensure!(
            current == input.contents,
            "{} changed on disk while the check was running; not writing",
            input.path.display()
        );
    }

    let permissions = fs::metadata(&target).context(metadata_failure)?.permissions();
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

/// Re-resolve workspace membership and refuse a fix when its input set changed.
fn verify_members_unchanged(manifest_path: &Path, expected: &[PathBuf]) -> Result<()> {
    let current = members_of(manifest_path)?;
    let expected: BTreeSet<&Path> = expected.iter().map(PathBuf::as_path).collect();
    let current: BTreeSet<&Path> = current.iter().map(PathBuf::as_path).collect();

    ensure!(
        current == expected,
        "workspace membership changed while the check was running; not writing"
    );

    Ok(())
}

/// Report allow-list entries that suppressed nothing.
fn report_stale(stale: &[String]) {
    for name in stale {
        eprintln!("⚠️ '{name}' is allowed but is inherited or not declared; the allow-list entry can be removed.");
    }
}

/// The workspace as the source-level checks need it.
struct Workspace {
    /// Every member, with what it declares.
    packages: Vec<verdict::Package>,

    /// Names exempted workspace-wide.
    allowed: BTreeSet<String>,

    /// Where the evidence build writes, kept apart from the shared cache
    /// because the lint flag changes the build fingerprint.
    evidence_target_dir: PathBuf,
}

/// Read every member's declarations, the workspace allow-list, and where to build.
fn workspace_of(manifest_path: &Path) -> Result<Workspace> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()
        .with_context(|| format!("failed to enumerate the workspace members of {}", manifest_path.display()))?;

    let mut packages = Vec::new();
    for package in metadata.workspace_packages() {
        let path = package.manifest_path.clone().into_std_path_buf();
        let document = detect::read_manifest(&path)?;

        packages.push(verdict::Package {
            name: package.name.to_string(),
            declared: detect::declared_dependencies(&document),
            has_library: package
                .targets
                .iter()
                .any(|target| target.kind.iter().any(|kind| kind.to_string().contains("lib"))),
            manifest_path: path,
        });
    }

    let root = detect::read_manifest(manifest_path)?;
    let allowed = match detect::catalog(&root) {
        Catalog::Workspace(catalog) => catalog.allowed,
        Catalog::NotAWorkspace => BTreeSet::new(),
    };

    Ok(Workspace {
        packages,
        allowed,
        evidence_target_dir: metadata.target_directory.into_std_path_buf().join("unused-deps"),
    })
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
                lines(carry.lines.get())
            ),
            None => eprintln!(
                "⚠️ Dropped {} comment {} from '{sources}': no surviving entry could carry the comments.",
                carry.lines,
                lines(carry.lines.get())
            ),
        }
    }
}

/// Report a catalog in which every entry is inherited or allowed.
fn report_clean(manifest_path: &Path, catalog: &WorkspaceCatalog, inherited: &BTreeSet<String>, members: usize) {
    let declared = catalog.declared.len();
    let covered = catalog.declared.iter().filter(|name| inherited.contains(name.as_str())).count();

    let qualifier = if declared == covered { "" } else { " or explicitly allowed" };

    println!(
        "✅ {}: every workspace dependency is inherited by a workspace member{qualifier} (declared: {declared}, members: {members}).",
        manifest_path.display()
    );
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
    eprintln!("\nRe-run with --fix to remove what is listed above.");
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

    use super::{verify_members_unchanged, write_back};

    /// The unchanged-input guard cannot be driven from an integration test: the
    /// window it protects is between the read and the write of a single run, so
    /// forcing a change inside it would mean racing a child process. Exercised
    /// directly instead.
    #[test]
    fn write_back_replaces_a_manifest_that_is_unchanged() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "original").expect("failed to seed the manifest");

        write_back(&path, "original", &[], "replacement").expect("an unchanged manifest is replaced");

        assert_eq!(fs::read_to_string(&path).expect("failed to read back"), "replacement");
    }

    #[test]
    fn write_back_refuses_a_manifest_that_changed_under_it() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "edited by someone else").expect("failed to seed the manifest");

        let error = write_back(&path, "original", &[], "replacement").expect_err("a changed manifest is refused");

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

    #[test]
    fn write_back_refuses_a_member_manifest_that_changed_under_it() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().join("Cargo.toml");
        let member = dir.path().join("member.toml");
        fs::write(&root, "original").expect("failed to seed the root manifest");
        fs::write(&member, "edited by someone else").expect("failed to seed the member manifest");
        let inputs = [crate::detect::ManifestInput {
            path: member,
            contents: "member original".to_owned(),
        }];

        let error = write_back(&root, "original", &inputs, "replacement").expect_err("a changed member manifest is refused");

        assert!(
            error.to_string().contains("changed on disk while the check was running"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(&root).expect("failed to read back"),
            "original",
            "the root manifest must not be replaced"
        );
    }

    #[test]
    fn write_back_refuses_a_member_manifest_that_disappeared() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().join("Cargo.toml");
        let missing = dir.path().join("missing.toml");
        fs::write(&root, "original").expect("failed to seed the root manifest");
        let inputs = [crate::detect::ManifestInput {
            path: missing.clone(),
            contents: "member original".to_owned(),
        }];

        let error = write_back(&root, "original", &inputs, "replacement").expect_err("an unreadable member manifest is refused");

        assert!(
            error
                .to_string()
                .contains(&format!("failed to re-read {} before writing", missing.display())),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read_to_string(&root).expect("failed to read back"),
            "original",
            "the root manifest must not be replaced"
        );
    }

    #[test]
    fn membership_comparison_ignores_order() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().join("Cargo.toml");
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        fs::create_dir_all(alpha.join("src")).expect("failed to create alpha");
        fs::create_dir_all(beta.join("src")).expect("failed to create beta");
        fs::write(&root, "[workspace]\nmembers = [\"alpha\", \"beta\"]\nresolver = \"2\"\n").expect("failed to write root manifest");
        for (path, name) in [(&alpha, "alpha"), (&beta, "beta")] {
            fs::write(
                path.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .expect("failed to write member manifest");
            fs::write(path.join("src/lib.rs"), "").expect("failed to write member source");
        }
        let expected = vec![beta.join("Cargo.toml"), alpha.join("Cargo.toml")];

        verify_members_unchanged(&root, &expected).expect("member order is not significant");
    }

    #[test]
    fn membership_comparison_rejects_a_new_member() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let root = dir.path().join("Cargo.toml");
        let member = dir.path().join("member");
        fs::create_dir_all(member.join("src")).expect("failed to create member");
        fs::write(&root, "[workspace]\nmembers = [\"*\"]\nresolver = \"2\"\n").expect("failed to write root manifest");
        fs::write(member.join("Cargo.toml"), "[package]\nname = \"member\"\nversion = \"0.1.0\"\n")
            .expect("failed to write member manifest");
        fs::write(member.join("src/lib.rs"), "").expect("failed to write member source");

        let error = verify_members_unchanged(&root, &[]).expect_err("a newly matched member must invalidate the fix");

        assert!(
            error
                .to_string()
                .contains("workspace membership changed while the check was running"),
            "unexpected error: {error}"
        );
    }
}
