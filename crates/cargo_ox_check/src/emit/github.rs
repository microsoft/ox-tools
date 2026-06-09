// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub Actions backend emitter.
//!
//! Emits three layers per [github.md](../../../docs/design/github.md):
//!
//! 1. Composite actions under `.github/actions/ox-check-*/action.yml`.
//! 2. Reusable workflows (`ox-check-pr-impl.yml`, `ox-check-nightly-impl.yml`).
//! 3. Root workflows (`ox-check-pr.yml`, `ox-check-nightly.yml`).
//!
//! All emitted files are owned files (no managed regions). Users who
//! customize take ownership via the standard dirty-file flow.

use std::path::Path;

use anyhow::Result;

use crate::manifest::Manifest;
use crate::plan::PlanItem;

use super::owned_file::plan_owned_file;

/// Embedded body of the shared setup composite action.
pub const SETUP_ACTION: &str =
    include_str!("../../templates/github/setup-action.yml");

/// Embedded body of the cargo-delta impact composite action.
pub const IMPACT_ACTION: &str =
    include_str!("../../templates/github/impact-action.yml");

/// Embedded body of the PR reusable workflow.
pub const PR_IMPL_WORKFLOW: &str =
    include_str!("../../templates/github/pr-impl-workflow.yml");

/// Embedded body of the nightly reusable workflow.
pub const NIGHTLY_IMPL_WORKFLOW: &str =
    include_str!("../../templates/github/nightly-impl-workflow.yml");

/// Embedded body of the PR root workflow.
pub const PR_ROOT_WORKFLOW: &str =
    include_str!("../../templates/github/pr-root-workflow.yml");

/// Embedded body of the nightly root workflow.
pub const NIGHTLY_ROOT_WORKFLOW: &str =
    include_str!("../../templates/github/nightly-root-workflow.yml");

/// All check groups for which the GitHub backend emits a composite action.
///
/// Mirrors [`checks.md`](../../../docs/design/checks.md) §1.
pub const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-mutants",
    "nightly-test",
    "nightly-advisories",
    "nightly-runtime",
    "nightly-exhaustive",
];

/// Render the `action.yml` for one check group's composite action.
///
/// The action takes two inputs (`excludes`, `skip`) supplied by the
/// reusable workflow from the impact job's outputs, sets them as
/// environment variables, and invokes `just ox-check-<group>`.
#[must_use]
pub fn render_group_action(group: &str) -> String {
    format!(
        "# Copyright (c) Microsoft Corporation.\n\
         # Licensed under the MIT License.\n\
         # Owned by cargo-ox-check; edit via `cargo ox-check update`.\n\
         name: ox-check-{group}\n\
         description: Run the {group} check group.\n\
         inputs:\n  \
           excludes:\n    \
             description: Comma-separated package excludes from the impact job.\n    \
             default: \"\"\n    \
             required: false\n  \
           skip:\n    \
             description: If \"true\", skip this group entirely.\n    \
             default: \"false\"\n    \
             required: false\n\
         runs:\n  \
           using: composite\n  \
           steps:\n    \
             - if: inputs.skip != 'true'\n      \
               uses: ./.github/actions/ox-check-setup\n    \
             - if: inputs.skip != 'true'\n      \
               name: Run just ox-check-{group}\n      \
               shell: bash\n      \
               env:\n        \
                 OX_CHECK_EXCLUDES: ${{{{ inputs.excludes }}}}\n      \
               run: just ox-check-{group}\n"
    )
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
pub fn plan_composite_actions(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
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
        items.push(plan_owned_file(
            repo_root,
            manifest,
            &group_action_path(group),
            &body,
        )?);
    }
    Ok(items)
}

/// Plan the two reusable workflows.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_reusable_workflows(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
    Ok(vec![
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-pr-impl.yml",
            PR_IMPL_WORKFLOW,
        )?,
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-nightly-impl.yml",
            NIGHTLY_IMPL_WORKFLOW,
        )?,
    ])
}

/// Plan the two root workflows.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_root_workflows(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
    Ok(vec![
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-pr.yml",
            PR_ROOT_WORKFLOW,
        )?,
        plan_owned_file(
            repo_root,
            manifest,
            ".github/workflows/ox-check-nightly.yml",
            NIGHTLY_ROOT_WORKFLOW,
        )?,
    ])
}

/// Plan every file the GitHub backend emits.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_github_backend(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
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
    fn render_group_action_uses_correct_name() {
        let body = render_group_action("pr-fast");
        assert!(body.contains("name: ox-check-pr-fast"));
        assert!(body.contains("just ox-check-pr-fast"));
        assert!(body.contains("OX_CHECK_EXCLUDES"));
    }

    #[test]
    fn group_actions_skip_when_input_is_true() {
        let body = render_group_action("nightly-test");
        assert!(body.contains("inputs.skip != 'true'"));
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
            assert_eq!(
                indent % 2,
                0,
                "non-aligned indent in:\n{body}\n>>> at line: {line}"
            );
        }
    }

    #[test]
    fn plan_composite_actions_emits_setup_impact_and_seven_groups() {
        let tmp = TempDir::new().unwrap();
        let items = plan_composite_actions(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), GROUPS.len() + 2);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

    #[test]
    fn pr_impl_workflow_has_expected_jobs() {
        assert!(PR_IMPL_WORKFLOW.contains("workflow_call:"));
        for needle in ["impact:", "pr-fast:", "pr-test:", "pr-mutants:"] {
            assert!(
                PR_IMPL_WORKFLOW.contains(needle),
                "PR impl workflow missing job '{needle}'"
            );
        }
        // PR-test fans out across the OS matrix from `test_os`.
        assert!(PR_IMPL_WORKFLOW.contains("test_os"));
        // pr-fast carries the PR title for the ox-check-pr-title check.
        assert!(PR_IMPL_WORKFLOW.contains("PR_TITLE"));
        // pr-mutants needs the base SHA for diff scoping.
        assert!(PR_IMPL_WORKFLOW.contains("BASE_REF"));
    }

    #[test]
    fn nightly_impl_workflow_has_expected_jobs() {
        for needle in [
            "nightly-test:",
            "nightly-advisories:",
            "nightly-runtime:",
            "nightly-exhaustive:",
        ] {
            assert!(
                NIGHTLY_IMPL_WORKFLOW.contains(needle),
                "nightly impl workflow missing job '{needle}'"
            );
        }
        // Nightly uploads the lcov artifact.
        assert!(NIGHTLY_IMPL_WORKFLOW.contains("upload-artifact"));
    }

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
        assert!(NIGHTLY_ROOT_WORKFLOW.contains("uses: ./.github/workflows/ox-check-nightly-impl.yml"));
        assert!(NIGHTLY_ROOT_WORKFLOW.contains("schedule:"));
    }

    #[test]
    fn plan_github_backend_emits_full_file_set() {
        let tmp = TempDir::new().unwrap();
        let items = plan_github_backend(tmp.path(), &Manifest::default()).unwrap();
        // 2 shared actions + 7 group actions + 2 reusable workflows + 2 root workflows
        assert_eq!(items.len(), 2 + GROUPS.len() + 2 + 2);
    }

    #[test]
    fn group_action_path_is_under_dot_github() {
        assert_eq!(
            group_action_path("pr-fast"),
            ".github/actions/ox-check-pr-fast/action.yml"
        );
    }
}
