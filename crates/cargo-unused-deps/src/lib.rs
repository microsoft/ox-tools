// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Cargo subcommand that finds unused dependencies.
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
//! Package-level checks require a nightly toolchain because compiling doctests
//! without running them is unstable. The catalog-only invocation with no package
//! selector is manifest-only and runs on stable.
//!
//! # Usage
//!
//! Run every check across a Cargo workspace:
//!
//! ```bash
//! cargo +nightly unused-deps --workspace
//! ```
//!
//! Restrict the compiled evidence the way cargo does, which is what lets an
//! impact-scoped pipeline pass its own package list straight through:
//!
//! ```bash
//! cargo +nightly unused-deps --package my-crate --package other-crate
//! ```
//!
//! Run only the workspace-global catalog check by omitting package selection:
//!
//! ```bash
//! cargo unused-deps
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
//! every member was consulted. With no `--package` or `--workspace`, no package
//! is compiled and only that catalog check runs.
//!
//! # Configuration
//!
//! A dependency kept on purpose is exempted in the workspace manifest:
//!
//! ```toml
//! [workspace.metadata.unused-deps]
//! allowed = ["kept-on-purpose"]
//!
//! [package.metadata.unused-deps]
//! allowed = ["package-local-side-effect"]
//! ```
//!
//! A workspace-level `allowed` name declared by neither the catalog nor any
//! member is reported as stale without failing the run. The lists are also the
//! answer for a dependency linked for its side effects and never named -- an
//! allocator or `-sys` shim -- where "unused" is literally true and
//! operationally wrong.
//!
//! # Fixing
//!
//! `--fix` covers the catalog only. It replaces the manifest atomically -- a
//! temporary file in the same directory, renamed over the original, carrying the
//! permissions of the manifest it replaces and following a symlinked manifest to
//! its target. Before replacement it rechecks workspace membership and every
//! manifest input; a change detected there aborts the write. This narrows but
//! cannot close the final comparison-to-rename race.
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
//!       remove it from [dependencies], or preserve that scope while narrowing where it is declared.
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

use cargo_metadata::MetadataCommand;
use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
use clap::{Parser, Subcommand};
use ohno::{AppError, IntoAppError};
use tempfile::NamedTempFile;

use crate::detect::{Catalog, ManifestInput, Section, WorkspaceCatalog};
use crate::fix::Carry;
use crate::verdict::Verdict;

type Result<T> = std::result::Result<T, AppError>;

// Deliberately identical to the palette of the repository's other styled Cargo
// subcommands, so help output looks the same whichever one the user reaches for.
const CLAP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default());

/// Cargo subcommand to find unused and misplaced dependencies.
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

        /// Exact package selector to gather compile evidence for. Repeatable
        #[arg(short = 'p', long = "package", value_name = "SPEC")]
        packages: Vec<String>,

        /// Gather compile evidence for every workspace member
        #[arg(long, conflicts_with = "packages")]
        workspace: bool,

        /// Exclude a member from a --workspace run. Repeatable
        #[arg(long, value_name = "SPEC", requires = "workspace")]
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
enum Check {
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
//
// The shim branch runs in rustdoc's child process. Its behavior is covered by
// the doctest integration cases, but that child does not contribute a profile
// to the parent coverage run.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn dispatch(args: &[OsString]) -> std::result::Result<ExitCode, AppError> {
    dispatch_with(
        args,
        |key| std::env::var_os(key),
        evidence::wrapper,
        doctests::rustdoc_wrapper,
        doctests::shim,
        run,
    )
}

fn dispatch_with(
    args: &[OsString],
    environment: impl Fn(&str) -> Option<OsString>,
    wrapper: impl FnOnce(&[OsString]) -> Result<ExitCode>,
    rustdoc: impl FnOnce(&[OsString]) -> Result<ExitCode>,
    shim: impl FnOnce(&[OsString], &Path) -> Result<ExitCode>,
    run: impl FnOnce() -> Result<ExitCode>,
) -> Result<ExitCode> {
    if environment(evidence::WRAPPER_VAR).is_some() {
        return wrapper(&args[1..]);
    }
    if environment(doctests::RUSTDOC_WRAPPER_VAR).is_some() {
        return rustdoc(&args[1..]);
    }
    if let Some(capture) = environment(doctests::CAPTURE_VAR) {
        let subcommand = args.get(1).map(OsString::as_os_str);
        if subcommand != Some(SUBCOMMAND.as_ref()) {
            return shim(&args[1..], Path::new(&capture));
        }
    }

    run()
}

/// The cargo subcommand this binary answers to.
const SUBCOMMAND: &str = "unused-deps";

/// Parse the Cargo subcommand arguments and execute the requested checks.
///
/// Returns [`ExitCode::SUCCESS`] when every catalog entry is inherited or
/// allowed and the selected package checks find no unused or misplaced
/// declarations. Under `--fix`, successfully removing uninherited catalog
/// entries satisfies the catalog half. Returns [`ExitCode::FAILURE`] for any
/// remaining finding. Returning an exit code (rather than calling
/// `std::process::exit`) lets `main` unwind normally so the process terminates
/// through the standard runtime path -- important under coverage
/// instrumentation, where an abrupt exit can skip the profile flush.
///
/// # Errors
///
/// Returns an error if a manifest cannot be read or parsed, workspace members
/// cannot be enumerated, compiler or doctest evidence collection fails, package
/// selectors are invalid, or a fixed manifest cannot be written back.
fn run() -> Result<ExitCode> {
    run_with(Cli::parse(), workspace_manifest_of, workspace_of, catalog_check, source_checks)
}

