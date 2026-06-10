// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub Actions backend emitter.
//!
//! Emits three layers per [`github.md`](../../../docs/design/github.md):
//!
//! 1. Composite actions under `.github/actions/ox-check-*/action.yml`.
//! 2. Reusable workflows (`ox-check-pr-impl.yml`, `ox-check-scheduled-impl.yml`).
//! 3. Root workflows (`ox-check-pr.yml`, `ox-check-scheduled.yml`).
//!
//! All emitted files are owned files (no managed regions). Users who
//! customize take ownership via the standard dirty-file flow.

use std::path::Path;

use ohno::AppError;

use super::owned_file::plan_owned_file;
use crate::manifest::Manifest;
use crate::plan::PlanItem;

/// Embedded body of the shared setup composite action.
pub const SETUP_ACTION: &str = include_str!("../../templates/github/setup-action.yml");

/// Embedded body of the cargo-delta impact composite action.
pub const IMPACT_ACTION: &str = include_str!("../../templates/github/impact-action.yml");

/// Embedded body of the PR reusable workflow.
pub const PR_IMPL_WORKFLOW: &str = include_str!("../../templates/github/pr-impl-workflow.yml");

/// Embedded body of the scheduled reusable workflow.
pub const SCHEDULED_IMPL_WORKFLOW: &str = include_str!("../../templates/github/scheduled-impl-workflow.yml");

/// Embedded body of the PR root workflow.
pub const PR_ROOT_WORKFLOW: &str = include_str!("../../templates/github/pr-root-workflow.yml");

/// Embedded body of the scheduled root workflow.
pub const SCHEDULED_ROOT_WORKFLOW: &str = include_str!("../../templates/github/scheduled-root-workflow.yml");

/// All check groups for which the GitHub backend emits a composite action.
///
/// All ox-check groups that get a per-group composite action.
///
/// Mirrors [`checks.md`](../../../docs/design/checks.md) `§1`.
///
/// The PR-tier "pr-slow" umbrella is split into three CI-visible
/// sub-groups (`pr-test`, `pr-runtime-analysis`, `pr-mutants`) so each runs as
/// its own job and they execute in parallel across the matrix. The
/// umbrella `ox-check-pr-slow` recipe is preserved in `groups.just`
/// for local convenience but does not appear in CI as a discrete
/// job. `pr-mutants` (mutants) self-skips on aarch64-pc-windows-msvc
/// where cargo-mutants doesn't build.
pub const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-runtime-analysis",
    "pr-mutants",
    "scheduled-test",
    "scheduled-advisories",
    "scheduled-exhaustive",
];

/// Embedded template for one per-group composite action. `__GROUP__` is
/// substituted with the group name at emit time.
pub const GROUP_ACTION_TEMPLATE: &str = include_str!("../../templates/github/group-action.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Render the `action.yml` for one check group's composite action.
///
/// Substitutes the group name into [`GROUP_ACTION_TEMPLATE`]. The action
/// takes two inputs (`excludes`, `skip`) supplied by the reusable
/// workflow from the impact job's outputs, sets them as environment
/// variables, and invokes `just ox-check-<group>`.
#[must_use]
pub fn render_group_action(group: &str) -> String {
    GROUP_ACTION_TEMPLATE.replace(GROUP_PLACEHOLDER, group)
}

/// Repo-root-relative path for a per-group composite action.
#[must_use]
pub fn group_action_path(group: &str) -> String {
    format!(".github/actions/ox-check-{group}/action.yml")
}

/// Plan every composite-action file: setup, impact, and the seven per-group actions.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_composite_actions(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::with_capacity(GROUPS.len() + 2);
    items.push(plan_owned_file(
        repo_root,
        manifest,
        ".github/actions/ox-check-setup/action.yml",
        SETUP_ACTION,
    )?);
    items.push(plan_owned_file(
        repo_root,
        manifest,
        ".github/actions/ox-check-impact/action.yml",
        IMPACT_ACTION,
    )?);
    for group in GROUPS {
        let body = render_group_action(group);
        items.push(plan_owned_file(repo_root, manifest, &group_action_path(group), &body)?);
    }
    Ok(items)
}

