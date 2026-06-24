// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level `update` driver.
//!
//! Orchestrates: workspace discovery, manifest load, backend resolution,
//! emitter invocation, plan accumulation, and final apply/summarize.

use std::collections::BTreeSet;
use std::path::Path;

use ohno::{AppError, bail};
use tracing::info;

use crate::backend::{self, Backend};
use crate::catalog::Catalog;
use crate::catalog::artifact::{Artifact, HostSelector, RegionSpec};
use crate::checksum::checksum_str;
use crate::cli::Cli;
use crate::decision::{Decision, decide_removal};
use crate::emit::{plan_managed_region, plan_owned_file};
use crate::io::{read_file_if_present, resolve_existing_case_insensitive};
use crate::manifest::Manifest;
use crate::plan::{Plan, PlanItem, Target};
use crate::region::{CommentSyntax, find_region, remove_region};
use crate::workspace::{self, Workspace};

/// Outcome of an `update` invocation.
#[derive(Debug)]
pub struct RunOutcome {
    /// The plan that was built.
    pub plan: Plan,
    /// The manifest as it existed before this run (useful for the
    /// categorized summary so `Will create` and `Will update` can be
    /// distinguished, and so stale entries can be enumerated).
    pub previous_manifest: Manifest,
    /// Whether the plan was actually applied to disk.
    pub applied: bool,
    /// The resolved backend set.
    pub backends: Vec<Backend>,
}

/// Run the parsed CLI.
///
/// # Errors
///
/// Returns an error when the underlying update flow fails.
#[mutants::skip] // Thin process-boundary glue (cwd lookup, stdout print, `std::process::exit`); behavior covered by `run_update` tests which exercise every dispatch path.
pub fn run(catalog: &Catalog, cli: &Cli) -> Result<(), AppError> {
    let outcome = run_update(catalog, cli, &std::env::current_dir()?)?;
    print!("{}", outcome.plan.summary(Some(&outcome.previous_manifest)));
    if cli.dry_run && outcome.plan.has_changes() {
        std::process::exit(1);
    }
    Ok(())
}

/// Run the update flow against the workspace containing `start_dir`.
///
/// Exposed for integration tests that want to drive the algorithm
/// without `std::process::exit`.
///
/// # Errors
///
/// Propagates errors from any subsystem (workspace discovery, manifest
/// I/O, emitter, plan application).
pub fn run_update(catalog: &Catalog, args: &Cli, start_dir: &Path) -> Result<RunOutcome, AppError> {
    let repo_root = workspace::find_workspace_root(start_dir)?;
    let manifest = Manifest::load(&repo_root)?;

    // The single-tool guard: a repository is managed by exactly one
    // anvil-family tool. If the lock records a *different* tool, refuse
    // before doing any other work — content-free, and honored even under
    // --dry-run — unless --force is passed to switch ownership to this tool.
    // A lock with no `tool` field (first run, or a legacy pre-split lock) is
    // never blocked. See updates.md §1 "The single-tool guard".
    //
    // This runs immediately after loading the lock and before
    // `load_workspace`, so a mismatched lock reliably refuses regardless of
    // the workspace shape — the wrong tool never reaches workspace/member
    // parsing, which could otherwise surface unrelated errors first.
    enforce_single_tool_guard(catalog, args, &manifest)?;

    let ws = workspace::load_workspace(&repo_root)?;

    let backends = backend::resolve(&args.backends, args.no_backends, &repo_root)?;
    info!(
        repo_root = %repo_root.display(),
        backends = ?backends.iter().map(|b| b.name()).collect::<Vec<_>>(),
        dry_run = args.dry_run,
        "anvil"
    );

    let plan = build_plan(&repo_root, &ws, &manifest, &backends, catalog)?;

    let applied = if args.dry_run {
        false
    } else {
        let mut next = plan.apply(&repo_root, &manifest)?;
        // Stamp this tool's provenance on every save.
        next.tool = Some(catalog.cli().subcommand.clone());
        next.tool_version = Some(catalog.cli().version.clone());
        next.catalog_checksum = Some(catalog.checksum());
        next.save(&repo_root)?;
        true
    };

    Ok(RunOutcome {
        plan,
        previous_manifest: manifest,
        applied,
        backends,
    })
}

