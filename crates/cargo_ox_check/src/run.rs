// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level `update` driver.
//!
//! Orchestrates: workspace discovery, manifest load, backend resolution,
//! emitter invocation, plan accumulation, and final apply/summarize.

use std::path::Path;

use ohno::AppError;
use tracing::info;

use crate::backend::{self, Backend};
use crate::cli::{Command, UpdateArgs};
use crate::emit::{ado, cargo_toml, github, local, shared_configs};
use crate::manifest::Manifest;
use crate::plan::Plan;
use crate::workspace::{self, Workspace};

/// Outcome of an `update` invocation.
#[derive(Debug)]
pub struct RunOutcome {
    /// The plan that was built.
    pub plan: Plan,
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
            print!("{}", outcome.plan.summary());
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
    let manifest = Manifest::load(&repo_root)?;

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
        applied,
        backends,
    })
}

/// Build the full plan: local files + selected CI backends.
fn build_plan(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
    backends: &[Backend],
) -> Result<Plan, AppError> {
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

    Ok(plan)
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
            "deny.toml",
            "rustfmt.toml",
            ".delta.toml",
            ".ox-check.lock",
        ] {
            assert!(
                tmp.path().join(expected).is_file(),
                "expected '{expected}' after update"
            );
        }

        let root_manifest = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(root_manifest.contains("# >>> ox-check-managed: ox-check-workspace-lints"));
        let member_manifest =
            fs::read_to_string(tmp.path().join("crates/alpha/Cargo.toml")).unwrap();
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
        let updated = crate::region::upsert_region(
            &host,
            shared_configs::RUSTFMT_REGION_ID,
            "",
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
        assert_eq!(
            rustfmt_item.decision,
            crate::decision::Decision::LeaveAlone
        );
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("edition = \"2021\""));
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
            assert!(
                tmp.path().join(expected).is_file(),
                "expected '{expected}' after github update"
            );
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
            second.plan.summary()
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
            assert!(
                tmp.path().join(expected).is_file(),
                "expected '{expected}' after ado update"
            );
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
