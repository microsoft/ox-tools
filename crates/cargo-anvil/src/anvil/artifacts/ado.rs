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

/// Render the step template for one group.
#[must_use]
fn render_group_step(group: &str) -> String {
    GROUP_STEP_TEMPLATE.replace(GROUP_PLACEHOLDER, group)
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
        assert!(IMPACT_STEP.contains("##vso[task.setvariable"));
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
            "name: artifacts",
            "PublishPipelineArtifact@1",
        ] {
            assert!(JOB_WRAPPER.contains(needle), "wrapper missing '{needle}'");
        }
    }

    #[test]
    fn render_group_step_has_include_inputs_and_env() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("parameters:"));
        assert!(body.contains("name: include_modified"));
        assert!(body.contains("name: include_affected"));
        assert!(body.contains("name: include_required"));
        assert!(body.contains("just anvil-pr-fast"));
        assert!(body.contains("ANVIL_INCLUDE_MODIFIED"));
        assert!(body.contains("ANVIL_INCLUDE_AFFECTED"));
        assert!(body.contains("ANVIL_INCLUDE_REQUIRED"));
        assert!(body.contains("PR_TITLE: $(System.PullRequest.Title)"));
    }

    #[test]
    fn group_step_path_is_under_pipelines() {
        assert_eq!(group_step_path("scheduled-test"), ".pipelines/anvil/steps/scheduled-test.yml");
    }

    #[test]
    fn pr_stages_has_impact_and_group_stages() {
        for needle in [
            "stage: impact_linux",
            "stage: impact_windows",
            "stage: pr_fast",
            "stage: pr_test",
            "stage: pr_runtime_analysis",
            "stage: pr_mutants",
        ] {
            assert!(PR_STAGES.contains(needle), "PR stages missing '{needle}'");
        }
        for needle in ["stage: pr_slow\n", "stage: pr_slow1\n", "stage: pr_slow2\n", "stage: pr_slow3\n"] {
            assert!(
                !PR_STAGES.contains(needle),
                "Stale stage '{needle}' should be gone after the pr-slow rename"
            );
        }
        assert!(PR_STAGES.contains("stageDependencies.impact_linux.compute.outputs"));
        assert!(PR_STAGES.contains("stageDependencies.impact_windows.compute.outputs"));
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