fn run_with(
    cli: Cli,
    resolve_manifest: impl FnOnce(&Path) -> Result<PathBuf>,
    load_workspace: impl FnOnce(&Path) -> Result<Workspace>,
    check_catalog: impl FnOnce(&Path, bool, bool) -> Result<ExitCode>,
    check_sources: impl FnOnce(&Path, &[OsString], &Workspace, &BTreeSet<PathBuf>, &[Check]) -> Result<bool>,
) -> Result<ExitCode> {
    let Commands::UnusedDeps {
        manifest_path,
        packages,
        workspace,
        exclude,
        checks,
        fix,
        require_workspace,
    } = cli.command;
    let manifest_path = resolve_manifest(&manifest_path)?;

    let package_context = if package_checks_selected(&packages, workspace) {
        let selection = PackageSelection {
            packages,
            workspace,
            exclude,
        };
        let workspace = load_workspace(&manifest_path)?;
        let selected = selection.resolve(&workspace.packages)?;
        Some((selection.flags(), workspace, selected))
    } else {
        None
    };

    // The catalog question is workspace-global and always runs. Package
    // selection applies only to checks backed by compile evidence.
    let mut failed = check_catalog(&manifest_path, fix, require_workspace)? != ExitCode::SUCCESS;

    if let Some((selection_flags, workspace, selected)) = package_context {
        failed |= check_sources(&manifest_path, &selection_flags, &workspace, &selected, &checks)?;
    }

    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

/// Whether the caller requested compiler-backed package checks.
///
/// Mutating this predicate in either direction makes a selector-free catalog
/// run compile the entire workspace, which is an unbounded timeout rather than
/// a useful mutation-test result. CLI integration tests cover both outcomes.
#[mutants::skip]
fn package_checks_selected(packages: &[String], workspace: bool) -> bool {
    !packages.is_empty() || workspace
}

/// Resolve any member manifest to the root manifest Cargo associates with it.
fn workspace_manifest_of(manifest_path: &Path) -> Result<PathBuf> {
    workspace_manifest_with(manifest_path, members_of)
}

fn workspace_manifest_with(manifest_path: &Path, members: impl FnMut(&Path) -> Result<Vec<PathBuf>>) -> Result<PathBuf> {
    workspace_manifest_resolved_with(manifest_path, members, Path::canonicalize, detect::read_manifest, Path::is_file)
}

fn workspace_manifest_resolved_with(
    manifest_path: &Path,
    mut members: impl FnMut(&Path) -> Result<Vec<PathBuf>>,
    canonicalize: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
    mut read_manifest: impl FnMut(&Path) -> Result<toml_edit::DocumentMut>,
    mut is_file: impl FnMut(&Path) -> bool,
) -> Result<PathBuf> {
    let manifest = read_manifest(manifest_path)?;
    if matches!(detect::catalog(&manifest)?, Catalog::Workspace(_)) {
        return Ok(manifest_path.to_path_buf());
    }

    let supplied = canonical_manifest(manifest_path, canonicalize(manifest_path))?;
    let package_dir = supplied
        .parent()
        .expect("a canonical manifest file path always has a parent directory");

    for ancestor in package_dir.ancestors().skip(1) {
        let candidate = ancestor.join("Cargo.toml");
        if !is_file(&candidate) {
            continue;
        }
        let document = read_manifest(&candidate)?;
        if !matches!(detect::catalog(&document)?, Catalog::Workspace(_)) {
            continue;
        }
        if members(&candidate)?.iter().any(|member| member == &supplied) {
            return Ok(candidate);
        }
    }

    Ok(manifest_path.to_path_buf())
}

fn canonical_manifest(manifest_path: &Path, canonical: std::io::Result<PathBuf>) -> Result<PathBuf> {
    let canonical = canonical.into_app_err(format!("failed to resolve {}", manifest_path.display()))?;
    Ok(canonical)
}

/// Cargo-style package selection for compiler-backed checks.
struct PackageSelection {
    /// Explicit package names or exact `name@version` specs.
    packages: Vec<String>,
    /// Whether every workspace package is selected.
    workspace: bool,
    /// Packages removed from a workspace selection.
    exclude: Vec<String>,
}

impl PackageSelection {
    /// Flags forwarded to Cargo after the same selectors are resolved locally.
    fn flags(&self) -> Vec<OsString> {
        let mut flags = Vec::new();
        for package in &self.packages {
            flags.push(OsString::from("--package"));
            flags.push(OsString::from(package));
        }
        if self.workspace {
            flags.push(OsString::from("--workspace"));
        }
        for excluded in &self.exclude {
            flags.push(OsString::from("--exclude"));
            flags.push(OsString::from(excluded));
        }
        flags
    }

    /// Resolve the explicitly selected roots against workspace metadata.
    fn resolve(&self, packages: &[verdict::Package]) -> Result<BTreeSet<PathBuf>> {
        let mut selected = if self.workspace {
            packages.iter().map(|package| package.manifest_path.clone()).collect()
        } else {
            resolve_selectors(packages, &self.packages, false)?
        };

        for excluded in resolve_selectors(packages, &self.exclude, true)? {
            selected.remove(&excluded);
        }

        Ok(selected)
    }
}

/// Resolve exact package names and `name@version` specs.
fn resolve_selectors(packages: &[verdict::Package], selectors: &[String], allow_missing: bool) -> Result<BTreeSet<PathBuf>> {
    let mut resolved = BTreeSet::new();
    for selector in selectors {
        let matches: Vec<&verdict::Package> = packages
            .iter()
            .filter(|package| selector == &package.name || selector == &format!("{}@{}", package.name, package.version))
            .collect();
        match matches.as_slice() {
            [] if allow_missing => {}
            [] => ohno::bail!("package selector `{selector}` did not match any workspace member"),
            [package] => {
                resolved.insert(package.manifest_path.clone());
            }
            _ => ohno::bail!("package selector `{selector}` matched more than one workspace member; qualify it with @version"),
        }
    }
    Ok(resolved)
}

/// The checks that read compile evidence: unused declarations and misplaced ones.
///
/// Returns whether anything was found.
fn source_checks(
    manifest_path: &Path,
    selection_flags: &[OsString],
    workspace: &Workspace,
    selected: &BTreeSet<PathBuf>,
    checks: &[Check],
) -> Result<bool> {
    source_checks_with(
        manifest_path,
        selection_flags,
        workspace,
        selected,
        checks,
        evidence::gather,
        doctest_evidence,
    )
}

fn source_checks_with(
    manifest_path: &Path,
    selection_flags: &[OsString],
    workspace: &Workspace,
    selected: &BTreeSet<PathBuf>,
    checks: &[Check],
    mut gather: impl FnMut(&Path, &[OsString], &Path, bool) -> Result<evidence::Evidence>,
    gather_doctests: impl FnOnce(&Path, &Workspace, &[verdict::Finding]) -> Result<doctests::DoctestEvidence>,
) -> Result<bool> {
    let plain_target_dir = workspace.evidence_target_dir.path().join("plain");
    let all_target_dir = workspace.evidence_target_dir.path().join("all-targets");
    // Two passes: default targets first, where a report can only have come from
    // a target's single plain unit, then everything.
    let plain = gather(manifest_path, selection_flags, &plain_target_dir, false)?;
    let all = gather(manifest_path, selection_flags, &all_target_dir, true)?;

    // Doctest evidence can only ever spare a dependency, never accuse one, so
    // it is gathered lazily: judge first without it, then compile the doctests
    // of only those packages that produced a finding, and judge again. On a
    // large workspace that is the difference between a handful of doctest
    // builds and one per package.
    let mut candidates = verdict::judge(
        &workspace.packages,
        selected,
        &plain,
        &all,
        &doctests::DoctestEvidence::default(),
        &workspace.allowed,
    );
    candidates.retain(|finding| {
        // Doctests can spare unused normal/dev declarations and can turn an
        // unused normal declaration into a misplaced one. They cannot affect
        // build dependencies or a declaration already proven misplaced.
        finding.verdict == Verdict::Unused
            && finding.section != Section::Build
            && (Check::Unused.wanted(checks) || finding.section == Section::Normal)
    });
    let doctests = gather_doctests(manifest_path, workspace, &candidates)?;

    let findings = verdict::judge(&workspace.packages, selected, &plain, &all, &doctests, &workspace.allowed);

    let wanted: Vec<&verdict::Finding> = findings
        .iter()
        .filter(|finding| match finding.verdict {
            Verdict::Unused => Check::Unused.wanted(checks),
            Verdict::Misplaced => Check::Misplaced.wanted(checks),
        })
        .collect();

    if wanted.is_empty() {
        println!(
            "✅ No selected dependency problems found in {} {}.",
            selected.len(),
            if selected.len() == 1 { "package" } else { "packages" }
        );
        return Ok(false);
    }

    // #[gamma::skip(call_result.default, reason = "report_findings returns unit; diagnostics still execute and () is replaced with ()")]
    report_findings(&wanted);

    Ok(true)
}

/// Compile the doctests of the packages a finding was raised against.
///
/// Nothing else needs them: a doctest can only reveal that a dependency *is*
/// used, so a package with no findings has nothing a doctest could change.
fn doctest_evidence(manifest_path: &Path, workspace: &Workspace, candidates: &[verdict::Finding]) -> Result<doctests::DoctestEvidence> {
    doctest_evidence_with(
        manifest_path,
        workspace,
        candidates,
        std::env::current_exe,
        doctests::gather_package,
    )
}

fn doctest_evidence_with(
    manifest_path: &Path,
    workspace: &Workspace,
    candidates: &[verdict::Finding],
    executable: impl FnOnce() -> std::io::Result<PathBuf>,
    mut gather: impl FnMut(&Path, &str, &Path, &Path) -> Result<doctests::PackageDoctests>,
) -> Result<doctests::DoctestEvidence> {
    let mut evidence = doctests::DoctestEvidence::default();
    if candidates.is_empty() {
        return Ok(evidence);
    }

    let shim = executable().into_app_err("failed to locate this executable to use as the doctest shim")?;
    let accused: BTreeSet<&Path> = candidates.iter().map(|finding| finding.manifest_path.as_path()).collect();

    for package in &workspace.packages {
        // Only a library target can have doctests; asking cargo for the
        // doctests of a bin-only package is an error, not an empty answer.
        if package.has_doctests && accused.contains(package.manifest_path.as_path()) {
            let selector = format!("{}@{}", package.name, package.version);
            let found = gather(
                manifest_path,
                &selector,
                &workspace.evidence_target_dir.path().join("doctests"),
                &shim,
            )?;
            evidence.insert(package.manifest_path.clone(), found);
        }
    }

    Ok(evidence)
}

/// Report every source-level finding, grouped the way a reader fixes them.
fn report_findings(findings: &[&verdict::Finding]) {
    eprintln!(
        "❌ Found {} dependency {}:\n",
        findings.len(),
        if findings.len() == 1 { "problem" } else { "problems" }
    );

    for finding in findings {
        let section = dependency_section(finding.section);
        let table = dependency_table(finding.target.as_deref(), section);

        match finding.verdict {
            Verdict::Unused => {
                eprintln!("  {} {table} {}: no compiled unit loaded it.", finding.package, finding.name);
                eprintln!("      remove it from {table}, or preserve that scope while narrowing where it is declared.");
            }
            Verdict::Misplaced => {
                let destination = dependency_table(finding.target.as_deref(), "dev-dependencies");
                eprintln!("  {} {table} {}: only development units load it.", finding.package, finding.name);
                eprintln!("      move it to {destination}.");
            }
        }

        eprintln!("      {}", finding.manifest_path.display());
    }
}

/// Render an unconditional or target-specific dependency table.
fn dependency_table(target: Option<&str>, section: &str) -> String {
    target.map_or_else(|| format!("[{section}]"), |target| format!("[target.'{target}'.{section}]"))
}

fn dependency_section(section: Section) -> &'static str {
    match section {
        Section::Normal => "dependencies",
        Section::Development => "dev-dependencies",
        Section::Build => "build-dependencies",
    }
}

