// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Azure DevOps Pipelines backend files: step templates, stages templates,
//! and root pipelines, each an owned file gated on [`Backend::Ado`].
//!
//! Holds the embedded templates, the per-group fan-out, and the registry
//! functions. See [`ado.md`](../../../docs/design/ado.md).

use crate::backend::Backend;
use crate::catalog::Artifact;

/// Embedded body of the shared setup step template.
const SETUP_STEP: &str = include_str!("../../../templates/ado/steps/setup.yml");

/// Embedded body of the cargo-delta impact step template.
const IMPACT_STEP: &str = include_str!("../../../templates/ado/steps/impact.yml");

/// Embedded body of the advisory-comments step template.
const ADVISORY_COMMENTS_STEP: &str = include_str!("../../../templates/ado/steps/advisory-comments.yml");

/// Embedded body of the dirty-file job wrapper.
const JOB_WRAPPER: &str = include_str!("../../../templates/ado/steps/job.yml");

/// Embedded body of the PR-tier stages template.
const PR_STAGES: &str = include_str!("../../../templates/ado/pr-stages.yml");

/// Embedded body of the scheduled-tier stages template.
const SCHEDULED_STAGES: &str = include_str!("../../../templates/ado/scheduled-stages.yml");

/// Embedded body of the user-owned PR-tier custom-stages extension stub.
const CUSTOM_PR_STAGES: &str = include_str!("../../../templates/ado/custom-pr-stages.yml");

/// Embedded body of the user-owned scheduled-tier custom-stages extension stub.
const CUSTOM_SCHEDULED_STAGES: &str = include_str!("../../../templates/ado/custom-scheduled-stages.yml");

/// Embedded body of the PR root pipeline.
const PR_ROOT_PIPELINE: &str = include_str!("../../../templates/ado/pr-root-pipeline.yml");

/// Embedded body of the scheduled root pipeline.
const SCHEDULED_ROOT_PIPELINE: &str = include_str!("../../../templates/ado/scheduled-root-pipeline.yml");

/// All check groups that get a per-group step template.
///
/// See `github::GROUPS` for the rationale around splitting `pr-slow` into
/// three cloud-workflow-visible sub-stages.
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

/// Embedded template for one per-group step. `__GROUP__` is substituted with
/// the group name at emit time.
const GROUP_STEP_TEMPLATE: &str = include_str!("../../../templates/ado/steps/group.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Placeholder token the per-group template uses for the impact-mode selection.
const IMPACT_MODE_PLACEHOLDER: &str = "__IMPACT_MODE__";

/// Impact-mode selection for a PR group job. A PR group always downloads the
/// `target/anvil/impact` artifact (a required step, gated by `dependsOn` the
/// impact stage), so it trusts that cache verbatim. The mode is chosen by tier
/// here, at emit time -- never by probing `impact.state`, which `setup.yml` can
/// restore stale from the `target/` cache (see [`IMPACT_MODE_SCHEDULED`]).
const IMPACT_MODE_PR: &str = "      # This PR group job downloaded the target/anvil/impact artifact, so it\n      # trusts that cache verbatim: \"consume\" makes anvil-impact a no-op (no\n      # snapshot / cargo-delta / base ref). The mode is fixed by tier here,\n      # not probed from a cacheable file (see the scheduled variant).\n      export ANVIL_IMPACT=consume";

/// Impact-mode selection for a scheduled group job. The scheduled tier always
/// validates the full workspace, so it is forced off UNCONDITIONALLY -- it must
/// not key off `target/anvil/impact/impact.state`, which `setup.yml` can restore
/// stale from the `target/` cache and would otherwise wrongly enable impact
/// scoping (skipping the full-workspace backstop).
const IMPACT_MODE_SCHEDULED: &str = "      # Scheduled tier always validates the FULL workspace: force off\n      # (anvil-impact no-ops, every tier -> --workspace). Unconditional on\n      # purpose -- setup.yml restores target/ from cache and a stale\n      # impact.state must NOT flip this job into impact scoping.\n      export ANVIL_IMPACT=off";

/// Render the step template for one group.
#[must_use]
fn render_group_step(group: &str) -> String {
    let impact_mode = if group.starts_with("scheduled-") {
        IMPACT_MODE_SCHEDULED
    } else {
        IMPACT_MODE_PR
    };
    GROUP_STEP_TEMPLATE
        .replace(GROUP_PLACEHOLDER, group)
        .replace(IMPACT_MODE_PLACEHOLDER, impact_mode)
}

/// Repo-root-relative path for one group's step template.
#[cfg(test)]
#[must_use]
fn group_step_path(group: &str) -> String {
    format!(".pipelines/anvil/steps/{group}.yml")
}

/// `.pipelines/anvil/steps/setup.yml`.
#[must_use]
pub fn setup_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/setup.yml", SETUP_STEP)
}