/// Plan the two reusable workflows.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_reusable_workflows(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_owned_file(repo_root, manifest, ".github/workflows/ox-check-pr-impl.yml", PR_IMPL_WORKFLOW)?,
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-scheduled-impl.yml",
            SCHEDULED_IMPL_WORKFLOW,
        )?,
    ])
}

/// Plan the two root workflows.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_root_workflows(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_owned_file(repo_root, manifest, ".github/workflows/ox-check-pr.yml", PR_ROOT_WORKFLOW)?,
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-scheduled.yml",
            SCHEDULED_ROOT_WORKFLOW,
        )?,
    ])
}

/// Plan every file the GitHub backend emits.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_github_backend(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::new();
    items.extend(plan_composite_actions(repo_root, manifest)?);
    items.extend(plan_reusable_workflows(repo_root, manifest)?);
    items.extend(plan_root_workflows(repo_root, manifest)?);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;

    #[test]
    fn setup_and_impact_templates_are_non_empty() {
        assert!(SETUP_ACTION.contains("name: ox-check-setup"));
        assert!(IMPACT_ACTION.contains("name: ox-check-impact"));
        assert!(IMPACT_ACTION.contains("cargo-delta"));
    }

    #[test]
    fn setup_action_takes_group_input_and_dispatches() {
        // The group input drives whether we install the full catalog,
        // skip tool install entirely (group=none, used by impact), or
        // scope install to one group.
        assert!(SETUP_ACTION.contains("group:"));
        assert!(SETUP_ACTION.contains("just ox-check-setup binstall"));
        assert!(SETUP_ACTION.contains("just \"ox-check-${{ inputs.group }}-setup\" binstall"));
        assert!(SETUP_ACTION.contains("none)"));
    }

    #[test]
    fn group_action_passes_group_to_setup() {
        let body = render_group_action("pr-fast");
        // The per-group composite invokes ox-check-setup with its own
        // group name so only that group's prerequisites get installed.
        assert!(body.contains("uses: ./.github/actions/ox-check-setup"));
        assert!(body.contains("group: pr-fast"));
    }

    #[test]
    fn impact_action_uses_group_none_and_installs_only_cargo_delta() {
        assert!(IMPACT_ACTION.contains("group: none"));
        assert!(IMPACT_ACTION.contains("ox-check-tool-cargo-delta-install"));
    }

    #[test]
    fn render_group_action_uses_correct_name() {
        let body = render_group_action("pr-fast");
        assert!(body.contains("name: ox-check-pr-fast"));
        assert!(body.contains("just ox-check-pr-fast"));
        assert!(body.contains("OX_CHECK_INCLUDE_MODIFIED"));
        assert!(body.contains("OX_CHECK_INCLUDE_AFFECTED"));
        assert!(body.contains("OX_CHECK_INCLUDE_REQUIRED"));
    }

    #[test]
    fn group_actions_declare_include_inputs() {
        let body = render_group_action("scheduled-test");
        assert!(body.contains("include_modified:"));
        assert!(body.contains("include_affected:"));
        assert!(body.contains("include_required:"));
    }

    #[test]
    fn rendered_action_is_valid_yaml() {
        // Use serde_yaml? Not in workspace. Use a string-based sanity check:
        // every line is either empty, a comment, or 2-space indented.
        let body = render_group_action("pr-test");
        for line in body.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let trimmed_indent = line.trim_start_matches(' ').len();
            let indent = line.len() - trimmed_indent;
            assert_eq!(indent % 2, 0, "non-aligned indent in:\n{body}\n>>> at line: {line}");
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_composite_actions_emits_setup_impact_and_all_groups() {
        let tmp = TempDir::new().unwrap();
        let items = plan_composite_actions(tmp.path(), &Manifest::default()).unwrap();
        // GROUPS.len() per-group composites + 2 shared composites (setup, impact)
        assert_eq!(items.len(), GROUPS.len() + 2);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

    #[test]
    fn pr_impl_workflow_has_expected_jobs() {
        assert!(PR_IMPL_WORKFLOW.contains("workflow_call:"));
        // pr-slow is split into three CI-visible jobs (pr-test,
        // pr-runtime-analysis, pr-mutants) that run in parallel. The
        // umbrella `ox-check-pr-slow` recipe exists in groups.just for
        // local convenience but does NOT appear as a CI job here.
        for needle in [
            "impact-linux:",
            "impact-windows:",
            "pr-fast:",
            "pr-test:",
            "pr-runtime-analysis:",
            "pr-mutants:",
        ] {
            assert!(PR_IMPL_WORKFLOW.contains(needle), "PR impl workflow missing job '{needle}'");
        }
        // Stale historical names must not reappear.
        for needle in ["\n  pr-slow:\n", "\n  pr-slow1:\n", "\n  pr-slow2:\n", "\n  pr-slow3:\n"] {
            assert!(
                !PR_IMPL_WORKFLOW.contains(needle),
                "Stale job '{needle}' should be gone after the pr-slow rename"
            );
        }
        // Downstream groups must fan in BOTH per-OS impact jobs.
        assert!(PR_IMPL_WORKFLOW.contains("needs: [impact-linux, impact-windows]"));
        // All three pr-slow* jobs run on the 4-leg matrix.
        assert!(PR_IMPL_WORKFLOW.contains("os: [linux, windows, linux-arm, windows-arm]"));
        assert!(!PR_IMPL_WORKFLOW.contains("fromJSON"));
        // pr-fast carries the PR title for the ox-check-pr-title check.
        assert!(PR_IMPL_WORKFLOW.contains("PR_TITLE"));
        // pr-mutants needs the base SHA for diff-scoped mutants.
        assert!(PR_IMPL_WORKFLOW.contains("BASE_REF"));
        // Coverage upload lives in pr-test (after llvm-cov runs). It's
        // a single YAML step that runs on every leg except windows-arm
        // (skipped because of LLVM-coverage instrumentation bugs).
        // Therefore the codecov-action reference appears once in the
        // workflow YAML; the per-leg behaviour is the `if:` condition.
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("codecov/codecov-action").count(),
            1,
            "Codecov upload step should be declared exactly once (gated per-leg via `if:`)"
        );
        // The gating condition must exclude windows-arm and reference
        // both per-OS impact outputs.
        assert!(PR_IMPL_WORKFLOW.contains("matrix.os != 'windows-arm'"));
        assert!(PR_IMPL_WORKFLOW.contains("flags: ${{ matrix.os }}"));
    }

    #[test]
    fn scheduled_impl_workflow_has_expected_jobs() {
        for needle in ["scheduled-test:", "scheduled-advisories:", "scheduled-exhaustive:"] {
            assert!(
                SCHEDULED_IMPL_WORKFLOW.contains(needle),
                "scheduled impl workflow missing job '{needle}'"
            );
        }
        // Nightly uploads the lcov artifact.
        // Nightly uploads the lcov to Codecov.
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("codecov/codecov-action"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_reusable_workflows_emits_two() {
        let tmp = TempDir::new().unwrap();
        let items = plan_reusable_workflows(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

    #[test]
    fn root_workflows_call_reusable_workflows() {
        assert!(PR_ROOT_WORKFLOW.contains("uses: ./.github/workflows/ox-check-pr-impl.yml"));
        assert!(PR_ROOT_WORKFLOW.contains("pull_request:"));
        assert!(PR_ROOT_WORKFLOW.contains("merge_group:"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("uses: ./.github/workflows/ox-check-scheduled-impl.yml"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("schedule:"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_github_backend_emits_full_file_set() {
        let tmp = TempDir::new().unwrap();
        let items = plan_github_backend(tmp.path(), &Manifest::default()).unwrap();
        // 2 shared actions + 6 group actions + 2 reusable workflows + 2 root workflows
        assert_eq!(items.len(), 2 + GROUPS.len() + 2 + 2);
    }

    #[test]
    fn group_action_path_is_under_dot_github() {
        assert_eq!(group_action_path("pr-fast"), ".github/actions/ox-check-pr-fast/action.yml");
    }
}