/// Split from [`run`] so tests can drive it without a process boundary or a
/// parsed command line.
fn catalog_check(manifest_path: &Path, fix: bool, require_workspace: bool) -> Result<ExitCode> {
    catalog_check_with_hook(manifest_path, fix, require_workspace, |_| {})
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckStage {
    MembersEnumerated,
    InheritanceCollected,
    MembersVerified,
}

fn catalog_check_with_hook(manifest_path: &Path, fix: bool, require_workspace: bool, mut hook: impl FnMut(CheckStage)) -> Result<ExitCode> {
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

    if catalog.declared.is_empty() && catalog.allowed.is_empty() {
        println!("✅ {} declares no workspace dependencies.", manifest_path.display());
        return Ok(ExitCode::SUCCESS);
    }

    let members = members_of(manifest_path)?;
    hook(CheckStage::MembersEnumerated);
    let inheritance = detect::inherited(&members)?;
    hook(CheckStage::InheritanceCollected);
    let (unused, stale) = detect::partition(&catalog, &inheritance.keys, &inheritance.declarations);

    // #[gamma::skip(call_result.default, reason = "report_stale returns unit; diagnostics still execute and () is replaced with ()")]
    report_stale(&stale);

    if catalog.declared.is_empty() {
        println!("✅ {} declares no workspace dependencies.", manifest_path.display());
        return Ok(ExitCode::SUCCESS);
    }

    if unused.is_empty() {
        let member_count = members.len();
        // #[gamma::skip(call_result.default, reason = "report_clean returns unit; diagnostics still execute and () is replaced with ()")]
        report_clean(manifest_path, &catalog, &inheritance.keys, member_count);
        return Ok(ExitCode::SUCCESS);
    }

    if !fix {
        // #[gamma::skip(call_result.default, reason = "report_unused returns unit; diagnostics still execute and () is replaced with ()")]
        report_unused(manifest_path, &unused);
        return Ok(ExitCode::FAILURE);
    }

    let outcome = fix::remove(&mut manifest, &unused);
    verify_members_unchanged(manifest_path, &members)?;
    hook(CheckStage::MembersVerified);
    write_back(manifest_path, &original, &inheritance.inputs, &manifest.to_string())?;

    println!(
        "🧹 Removed {} unused workspace {} from {}.",
        outcome.removed,
        entries(outcome.removed),
        manifest_path.display()
    );
    // #[gamma::skip(call_result.default, reason = "report_carries returns unit; diagnostics still execute and () is replaced with ()")]
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
    write_back_with_fault(manifest_path, original, member_inputs, contents, None)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteBackStep {
    Resolve,
    Read,
    Metadata,
    Create,
    Write,
    Permissions,
    Persist,
}

fn faultable<T>(step: WriteBackStep, fault: Option<WriteBackStep>, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if fault == Some(step) {
        ohno::bail!("injected {step:?} failure");
    }

    operation()
}

fn write_back_with_fault(
    manifest_path: &Path,
    original: &str,
    member_inputs: &[ManifestInput],
    contents: &str,
    fault: Option<WriteBackStep>,
) -> Result<()> {
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
    let target = faultable(WriteBackStep::Resolve, fault, || {
        fs::canonicalize(manifest_path).into_app_err(resolve_failure)
    })?;

    let current = faultable(WriteBackStep::Read, fault, || {
        fs::read_to_string(&target).into_app_err(read_failure)
    })?;
    if current != original {
        ohno::bail!(
            "{} changed on disk while the check was running; not writing",
            manifest_path.display()
        );
    }

    for input in member_inputs {
        let current = fs::read_to_string(&input.path).into_app_err(format!("failed to re-read {} before writing", input.path.display()))?;
        if current != input.contents {
            ohno::bail!("{} changed on disk while the check was running; not writing", input.path.display());
        }
    }

    let permissions = faultable(WriteBackStep::Metadata, fault, || {
        fs::metadata(&target).into_app_err(metadata_failure)
    })?
    .permissions();
    let directory = target
        .parent()
        .expect("a canonicalized file path always names a file, so it always has a parent directory");

    // Same directory as the manifest, so the rename stays on one filesystem.
    let mut staged = faultable(WriteBackStep::Create, fault, || {
        NamedTempFile::new_in(directory).into_app_err(write_failure.clone())
    })?;
    faultable(WriteBackStep::Write, fault, || {
        staged.write_all(contents.as_bytes()).into_app_err(write_failure)
    })?;
    faultable(WriteBackStep::Permissions, fault, || {
        staged.as_file().set_permissions(permissions).into_app_err(permissions_failure)
    })?;
    faultable(WriteBackStep::Persist, fault, || {
        staged.persist(&target).map(|_| ()).into_app_err(persist_failure)
    })?;

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
        .into_app_err(format!("failed to enumerate the workspace members of {}", manifest_path.display()))?;

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

    if current != expected {
        ohno::bail!("workspace membership changed while the check was running; not writing");
    }

    Ok(())
}

/// Report workspace allow-list entries declared nowhere.
fn report_stale(stale: &[String]) {
    for name in stale {
        eprintln!("⚠️ '{name}' is allowed but is not declared by the workspace or any member; the allow-list entry can be removed.");
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
    evidence_target_dir: tempfile::TempDir,
}

/// Read every member's declarations, the workspace allow-list, and where to build.
fn workspace_of(manifest_path: &Path) -> Result<Workspace> {
    workspace_of_with(manifest_path, || {
        MetadataCommand::new().manifest_path(manifest_path).no_deps().exec()
    })
}

fn workspace_of_with(
    manifest_path: &Path,
    execute: impl FnOnce() -> std::result::Result<cargo_metadata::Metadata, cargo_metadata::Error>,
) -> Result<Workspace> {
    let metadata = execute().into_app_err(format!("failed to enumerate the workspace members of {}", manifest_path.display()))?;

    workspace_from(manifest_path, metadata, create_evidence_directory)
}

fn create_evidence_directory(root: &Path) -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new().prefix("run-").tempdir_in(root)
}

fn workspace_from(
    manifest_path: &Path,
    metadata: cargo_metadata::Metadata,
    create_directory: impl FnOnce(&Path) -> std::io::Result<tempfile::TempDir>,
) -> Result<Workspace> {
    let mut packages = Vec::new();
    for package in metadata.workspace_packages() {
        let path = package.manifest_path.clone().into_std_path_buf();
        let document = detect::read_manifest(&path)?;

        packages.push(verdict::Package {
            name: package.name.to_string(),
            version: package.version.to_string(),
            declared: detect::declared_dependencies(&document),
            allowed: detect::package_allowed(&document)?,
            has_doctests: package.targets.iter().any(|target| {
                target.kind.iter().any(|kind| {
                    matches!(
                        kind.to_string().as_str(),
                        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                    )
                })
            }),
            manifest_path: path,
        });
    }

    let root = detect::read_manifest(manifest_path)?;
    let allowed = match detect::catalog(&root)? {
        Catalog::Workspace(catalog) => catalog.allowed,
        Catalog::NotAWorkspace => BTreeSet::new(),
    };

    let evidence_root = metadata.target_directory.into_std_path_buf().join("unused-deps");
    fs::create_dir_all(&evidence_root).into_app_err(format!("failed to create {}", evidence_root.display()))?;
    let evidence_target_dir = create_directory(&evidence_root)
        .into_app_err(format!("failed to create an evidence directory under {}", evidence_root.display()))?;

    Ok(Workspace {
        packages,
        allowed,
        evidence_target_dir,
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

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod source_campaign_tests {
    use super::*;

    #[test]
    fn dispatch_preserves_wrapper_precedence_arguments_and_subcommand_escape() {
        for (marker, subcommand, expected) in [
            (evidence::WRAPPER_VAR, "rustc", "compiler"),
            (doctests::RUSTDOC_WRAPPER_VAR, "input.rs", "rustdoc"),
            (doctests::CAPTURE_VAR, "input.rs", "shim"),
            (doctests::CAPTURE_VAR, SUBCOMMAND, "run"),
            ("unrelated", SUBCOMMAND, "run"),
        ] {
            let args = ["binary", subcommand, "last"].map(OsString::from);
            dispatch_with(
                &args,
                |key| (key == marker).then(|| "capture".into()),
                |forwarded| {
                    assert_eq!(expected, "compiler");
                    assert_eq!(forwarded, &args[1..]);
                    Ok(ExitCode::SUCCESS)
                },
                |forwarded| {
                    assert_eq!(expected, "rustdoc");
                    assert_eq!(forwarded, &args[1..]);
                    Ok(ExitCode::SUCCESS)
                },
                |forwarded, capture| {
                    assert_eq!(expected, "shim");
                    assert_eq!(forwarded, &args[1..]);
                    assert_eq!(capture, Path::new("capture"));
                    Ok(ExitCode::SUCCESS)
                },
                || {
                    assert_eq!(expected, "run");
                    Ok(ExitCode::SUCCESS)
                },
            )
            .expect("the selected dispatch branch succeeds");
        }
    }

    fn cli(package: &str) -> Cli {
        Cli::try_parse_from(["cargo", SUBCOMMAND, "--manifest-path", "Cargo.toml", "--package", package])
            .expect("test CLI arguments are valid")
    }

    #[test]
    fn run_preserves_all_error_boundaries_and_combines_catalog_and_source_findings() {
        for failing_stage in 0..5 {
            let result = run_with(
                cli(if failing_stage == 2 { "missing" } else { "fixture" }),
                |_| {
                    if failing_stage == 0 {
                        Err(ohno::app_err!("root failed"))
                    } else {
                        Ok("resolved.toml".into())
                    }
                },
                |path| {
                    assert_eq!(path, Path::new("resolved.toml"));
                    if failing_stage == 1 {
                        Err(ohno::app_err!("workspace failed"))
                    } else {
                        Ok(workspace(vec![package("fixture", true)]))
                    }
                },
                |path, fix, require| {
                    assert_eq!(path, Path::new("resolved.toml"));
                    assert!(!fix);
                    assert!(!require);
                    if failing_stage == 3 {
                        Err(ohno::app_err!("catalog failed"))
                    } else {
                        Ok(ExitCode::SUCCESS)
                    }
                },
                |path, flags, workspace, selected, checks| {
                    assert_eq!(path, Path::new("resolved.toml"));
                    assert_eq!(flags, [OsString::from("--package"), OsString::from("fixture")]);
                    assert_eq!(selected, &BTreeSet::from([workspace.packages[0].manifest_path.clone()]));
                    assert!(checks.is_empty());
                    Err(ohno::app_err!("source failed"))
                },
            );
            let error = result.expect_err("every injected stage failure propagates");
            let expected = ["root failed", "workspace failed", "missing", "catalog failed", "source failed"][failing_stage];
            assert!(error.to_string().contains(expected), "{error}");
        }
        for catalog_failed in [false, true] {
            for source_failed in [false, true] {
                let result = run_with(
                    cli("fixture"),
                    |_| Ok("resolved.toml".into()),
                    |_| Ok(workspace(vec![package("fixture", true)])),
                    |_, _, _| Ok(if catalog_failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }),
                    |_, _, _, _, _| Ok(source_failed),
                )
                .expect("both check stages complete");
                assert_eq!(
                    result,
                    if catalog_failed || source_failed {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    }
                );
            }
        }
    }

    fn write_manifest(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("fixture manifests have a parent")).expect("fixture directory exists");
        fs::write(path, text).expect("fixture manifest is writable");
    }

    #[test]
    fn workspace_resolution_returns_canonicalization_errors_after_reading_a_valid_package() {
        let manifest = Path::new("fixture/Cargo.toml");
        let read = std::cell::Cell::new(false);
        let error = workspace_manifest_resolved_with(
            manifest,
            |_| panic!("failed canonicalization must stop before workspace membership queries"),
            |path| {
                assert_eq!(path, manifest);
                assert!(read.get(), "the valid package must be read before canonicalization");
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected canonicalization failure",
                ))
            },
            |path| {
                assert_eq!(path, manifest);
                assert!(!read.replace(true), "the supplied manifest is read exactly once");
                detect::parse_manifest("[package]\nname='fixture'\nversion='0.1.0'\n", path)
            },
            |_| panic!("failed canonicalization must stop before ancestor classification"),
        )
        .unwrap_err();
        assert_eq!(error.message(), "injected canonicalization failure");
        assert!(
            error.to_string().contains(&format!("failed to resolve {}", manifest.display())),
            "{error}"
        );
        let cause = error
            .source()
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .expect("the injected canonicalization error is preserved");
        assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(cause.to_string(), "injected canonicalization failure");
    }

    #[test]
    fn ancestor_resolution_skips_missing_and_nonworkspace_manifests_and_propagates_errors() {
        let directory = tempfile::tempdir().expect("fixture directory exists");
        let root = directory.path().canonicalize().expect("fixture directory is canonical");
        let root_manifest = root.join("Cargo.toml");
        let intermediate = root.join("nested").join("Cargo.toml");
        let member = root.join("nested").join("missing").join("member").join("Cargo.toml");
        write_manifest(&root_manifest, "[workspace]\n");
        write_manifest(&intermediate, "[package]\nname='intermediate'\nversion='0.1.0'\n");
        write_manifest(&member, "[package]\nname='member'\nversion='0.1.0'\n");
        let resolved = workspace_manifest_with(&member, |path| {
            assert_eq!(path, root_manifest);
            Ok(vec![root.join("unrelated").join("Cargo.toml"), member.clone()])
        })
        .expect("a later matching workspace is resolved");
        assert_eq!(resolved, root_manifest);
        let error = workspace_manifest_with(&member, |_| Err(ohno::app_err!("members unavailable")))
            .expect_err("member enumeration failure propagates");
        assert!(error.to_string().contains("members unavailable"), "{error}");
        assert_eq!(
            workspace_manifest_with(&root_manifest, |_| panic!("a root needs no enumeration")).expect("root is accepted"),
            root_manifest
        );
        write_manifest(&intermediate, "not valid toml ]");
        workspace_manifest_with(&member, |_| panic!("invalid ancestor stops enumeration")).expect_err("invalid ancestor propagates");
        workspace_manifest_with(&root.join("absent.toml"), |_| panic!("missing input stops enumeration"))
            .expect_err("missing input propagates");
    }

    #[test]
    fn a_supplied_workspace_wins_over_an_enclosing_workspace() {
        let inner = Path::new("outer/nested/Cargo.toml");
        let mut reads = Vec::new();
        let resolved = workspace_manifest_resolved_with(
            inner,
            |_| panic!("a supplied workspace must not enumerate an enclosing workspace"),
            |_| panic!("a supplied workspace must resolve before canonicalization"),
            |path| {
                assert_eq!(path, inner, "an enclosing workspace must not be read");
                reads.push(path.to_path_buf());
                detect::parse_manifest("[workspace]\n", path)
            },
            |_| panic!("a supplied workspace must resolve before ancestor classification"),
        )
        .expect("the supplied workspace resolves directly");

        assert_eq!(resolved, inner);
        assert_eq!(reads, [inner]);
    }

    #[test]
    fn empty_catalog_is_successful_without_member_enumeration() {
        let directory = tempfile::tempdir().expect("fixture directory exists");
        let manifest = directory.path().join("Cargo.toml");
        write_manifest(&manifest, "[workspace]\n");
        assert_eq!(
            catalog_check_with_hook(&manifest, false, false, |_| panic!("empty catalogs need no Cargo metadata"))
                .expect("an empty catalog is successful"),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn exclusions_preserve_missing_matches_and_reject_ambiguous_selectors() {
        let packages = [package("fixture", true)];
        let selection = PackageSelection {
            packages: Vec::new(),
            workspace: true,
            exclude: vec!["missing".into()],
        };
        assert_eq!(
            selection.resolve(&packages).expect("missing excludes are allowed"),
            BTreeSet::from([packages[0].manifest_path.clone()])
        );
        let mut second = package("fixture", true);
        second.version = "2.0.0".into();
        second.manifest_path = PathBuf::from("second").join("Cargo.toml");
        let packages = [package("fixture", true), second];
        let selection = PackageSelection {
            packages: Vec::new(),
            workspace: true,
            exclude: vec!["fixture".into()],
        };
        let error = selection.resolve(&packages).expect_err("ambiguous exclusions must return errors");
        assert!(error.to_string().contains("matched more than one workspace member"), "{error}");
    }

    #[test]
    fn workspace_metadata_and_canonicalization_failures_preserve_manifest_context() {
        let manifest = Path::new("Cargo.toml");
        let error = workspace_of_with(manifest, || {
            Err(cargo_metadata::Error::Io(std::io::Error::other("metadata unavailable")))
        })
        .err()
        .expect("metadata errors must propagate instead of panicking");
        assert!(
            error
                .to_string()
                .contains("failed to enumerate the workspace members of Cargo.toml"),
            "{error}"
        );
        let error = canonical_manifest(manifest, Err(std::io::Error::other("canonicalization unavailable")))
            .expect_err("canonicalization errors must propagate instead of panicking");
        assert!(error.to_string().contains("failed to resolve Cargo.toml"), "{error}");
    }

    fn metadata(root: &Path, manifests: &[PathBuf]) -> cargo_metadata::Metadata {
        let packages = manifests.iter().enumerate().map(|(index, path)| serde_json::json!({
            "name": format!("package{index}"), "version": "1.2.3", "id": format!("package{index} 1.2.3 (path+file:///fixture{index})"),
            "license": null, "license_file": null, "description": null, "source": null, "dependencies": [],
            "targets": [{"kind":["bin"], "crate_types":["bin"], "name":"binary", "src_path":root.join("main.rs"), "edition":"2024", "doc":true, "doctest":false, "test":true},
                        {"kind":["rlib", "bin"], "crate_types":["rlib"], "name":"library", "src_path":root.join("lib.rs"), "edition":"2024", "doc":true, "doctest":true, "test":true}],
            "features": {}, "manifest_path": path, "metadata": {}, "publish": null,
            "authors": [], "categories": [], "keywords": [], "readme": null, "repository": null,
            "homepage": null, "documentation": null, "edition":"2024", "links":null, "default_run":null, "rust_version":null
        })).collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "workspace_members": packages.iter().map(|package| package["id"].clone()).collect::<Vec<_>>(),
            "workspace_default_members": [], "packages": packages, "resolve":null, "target_directory":root.join("target"),
            "workspace_root": root, "version":1, "metadata":{}
        }))
        .expect("the synthetic metadata follows Cargo's JSON format")
    }

    #[test]
    fn workspace_materialization_preserves_targets_paths_allowlists_and_filesystem_errors() {
        let directory = tempfile::tempdir().expect("fixture directory exists");
        let root = directory.path();
        let manifest = root.join("Cargo.toml");
        let member = root.join("member").join("Cargo.toml");
        write_manifest(
            &manifest,
            "[workspace]\n[workspace.metadata.unused-deps]\nallowed=['root-allowed']\n",
        );
        write_manifest(
            &member,
            "[package]\nname='member'\nversion='1.2.3'\n[package.metadata.unused-deps]\nallowed=['member-allowed']\n[dependencies]\ndep='1'\n",
        );
        let workspace = workspace_from(&manifest, metadata(root, std::slice::from_ref(&member)), create_evidence_directory)
            .expect("valid metadata is materialized");
        assert_eq!(
            workspace.evidence_target_dir.path().parent(),
            Some(root.join("target").join("unused-deps").as_path())
        );
        assert!(
            workspace
                .evidence_target_dir
                .path()
                .file_name()
                .expect("run directory has a name")
                .to_string_lossy()
                .starts_with("run-")
        );
        assert_eq!(workspace.allowed, BTreeSet::from(["root-allowed".to_owned()]));
        assert_eq!(workspace.packages.len(), 1);
        assert_eq!(workspace.packages[0].manifest_path, member);
        assert!(workspace.packages[0].has_doctests);
        assert_eq!(workspace.packages[0].declared[0].name, "dep");
        assert_eq!(workspace.packages[0].allowed, BTreeSet::from(["member-allowed".to_owned()]));
        workspace_from(&manifest, metadata(root, std::slice::from_ref(&member)), |_| {
            Err(std::io::Error::other("run directory unavailable"))
        })
        .err()
        .expect("run directory failure propagates");
        write_manifest(&member, "[package.metadata.unused-deps]\nallowed=123\n");
        workspace_from(&manifest, metadata(root, std::slice::from_ref(&member)), create_evidence_directory)
            .err()
            .expect("invalid package allowlist propagates");
        fs::remove_file(&member).expect("member fixture can be removed");
        workspace_from(&manifest, metadata(root, std::slice::from_ref(&member)), create_evidence_directory)
            .err()
            .expect("missing member manifest propagates");
        write_manifest(&manifest, "[workspace.metadata.unused-deps]\nallowed=123\n");
        workspace_from(&manifest, metadata(root, &[]), create_evidence_directory)
            .err()
            .expect("invalid root catalog propagates");
        fs::remove_file(&manifest).expect("root fixture can be removed");
        workspace_from(&manifest, metadata(root, &[]), create_evidence_directory)
            .err()
            .expect("missing root manifest propagates");
        write_manifest(&manifest, "[workspace]\n");
        fs::remove_dir_all(root.join("target")).expect("target fixture can be removed");
        fs::write(root.join("target"), "").expect("blocking target file exists");
        workspace_from(&manifest, metadata(root, &[]), create_evidence_directory)
            .err()
            .expect("evidence root creation failure propagates");
    }

    fn package(name: &str, has_doctests: bool) -> verdict::Package {
        verdict::Package {
            name: name.to_owned(),
            version: "1.2.3".to_owned(),
            manifest_path: PathBuf::from(name).join("Cargo.toml"),
            declared: vec![],
            allowed: BTreeSet::new(),
            has_doctests,
        }
    }

    fn workspace(packages: Vec<verdict::Package>) -> Workspace {
        Workspace {
            packages,
            allowed: BTreeSet::new(),
            evidence_target_dir: tempfile::tempdir().expect("evidence directory exists"),
        }
    }

    fn finding(package: &verdict::Package, section: Section) -> verdict::Finding {
        verdict::Finding {
            package: package.name.clone(),
            manifest_path: package.manifest_path.clone(),
            name: "dependency".to_owned(),
            section,
            target: None,
            verdict: Verdict::Unused,
        }
    }

    #[test]
    fn source_collection_preserves_both_passes_and_each_failure_boundary() {
        let workspace = workspace(Vec::new());
        let selected = BTreeSet::new();
        let flags = [OsString::from("--workspace")];
        for failing_pass in [0, 1, 2] {
            let mut calls = 0;
            let error = source_checks_with(
                Path::new("Cargo.toml"),
                &flags,
                &workspace,
                &selected,
                &[],
                |manifest, selection, target, all_targets| {
                    assert_eq!(manifest, Path::new("Cargo.toml"));
                    assert_eq!(selection, flags);
                    assert_eq!(
                        target,
                        workspace
                            .evidence_target_dir
                            .path()
                            .join(if calls == 0 { "plain" } else { "all-targets" })
                    );
                    assert_eq!(all_targets, calls != 0);
                    let current = calls;
                    calls += 1;
                    if current == failing_pass {
                        Err(ohno::app_err!("evidence pass {current} failed"))
                    } else {
                        Ok(evidence::Evidence::default())
                    }
                },
                |_, _, candidates| {
                    assert!(candidates.is_empty());
                    assert_eq!(failing_pass, 2);
                    Err(ohno::app_err!("doctest pass failed"))
                },
            )
            .expect_err("every evidence failure must propagate");
            assert!(
                error.to_string().contains(if failing_pass == 2 {
                    "doctest pass failed"
                } else {
                    "evidence pass"
                }),
                "{error}"
            );
            assert_eq!(calls, if failing_pass == 0 { 1 } else { 2 });
        }
    }

    #[test]
    fn doctest_candidates_obey_section_and_requested_check_rules() {
        let mut package = package("fixture", true);
        package.declared = [Section::Normal, Section::Development, Section::Build]
            .map(|section| detect::Declared {
                name: format!("{section:?}"),
                section,
                target: None,
                feature_referenced: false,
            })
            .to_vec();
        let selected = BTreeSet::from([package.manifest_path.clone()]);
        let workspace = workspace(vec![package]);
        for check in [Check::Unused, Check::Misplaced] {
            let found = source_checks_with(
                Path::new("Cargo.toml"),
                &[],
                &workspace,
                &selected,
                &[check],
                |_, _, _, _| Ok(evidence::Evidence::default()),
                |_, _, candidates| {
                    let sections = candidates.iter().map(|finding| finding.section).collect::<Vec<_>>();
                    assert_eq!(
                        sections,
                        if check == Check::Unused {
                            vec![Section::Normal, Section::Development]
                        } else {
                            vec![Section::Normal]
                        }
                    );
                    Ok(doctests::DoctestEvidence::default())
                },
            )
            .expect("empty evidence is valid");
            assert_eq!(found, check == Check::Unused);
        }
    }

    #[test]
    fn doctest_dispatch_is_lazy_and_collects_only_accused_libraries() {
        let workspace = workspace(vec![package("accused", true), package("unaccused", true), package("binary", false)]);
        doctest_evidence_with(
            Path::new("Cargo.toml"),
            &workspace,
            &[],
            || panic!("empty candidates need no shim"),
            |_, _, _, _| panic!("empty candidates need no doctests"),
        )
        .expect("empty candidates are valid");
        let candidates = [
            finding(&workspace.packages[0], Section::Normal),
            finding(&workspace.packages[2], Section::Normal),
        ];
        let mut calls = 0;
        doctest_evidence_with(
            Path::new("Cargo.toml"),
            &workspace,
            &candidates,
            || Ok("shim".into()),
            |manifest, selector, target, shim| {
                calls += 1;
                assert_eq!(manifest, Path::new("Cargo.toml"));
                assert_eq!(selector, "accused@1.2.3");
                assert_eq!(target, workspace.evidence_target_dir.path().join("doctests"));
                assert_eq!(shim, Path::new("shim"));
                Ok(doctests::PackageDoctests::default())
            },
        )
        .expect("only the accused library is collected");
        assert_eq!(calls, 1);
        let error = doctest_evidence_with(
            Path::new("Cargo.toml"),
            &workspace,
            &candidates,
            || Err(std::io::Error::other("executable unavailable")),
            |_, _, _, _| panic!("shim failure stops collection"),
        )
        .expect_err("shim lookup failure propagates");
        assert!(
            error
                .to_string()
                .contains("failed to locate this executable to use as the doctest shim"),
            "{error}"
        );
        let error = doctest_evidence_with(
            Path::new("Cargo.toml"),
            &workspace,
            &candidates,
            || Ok("shim".into()),
            |_, _, _, _| Err(ohno::app_err!("collection failed")),
        )
        .expect_err("package doctest collection failure propagates");
        assert!(error.to_string().contains("collection failed"), "{error}");
    }

    #[test]
    fn reported_dependency_tables_preserve_every_section_and_target() {
        for (section, name) in [
            (Section::Normal, "dependencies"),
            (Section::Development, "dev-dependencies"),
            (Section::Build, "build-dependencies"),
        ] {
            assert_eq!(dependency_section(section), name);
            assert_eq!(dependency_table(None, dependency_section(section)), format!("[{name}]"));
            assert_eq!(
                dependency_table(Some("cfg(windows)"), dependency_section(section)),
                format!("[target.'cfg(windows)'.{name}]")
            );
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod selection_tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Check, PackageSelection, resolve_selectors, verdict};

    #[test]
    fn version_qualified_selectors_resolve_and_errors_are_propagated() {
        let packages = [verdict::Package {
            name: "fixture".to_owned(),
            version: "1.2.3".to_owned(),
            manifest_path: PathBuf::from("fixture").join("Cargo.toml"),
            declared: Vec::new(),
            allowed: BTreeSet::new(),
            has_doctests: false,
        }];
        assert_eq!(
            resolve_selectors(&packages, &["fixture@1.2.3".to_owned()], false).expect("qualified selector matches"),
            BTreeSet::from([packages[0].manifest_path.clone()])
        );
        let selection = PackageSelection {
            packages: vec!["missing".to_owned()],
            workspace: false,
            exclude: Vec::new(),
        };
        selection.resolve(&packages).expect_err("a missing selected package is an error");
        let selection = PackageSelection {
            packages: Vec::new(),
            workspace: true,
            exclude: vec!["fixture".to_owned()],
        };
        assert!(selection.resolve(&packages).expect("a matching exclusion resolves").is_empty());
    }

    #[test]
    fn check_filtering_selects_only_requested_findings() {
        assert!(Check::Unused.wanted(&[]));
        assert!(Check::Unused.wanted(&[Check::Unused]));
        assert!(!Check::Unused.wanted(&[Check::Misplaced]));
    }

    #[test]
    fn package_selection_forwards_every_cargo_flag() {
        let selection = PackageSelection {
            packages: vec!["alpha@1.0.0".to_owned()],
            workspace: false,
            exclude: Vec::new(),
        };
        assert_eq!(selection.flags(), [OsString::from("--package"), OsString::from("alpha@1.0.0")]);

        let workspace = PackageSelection {
            packages: Vec::new(),
            workspace: true,
            exclude: vec!["beta@1.0.0".to_owned()],
        };
        assert_eq!(
            workspace.flags(),
            [
                OsString::from("--workspace"),
                OsString::from("--exclude"),
                OsString::from("beta@1.0.0"),
            ]
        );
    }

    #[test]
    fn ambiguous_package_names_require_a_version() {
        let packages = [("one", "1.0.0"), ("two", "2.0.0")].map(|(path, version)| verdict::Package {
            name: "same".to_owned(),
            version: version.to_owned(),
            manifest_path: PathBuf::from(path),
            declared: Vec::new(),
            allowed: BTreeSet::new(),
            has_doctests: false,
        });

        let error = resolve_selectors(&packages, &["same".to_owned()], false).expect_err("ambiguous selectors must fail");
        assert!(error.to_string().contains("matched more than one workspace member"));
    }

    #[test]
    fn missing_exclusions_are_ignored_locally_for_cargo_to_warn_about() {
        assert!(
            resolve_selectors(&[], &["optional".to_owned()], true)
                .expect("missing excludes are allowed")
                .is_empty()
        );
    }
}

#[cfg(test)]
// Miri runs with filesystem isolation, and these tests need real files in a
// real temp directory.
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use ohno::app_err;
    use tempfile::TempDir;

    use super::{
        CheckStage, WriteBackStep, catalog_check, catalog_check_with_hook, verify_members_unchanged, workspace_manifest_resolved_with,
        write_back, write_back_with_fault,
    };
    use crate::detect::parse_manifest;

    #[test]
    fn survivor_ancestor_search_does_not_reopen_the_supplied_package_manifest() {
        let input = std::path::Path::new("input-member/Cargo.toml");
        let canonical = std::path::Path::new("workspace/member/Cargo.toml");
        let enclosing = std::path::Path::new("workspace/Cargo.toml");
        let mut reads = Vec::new();
        let mut inspected = Vec::new();
        let result = workspace_manifest_resolved_with(
            input,
            |_| panic!("injected package documents are never workspaces"),
            |_| Ok(canonical.to_path_buf()),
            |path| {
                reads.push(path.to_path_buf());
                if path == canonical {
                    return Err(app_err!("the package manifest disappeared after its first read"));
                }
                parse_manifest("[package]\nname='member'\nversion='0.1.0'\n", path)
            },
            |path| {
                inspected.push(path.to_path_buf());
                path == canonical || path == enclosing
            },
        )
        .unwrap();
        assert_eq!(result, input);
        assert_eq!(reads, [input, enclosing]);
        assert_eq!(inspected, [enclosing, std::path::Path::new("Cargo.toml")]);
    }

    fn write_member(path: &std::path::Path, name: &str, body: &str) {
        fs::create_dir_all(path.join("src")).expect("create member source directory");
        fs::write(path.join("src/lib.rs"), "").expect("write member source");
        fs::write(
            path.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n{body}"),
        )
        .expect("write member manifest");
    }

    #[test]
    fn check_propagates_root_read_parse_and_catalog_errors() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let manifest = dir.path().join("Cargo.toml");

        let read_error = catalog_check(&manifest, false, false).expect_err("a missing root manifest must fail");
        assert!(read_error.to_string().contains("failed to read"), "unexpected error: {read_error}");

        fs::write(&manifest, "[workspace").expect("write malformed root manifest");
        let parse_error = catalog_check(&manifest, false, false).expect_err("malformed TOML must fail");
        assert!(
            parse_error.to_string().contains("failed to parse"),
            "unexpected error: {parse_error}"
        );

        fs::write(&manifest, "[workspace]\ndependencies = \"invalid\"\n").expect("write invalid catalog");
        let catalog_error = catalog_check(&manifest, false, false).expect_err("an invalid catalog must fail");
        assert!(
            catalog_error.to_string().contains("[workspace.dependencies] must be a table"),
            "unexpected error: {catalog_error}"
        );
    }

    #[test]
    fn check_propagates_member_enumeration_errors() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let manifest = dir.path().join("Cargo.toml");
        fs::write(
            &manifest,
            "[workspace]\nmembers = [\"missing\"]\n\n[workspace.dependencies]\nunused = \"1\"\n",
        )
        .expect("write workspace");

        let error = catalog_check(&manifest, false, false).expect_err("invalid workspace membership must fail");

        assert!(
            error.to_string().contains("failed to enumerate the workspace members"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn check_propagates_member_read_errors() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let manifest = dir.path().join("Cargo.toml");
        let member = dir.path().join("member");
        fs::write(
            &manifest,
            "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nunused = \"1\"\n",
        )
        .expect("write workspace");
        write_member(&member, "member", "");

        let error = catalog_check_with_hook(&manifest, false, false, |stage| {
            if stage == CheckStage::MembersEnumerated {
                fs::remove_file(member.join("Cargo.toml")).expect("remove member manifest");
            }
        })
        .expect_err("a disappearing member manifest must fail");

        assert!(error.to_string().contains("failed to read"), "unexpected error: {error}");
    }

    #[test]
    fn check_propagates_membership_changes_before_fixing() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let manifest = dir.path().join("Cargo.toml");
        let member = dir.path().join("member");
        let newcomer = dir.path().join("newcomer");
        fs::write(
            &manifest,
            "[workspace]\nmembers = [\"*\"]\n\n[workspace.dependencies]\nunused = \"1\"\n",
        )
        .expect("write workspace");
        write_member(&member, "member", "");

        let error = catalog_check_with_hook(&manifest, true, false, |stage| {
            if stage == CheckStage::InheritanceCollected {
                write_member(&newcomer, "newcomer", "");
            }
        })
        .expect_err("changed workspace membership must abort the fix");

        assert!(
            error
                .to_string()
                .contains("workspace membership changed while the check was running"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn check_propagates_write_back_errors() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let manifest = dir.path().join("Cargo.toml");
        let member = dir.path().join("member");
        fs::write(
            &manifest,
            "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\nunused = \"1\"\n",
        )
        .expect("write workspace");
        write_member(&member, "member", "");

        let error = catalog_check_with_hook(&manifest, true, false, |stage| {
            if stage == CheckStage::MembersVerified {
                fs::write(
                    &manifest,
                    "[workspace]\nmembers = [\"member\"]\n\n[workspace.dependencies]\ncompeting = \"1\"\n",
                )
                .expect("write competing edit");
            }
        })
        .expect_err("a competing edit must abort the fix");

        assert!(
            error.to_string().contains("changed on disk while the check was running"),
            "unexpected error: {error}"
        );
    }

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
    fn write_back_propagates_each_atomic_replacement_failure() {
        for step in [
            WriteBackStep::Resolve,
            WriteBackStep::Read,
            WriteBackStep::Metadata,
            WriteBackStep::Create,
            WriteBackStep::Write,
            WriteBackStep::Permissions,
            WriteBackStep::Persist,
        ] {
            let dir = TempDir::new().expect("failed to create temp dir");
            let path = dir.path().join("Cargo.toml");
            fs::write(&path, "original").expect("failed to seed the manifest");

            let error = write_back_with_fault(&path, "original", &[], "replacement", Some(step))
                .expect_err("injected I/O failures must be returned");

            assert_eq!(error.to_string(), format!("injected {step:?} failure"));
            assert_eq!(
                fs::read_to_string(&path).expect("failed to read back"),
                "original",
                "{step:?} failure must not replace the manifest"
            );
        }
    }

    #[test]
    fn write_back_returns_real_resolve_and_read_errors() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let missing = dir.path().join("missing.toml");
        let resolve_error = write_back(&missing, "", &[], "replacement").expect_err("missing path must fail");
        assert!(
            resolve_error.to_string().contains("failed to resolve"),
            "unexpected error: {resolve_error}"
        );

        let read_error = write_back(dir.path(), "", &[], "replacement").expect_err("a directory is not a manifest");
        assert!(
            read_error.to_string().contains("failed to re-read"),
            "unexpected error: {read_error}"
        );
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

    #[test]
    fn membership_lookup_failure_is_returned_instead_of_panicking() {
        let error = verify_members_unchanged(std::path::Path::new("a-manifest-that-does-not-exist"), &[])
            .expect_err("cargo metadata failure must be returned");
        assert!(error.to_string().contains("failed to enumerate the workspace members"), "{error}");
    }
}