/// `.pipelines/anvil/steps/impact.yml`.
#[must_use]
pub fn impact_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/impact.yml", IMPACT_STEP)
}

/// `.pipelines/anvil/steps/advisory-comments.yml`.
#[must_use]
pub fn advisory_comments() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/advisory-comments.yml", ADVISORY_COMMENTS_STEP)
}

/// `.pipelines/anvil/steps/job.yml` — the dirty-file job wrapper.
#[must_use]
pub fn job_wrapper() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/job.yml", JOB_WRAPPER)
}

/// `.pipelines/anvil/pr.yml` — the PR-tier stages template.
#[must_use]
pub fn pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/pr.yml", PR_STAGES)
}

/// `.pipelines/anvil/scheduled.yml` — the scheduled-tier stages template.
#[must_use]
pub fn scheduled_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/scheduled.yml", SCHEDULED_STAGES)
}

/// `.pipelines/anvil/custom-pr-stages.yml` — the repo-owned extension point
/// for PR-tier stages.
///
/// Emitted once as an empty `stages: []` stub. The PR root pipeline
/// references it after the anvil stages, so an adopter can add their own
/// stages here without editing the anvil-owned root or stages template.
/// Once edited it follows the standard dirty-file flow (Propose, don't
/// overwrite), exactly like `steps/job.yml`.
#[must_use]
pub fn custom_pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil/custom-pr-stages.yml", CUSTOM_PR_STAGES)
}

/// `.pipelines/anvil/custom-scheduled-stages.yml` — the repo-owned extension
/// point for scheduled-tier stages. See [`custom_pr_stages`].
#[must_use]
pub fn custom_scheduled_stages() -> Artifact {
    Artifact::backend_file(
        Backend::Ado,
        ".pipelines/anvil/custom-scheduled-stages.yml",
        CUSTOM_SCHEDULED_STAGES,
    )
}

/// `.pipelines/anvil-pr.yml` — the PR root pipeline.
#[must_use]
pub fn pr_root_pipeline() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil-pr.yml", PR_ROOT_PIPELINE)
}

/// `.pipelines/anvil-scheduled.yml` — the scheduled root pipeline.
#[must_use]
pub fn scheduled_root_pipeline() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".pipelines/anvil-scheduled.yml", SCHEDULED_ROOT_PIPELINE)
}

/// The per-group step templates, one concrete owned file per group.
///
/// Each `(group, path)` pair's `path` must equal [`group_step_path`] for its
/// group (asserted in tests); the body is [`render_group_step`].
pub(crate) const GROUP_STEPS: &[(&str, &str)] = &[
    ("pr-fast", ".pipelines/anvil/steps/pr-fast.yml"),
    ("pr-test", ".pipelines/anvil/steps/pr-test.yml"),
    ("pr-runtime-analysis", ".pipelines/anvil/steps/pr-runtime-analysis.yml"),
    ("pr-mutants", ".pipelines/anvil/steps/pr-mutants.yml"),
    ("scheduled-test", ".pipelines/anvil/steps/scheduled-test.yml"),
    ("scheduled-advisories", ".pipelines/anvil/steps/scheduled-advisories.yml"),
    (
        "scheduled-runtime-analysis",
        ".pipelines/anvil/steps/scheduled-runtime-analysis.yml",
    ),
    ("scheduled-exhaustive", ".pipelines/anvil/steps/scheduled-exhaustive.yml"),
];

