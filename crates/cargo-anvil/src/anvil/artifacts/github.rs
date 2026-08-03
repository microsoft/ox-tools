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

/// Render the `action.yml` for one check group's composite action.
#[must_use]
fn render_group_action(group: &str) -> String {
    GROUP_ACTION_TEMPLATE.replace(GROUP_PLACEHOLDER, group)
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
    use std::{fs, process::Command};

    use super::*;

    #[test]
    fn setup_and_impact_templates_are_non_empty() {
        assert!(SETUP_ACTION.contains("name: anvil-setup"));
        assert!(IMPACT_ACTION.contains("name: anvil-impact"));
        assert!(IMPACT_ACTION.contains("cargo-delta"));
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
        assert!(!SETUP_ACTION.contains("Install libclang"));
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
    fn render_group_action_uses_correct_name() {
        let body = render_group_action("pr-fast");
        assert!(body.contains("name: anvil-pr-fast"));
        assert!(body.contains("just anvil-pr-fast"));
        assert!(body.contains("ANVIL_INCLUDE_MODIFIED"));
        assert!(body.contains("ANVIL_INCLUDE_AFFECTED"));
        assert!(body.contains("ANVIL_INCLUDE_REQUIRED"));
    }

    #[test]
    fn group_actions_declare_include_inputs() {
        let body = render_group_action("scheduled-test");
        assert!(body.contains("include_modified:"));
        assert!(body.contains("include_affected:"));
        assert!(body.contains("include_required:"));
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
    fn scheduled_impl_workflow_has_expected_jobs() {
        for needle in [
            "scheduled-test:",
            "scheduled-advisories:",
            "scheduled-runtime-analysis:",
            "scheduled-exhaustive:",
            "publish-failure:",
        ] {
            assert!(
                SCHEDULED_IMPL_WORKFLOW.contains(needle),
                "scheduled impl workflow missing job '{needle}'"
            );
        }
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("codecov/codecov-action"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("vars.ANVIL_PUBLISH_FAILURE_ISSUE != 'false'"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("contains(needs.*.result, 'failure')"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("actions/github-script@ed597411d8f924073f98dfc5c65a23a2325f34cd"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.search.issuesAndPullRequests"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.issues.createComment"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.issues.create"));
        assert_eq!(
            SCHEDULED_IMPL_WORKFLOW.matches("free-disk-space: true").count(),
            1,
            "disk cleanup should be enabled for the scheduled test group"
        );
    }

    #[test]
    fn scheduled_failure_script_upserts_marker_owned_issues() {
        let script = SCHEDULED_IMPL_WORKFLOW
            .split_once("          script: |\n")
            .expect("scheduled workflow should contain an inline script")
            .1
            .lines()
            .map(|line| line.strip_prefix("            ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let harness = format!("const workflowScript = {script:?};\n")
            + r#"
const assert = require("node:assert/strict");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction("github", "context", "process", workflowScript);
const marker = "<!-- anvil scheduled failure -->";
const title = "[Anvil] Scheduled checks failed";
const context = {
  serverUrl: "https://github.com",
  repo: { owner: "microsoft", repo: "ox-tools" },
  runId: 42,
};

async function scenario(items) {
  const calls = { search: [], create: [], comment: [] };
  const github = {
    rest: {
      search: {
        issuesAndPullRequests: async args => {
          calls.search.push(args);
          return { data: { items } };
        },
      },
      issues: {
        create: async args => calls.create.push(args),
        createComment: async args => calls.comment.push(args),
      },
    },
  };
  const process = {
    env: {
      ANVIL_JOB_RESULTS: JSON.stringify({
        "scheduled-test": { result: "failure" },
        "scheduled-advisories": { result: "success" },
        "scheduled-runtime-analysis": { result: "cancelled" },
        "scheduled-exhaustive": { result: "failure" },
      }),
    },
  };
  await run(github, context, process);
  return calls;
}

(async () => {
  const created = await scenario([]);
  assert.equal(created.search.length, 1);
  assert.match(created.search[0].q, /in:body/);
  assert.equal(created.create.length, 1);
  assert.equal(created.comment.length, 0);
  assert.equal(created.create[0].title, title);
  assert.match(created.create[0].body, new RegExp(marker));
  assert.match(created.create[0].body, /- `scheduled-test`/);
  assert.match(created.create[0].body, /- `scheduled-exhaustive`/);
  assert.doesNotMatch(created.create[0].body, /scheduled-advisories/);
  assert.doesNotMatch(created.create[0].body, /scheduled-runtime-analysis/);
  assert.match(
    created.create[0].body,
    /https:\/\/github\.com\/microsoft\/ox-tools\/actions\/runs\/42/,
  );

  const existing = await scenario([
    { number: 17, title: "Maintainer-renamed incident", body: marker },
  ]);
  assert.equal(existing.create.length, 0);
  assert.equal(existing.comment.length, 1);
  assert.equal(existing.comment[0].issue_number, 17);

  const collision = await scenario([
    { number: 23, title, body: "A human-authored issue without the marker." },
  ]);
  assert.equal(collision.create.length, 1);
  assert.equal(collision.comment.length, 0);
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
"#;

        let dir = tempfile::tempdir().expect("create temporary test directory");
        let path = dir.path().join("scheduled-failure.test.cjs");
        fs::write(&path, harness).expect("write JavaScript behavior test");
        let output = Command::new("node")
            .arg(&path)
            .output()
            .expect("Node.js is required to test the generated github-script");
        assert!(
            output.status.success(),
            "generated github-script behavior test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn root_workflows_call_reusable_workflows() {
        assert!(PR_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-pr-impl.yml"));
        assert!(PR_ROOT_WORKFLOW.contains("pull_request:"));
        assert!(PR_ROOT_WORKFLOW.contains("merge_group:"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-scheduled-impl.yml"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("schedule:"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("issues: write"));
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
