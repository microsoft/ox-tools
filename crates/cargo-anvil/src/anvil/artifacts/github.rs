// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub Actions backend files: composite actions, reusable workflows, and
//! root workflows, each an owned file gated on [`Backend::GitHub`].
//!
//! Holds the embedded templates and registry functions.
//!
//! See [`github.md`](../../../docs/design/github.md).

use crate::backend::Backend;
use crate::catalog::Artifact;

/// Embedded body of the shared setup composite action.
const SETUP_ACTION: &str = include_str!("../../../templates/github/setup-action.yml");

/// GitHub problem matcher that promotes Just recipe failures to annotations.
const JUST_PROBLEM_MATCHER: &str = include_str!("../../../templates/github/just-problem-matcher.json");

/// Shared composite action that runs any Anvil group.
const RUN_GROUP_ACTION: &str = include_str!("../../../templates/github/run-group-action.yml");

/// Shared composite action that publishes a stable commit status.
const REPORT_STATUS_ACTION: &str = include_str!("../../../templates/github/report-status-action.yml");

/// Embedded body of the cargo-delta impact composite action.
const IMPACT_ACTION: &str = include_str!("../../../templates/github/impact-action.yml");

/// Embedded body of the PR reusable workflow.
const PR_IMPL_WORKFLOW: &str = include_str!("../../../templates/github/pr-impl-workflow.yml");

/// Embedded body of the scheduled reusable workflow.
const SCHEDULED_IMPL_WORKFLOW: &str = include_str!("../../../templates/github/scheduled-impl-workflow.yml");

/// Embedded body of the PR root workflow.
const PR_ROOT_WORKFLOW: &str = include_str!("../../../templates/github/pr-root-workflow.yml");

/// Embedded body of the scheduled root workflow.
const SCHEDULED_ROOT_WORKFLOW: &str = include_str!("../../../templates/github/scheduled-root-workflow.yml");

/// `.github/actions/anvil-setup/action.yml`.
#[must_use]
pub fn setup_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-setup/action.yml", SETUP_ACTION)
}

/// `.github/actions/anvil-setup/just-problem-matcher.json`.
#[must_use]
pub fn just_problem_matcher() -> Artifact {
    Artifact::backend_file(
        Backend::GitHub,
        ".github/actions/anvil-setup/just-problem-matcher.json",
        JUST_PROBLEM_MATCHER,
    )
}

/// `.github/actions/anvil-run-group/action.yml`.
#[must_use]
pub fn run_group_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-run-group/action.yml", RUN_GROUP_ACTION)
}

/// `.github/actions/anvil-report-status/action.yml`.
#[must_use]
pub fn report_status_action() -> Artifact {
    Artifact::backend_file(
        Backend::GitHub,
        ".github/actions/anvil-report-status/action.yml",
        REPORT_STATUS_ACTION,
    )
}

/// `.github/actions/anvil-impact/action.yml`.
#[must_use]
pub fn impact_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-impact/action.yml", IMPACT_ACTION)
}

/// `.github/workflows/anvil-pr-impl.yml` — the PR reusable workflow.
#[must_use]
pub fn pr_impl_workflow() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-pr-impl.yml", PR_IMPL_WORKFLOW)
}

/// `.github/workflows/anvil-scheduled-impl.yml` — the scheduled reusable workflow.
#[must_use]
pub fn scheduled_impl_workflow() -> Artifact {
    Artifact::backend_file(
        Backend::GitHub,
        ".github/workflows/anvil-scheduled-impl.yml",
        SCHEDULED_IMPL_WORKFLOW,
    )
}

/// `.github/workflows/anvil-pr.yml` — the PR root workflow.
#[must_use]
pub fn pr_root_workflow() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-pr.yml", PR_ROOT_WORKFLOW)
}

/// `.github/workflows/anvil-scheduled.yml` — the scheduled root workflow.
#[must_use]
pub fn scheduled_root_workflow() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-scheduled.yml", SCHEDULED_ROOT_WORKFLOW)
}

