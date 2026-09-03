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

/// Shared composite action that publishes dynamic failure commit statuses.
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
#[inline]
#[must_use]
pub fn setup_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-setup/action.yml", SETUP_ACTION)
}

/// `.github/actions/anvil-setup/just-problem-matcher.json`.
#[inline]
#[must_use]
pub fn just_problem_matcher() -> Artifact {
    Artifact::backend_file(
        Backend::GitHub,
        ".github/actions/anvil-setup/just-problem-matcher.json",
        JUST_PROBLEM_MATCHER,
    )
}

/// `.github/actions/anvil-run-group/action.yml`.
#[inline]
#[must_use]
pub fn run_group_action() -> Artifact {
    Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-run-group/action.yml", RUN_GROUP_ACTION)
}

/// `.github/actions/anvil-report-status/action.yml`.
#[inline]
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
    #[cfg(windows)]
    use std::env::var_os;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, ExitStatus};

    use tempfile::tempdir;

    use super::*;

    const PR_GROUPS: &[&str] = &["pr-fast", "pr-test", "pr-msrv", "pr-runtime-analysis", "pr-mutants"];
    const SCHEDULED_GROUPS: &[&str] = &[
        "scheduled-test",
        "scheduled-advisories",
        "scheduled-runtime-analysis",
        "scheduled-exhaustive",
    ];

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
        assert!(JUST_PROBLEM_MATCHER.contains("failed( on line \\\\d+)? with exit code"));
    }

    #[test]
    fn setup_action_takes_group_input_and_dispatches() {
        assert!(SETUP_ACTION.contains("group:"));
        assert!(SETUP_ACTION.contains("just anvil-setup binstall"));
        assert!(!SETUP_ACTION.contains("_anvil-resolve-stable"));
        assert!(!SETUP_ACTION.contains("just anvil-toolchain-stable-install"));
        assert!(!SETUP_ACTION.contains("rustc-version"));
        assert!(!SETUP_ACTION.contains("hashFiles('Cargo.toml'"));
        assert!(!SETUP_ACTION.contains("'Cargo.lock'"));
        assert!(SETUP_ACTION.contains("'rust-toolchain.toml'"));
        let cache_restore = SETUP_ACTION
            .find("name: Restore cargo cache")
            .expect("setup must restore Cargo home");
        let just_bootstrap = SETUP_ACTION.find("name: Install just").expect("setup must bootstrap Just");
        let catalog_setup = SETUP_ACTION
            .find("name: Install anvil toolchains + tools")
            .expect("setup must dispatch catalog setup");
        assert!(
            cache_restore < just_bootstrap,
            "Cargo home must be restored before Just is bootstrapped"
        );
        assert!(just_bootstrap < catalog_setup, "Just must be bootstrapped before catalog setup");
        assert!(SETUP_ACTION.contains("ANVIL_GROUP: ${{ inputs.group }}"));
        assert!(SETUP_ACTION.contains("just \"anvil-$ANVIL_GROUP-setup\" binstall"));
        assert!(SETUP_ACTION.contains(r"^[a-z0-9-]+$"));
        assert!(SETUP_ACTION.contains("::error::Invalid Anvil group;"));
        assert!(!SETUP_ACTION.contains("::error::Invalid Anvil group '$ANVIL_GROUP'"));
        assert!(SETUP_ACTION.contains("none)"));
    }

    #[test]
    fn setup_action_can_reclaim_github_hosted_runner_disk_space() {
        assert!(SETUP_ACTION.contains("free-disk-space:"));
        assert!(SETUP_ACTION.contains("runner.environment == 'github-hosted'"));
        assert!(SETUP_ACTION.contains("/usr/local/lib/android"));
        assert!(SETUP_ACTION.contains(r"C:\Program Files (x86)\Android"));
        assert!(!SETUP_ACTION.contains("Install libclang"));
        assert!(!SETUP_ACTION.contains("apt-get install -y libclang-dev"));
    }

    #[test]
    fn run_group_action_captures_and_reports_results() {
        assert!(RUN_GROUP_ACTION.contains("uses: ./.github/actions/anvil-setup"));
        assert!(RUN_GROUP_ACTION.contains("group: ${{ inputs.group }}"));
        assert!(RUN_GROUP_ACTION.contains("free-disk-space: ${{ inputs.free-disk-space }}"));
        assert!(RUN_GROUP_ACTION.contains("status=${PIPESTATUS[0]}"));
        assert!(RUN_GROUP_ACTION.contains("Failed Just recipe: ${{ steps.run.outputs.failed_recipe }}"));
        assert!(RUN_GROUP_ACTION.contains("uses: ./.github/actions/anvil-report-status"));
        // Impact reaches scoped checks through the downloaded impact cache
        // (read via `_anvil-impact-include`), not threaded --package env vars;
        // the executor only fixes the mode.
        assert!(RUN_GROUP_ACTION.contains("impact_mode:"));
        assert!(RUN_GROUP_ACTION.contains("ANVIL_IMPACT: ${{ inputs.impact_mode }}"));
        assert!(
            !RUN_GROUP_ACTION.contains("ANVIL_INCLUDE_"),
            "the shared executor must not thread ANVIL_INCLUDE_* env vars"
        );
    }

    #[test]
    fn pr_groups_consume_downloaded_impact_while_scheduled_defaults_off() {
        // PR group jobs download the per-OS impact artifact and run in
        // `consume` mode; scoped checks read that downloaded cache.
        assert!(PR_IMPL_WORKFLOW.contains("uses: actions/download-artifact"));
        assert!(PR_IMPL_WORKFLOW.contains("name: anvil-impact-${{ startsWith(matrix.os, 'linux') && 'Linux' || 'Windows' }}"));
        assert!(PR_IMPL_WORKFLOW.contains("impact_mode: consume"));
        assert!(
            !PR_IMPL_WORKFLOW.contains("include_modified:"),
            "PR groups consume the downloaded cache, not threaded --package inputs"
        );
        // The scheduled workflow passes no impact_mode, so the executor's
        // default keeps every scheduled tier full-workspace (never scoped).
        assert!(
            !SCHEDULED_IMPL_WORKFLOW.contains("impact_mode:"),
            "scheduled groups rely on the executor's default-off, never consume"
        );
        assert!(
            RUN_GROUP_ACTION.contains("default: \"off\""),
            "the executor must default impact_mode to off so an un-wired caller never scopes checks out"
        );
    }

    fn run_group_step(fake_output: &str, fake_status: i32) -> (ExitStatus, String) {
        let temp = tempdir().expect("the test must be able to create a temporary action workspace");
        let output_path = temp.path().join("github-output");
        let harness = r#"
just() {
  if [[ -n "$FAKE_JUST_OUTPUT" ]]; then
    printf '%s\n' "$FAKE_JUST_OUTPUT"
  fi
  return "$FAKE_JUST_STATUS"
}
export -f just
"#;
        let action = RUN_GROUP_ACTION.replace("\r\n", "\n");
        let run_step = action
            .split_once("    - name: Run Anvil group\n")
            .expect("group action should contain its run step")
            .1;
        let script = run_step
            .split_once("      run: |\n")
            .expect("group run step should contain an inline script")
            .1
            .split_once("\n    - ")
            .expect("group run step should be followed by another action step")
            .0
            .lines()
            .map(|line| line.strip_prefix("        ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let program = format!("{harness}\n{script}");
        #[cfg(windows)]
        // `bash` resolves to WSL in the Windows test environment, whose
        // filesystem namespace cannot address this Windows temporary path.
        let bash =
            PathBuf::from(var_os("ProgramFiles").expect("Git for Windows must be installed under Program Files on Windows test runners"))
                .join("Git")
                .join("bin")
                .join("bash.exe");
        #[cfg(not(windows))]
        let bash = PathBuf::from("bash");
        let status = Command::new(bash)
            .args(["-c", &program])
            .current_dir(temp.path())
            .env("ANVIL_GROUP", "pr-fast")
            .env("FAKE_JUST_OUTPUT", fake_output)
            .env("FAKE_JUST_STATUS", fake_status.to_string())
            .env("GITHUB_OUTPUT", "github-output")
            .env("RUNNER_TEMP", ".")
            .status()
            .expect("bash must be available because generated GitHub group actions require it");
        let outputs = fs::read_to_string(output_path).expect("the group script must write GitHub step outputs");
        (status, outputs)
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn run_group_step_exports_success() {
        let (status, outputs) = run_group_step("all checks passed", 0);

        assert!(
            status.success(),
            "the capture script must defer group failure to the named action step"
        );
        assert!(outputs.contains("failed_recipe=anvil-pr-fast"));
        assert!(outputs.contains("exit_code=0"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn run_group_step_exports_terminal_recipe_failure() {
        let diagnostic = "error: recipe `anvil-license-headers` failed with exit code 17";
        let (status, outputs) = run_group_step(diagnostic, 17);

        assert!(
            status.success(),
            "the capture script must defer group failure to the named action step"
        );
        assert!(outputs.contains("failed_recipe=anvil-license-headers"));
        assert!(outputs.contains("exit_code=17"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn run_group_step_exports_line_recipe_failure() {
        let diagnostic = "error: recipe `anvil-license-headers` failed on line 42 with exit code 17";
        let (status, outputs) = run_group_step(diagnostic, 17);

        assert!(
            status.success(),
            "the capture script must defer group failure to the named action step"
        );
        assert!(outputs.contains("failed_recipe=anvil-license-headers"));
        assert!(outputs.contains("exit_code=17"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn run_group_step_falls_back_to_group_without_terminal_diagnostic() {
        let (status, outputs) = run_group_step("unexpected tool failure", 9);

        assert!(
            status.success(),
            "the capture script must defer group failure to the named action step"
        );
        assert!(outputs.contains("failed_recipe=anvil-pr-fast"));
        assert!(outputs.contains("exit_code=9"));
    }

    #[test]
    fn impact_action_uses_group_none_and_runs_the_shared_recipe() {
        // The impact action reuses anvil-setup (group=none) + the cargo-delta
        // install, then runs the same `just anvil-impact` recipe adopters run
        // locally and uploads the whole cache as a per-OS artifact. The include
        // lists reach group jobs through that downloaded cache, never job
        // outputs, so CI and local execution stay identical by construction.
        assert!(IMPACT_ACTION.contains("group: none"));
        assert!(IMPACT_ACTION.contains("just anvil-tool-cargo-delta-install binstall"));
        assert!(IMPACT_ACTION.contains("run: just anvil-impact"));
        assert!(IMPACT_ACTION.contains("uses: actions/upload-artifact"));
        assert!(IMPACT_ACTION.contains("name: anvil-impact-${{ runner.os }}"));
        assert!(IMPACT_ACTION.contains("path: target/anvil/impact"));
    }

    #[test]
    fn status_action_manages_dynamic_failure_contexts_on_pr_head() {
        assert!(REPORT_STATUS_ACTION.contains("github.rest.repos.createCommitStatus"));
        assert!(REPORT_STATUS_ACTION.contains("github.rest.repos.listCommitStatusesForRef"));
        assert!(REPORT_STATUS_ACTION.contains("context.payload.pull_request.head.sha"));
        assert!(REPORT_STATUS_ACTION.contains("requires a pull_request event with a head SHA"));
        assert!(REPORT_STATUS_ACTION.contains("const statusPrefix = \"Anvil / \""));
        assert!(REPORT_STATUS_ACTION.contains("if (seen.has(status.context))"));
        assert!(REPORT_STATUS_ACTION.contains("status.context.endsWith(statusSuffix)"));
        assert!(REPORT_STATUS_ACTION.contains("status.target_url?.endsWith(groupMarker)"));
        assert!(REPORT_STATUS_ACTION.contains("MAX_CONTEXT_LENGTH - statusPrefix.length - statusSuffix.length"));
        assert!(REPORT_STATUS_ACTION.contains("failedRecipe.replace(/^anvil-/, \"\")"));
        assert!(REPORT_STATUS_ACTION.contains("statusDescription.slice(0, MAX_DESCRIPTION_LENGTH)"));
    }

    #[test]
    fn pr_impl_workflow_has_expected_jobs() {
        assert!(PR_IMPL_WORKFLOW.contains("workflow_call:"));
        assert!(
            !PR_IMPL_WORKFLOW.contains("ANVIL_SPELLCHECK_SKIP_UNSUPPORTED_ARM64"),
            "the PR workflow must run spellcheck on ARM64"
        );
        for needle in [
            "impact-linux:",
            "impact-windows:",
            "pr-fast:",
            "pr-test:",
            "pr-msrv:",
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
        // The matrix is intentionally always present; the generated MSRV
        // recipe owns the successful no-MSRV no-op.
        assert!(!PR_IMPL_WORKFLOW.contains("msrv_test_required"));
        assert!(PR_IMPL_WORKFLOW.contains("Check Group: MSRV Tests (${{ matrix.os }})"));
        assert!(PR_IMPL_WORKFLOW.contains("os: [linux, windows, linux-arm, windows-arm]"));
        assert!(!PR_IMPL_WORKFLOW.contains("fromJSON"));
        assert!(PR_IMPL_WORKFLOW.contains("PR_TITLE"));
        assert!(PR_IMPL_WORKFLOW.contains("BASE_REF"));
        for name in [
            "Check Group: Fast Checks (${{ matrix.os }})",
            "Check Group: Tests and Coverage (${{ matrix.os }})",
            "Check Group: Runtime Analysis (${{ matrix.os }})",
            "Check Group: Mutation Testing (${{ matrix.os }})",
        ] {
            assert!(PR_IMPL_WORKFLOW.contains(name), "PR impl workflow missing display name '{name}'");
        }
        assert!(PR_IMPL_WORKFLOW.contains("publish_commit_statuses:"));
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("BASE_REF: ${{ inputs.base_ref }}").count(),
            4,
            "both impact jobs plus SemVer and mutation checks must use the event-specific base"
        );
        for group in PR_GROUPS {
            assert!(
                PR_IMPL_WORKFLOW.contains(&format!("group: {group}")),
                "PR impl workflow missing group '{group}'"
            );
        }
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("uses: ./.github/actions/anvil-run-group").count(),
            PR_GROUPS.len(),
            "every PR group job must use the shared group action"
        );
        assert_eq!(
            PR_IMPL_WORKFLOW
                .matches("publish_commit_statuses: ${{ inputs.publish_commit_statuses }}")
                .count(),
            PR_GROUPS.len(),
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
            PR_IMPL_WORKFLOW.matches("permissions:").count(),
            0,
            "the shared implementation must inherit the root caller's permission ceiling"
        );
        assert_eq!(
            PR_IMPL_WORKFLOW.matches("free-disk-space: true").count(),
            2,
            "disk cleanup should be enabled for the PR test and MSRV groups"
        );
    }

    #[test]
    fn scheduled_impl_workflow_has_expected_jobs() {
        for group in SCHEDULED_GROUPS {
            let needle = format!("{group}:");
            assert!(
                SCHEDULED_IMPL_WORKFLOW.contains(&needle),
                "scheduled impl workflow missing job '{needle}'"
            );
            assert!(
                SCHEDULED_IMPL_WORKFLOW.contains(&format!("group: {group}")),
                "scheduled impl workflow missing group '{group}'"
            );
        }
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("publish-failure:"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("codecov/codecov-action"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("vars.ANVIL_PUBLISH_FAILURE_ISSUE != 'false'"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("contains(needs.*.result, 'failure')"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("actions/github-script@ed597411d8f924073f98dfc5c65a23a2325f34cd"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.search.issuesAndPullRequests"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.issues.createComment"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("github.rest.issues.create"));
        assert!(SCHEDULED_IMPL_WORKFLOW.contains("\npermissions:\n  contents: read\n"));
        assert_eq!(SCHEDULED_IMPL_WORKFLOW.matches("issues: write").count(), 1);
        let publisher_permissions = SCHEDULED_IMPL_WORKFLOW
            .split_once("\n  publish-failure:")
            .expect("scheduled workflow should contain publish-failure")
            .1
            .split_once("\n    steps:")
            .expect("publish-failure should contain steps")
            .0;
        assert!(publisher_permissions.contains("\n    permissions:\n      issues: write"));
        assert!(!publisher_permissions.contains("contents: read"));
        assert_eq!(
            SCHEDULED_IMPL_WORKFLOW.matches("uses: ./.github/actions/anvil-run-group").count(),
            SCHEDULED_GROUPS.len(),
            "every scheduled group job must use the shared group action"
        );
        assert_eq!(
            SCHEDULED_IMPL_WORKFLOW.matches("free-disk-space: true").count(),
            1,
            "disk cleanup should be enabled for the scheduled test group"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    #[expect(clippy::too_many_lines, reason = "keeps the reporter lifecycle scenarios together")]
    fn status_script_manages_failure_lifecycle() {
        let action = REPORT_STATUS_ACTION.replace("\r\n", "\n");
        let script = action
            .split_once("        script: |\n")
            .expect("status action should contain an inline script")
            .1
            .lines()
            .map(|line| line.strip_prefix("          ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let harness = format!("const workflowScript = {script:?};\n")
            + r##"
const assert = require("node:assert/strict");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction("github", "context", "process", workflowScript);

const baseContext = {
  repo: { owner: "microsoft", repo: "ox-tools" },
  payload: { pull_request: { head: { sha: "head-sha" } } },
};
const baseEnv = {
  ANVIL_GROUP: "pr-fast",
  ANVIL_SETUP_OUTCOME: "success",
  ANVIL_EXIT_CODE: "17",
  ANVIL_FAILED_RECIPE: "anvil-license-headers",
  RUNNER_OS: "Linux",
  RUNNER_ARCH: "X64",
  GITHUB_SERVER_URL: "https://github.com",
  GITHUB_REPOSITORY: "microsoft/ox-tools",
  GITHUB_RUN_ID: "42",
};
const GITHUB_MAX_STATUS_PAGE_SIZE = 100;
const OVER_LIMIT_RECIPE_LENGTH = 150;

async function scenario({ env = {}, history = [], context = baseContext } = {}) {
  const calls = { paginate: [], publish: [] };
  const github = {
    paginate: async (method, args) => {
      calls.paginate.push({ method, args });
      return history;
    },
    rest: {
      repos: {
        listCommitStatusesForRef: function listCommitStatusesForRef() {},
        createCommitStatus: async args => calls.publish.push(args),
      },
    },
  };
  const process = { env: { ...baseEnv, ...env } };
  let error;
  try {
    await run(github, context, process);
  } catch (caught) {
    error = caught;
  }
  return { calls, error };
}

(async () => {
  const groupMarker = "#anvil-group=pr-fast";
  const staleContext = "Anvil / clippy [pr-fast] (linux-x64)";
  const failure = await scenario({
    history: [
      {
        context: staleContext,
        state: "failure",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/41${groupMarker}`,
      },
      {
        context: staleContext,
        state: "success",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/40${groupMarker}`,
      },
      {
        context: "Anvil / already-clear [pr-fast] (linux-x64)",
        state: "success",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/41${groupMarker}`,
      },
      {
        context: "Anvil / already-clear [pr-fast] (linux-x64)",
        state: "failure",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/40${groupMarker}`,
      },
      {
        context: "Anvil / license-headers [pr-test] (linux-x64)",
        state: "failure",
        target_url: "https://github.com/microsoft/ox-tools/actions/runs/41#anvil-group=pr-test",
      },
      {
        context: "Anvil / other-runner [pr-fast] (windows-x64)",
        state: "failure",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/41${groupMarker}`,
      },
    ],
  });
  assert.equal(failure.error, undefined);
  assert.equal(failure.calls.paginate.length, 1);
  assert.equal(failure.calls.paginate[0].args.ref, "head-sha");
  assert.equal(
    failure.calls.paginate[0].args.per_page,
    GITHUB_MAX_STATUS_PAGE_SIZE,
  );
  assert.equal(failure.calls.publish.length, 2);
  assert.deepEqual(failure.calls.publish[0], {
    owner: "microsoft",
    repo: "ox-tools",
    sha: "head-sha",
    state: "failure",
    context: "Anvil / license-headers [pr-fast] (linux-x64)",
    description: "license-headers failed",
    target_url: `https://github.com/microsoft/ox-tools/actions/runs/42${groupMarker}`,
  });
  assert.equal(failure.calls.publish[1].context, staleContext);
  assert.equal(failure.calls.publish[1].state, "success");
  assert.equal(failure.calls.publish[1].description, "superseded by license-headers");

  const clean = await scenario({
    env: { ANVIL_EXIT_CODE: "0", ANVIL_FAILED_RECIPE: "" },
    history: [
      {
        context: staleContext,
        state: "error",
        target_url: `https://github.com/microsoft/ox-tools/actions/runs/41${groupMarker}`,
      },
    ],
  });
  assert.equal(clean.calls.publish.length, 1);
  assert.equal(clean.calls.publish[0].context, staleContext);
  assert.equal(clean.calls.publish[0].state, "success");
  assert.equal(clean.calls.publish[0].description, "superseded: all pr-fast recipes passed");

  const setup = await scenario({
    env: { ANVIL_SETUP_OUTCOME: "failure", ANVIL_EXIT_CODE: "", ANVIL_FAILED_RECIPE: "" },
  });
  assert.equal(setup.calls.publish.length, 1);
  assert.equal(setup.calls.publish[0].state, "error");
  assert.equal(setup.calls.publish[0].context, "Anvil / pr-fast setup [pr-fast] (linux-x64)");
  assert.equal(setup.calls.publish[0].description, "setup failed before pr-fast ran");

  const noResult = await scenario({
    env: { ANVIL_EXIT_CODE: "", ANVIL_FAILED_RECIPE: "" },
  });
  assert.equal(noResult.calls.publish.length, 1);
  assert.equal(noResult.calls.publish[0].state, "error");
  assert.equal(noResult.calls.publish[0].context, "Anvil / pr-fast no result [pr-fast] (linux-x64)");

  // Deliberately exceed the API context limit to verify label-only truncation.
  const longRecipe = `anvil-${"x".repeat(OVER_LIMIT_RECIPE_LENGTH)}`;
  const truncated = await scenario({ env: { ANVIL_FAILED_RECIPE: longRecipe } });
  assert.equal(truncated.calls.publish[0].context.length, 100);
  assert.match(truncated.calls.publish[0].context, /^Anvil \/ x+ \[pr-fast\] \(linux-x64\)$/);

  const missingPullRequest = await scenario({ context: { repo: baseContext.repo, payload: {} } });
  assert.match(
    missingPullRequest.error.message,
    /requires a pull_request event with a head SHA/,
  );
  assert.equal(missingPullRequest.calls.paginate.length, 0);
  assert.equal(missingPullRequest.calls.publish.length, 0);
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
"##;

        if Command::new("node").arg("--version").output().is_err() {
            return;
        }

        let dir = tempdir().expect("create temporary test directory");
        let path = dir.path().join("report-status.test.cjs");
        fs::write(&path, harness).expect("write status reporter behavior test");
        let output = Command::new("node")
            .arg(&path)
            .output()
            .expect("execute generated status reporter behavior test");
        assert!(
            output.status.success(),
            "generated status reporter behavior test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
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
// github-script executes an asynchronous body with injected runtime values.
// Model only the github/context/process values this script uses; the API client
// remains mocked rather than recreating the complete action runtime or Node image.
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction("github", "context", "process", workflowScript);
const marker = "<!-- anvil scheduled failure -->";
const searchableMarker = marker.replace(/^<!--\s*|\s*-->$/g, "");
const title = "[Anvil] Scheduled checks failed";
const context = {
  serverUrl: "https://github.com",
  repo: { owner: "microsoft", repo: "ox-tools" },
  runId: 42,
};
const expectedQuery = `repo:${context.repo.owner}/${context.repo.repo} is:issue is:open in:body "${searchableMarker}"`;

async function scenario(items) {
  const calls = { search: [], create: [], comment: [] };
  const github = {
    rest: {
      search: {
        issuesAndPullRequests: async args => {
          calls.search.push(args);
          return { data: { items: args.q === expectedQuery ? items : [] } };
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
  assert.equal(created.search[0].q, expectedQuery);
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

        if Command::new("node").arg("--version").output().is_err() {
            return;
        }

        let dir = tempdir().expect("create temporary test directory");
        let path = dir.path().join("scheduled-failure.test.cjs");
        fs::write(&path, harness).expect("write JavaScript behavior test");
        let output = Command::new("node")
            .arg(&path)
            .output()
            .expect("execute generated github-script behavior test");
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
        assert!(PR_ROOT_WORKFLOW.contains("statuses: write"));
        assert!(PR_ROOT_WORKFLOW.contains("publish_commit_statuses: ${{ github.event_name == 'pull_request' }}"));
        assert!(PR_ROOT_WORKFLOW.contains("base_ref: ${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha }}"));
        assert!(PR_ROOT_WORKFLOW.contains("name: Anvil"));
        assert!(PR_ROOT_WORKFLOW.contains("\n  validation:\n"));
        assert!(PR_ROOT_WORKFLOW.contains("name: PR Job"));
        assert!(!PR_ROOT_WORKFLOW.contains("\n  merge-validation:\n"));
        assert_eq!(
            PR_ROOT_WORKFLOW.matches("name: PR Job").count(),
            1,
            "one caller must handle both pull-request and merge-group events"
        );
        let validation_caller = PR_ROOT_WORKFLOW
            .split_once("\n  validation:")
            .expect("PR root workflow should contain validation")
            .1;
        assert!(validation_caller.contains("\n    permissions:\n      contents: read"));
        assert!(validation_caller.contains("statuses: write"));
        assert!(validation_caller.contains("pull-requests: write"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("uses: ./.github/workflows/anvil-scheduled-impl.yml"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("schedule:"));
        assert!(SCHEDULED_ROOT_WORKFLOW.contains("issues: write"));
    }
}