/// All ADO backend artifacts in emission order.
#[must_use]
pub(crate) fn all() -> Vec<Artifact> {
    let mut out = vec![setup_step(), impact_step(), advisory_comments(), job_wrapper()];
    for (group, path) in GROUP_STEPS {
        out.push(Artifact::backend_file(Backend::Ado, path, render_group_step(group)));
    }
    out.push(pr_stages());
    out.push(scheduled_stages());
    out.push(custom_pr_stages());
    out.push(custom_scheduled_stages());
    out.push(pr_root_pipeline());
    out.push(scheduled_root_pipeline());
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn setup_and_impact_step_templates_are_non_empty() {
        assert!(SETUP_STEP.contains("just anvil-setup"));
        assert!(IMPACT_STEP.contains("cargo-delta"));
        // Impact runs the shared recipe; the compute job publishes its
        // target/anvil/impact cache as an artifact (see pr-stages.yml).
        assert!(IMPACT_STEP.contains("just anvil-impact"));
        assert!(!IMPACT_STEP.contains("##vso[task.setvariable"));
    }

    #[test]
    fn setup_step_takes_group_parameter_and_dispatches() {
        assert!(SETUP_STEP.contains("name: group"));
        assert!(SETUP_STEP.contains("just anvil-setup"));
        assert!(SETUP_STEP.contains("just anvil-${{ parameters.group }}-setup"));
        assert!(SETUP_STEP.contains("eq(parameters.group, 'none')"));
    }

    #[test]
    fn setup_step_quotes_inline_command_values_containing_colons() {
        // An inline `- bash: echo "x: y"` is a YAML *plain scalar*; the inner
        // `: ` is parsed as a mapping separator ("Mapping values are not
        // allowed in this context"), which ADO rejects at compile time. Such
        // values must be wrapped in quotes. Guard every inline command scalar
        // in the setup step (and catch the specific group=none echo).
        assert!(
            SETUP_STEP.contains(r#"- bash: 'echo "anvil-setup: group=none, skipping tool install"'"#),
            "the group=none echo must be single-quoted so its colon stays literal",
        );
        for line in SETUP_STEP.lines() {
            let trimmed = line.trim_start();
            let Some(value) = trimmed
                .strip_prefix("- bash:")
                .or_else(|| trimmed.strip_prefix("- script:"))
                .or_else(|| trimmed.strip_prefix("- pwsh:"))
                .or_else(|| trimmed.strip_prefix("- powershell:"))
            else {
                continue;
            };
            let value = value.trim();
            // A quoted scalar or a block scalar (`|`/`>`) is safe; a plain
            // scalar must not contain a `: ` mapping-separator sequence.
            if value.starts_with('\'') || value.starts_with('"') || value.starts_with('|') || value.starts_with('>') {
                continue;
            }
            assert!(
                !value.contains(": "),
                "unquoted inline command scalar with a colon will break ADO YAML compilation: {line}",
            );
        }
    }

    #[test]
    fn group_step_passes_group_to_setup() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("template: setup.yml"));
        assert!(body.contains("group: pr-fast"));
    }

    #[test]
    fn impact_step_uses_group_none_and_installs_only_cargo_delta() {
        assert!(IMPACT_STEP.contains("group: none"));
        assert!(IMPACT_STEP.contains("anvil-tool-cargo-delta-install"));
        assert!(!IMPACT_STEP.contains("cargo install --locked cargo-delta"));
    }

    #[test]
    fn job_wrapper_declares_expected_contract() {
        for needle in [
            "name: name",
            "name: pool",
            "name: steps",
            "type: stepList",
            "name: inputArtifacts",
            "DownloadPipelineArtifact@2",
            "name: artifacts",
            "PublishPipelineArtifact@1",
        ] {
            assert!(JOB_WRAPPER.contains(needle), "wrapper missing '{needle}'");
        }
    }

    #[test]
    fn render_group_step_shares_impact_via_cache_not_env() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("just anvil-pr-fast"));
        // The impact set is shared as a downloaded artifact, so the group step
        // declares no include_* params and threads no ANVIL_INCLUDE_* env vars.
        assert!(!body.contains("name: include_modified"));
        assert!(
            !body.contains("ANVIL_INCLUDE_"),
            "group step must not thread ANVIL_INCLUDE_* env vars"
        );
        // A PR group always downloads the artifact, so it consumes it. The mode
        // is fixed by tier -- not probed from the cacheable impact.state file.
        assert!(body.contains("export ANVIL_IMPACT=consume"));
        assert!(
            !body.contains("ANVIL_IMPACT=off"),
            "a PR group must not fall back to off (it always has the artifact)"
        );
        assert!(
            !body.contains("[ -f target/anvil/impact/impact.state ]"),
            "PR group must not gate its mode on the (cacheable) impact.state file"
        );
        // PR_TITLE is resolved from the REST API (ADO has no PR-title
        // predefined variable) and threaded via the PR_TITLE pipeline var.
        assert!(body.contains("PR_TITLE: $(PR_TITLE)"));
        assert!(body.contains("setvariable variable=PR_TITLE"));
        assert!(!body.contains("PR_TITLE: $(System.PullRequest.Title)"));
    }

    #[test]
    fn scheduled_group_step_forces_impact_off_unconditionally() {
        // The scheduled tier always validates the full workspace. It must NOT
        // key its impact mode off target/anvil/impact/impact.state: setup.yml
        // restores target/ from cache and could carry a stale impact.state,
        // which would wrongly enable scoping and skip the full-workspace
        // backstop. So a scheduled group forces off unconditionally.
        let body = render_group_step("scheduled-test");
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
    fn group_step_path_is_under_pipelines() {
        assert_eq!(group_step_path("scheduled-test"), ".pipelines/anvil/steps/scheduled-test.yml");
    }

    #[test]
    fn pr_stages_has_impact_and_group_stages() {
        for needle in [
            "stage: impact\n",
            "stage: pr_fast",
            "stage: pr_test",
            "stage: pr_runtime_analysis",
            "stage: pr_mutants",
        ] {
            assert!(PR_STAGES.contains(needle), "PR stages missing '{needle}'");
        }
        // The impact computation is a single stage with two per-OS jobs, not a
        // stage per OS -- matching how the pr-* stages run per-OS jobs.
        for needle in ["stage: impact_linux", "stage: impact_windows"] {
            assert!(
                !PR_STAGES.contains(needle),
                "impact should be one stage with per-OS jobs, not '{needle}'"
            );
        }
        for needle in ["stage: pr_slow\n", "stage: pr_slow1\n", "stage: pr_slow2\n", "stage: pr_slow3\n"] {
            assert!(
                !PR_STAGES.contains(needle),
                "Stale stage '{needle}' should be gone after the pr-slow rename"
            );
        }
        // Two per-OS jobs in the single impact stage.
        assert!(PR_STAGES.contains("name: compute_linux"));
        assert!(PR_STAGES.contains("name: compute_windows"));
        // The impact set propagates as a per-OS pipeline ARTIFACT (published by
        // the compute jobs, downloaded by each group job) -- not stage-output
        // variables.
        assert!(PR_STAGES.contains("name: anvil-impact-linux"));
        assert!(PR_STAGES.contains("name: anvil-impact-windows"));
        assert!(PR_STAGES.contains("inputArtifacts:"));
        assert!(!PR_STAGES.contains("stageDependencies.impact"));
        assert!(!PR_STAGES.contains("include_modified"));
        assert!(PR_STAGES.contains("- template: steps/job.yml"));
        assert!(
            !PR_STAGES.contains("\n      - job: "),
            "PR stages defines a bare `- job:` instead of going through steps/job.yml"
        );
        assert_eq!(
            PR_STAGES.matches("- task: PublishCodeCoverageResults@2").count(),
            2,
            "cobertura publish should appear once per pr_test job (linux + windows)"
        );
        // Every pr-* stage depends on the single impact stage.
        assert_eq!(
            PR_STAGES.matches("dependsOn: [impact]").count(),
            4,
            "each of the four pr-* stages must depend on the single impact stage"
        );
    }

    #[test]
    fn scheduled_stages_has_four_groups() {
        for needle in [
            "stage: scheduled_test",
            "stage: scheduled_advisories",
            "stage: scheduled_runtime_analysis",
            "stage: scheduled_exhaustive",
        ] {
            assert!(SCHEDULED_STAGES.contains(needle), "scheduled stages missing '{needle}'");
        }
        assert!(SCHEDULED_STAGES.contains("PublishCodeCoverageResults@2"));
        assert!(SCHEDULED_STAGES.contains("- template: steps/job.yml"));
        assert!(
            !SCHEDULED_STAGES.contains("\n      - job: "),
            "Scheduled stages defines a bare `- job:` instead of going through steps/job.yml"
        );
    }

    #[test]
    fn custom_stages_stubs_are_empty_and_take_pool_parameters() {
        // The extension stubs must emit a valid empty stages list (so the
        // default emit doesn't break the pipeline) and declare the pool
        // parameters the root pipelines pass them.
        for body in [CUSTOM_PR_STAGES, CUSTOM_SCHEDULED_STAGES] {
            assert!(
                body.contains("stages: []"),
                "custom-stages stub must default to an empty stages list"
            );
            assert!(body.contains("name: linuxPool"), "custom-stages stub must declare linuxPool");
            assert!(body.contains("name: windowsPool"), "custom-stages stub must declare windowsPool");
            // It must NOT define any concrete stage by default (the
            // commented-out example doesn't count).
            let defines_stage = body
                .lines()
                .map(str::trim_start)
                .any(|l| !l.starts_with('#') && l.starts_with("- stage:"));
            assert!(!defines_stage, "default custom-stages stub must not define a stage");
        }
    }

    #[test]
    fn custom_stages_artifacts_are_under_pipelines_anvil() {
        match custom_pr_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".pipelines/anvil/custom-pr-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
        match custom_scheduled_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".pipelines/anvil/custom-scheduled-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
    }

    #[test]
    fn root_pipelines_reference_their_custom_stages_extension() {
        assert!(
            PR_ROOT_PIPELINE.contains("template: anvil/custom-pr-stages.yml"),
            "PR root must reference the custom-pr-stages extension point"
        );
        assert!(
            SCHEDULED_ROOT_PIPELINE.contains("template: anvil/custom-scheduled-stages.yml"),
            "scheduled root must reference the custom-scheduled-stages extension point"
        );
    }

    #[test]
    fn group_step_paths_match_render() {
        assert_eq!(GROUP_STEPS.len(), GROUPS.len());
        for ((group, path), expected_group) in GROUP_STEPS.iter().zip(GROUPS) {
            assert_eq!(group, expected_group, "group order must match GROUPS");
            assert_eq!(
                *path,
                group_step_path(group),
                "registry path must match group_step_path for {group}"
            );
        }
    }
}