/// All GitHub backend artifacts in emission order.
#[must_use]
pub(crate) fn all() -> Vec<Artifact> {
    vec![
        setup_action(),
        just_problem_matcher(),
        run_group_action(),
        report_status_action(),
        impact_action(),
        pr_impl_workflow(),
        scheduled_impl_workflow(),
        pr_root_workflow(),
        scheduled_root_workflow(),
    ]
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn shared_action_templates_are_non_empty() {
        assert!(SETUP_ACTION.contains("name: anvil-setup"));
        assert!(JUST_PROBLEM_MATCHER.contains("\"owner\": \"anvil-just\""));
        assert!(RUN_GROUP_ACTION.contains("name: anvil-run-group"));
        assert!(REPORT_STATUS_ACTION.contains("name: anvil-report-status"));
        assert!(IMPACT_ACTION.contains("name: anvil-impact"));
        assert!(IMPACT_ACTION.contains("cargo-delta"));
    }

    #[test]
    fn setup_registers_just_problem_matcher() {
        assert!(SETUP_ACTION.contains("::add-matcher::$GITHUB_ACTION_PATH/just-problem-matcher.json"));
        assert!(JUST_PROBLEM_MATCHER.contains("error: recipe `[^`]+` failed with exit code"));
    }

    #[test]
    fn setup_action_takes_group_input_and_dispatches() {
        assert!(SETUP_ACTION.contains("group:"));
        assert!(SETUP_ACTION.contains("just anvil-setup binstall"));
        assert!(SETUP_ACTION.contains("ANVIL_GROUP: ${{ inputs.group }}"));
        assert!(SETUP_ACTION.contains("just \"anvil-$ANVIL_GROUP-setup\" binstall"));
        assert!(SETUP_ACTION.contains(r"^[a-z0-9-]+$"));
        assert!(SETUP_ACTION.contains("none)"));
    }

    #[test]
    fn setup_action_can_reclaim_github_hosted_runner_disk_space() {
        assert!(SETUP_ACTION.contains("free-disk-space:"));
        assert!(SETUP_ACTION.contains("runner.environment == 'github-hosted'"));
        assert!(SETUP_ACTION.contains("/usr/local/lib/android"));
        assert!(SETUP_ACTION.contains(r"C:\Program Files (x86)\Android"));
        assert!(!SETUP_ACTION.contains("Install libclang"));
    }

    #[test]
    fn run_group_action_captures_and_reports_results() {
        assert!(RUN_GROUP_ACTION.contains("uses: ./.github/actions/anvil-setup"));
        assert!(RUN_GROUP_ACTION.contains("group: ${{ inputs.group }}"));
        assert!(RUN_GROUP_ACTION.contains("free-disk-space: ${{ inputs.free-disk-space }}"));
        assert!(RUN_GROUP_ACTION.contains("status=${PIPESTATUS[0]}"));
        assert!(RUN_GROUP_ACTION.contains("Failed Just recipe: ${{ steps.run.outputs.failed_recipe }}"));
        assert!(RUN_GROUP_ACTION.contains("uses: ./.github/actions/anvil-report-status"));
        assert!(RUN_GROUP_ACTION.contains("ANVIL_INCLUDE_MODIFIED"));
        assert!(RUN_GROUP_ACTION.contains("ANVIL_INCLUDE_AFFECTED"));
        assert!(RUN_GROUP_ACTION.contains("ANVIL_INCLUDE_REQUIRED"));
    }

    #[test]
    fn impact_action_uses_group_none_and_installs_only_cargo_delta() {
        assert!(IMPACT_ACTION.contains("group: none"));
        assert!(IMPACT_ACTION.contains("anvil-tool-cargo-delta-install"));
        assert!(IMPACT_ACTION.contains("delta_config=\"$(pwd)/.delta.toml\""));
        assert!(!IMPACT_ACTION.contains("remote_branch ="));
        assert_eq!(
            IMPACT_ACTION.matches("--config \"$delta_config\"").count(),
            3,
            "current snapshot, baseline snapshot, and impact must share the repository config"
        );
        assert!(IMPACT_ACTION.contains("cargo delta --config \"$delta_config\" snapshot"));
        assert!(IMPACT_ACTION.contains("cargo delta --config \"$delta_config\" impact"));
    }

    #[test]
    fn status_action_uses_stable_context_and_pr_head() {
        assert!(REPORT_STATUS_ACTION.contains("github.rest.repos.createCommitStatus"));
        assert!(REPORT_STATUS_ACTION.contains("context.payload.pull_request.head.sha"));
        assert!(REPORT_STATUS_ACTION.contains("anvil-pr / ${group} details (${runner})"));
        assert!(REPORT_STATUS_ACTION.contains("failedRecipe.replace(/^anvil-/, \"\")"));
        assert!(REPORT_STATUS_ACTION.contains("description: description.slice(0, 140)"));
    }

    #[test]
    fn pr_impl_workflow_has_expected_jobs() {
        assert!(PR_IMPL_WORKFLOW.contains("workflow_call:"));
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
        for needle in ["\n  pr-slow:\n", "\n  pr-slow1:\n", "\n  pr-slow2:\n", "\n  pr-slow3:\n"] {
            assert!(
                !PR_IMPL_WORKFLOW.contains(needle),
                "Stale job '{needle}' should be gone after the pr-slow rename"
            );
        }
        assert!(PR_IMPL_WORKFLOW.contains("needs: [impact-linux, impact-windows]"));
        assert!(PR_IMPL_WORKFLOW.contains("os: [linux, windows, linux-arm, windows-arm]"));
        assert!(!PR_IMPL_WORKFLOW.contains("fromJSON"));
        assert!(PR_IMPL_WORKFLOW.contains("PR_TITLE"));
        assert!(PR_IMPL_WORKFLOW.contains("BASE_REF"));
        assert!(PR_IMPL_WORKFLOW.contains("publish_job_statuses:"));
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("uses: ./.github/actions/anvil-run-group").count(),
            4,
            "every PR group job must use the shared group action"
        );
        assert_eq!(
            PR_IMPL_WORKFLOW
                .matches("publish_job_statuses: ${{ inputs.publish_job_statuses }}")
                .count(),
            4,
            "every PR group job must receive the status opt-in"
        );
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("codecov/codecov-action").count(),
            1,
            "Codecov upload step should be declared exactly once (gated per-leg via `if:`)"
        );
        assert!(PR_IMPL_WORKFLOW.contains("matrix.os != 'windows-arm'"));
        assert!(PR_IMPL_WORKFLOW.contains("flags: ${{ matrix.os }}"));
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("free-disk-space: true").count(),
            1,
            "disk cleanup should be enabled for the PR test group"
        );
    }

    #[test]
    fn scheduled_impl_workflow_has_expected_jobs() {
        for needle in [
            "scheduled-test:",
            "scheduled-advisories:",
            "scheduled-runtime-analysis:",
            "scheduled-exhaustive:",
        ] {
            assert!(
                SCHEDULED_IMPL_WORKFLOW.contains(needle),
                "scheduled impl workflow missing job '{needle}'"
            );
        }
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("codecov/codecov-action"));
        assert_eq!(
            SCHEDULED_IMPL_WORKFLOW.matches("uses: ./.github/actions/anvil-run-group").count(),
            4,
            "every scheduled group job must use the shared group action"
        );
        assert_eq!(
            SCHEDULED_IMPL_WORKFLOW.matches("free-disk-space: true").count(),
            1,
            "disk cleanup should be enabled for the scheduled test group"
        );
    }

    #[test]
    fn root_workflows_call_reusable_workflows() {
        assert!(PR_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-pr-impl.yml"));
        assert!(PR_ROOT_WORKFLOW.contains("pull_request:"));
        assert!(PR_ROOT_WORKFLOW.contains("merge_group:"));
        assert!(PR_ROOT_WORKFLOW.contains("statuses: write"));
        assert!(PR_ROOT_WORKFLOW.contains("publish_job_statuses: true"));
        assert!(PR_ROOT_WORKFLOW.contains("if: github.event_name == 'pull_request'"));
        assert!(PR_ROOT_WORKFLOW.contains("if: github.event_name == 'merge_group'"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-scheduled-impl.yml"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("schedule:"));
    }
}