/// Enforce the single-tool guard: refuse if the lock names a different tool
/// and `--force` was not passed.
///
/// # Errors
///
/// Returns a refusal error when `manifest.tool` is `Some` and differs from
/// `catalog.cli().subcommand` and `args.force` is `false`.
fn enforce_single_tool_guard(catalog: &Catalog, args: &Cli, manifest: &Manifest) -> Result<(), AppError> {
    let current = &catalog.cli().subcommand;
    if let Some(owner) = &manifest.tool
        && owner != current
        && !args.force
    {
        bail!(
            "this repository is managed by '{owner}' (per .anvil.lock); refusing to run '{current}'. \
             A repository must be managed by a single anvil-family tool. Run '{owner}' instead, \
             or re-run with --force to switch this repository to '{current}'."
        );
    }
    Ok(())
}

/// Build the full plan by iterating the catalog's artifacts.
///
/// Each artifact dispatches to the generic owned-file / managed-region
/// driver. Owned files carrying a backend `gate` are emitted only when that
/// backend is in the resolved set. Managed-region host selectors are expanded
/// against the discovered workspace (see [`push_region`]). Every path is
/// resolved to its on-disk casing so anvil follows whatever a repo already
/// uses (e.g. `justfile` vs `Justfile`).
fn build_plan(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    backends: &[Backend],
    catalog: &Catalog,
) -> Result<Plan, AppError> {
    let mut plan = Plan::default();

    for artifact in catalog.artifacts() {
        match artifact {
            Artifact::OwnedFile(spec) => {
                let selected = spec.gate.is_none_or(|gate| backends.contains(&gate));
                if selected {
                    let path = resolve_existing_case_insensitive(repo_root, spec.path);
                    plan.push(plan_owned_file(repo_root, manifest, &path, &spec.body)?);
                }
            }
            Artifact::Region(spec) => {
                push_region(repo_root, workspace, manifest, &mut plan, spec)?;
            }
        }
    }

    plan_removals(repo_root, manifest, &mut plan)?;

    Ok(plan)
}

/// Dispatch one managed-region artifact into the plan, expanding its host
/// selector against the discovered workspace.
///
/// - [`HostSelector::Path`] — a single literal host.
/// - [`HostSelector::EachMemberManifest`] — one host per workspace member (no
///   hosts in a single-crate repo, which has no workspace members).
/// - [`HostSelector::WorkspaceCargoToml`] / [`HostSelector::SingleCrateCargoToml`]
///   — the root `Cargo.toml`, gated on whether it declares a `[workspace]`
///   table.
fn push_region(repo_root: &Path, workspace: &Workspace, manifest: &Manifest, plan: &mut Plan, spec: &RegionSpec) -> Result<(), AppError> {
    let id = spec.id.as_str();
    match &spec.host {
        HostSelector::Path(path) => {
            push_region_at(repo_root, manifest, plan, path, id, &spec.body, spec.syntax)?;
        }
        HostSelector::WorkspaceCargoToml => {
            if workspace.has_workspace_table {
                push_region_at(repo_root, manifest, plan, "Cargo.toml", id, &spec.body, spec.syntax)?;
            }
        }
        HostSelector::SingleCrateCargoToml => {
            if !workspace.has_workspace_table {
                push_region_at(repo_root, manifest, plan, "Cargo.toml", id, &spec.body, spec.syntax)?;
            }
        }
        HostSelector::EachMemberManifest => {
            for member in &workspace.members {
                push_region_at(repo_root, manifest, plan, &member.manifest_relpath, id, &spec.body, spec.syntax)?;
            }
        }
    }
    Ok(())
}

/// Plan one managed region at a single host, resolving the host's on-disk
/// casing first.
fn push_region_at(
    repo_root: &Path,
    manifest: &Manifest,
    plan: &mut Plan,
    host: &str,
    id: &str,
    body: &str,
    syntax: crate::region::CommentSyntax,
) -> Result<(), AppError> {
    let host = resolve_existing_case_insensitive(repo_root, host);
    plan.push(plan_managed_region(repo_root, manifest, &host, id, body, syntax)?);
    Ok(())
}

