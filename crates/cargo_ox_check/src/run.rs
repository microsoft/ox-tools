// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level `update` driver.
//!
//! Orchestrates: workspace discovery, manifest load, backend resolution,
//! emitter invocation, plan accumulation, and final apply/summarize.

use std::collections::BTreeSet;
use std::path::Path;

use ohno::{AppError, IntoAppError as _};
use tracing::info;

use crate::backend::{self, Backend};
use crate::checksum::checksum_str;
use crate::cli::{Command, UpdateArgs};
use crate::decision::{Decision, decide_removal};
use crate::emit::{ado, cargo_toml, github, local, shared_configs};
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

/// Run the parsed CLI command.
///
/// # Errors
///
/// Returns an error when the underlying subcommand fails.
pub fn run(command: Command) -> Result<(), AppError> {
    match command {
        Command::Update(args) => {
            let outcome = run_update(&args, &std::env::current_dir()?)?;
            print!("{}", outcome.plan.summary(Some(&outcome.previous_manifest)));
            if args.dry_run && outcome.plan.has_changes() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// Run `update` against the workspace containing `start_dir`.
///
/// Exposed for integration tests that want to drive the algorithm
/// without `std::process::exit`.
///
/// # Errors
///
/// Propagates errors from any subsystem (workspace discovery, manifest
/// I/O, emitter, plan application).
pub fn run_update(args: &UpdateArgs, start_dir: &Path) -> Result<RunOutcome, AppError> {
    let repo_root = workspace::find_workspace_root(start_dir)?;
    let ws = workspace::load_workspace(&repo_root)?;
    let mut manifest = Manifest::load(&repo_root)?;

    // One-time legacy migration: an earlier version of cargo-ox-check
    // emitted the Justfile imports region under a lowercase `justfile`
    // host path. The canonical capitalization is `Justfile` (matching
    // Makefile / Dockerfile / Rakefile convention and the surveyed
    // Microsoft Rust repos). For repos whose manifest still carries
    // the lowercase entry, transfer it to the canonical case so the
    // orphan-detection pass doesn't spuriously try to splice the
    // region back out.
    local::migrate_legacy_justfile_case(&mut manifest);

    let backends = backend::resolve(&args.backends, args.no_backends, &repo_root)?;
    info!(
        repo_root = %repo_root.display(),
        backends = ?backends.iter().map(|b| b.name()).collect::<Vec<_>>(),
        dry_run = args.dry_run,
        "ox-check update"
    );

    let plan = build_plan(&repo_root, &ws, &manifest, &backends)?;

    let applied = if args.dry_run {
        false
    } else {
        let next = plan.apply(&repo_root, &manifest)?;
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

/// Build the full plan: local files + selected CI backends.
fn build_plan(repo_root: &Path, workspace: &Workspace, manifest: &Manifest, backends: &[Backend]) -> Result<Plan, AppError> {
    let mut plan = Plan::default();

    for item in local::plan_local_just_tree(repo_root, manifest)? {
        plan.push(item);
    }
    plan.push(local::plan_justfile_imports(repo_root, manifest)?);

    for item in cargo_toml::plan_cargo_lints(repo_root, workspace, manifest)? {
        plan.push(item);
    }
    for item in shared_configs::plan_shared_configs(repo_root, manifest)? {
        plan.push(item);
    }

    for backend in backends {
        match backend {
            Backend::GitHub => {
                for item in github::plan_github_backend(repo_root, manifest)? {
                    plan.push(item);
                }
            }
            Backend::Ado => {
                for item in ado::plan_ado_backend(repo_root, manifest)? {
                    plan.push(item);
                }
            }
        }
    }

    plan_removals(repo_root, manifest, &mut plan)?;

    Ok(plan)
}

/// Scan the previous manifest for entries that the active plan items
/// don't cover. For each, classify as Remove (user untouched since the
/// last render) or `OrphanedKept` (user customized — preserve and
/// transfer ownership).
///
/// This is what removes orphaned CI artifacts, dropped catalog entries,
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

fn read_file_if_present(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err::<Option<String>, _>(e).into_app_err_with(|| format!("failed to read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

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

    #[test]
    fn first_run_writes_everything_local_only() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        };
        let outcome = run_update(&args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert!(outcome.backends.is_empty());
        assert!(outcome.plan.has_changes());

        for expected in [
            "Justfile",
            "justfiles/ox-check/mod.just",
            "justfiles/ox-check/checks.just",
            "justfiles/ox-check/groups.just",
            "justfiles/ox-check/tiers.just",
            "justfiles/ox-check/tools.just",
            "justfiles/ox-check/tool-minimums.txt",
            "deny.toml",
            "rustfmt.toml",
            ".delta.toml",
            ".ox-check.lock",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after update");
        }

        let root_manifest = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(root_manifest.contains("# >>> ox-check-managed: ox-check-workspace-lints"));
        let member_manifest = fs::read_to_string(tmp.path().join("crates/alpha/Cargo.toml")).unwrap();
        assert!(member_manifest.contains("# >>> ox-check-managed: ox-check-lints"));
        assert!(member_manifest.contains("workspace = true"));
    }

    #[test]
    fn second_run_is_idempotent_and_in_sync() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        };
        let _ = run_update(&args, tmp.path()).unwrap();
        let second = run_update(&args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes(), "second run should be a no-op");
    }

    #[test]
    fn dry_run_does_not_write() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: true,
        };
        let outcome = run_update(&args, tmp.path()).unwrap();
        assert!(!outcome.applied);
        assert!(outcome.plan.has_changes());
        assert!(!tmp.path().join("justfiles/ox-check/tools.just").exists());
        assert!(!tmp.path().join(".ox-check.lock").exists());
    }

    #[test]
    fn opted_out_region_is_skipped_on_second_run() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        };
        let _ = run_update(&args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated =
            crate::region::upsert_region(&host, shared_configs::RUSTFMT_REGION_ID, "", crate::region::CommentSyntax::Hash).unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == shared_configs::RUSTFMT_REGION_ID)
            })
            .expect("rustfmt region item missing from plan");
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
    }

    #[test]
    fn user_edit_inside_region_left_alone_when_template_unchanged() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        };
        let _ = run_update(&args, tmp.path()).unwrap();

        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let updated = crate::region::upsert_region(
            &host,
            shared_configs::RUSTFMT_REGION_ID,
            "edition = \"2021\"\n",
            crate::region::CommentSyntax::Hash,
        )
        .unwrap();
        fs::write(&path, updated).unwrap();

        let outcome = run_update(&args, tmp.path()).unwrap();
        let rustfmt_item = outcome
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == shared_configs::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(rustfmt_item.decision, crate::decision::Decision::LeaveAlone);
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("edition = \"2021\""));
    }

    /// Verifies B6: after a Propose decision, the next run sees the
    /// divergence as `LeaveAlone` (no re-proposal) until the template
    /// itself moves again.
    #[test]
    fn propose_burns_through_after_one_run() {
        use crate::checksum::checksum_str;
        use crate::manifest::{Manifest, RegionKey};

        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        };

        // First update: write everything.
        let _ = run_update(&args, tmp.path()).unwrap();

        // User edits the rustfmt region.
        let path = tmp.path().join("rustfmt.toml");
        let host = fs::read_to_string(&path).unwrap();
        let edited = crate::region::upsert_region(
            &host,
            shared_configs::RUSTFMT_REGION_ID,
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
            id: shared_configs::RUSTFMT_REGION_ID.to_owned(),
        };
        manifest.regions.insert(key, checksum_str("synthetic old template"));
        manifest.save(tmp.path()).unwrap();
        let _ = manifest_path; // sanity

        // Second update: should Propose (user diverged + template moved).
        let second = run_update(&args, tmp.path()).unwrap();
        let item = second
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == shared_configs::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(item.decision, crate::decision::Decision::Propose);
        assert!(
            tmp.path().join("rustfmt.toml.ox-check-proposed").is_file(),
            "expected a proposed sibling after the Propose run"
        );

        // Third update: nothing has changed since the second run; the
        // proposal should have been "burned through" and the next run
        // should see LeaveAlone (D ≠ L, L = T) — not Propose.
        let third = run_update(&args, tmp.path()).unwrap();
        let item = third
            .plan
            .items()
            .iter()
            .find(|i| {
                matches!(&i.target, crate::plan::Target::Region { host, id }
                    if host == "rustfmt.toml" && id == shared_configs::RUSTFMT_REGION_ID)
            })
            .unwrap();
        assert_eq!(
            item.decision,
            crate::decision::Decision::LeaveAlone,
            "Propose should bump L = T so subsequent runs see LeaveAlone"
        );
    }

    #[test]
    fn github_backend_writes_full_dotgithub_tree() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
        };
        let outcome = run_update(&args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::GitHub]);
        for expected in [
            ".github/actions/ox-check-setup/action.yml",
            ".github/actions/ox-check-impact/action.yml",
            ".github/actions/ox-check-pr-fast/action.yml",
            ".github/actions/ox-check-pr-test/action.yml",
            ".github/actions/ox-check-pr-mutants/action.yml",
            ".github/actions/ox-check-nightly-test/action.yml",
            ".github/actions/ox-check-nightly-advisories/action.yml",
            ".github/actions/ox-check-nightly-runtime/action.yml",
            ".github/actions/ox-check-nightly-exhaustive/action.yml",
            ".github/workflows/ox-check-pr-impl.yml",
            ".github/workflows/ox-check-nightly-impl.yml",
            ".github/workflows/ox-check-pr.yml",
            ".github/workflows/ox-check-nightly.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after github update");
        }
    }

    #[test]
    fn github_backend_idempotent() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
        };
        let _ = run_update(&args, tmp.path()).unwrap();
        let second = run_update(&args, tmp.path()).unwrap();
        assert!(
            !second.plan.has_changes(),
            "second github run should be a no-op:\n{}",
            second.plan.summary(Some(&second.previous_manifest))
        );
    }

    #[test]
    fn ado_backend_writes_full_pipelines_tree() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec!["ado".to_owned()],
            no_backends: false,
            dry_run: false,
        };
        let outcome = run_update(&args, tmp.path()).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.backends, vec![Backend::Ado]);
        for expected in [
            ".pipelines/ox-check/steps/setup.yml",
            ".pipelines/ox-check/steps/impact.yml",
            ".pipelines/ox-check/steps/pr-fast.yml",
            ".pipelines/ox-check/steps/pr-test.yml",
            ".pipelines/ox-check/steps/pr-mutants.yml",
            ".pipelines/ox-check/steps/nightly-test.yml",
            ".pipelines/ox-check/steps/nightly-advisories.yml",
            ".pipelines/ox-check/steps/nightly-runtime.yml",
            ".pipelines/ox-check/steps/nightly-exhaustive.yml",
            ".pipelines/ox-check/pr.yml",
            ".pipelines/ox-check/nightly.yml",
            ".pipelines/ox-check-pr.yml",
            ".pipelines/ox-check-nightly.yml",
        ] {
            assert!(tmp.path().join(expected).is_file(), "expected '{expected}' after ado update");
        }
    }

    #[test]
    fn both_backends_idempotent() {
        let tmp = empty_workspace();
        let args = UpdateArgs {
            backends: vec!["github".to_owned(), "ado".to_owned()],
            no_backends: false,
            dry_run: false,
        };
        let _ = run_update(&args, tmp.path()).unwrap();
        let second = run_update(&args, tmp.path()).unwrap();
        assert!(!second.plan.has_changes());
    }
}
