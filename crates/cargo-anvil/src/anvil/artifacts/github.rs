// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GitHub Actions backend files: composite actions, reusable workflows, and
//! root workflows, each an owned file gated on [`Backend::GitHub`].
//!
//! Holds the embedded templates, the per-group fan-out (`__GROUP__`
//! substitution expanded to concrete files), and the registry functions.
//!
//! See [`github.md`](../../../docs/design/github.md).

use crate::backend::Backend;
use crate::catalog::Artifact;

/// Embedded body of the shared setup composite action.
const SETUP_ACTION: &str = include_str!("../../../templates/github/setup-action.yml");

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

/// All check groups that get a per-group composite action.
///
/// The PR-tier "pr-slow" umbrella is split into three cloud-workflow-visible
/// sub-groups (`pr-test`, `pr-runtime-analysis`, `pr-mutants`) so each runs
/// as its own job and they execute in parallel across the matrix.
#[cfg(test)]
const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-runtime-analysis",
    "pr-mutants",
    "scheduled-test",
    "scheduled-advisories",
    "scheduled-runtime-analysis",
    "scheduled-exhaustive",
];

/// Embedded template for one per-group composite action. `__GROUP__` is
/// substituted with the group name at emit time.
const GROUP_ACTION_TEMPLATE: &str = include_str!("../../../templates/github/group-action.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Placeholder token the per-group template uses for the impact-mode selection.
const IMPACT_MODE_PLACEHOLDER: &str = "__IMPACT_MODE__";

/// Impact-mode selection for a PR group job. It downloaded the
/// `target/anvil/impact` artifact, so it consumes that cache verbatim (falling
/// back to full-workspace only if the artifact is somehow absent).
const IMPACT_MODE_PR: &str = "        # This PR group job downloaded the target/anvil/impact artifact, so\n        # trust it: \"consume\" makes anvil-impact use the cache verbatim (no\n        # snapshot / cargo-delta / base ref). Fall back to \"off\" if absent.\n        if [ -f target/anvil/impact/impact.state ]; then\n          export ANVIL_IMPACT=consume\n        else\n          export ANVIL_IMPACT=off\n        fi";

/// Impact-mode selection for a scheduled group job. The scheduled tier always
/// validates the full workspace, so it is forced off UNCONDITIONALLY -- it must
/// not key off `target/anvil/impact/impact.state`, which `anvil-setup` can
/// restore stale from the `target/` cache and would otherwise wrongly enable
/// impact scoping (skipping the full-workspace backstop).
const IMPACT_MODE_SCHEDULED: &str = "        # Scheduled tier always validates the FULL workspace: force off\n        # (anvil-impact no-ops, every tier -> --workspace). Unconditional on\n        # purpose -- anvil-setup restores target/ from cache and a stale\n        # impact.state must NOT flip this job into impact scoping.\n        export ANVIL_IMPACT=off";

/// Render the `action.yml` for one check group's composite action.
#[must_use]
fn render_group_action(group: &str) -> String {
    let impact_mode = if group.starts_with("scheduled-") {
        IMPACT_MODE_SCHEDULED
    } else {
        IMPACT_MODE_PR
    };
    GROUP_ACTION_TEMPLATE
        .replace(GROUP_PLACEHOLDER, group)
        .replace(IMPACT_MODE_PLACEHOLDER, impact_mode)
}

/// Repo-root-relative path for a per-group composite action.
#[cfg(test)]
#[must_use]
fn group_action_path(group: &str) -> String {
    format!(".github/actions/anvil-{group}/action.yml")
}

/// `.github/actions/anvil-setup/action.yml`.
#[must_use]
pub fn setup_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-setup/action.yml", SETUP_ACTION)
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

/// The per-group composite actions, one concrete owned file per group.
///
/// Each `(group, path)` pair's `path` must equal [`group_action_path`] for
/// its group (asserted in tests); the body is [`render_group_action`].
pub(crate) const GROUP_ACTIONS: &[(&str, &str)] = &[
    ("pr-fast", ".github/actions/anvil-pr-fast/action.yml"),
    ("pr-test", ".github/actions/anvil-pr-test/action.yml"),
    ("pr-runtime-analysis", ".github/actions/anvil-pr-runtime-analysis/action.yml"),
    ("pr-mutants", ".github/actions/anvil-pr-mutants/action.yml"),
    ("scheduled-test", ".github/actions/anvil-scheduled-test/action.yml"),
    ("scheduled-advisories", ".github/actions/anvil-scheduled-advisories/action.yml"),
    (
        "scheduled-runtime-analysis",
        ".github/actions/anvil-scheduled-runtime-analysis/action.yml",
    ),
    ("scheduled-exhaustive", ".github/actions/anvil-scheduled-exhaustive/action.yml"),
];

/// All GitHub backend artifacts in emission order.
#[must_use]
pub(crate) fn all() -> Vec<Artifact> {
    let mut out = vec![setup_action(), impact_action()];
    for (group, path) in GROUP_ACTIONS {
        out.push(Artifact::backend_file(Backend::GitHub, path, render_group_action(group)));
    }
    out.push(pr_impl_workflow());
    out.push(scheduled_impl_workflow());
    out.push(pr_root_workflow());
    out.push(scheduled_root_workflow());
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn setup_and_impact_templates_are_non_empty() {
        assert!(SETUP_ACTION.contains("name: anvil-setup"));
        assert!(IMPACT_ACTION.contains("name: anvil-impact"));
        assert!(IMPACT_ACTION.contains("cargo-delta"));
        // Impact runs the shared recipe and publishes its target/anvil/impact
        // cache as a per-OS artifact (the group jobs download it).
        assert!(IMPACT_ACTION.contains("just anvil-impact"));
        assert!(IMPACT_ACTION.contains("actions/upload-artifact"));
        assert!(IMPACT_ACTION.contains("anvil-impact-${{ runner.os }}"));
    }

    #[test]
    fn setup_action_takes_group_input_and_dispatches() {
        assert!(SETUP_ACTION.contains("group:"));
        assert!(SETUP_ACTION.contains("just anvil-setup binstall"));
        assert!(SETUP_ACTION.contains("just \"anvil-${{ inputs.group }}-setup\" binstall"));
        assert!(SETUP_ACTION.contains("none)"));
    }

    #[test]
    fn setup_action_can_reclaim_github_hosted_runner_disk_space() {
        assert!(SETUP_ACTION.contains("free-disk-space:"));
        assert!(SETUP_ACTION.contains("runner.environment == 'github-hosted'"));
        assert!(SETUP_ACTION.contains("/usr/local/lib/android"));
        assert!(SETUP_ACTION.contains(r"C:\Program Files (x86)\Android"));
    }

    #[test]
    fn group_action_passes_group_to_setup() {
        let body = render_group_action("pr-fast");
        assert!(body.contains("uses: ./.github/actions/anvil-setup"));
        assert!(body.contains("group: pr-fast"));
        assert!(body.contains("free-disk-space: ${{ inputs.free-disk-space }}"));
    }

    #[test]
    fn impact_action_uses_group_none_and_installs_only_cargo_delta() {
        assert!(IMPACT_ACTION.contains("group: none"));
        assert!(IMPACT_ACTION.contains("anvil-tool-cargo-delta-install"));
    }

    #[test]
    fn render_group_action_uses_correct_name() {
        let body = render_group_action("pr-fast");
        assert!(body.contains("name: anvil-pr-fast"));
        assert!(body.contains("just anvil-pr-fast"));
        // Impact is consumed from the downloaded target/anvil/impact cache,
        // not threaded via env vars.
        assert!(
            !body.contains("ANVIL_INCLUDE_"),
            "group action must not thread ANVIL_INCLUDE_* env vars"
        );
        // A PR group downloads the artifact, so it consumes it (falling back to
        // off only if the artifact is somehow absent).
        assert!(body.contains("ANVIL_IMPACT=consume"));
        assert!(body.contains("ANVIL_IMPACT=off"));
        assert!(body.contains("target/anvil/impact/impact.state"));
    }

    #[test]
    fn scheduled_group_action_forces_impact_off_unconditionally() {
        // The scheduled tier always validates the full workspace. It must NOT
        // key its impact mode off target/anvil/impact/impact.state: anvil-setup
        // restores target/ from cache and could carry a stale impact.state,
        // which would wrongly enable scoping and skip the full-workspace
        // backstop. So a scheduled group forces off unconditionally.
        let body = render_group_action("scheduled-test");
        assert!(body.contains("export ANVIL_IMPACT=off"));
        assert!(
            !body.contains("ANVIL_IMPACT=consume"),
            "scheduled group must never consume the impact cache"
        );
        assert!(
            !body.contains("[ -f target/anvil/impact/impact.state ]"),
            "scheduled group must not gate its mode on the (cacheable) impact.state file"
        );
    }

    #[test]
    fn group_actions_do_not_thread_impact_inputs() {
        let body = render_group_action("scheduled-test");
        // The impact set is shared as a downloaded artifact, so the group
        // action declares no include_* inputs and threads no env vars.
        assert!(!body.contains("include_modified:"));
        assert!(!body.contains("include_affected:"));
        assert!(!body.contains("include_required:"));
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
    fn pr_impl_workflow_shares_impact_via_artifact_download() {
        // Group jobs consume the impact set by DOWNLOADING the per-OS artifact
        // the impact jobs uploaded -- not via job outputs / env vars.
        assert!(PR_IMPL_WORKFLOW.contains("actions/download-artifact"));
        assert!(PR_IMPL_WORKFLOW.contains("anvil-impact-${{ startsWith(matrix.os, 'linux') && 'Linux' || 'Windows' }}"));
        assert!(!PR_IMPL_WORKFLOW.contains("needs.impact-linux.outputs"));
        assert!(!PR_IMPL_WORKFLOW.contains("needs.impact-windows.outputs"));
        assert!(!PR_IMPL_WORKFLOW.contains("include_modified:"));
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
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-scheduled-impl.yml"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("schedule:"));
    }

    #[test]
    fn group_action_path_is_under_dot_github() {
        assert_eq!(group_action_path("pr-fast"), ".github/actions/anvil-pr-fast/action.yml");
    }

    #[test]
    fn group_action_paths_match_render() {
        assert_eq!(GROUP_ACTIONS.len(), GROUPS.len());
        for ((group, path), expected_group) in GROUP_ACTIONS.iter().zip(GROUPS) {
            assert_eq!(group, expected_group, "group order must match GROUPS");
            assert_eq!(
                *path,
                group_action_path(group),
                "registry path must match group_action_path for {group}"
            );
        }
    }
}