/// Scan the previous manifest for entries that the active plan items
/// don't cover. For each, classify as Remove (user untouched since the
/// last render) or `OrphanedKept` (user customized — preserve and
/// transfer ownership).
///
/// This is what removes orphaned cloud-workflow artifacts, dropped catalog entries,
/// disabled-backend files, and any other previously-tracked item that
/// is no longer in scope.
fn plan_removals(repo_root: &Path, previous: &Manifest, plan: &mut Plan) -> Result<(), AppError> {
    let live_files: BTreeSet<String> = plan
        .items()
        .iter()
        .filter_map(|i| match &i.target {
            Target::File { path } => Some(path.clone()),
            Target::Region { .. } => None,
        })
        .collect();
    let live_regions: BTreeSet<(String, String)> = plan
        .items()
        .iter()
        .filter_map(|i| match &i.target {
            Target::Region { host, id } => Some((host.clone(), id.clone())),
            Target::File { .. } => None,
        })
        .collect();

    for (path, last) in &previous.files {
        if live_files.contains(path) {
            continue;
        }
        let disk = read_file_if_present(&repo_root.join(path))?;
        let disk_checksum = disk.as_deref().map(checksum_str);
        match decide_removal(last, disk_checksum.as_deref()) {
            Decision::Remove => plan.push(PlanItem::remove_file(path.clone())),
            // `InSync` means the file is already gone but we still want
            // to purge the manifest entry, same as a customized orphan —
            // both surface as no-op plan items that drop the entry.
            Decision::OrphanedKept | Decision::InSync => plan.push(PlanItem::orphaned_kept(Target::File { path: path.clone() })),
            other => unreachable!("decide_removal returned {other:?} for a file orphan"),
        }
    }

    for (key, last) in &previous.regions {
        if live_regions.contains(&(key.host.clone(), key.id.clone())) {
            continue;
        }
        let host_path = repo_root.join(&key.host);
        let Some(host_text) = read_file_if_present(&host_path)? else {
            // Host file is gone entirely; just drop the manifest
            // entry. Emit OrphanedKept (no-op apply) so the plan
            // can record the transfer of ownership consistently.
            plan.push(PlanItem::orphaned_kept(Target::Region {
                host: key.host.clone(),
                id: key.id.clone(),
            }));
            continue;
        };

        // CommentSyntax is currently always Hash for managed regions.
        // When that assumption changes, the manifest will need to
        // record the syntax used.
        let syntax = CommentSyntax::Hash;
        let region = find_region(&host_text, &key.id, syntax)?;
        let body_checksum = region.as_ref().map(|r| checksum_str(r.body_str()));
        match decide_removal(last, body_checksum.as_deref()) {
            Decision::Remove => {
                let spliced = remove_region(&host_text, &key.id, syntax)?;
                plan.push(PlanItem::remove_region(key.host.clone(), key.id.clone(), spliced));
            }
            Decision::OrphanedKept | Decision::InSync => {
                plan.push(PlanItem::orphaned_kept(Target::Region {
                    host: key.host.clone(),
                    id: key.id.clone(),
                }));
            }
            other => unreachable!("decide_removal returned {other:?} for a region orphan"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::anvil::artifacts::region;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn empty_workspace() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
        );
        write(
            &root.join("crates/alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&root.join("crates/alpha/src/lib.rs"), "");
        tmp
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn existing_lowercase_justfile_is_reused_not_duplicated() {
        // Proposal: anvil follows whatever casing the repo already uses. A
        // pre-existing lowercase `justfile` must be spliced into, not shadowed
        // by a new capital `Justfile`.
        let tmp = empty_workspace();
        std::fs::write(tmp.path().join("justfile"), "# user recipes\n").unwrap();
        let args = local_only();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);

        let lower = std::fs::read_to_string(tmp.path().join("justfile")).unwrap();
        assert!(lower.contains("# user recipes"), "user content preserved");
        assert!(
            lower.contains("anvil-managed: anvil-imports"),
            "region spliced into the lowercase file"
        );

        // The manifest tracks the on-disk (lowercase) host, so a second run is
        // a no-op rather than re-proposing or orphaning the region.
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be idempotent");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn first_run_writes_everything_local_only() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert!(outcome.backends.is_empty());
        assert!(outcome.plan.has_changes());

        for expected in [
            "Justfile",
            "justfiles/anvil/mod.just",
            "justfiles/anvil/checks.just",
            "justfiles/anvil/groups.just",
            "justfiles/anvil/tiers.just",
            "justfiles/anvil/tools.just",
            "justfiles/anvil/versions.just",
            "deny.toml",
            "rustfmt.toml",
            ".delta.toml",
            "spellcheck.toml",
            "clippy.toml",
            ".anvil.lock",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after update");
        }

        let root_manifest = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(root_manifest.contains("# >>> anvil-managed: anvil-workspace-lints"));
        let member_manifest = fs::read_to_string(tmp.path().join("crates/alpha/Cargo.toml")).unwrap();
        assert!(member_manifest.contains("# >>> anvil-managed: anvil-lints"));
        assert!(member_manifest.contains("workspace = true"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn second_run_is_idempotent_and_in_sync() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be a no-op");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn dry_run_does_not_write() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: true,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!outcome.applied);
        assert!(outcome.plan.has_changes());
        assert!(!tmp.path().join("justfiles/anvil/tools.just").exists());
        assert!(!tmp.path().join(".anvil.lock").exists());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn run_stamps_tool_and_catalog_checksum_into_lock() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let catalog = Catalog::anvil();
        let _ = run_update(&catalog, &args, tmp.path()).unwrap();

        let saved = Manifest::load(tmp.path()).unwrap();
        assert_eq!(saved.tool.as_deref(), Some("anvil"));
        assert_eq!(saved.tool_version, Some(catalog.cli().version.clone()));
        assert_eq!(saved.catalog_checksum, Some(catalog.checksum()));
    }

    fn local_only() -> Cli {
        Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        }
    }

    fn seed_lock_owner(root: &Path, tool: &str) {
        let m = Manifest {
            tool: Some(tool.to_owned()),
            ..Manifest::default()
        };
        m.save(root).unwrap();
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_allows_matching_tool() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "anvil");
        let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_mismatched_tool_and_writes_nothing() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let err = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("managed by 'forge2'"), "got: {msg}");
        assert!(msg.contains("--force"), "refusal should suggest --force; got: {msg}");
        assert!(!tmp.path().join("justfiles/anvil/tools.just").exists(), "guard must write nothing");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_before_workspace_parsing() {
        // A root that `find_workspace_root` accepts (it declares
        // `[workspace]`) but that `load_workspace` would reject (the explicit
        // member `crates/missing` does not exist). With a lock naming a
        // different tool, the guard must fire first: the refusal — not a
        // workspace-parse error — is what surfaces. This pins the ordering
        // (guard runs immediately after loading the lock, before
        // `load_workspace`).
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/missing\"]\n",
        );
        seed_lock_owner(root, "forge2");

        let err = run_update(&Catalog::anvil(), &local_only(), root).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("managed by 'forge2'"),
            "guard must refuse before workspace parsing; got: {msg}"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn guard_refuses_mismatched_tool_under_dry_run() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let args = Cli {
            dry_run: true,
            ..local_only()
        };
        let err = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap_err();
        assert!(err.to_string().contains("managed by 'forge2'"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn force_switches_ownership_and_rewrites_provenance() {
        let tmp = empty_workspace();
        seed_lock_owner(tmp.path(), "forge2");
        let args = Cli {
            force: true,
            ..local_only()
        };
        let catalog = Catalog::anvil();
        let outcome = run_update(&catalog, &args, tmp.path()).unwrap();
        assert!(outcome.applied, "force should proceed as a normal update");
        assert!(tmp.path().join("justfiles/anvil/tools.just").is_file());
        let saved = Manifest::load(tmp.path()).unwrap();
        assert_eq!(saved.tool.as_deref(), Some("anvil"), "force rewrites the lock owner");
        assert_eq!(saved.catalog_checksum, Some(catalog.checksum()));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn legacy_lock_without_tool_is_not_blocked() {
        let tmp = empty_workspace();
        // A pre-split lock: rendered_by present, no `tool` field.
        std::fs::write(tmp.path().join(".anvil.lock"), "version = 1\nrendered_by = \"cargo-anvil 0.0.1\"\n").unwrap();
        let outcome = run_update(&Catalog::anvil(), &local_only(), tmp.path()).unwrap();
        assert!(outcome.applied, "a legacy lock with no tool must not trigger the guard");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn opted_out_region_is_skipped_on_second_run() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated = crate::region::upsert_region(&host, region::RUSTFMT_REGION_ID, "", crate::region::CommentSyntax::Hash).unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .expect("rustfmt region item missing from plan");
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn user_edit_inside_region_left_alone_when_template_unchanged() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated = crate::region::upsert_region(
            &host,
            region::RUSTFMT_REGION_ID,
            "edition = \"2021\"\n",
            crate::region::CommentSyntax::Hash,
        )
        .unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("edition = \"2021\""));
    }

    /// Verifies B6: after a Propose decision, the next run sees the
    /// divergence as `LeaveAlone` (no re-proposal) until the template
    /// itself moves again.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn propose_burns_through_after_one_run() {
        use crate::checksum::checksum_str;
        use crate::manifest::{Manifest, RegionKey};

        let tmp = empty_workspace();
        let args = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };

        // First update: write everything.
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();

        // User edits the rustfmt region.
        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let edited = crate::region::upsert_region(
            &host,
            region::RUSTFMT_REGION_ID,
            "edition = \"2021\"\n",
            crate::region::CommentSyntax::Hash,
        )
        .unwrap();
        fs::write(&path, edited).unwrap();

        // Simulate the template moving on by hand-editing the manifest's
        // recorded checksum for the region to a value other than what
        // the user has and other than the current template. That way
        // the next run sees D ≠ L ≠ T → Propose.
        let manifest_path = Manifest::path_for(tmp.path());
        let mut manifest = Manifest::load(tmp.path()).unwrap();
        let key = RegionKey {
            host: "rustfmt.toml".to_owned(),
            id: region::RUSTFMT_REGION_ID.to_owned(),
        };
        manifest.regions.insert(key, checksum_str("synthetic old template"));
        manifest.save(tmp.path()).unwrap();
        let _ = manifest_path; // sanity

        // Second update: should Propose (user diverged + template moved).
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let item = second
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(item.decision, crate::decision::Decision::Propose);
        assert!(
            tmp.path().join("rustfmt.toml.anvil-proposed").is_file(),
            "expected a proposed sibling after the Propose run"
        );

        // Third update: nothing has changed since the second run; the
        // proposal should have been "burned through" and the next run
        // should see LeaveAlone (D ≠ L, L = T) — not Propose.
        let third = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let item = third
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == region::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(
            item.decision,
            crate::decision::Decision::LeaveAlone,
            "Propose should bump L = T so subsequent runs see LeaveAlone"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn github_backend_writes_full_dotgithub_tree() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::GitHub]);
        for expected in [
            ".github/actions/anvil-setup/action.yml",
            ".github/actions/anvil-impact/action.yml",
            ".github/actions/anvil-pr-fast/action.yml",
            ".github/actions/anvil-pr-test/action.yml",
            ".github/actions/anvil-pr-runtime-analysis/action.yml",
            ".github/actions/anvil-pr-mutants/action.yml",
            ".github/actions/anvil-scheduled-test/action.yml",
            ".github/actions/anvil-scheduled-advisories/action.yml",
            ".github/actions/anvil-scheduled-exhaustive/action.yml",
            ".github/workflows/anvil-pr-impl.yml",
            ".github/workflows/anvil-scheduled-impl.yml",
            ".github/workflows/anvil-pr.yml",
            ".github/workflows/anvil-scheduled.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after github update");
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn github_backend_idempotent() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(
            !second.plan.has_changes(),
            "second github run should be a no-op:\n{}",
            second.plan.summary(Some(&second.previous_manifest))
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn ado_backend_writes_full_pipelines_tree() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["ado".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let outcome = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::Ado]);
        for expected in [
            ".pipelines/anvil/steps/setup.yml",
            ".pipelines/anvil/steps/impact.yml",
            ".pipelines/anvil/steps/advisory-comments.yml",
            ".pipelines/anvil/steps/pr-fast.yml",
            ".pipelines/anvil/steps/pr-test.yml",
            ".pipelines/anvil/steps/pr-runtime-analysis.yml",
            ".pipelines/anvil/steps/pr-mutants.yml",
            ".pipelines/anvil/steps/scheduled-test.yml",
            ".pipelines/anvil/steps/scheduled-advisories.yml",
            ".pipelines/anvil/steps/scheduled-exhaustive.yml",
            ".pipelines/anvil/pr.yml",
            ".pipelines/anvil/scheduled.yml",
            ".pipelines/anvil-pr.yml",
            ".pipelines/anvil-scheduled.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after ado update");
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn both_backends_idempotent() {
        let tmp = empty_workspace();
        let args = Cli {
            backends: vec!["github".to_owned(), "ado".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        let second = run_update(&Catalog::anvil(), &args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes());
    }

    /// Files that were previously rendered but are no longer in scope
    /// (e.g., a backend was disabled) must surface as `Remove` plan items
    /// when the on-disk content still matches what we last wrote. This
    /// exercises the `plan_removals` path end-to-end.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn disabling_a_backend_removes_its_orphaned_files() {
        use crate::decision::Decision;

        let tmp = empty_workspace();

        // First: write everything including the github backend.
        let with_gh = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let first = run_update(&Catalog::anvil(), &with_gh, tmp.path()).unwrap();
        assert!(first.applied);
        let github_workflow = tmp.path().join(".github/workflows/anvil-pr.yml");
        assert!(github_workflow.is_file());

        // Second: disable backends. The previously rendered github files
        // should now be queued for removal (file unchanged on disk since
        // last render → Decision::Remove).
        let no_be = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let second = run_update(&Catalog::anvil(), &no_be, tmp.path()).unwrap();

        let removed: Vec<&str> = second
            .plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::Remove)
            .filter_map(|i| match &i.target {
                crate::plan::Target::File { path } => Some(path.as_str()),
                crate::plan::Target::Region { .. } => None,
            })
            .collect();
        assert!(
            removed.contains(&".github/workflows/anvil-pr.yml"),
            "expected anvil-pr.yml to be queued for removal; got: {removed:?}"
        );
        assert!(
            !github_workflow.exists(),
            "expected the orphaned github workflow file to actually be removed from disk"
        );
    }

    /// User-customized orphans (file no longer in scope, but the on-disk
    /// contents diverge from what we last wrote) must be left alone via
    /// `OrphanedKept`, not deleted.
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn customized_orphans_are_kept_not_removed() {
        use crate::decision::Decision;

        let tmp = empty_workspace();
        let with_gh = Cli {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
            force: false,
        };
        let _ = run_update(&Catalog::anvil(), &with_gh, tmp.path()).unwrap();

        let github_workflow = tmp.path().join(".github/workflows/anvil-pr.yml");
        fs::write(&github_workflow, "# user edited this\n").unwrap();

        let no_be = Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        };
        let second = run_update(&Catalog::anvil(), &no_be, tmp.path()).unwrap();

        let kept: Vec<&str> = second
            .plan
            .items()
            .iter()
            .filter(|i| i.decision == Decision::OrphanedKept)
            .filter_map(|i| match &i.target {
                crate::plan::Target::File { path } => Some(path.as_str()),
                crate::plan::Target::Region { .. } => None,
            })
            .collect();
        assert!(
            kept.contains(&".github/workflows/anvil-pr.yml"),
            "expected customized orphan to surface as OrphanedKept; got: {kept:?}"
        );
        assert!(github_workflow.is_file(), "customized orphan must not be deleted from disk");
        assert_eq!(
            fs::read_to_string(&github_workflow).unwrap(),
            "# user edited this\n",
            "customized orphan contents must be preserved"
        );
    }
}
