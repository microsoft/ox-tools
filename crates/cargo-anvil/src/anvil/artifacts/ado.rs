// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Azure DevOps Pipelines backend files: step templates, stages templates,
//! and root pipelines, each an owned file gated on [`Backend::Ado`].
//!
//! Holds embedded templates, per-group fan-out, and repository check hooks.
//! Use these artifacts when specializing the ADO portion of a downstream catalog;
//! the shared catalog engine retains ownership and update behavior.
//!
//! # Examples
//!
//! ```
//! use cargo_anvil::artifacts::ado;
//!
//! let hook = ado::before_checks().with_body("steps: []\n");
//! ```

use crate::anvil::artifacts::impact_mode;
use crate::backend::Backend;
use crate::catalog::Artifact;

/// Embedded body of the shared setup step template.
const SETUP_STEP: &str = include_str!("../../../templates/ado/steps/setup.yml");

/// Embedded body of the cargo-delta impact step template.
const IMPACT_STEP: &str = include_str!("../../../templates/ado/steps/impact.yml");

/// Embedded body of the dirty-file job wrapper.
const JOB_WRAPPER: &str = include_str!("../../../templates/ado/steps/job.yml");

/// Embedded default for repository check prerequisites.
const BEFORE_CHECKS: &str = include_str!("../../../templates/ado/hooks/before-checks.yml");

/// Embedded default for repository check finalization.
const AFTER_CHECKS: &str = include_str!("../../../templates/ado/hooks/after-checks.yml");

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
/// The former umbrella group is split so cloud workflow UIs identify the
/// category that failed while Just remains the source of recipe membership.
#[cfg(test)]
const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-msrv",
    "pr-runtime-analysis",
    "pr-fuzzing",
    "pr-mutants",
    "scheduled-test",
    "scheduled-advisories",
    "scheduled-runtime-analysis",
    "scheduled-exhaustive",
];

/// Embedded template for one per-group step. `__GROUP__` is substituted with
/// the group name at emit time.
const GROUP_STEP_TEMPLATE: &str = include_str!("../../../templates/ado/steps/group.yml");

/// Optional ADO title lookup inserted only into the `pr-fast` group.
const PR_TITLE_STEP_TEMPLATE: &str = include_str!("../../../templates/ado/steps/pr-title-step.yml");

/// Optional `PR_TITLE` environment entry inserted only into `pr-fast`.
const PR_TITLE_ENV_TEMPLATE: &str = include_str!("../../../templates/ado/steps/pr-title-env.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Placeholder token the per-group template uses for the impact-mode selection.
const IMPACT_MODE_PLACEHOLDER: &str = "__IMPACT_MODE__";

/// Placeholder token for the recipe invoked after setup.
const RECIPE_PLACEHOLDER: &str = "__RECIPE__";

/// Placeholder token for the optional PR-title lookup step.
const PR_TITLE_STEP_PLACEHOLDER: &str = "__PR_TITLE_STEP__";

/// Placeholder token for the optional `PR_TITLE` environment entry.
const PR_TITLE_ENV_PLACEHOLDER: &str = "__PR_TITLE_ENV__";

/// Render the step template for one group.
#[must_use]
fn render_group_step(group: &str) -> String {
    let mut rendered = GROUP_STEP_TEMPLATE.replace(GROUP_PLACEHOLDER, group);
    rendered = rendered.replace(IMPACT_MODE_PLACEHOLDER, impact_mode(group));
    let recipe = if group.starts_with("scheduled-") {
        format!("_anvil-{group}")
    } else {
        format!("anvil-{group}")
    };
    rendered = rendered.replace(RECIPE_PLACEHOLDER, &recipe);
    let (title_step, title_env) = if group == "pr-fast" {
        (PR_TITLE_STEP_TEMPLATE.trim_end(), PR_TITLE_ENV_TEMPLATE.trim_end())
    } else {
        ("", "")
    };
    rendered = rendered.replace(PR_TITLE_STEP_PLACEHOLDER, title_step);
    rendered = rendered.replace(PR_TITLE_ENV_PLACEHOLDER, title_env);
    rendered
}

/// Repo-root-relative path for one group's step template.
#[cfg(test)]
#[must_use]
fn group_step_path(group: &str) -> String {
    format!(".anvil/ado/steps/{group}.yml")
}

/// `.anvil/ado/steps/setup.yml`.
#[must_use]
pub fn setup_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/steps/setup.yml", SETUP_STEP)
}

/// `.anvil/ado/steps/impact.yml`.
#[must_use]
pub fn impact_step() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/steps/impact.yml", IMPACT_STEP)
}

/// `.anvil/ado/steps/job.yml` — the dirty-file job wrapper.
#[must_use]
pub fn job_wrapper() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/steps/job.yml", JOB_WRAPPER)
}

/// Repository check prerequisites, emitted at `.anvil/ado/hooks/before-checks.yml`.
///
/// The empty step template runs after default checkout and input-artifact
/// downloads, before Anvil setup. Repository edits follow the owned-file flow.
///
/// # Examples
///
/// ```
/// use cargo_anvil::Catalog;
/// use cargo_anvil::artifacts::ado;
///
/// let catalog = Catalog::anvil()
///     .into_builder()
///     .replace_artifact(ado::before_checks().with_body("steps: []\n"))
///     .build()
///     .expect("before_checks replaces an existing catalog artifact");
/// ```
#[must_use]
pub fn before_checks() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/hooks/before-checks.yml", BEFORE_CHECKS)
}

/// Repository check finalization, emitted at `.anvil/ado/hooks/after-checks.yml`.
///
/// Runs after supplied check steps and before artifact publication. Cleanup
/// tasks can opt into `always()`; the wrapper adds no success-only condition.
///
/// # Examples
///
/// ```
/// use cargo_anvil::artifacts::ado;
///
/// let hook = ado::after_checks()
///     .with_body("steps:\n  - script: echo finalizing\n    condition: always()\n");
/// ```
#[must_use]
pub fn after_checks() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/hooks/after-checks.yml", AFTER_CHECKS)
}

/// `.anvil/ado/pr.yml` — the PR-tier stages template.
#[must_use]
pub fn pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/pr.yml", PR_STAGES)
}

/// `.anvil/ado/scheduled.yml` — the scheduled-tier stages template.
#[must_use]
pub fn scheduled_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/scheduled.yml", SCHEDULED_STAGES)
}

/// `.anvil/ado/custom/pr-stages.yml` — the repo-owned extension point
/// for PR-tier stages.
///
/// Emitted once as an empty `stages: []` stub. The PR root pipeline
/// references it after the anvil stages, so an adopter can add their own
/// stages here without editing the anvil-owned root or stages template.
/// Once edited it follows the standard dirty-file flow (Propose, don't
/// overwrite), exactly like `steps/job.yml`.
#[must_use]
pub fn custom_pr_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/custom/pr-stages.yml", CUSTOM_PR_STAGES)
}

/// `.anvil/ado/custom/scheduled-stages.yml` — the repo-owned extension
/// point for scheduled-tier stages. See [`custom_pr_stages`].
#[must_use]
pub fn custom_scheduled_stages() -> Artifact {
    Artifact::backend_file(Backend::Ado, ".anvil/ado/custom/scheduled-stages.yml", CUSTOM_SCHEDULED_STAGES)
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
    ("pr-fast", ".anvil/ado/steps/pr-fast.yml"),
    ("pr-test", ".anvil/ado/steps/pr-test.yml"),
    ("pr-msrv", ".anvil/ado/steps/pr-msrv.yml"),
    ("pr-runtime-analysis", ".anvil/ado/steps/pr-runtime-analysis.yml"),
    ("pr-fuzzing", ".anvil/ado/steps/pr-fuzzing.yml"),
    ("pr-mutants", ".anvil/ado/steps/pr-mutants.yml"),
    ("scheduled-test", ".anvil/ado/steps/scheduled-test.yml"),
    ("scheduled-advisories", ".anvil/ado/steps/scheduled-advisories.yml"),
    ("scheduled-runtime-analysis", ".anvil/ado/steps/scheduled-runtime-analysis.yml"),
    ("scheduled-exhaustive", ".anvil/ado/steps/scheduled-exhaustive.yml"),
];

/// All ADO backend artifacts in emission order.
#[must_use]
pub(crate) fn all() -> Vec<Artifact> {
    let mut out = vec![setup_step(), impact_step(), job_wrapper(), before_checks(), after_checks()];
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
    fn fixed_artifacts_keep_their_emission_order() {
        let paths: Vec<_> = all()
            .into_iter()
            .map(|artifact| match artifact {
                Artifact::OwnedFile(spec) => spec.path,
                Artifact::OwnedFileSection(_) | Artifact::Region(_) => {
                    panic!("fixed ADO artifacts must be owned files")
                }
            })
            .collect();
        let expected: Vec<_> = [
            [
                ".anvil/ado/steps/setup.yml",
                ".anvil/ado/steps/impact.yml",
                ".anvil/ado/steps/job.yml",
                ".anvil/ado/hooks/before-checks.yml",
                ".anvil/ado/hooks/after-checks.yml",
            ]
            .as_slice(),
            &GROUP_STEPS.iter().map(|(_, path)| *path).collect::<Vec<_>>(),
            [
                ".anvil/ado/pr.yml",
                ".anvil/ado/scheduled.yml",
                ".anvil/ado/custom/pr-stages.yml",
                ".anvil/ado/custom/scheduled-stages.yml",
                ".pipelines/anvil-pr.yml",
                ".pipelines/anvil-scheduled.yml",
            ]
            .as_slice(),
        ]
        .concat();
        assert_eq!(paths, expected);
    }

    #[test]
    fn check_hooks_default_to_empty_steps_and_surround_check_work() {
        for artifact in [before_checks(), after_checks()] {
            let Artifact::OwnedFile(spec) = artifact else {
                panic!("check hooks must be owned files");
            };
            assert_eq!(spec.gate, Some(Backend::Ado));
            let lines: Vec<_> = spec
                .body
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
                .collect();
            assert_eq!(lines, ["steps: []"]);
        }
        let before = "      - template: ../hooks/before-checks.yml";
        let after = "      - template: ../hooks/after-checks.yml";
        for include in [before, after] {
            assert_eq!(JOB_WRAPPER.lines().filter(|line| *line == include).count(), 1);
        }
        let positions = [
            JOB_WRAPPER.find("task: DownloadPipelineArtifact@2").unwrap(),
            JOB_WRAPPER.find(before).unwrap(),
            JOB_WRAPPER.find("${{ each step in parameters.steps }}").unwrap(),
            JOB_WRAPPER.find(after).unwrap(),
            JOB_WRAPPER.find("${{ each artifact in parameters.artifacts }}").unwrap(),
        ];
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn group_step_passes_group_to_setup() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("template: setup.yml"));
        assert!(body.contains("group: pr-fast"));
    }

    #[test]
    fn render_pr_fast_group_shares_impact_and_resolves_title() {
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
        // is fixed by tier -- not probed at runtime from a marker file.
        assert!(body.contains(r#"ANVIL_IMPACT: "consume""#));
        assert!(
            !body.contains(r#"ANVIL_IMPACT: "off""#),
            "a PR group must not fall back to off (it always has the artifact)"
        );
        assert!(
            !body.contains("[ -f target/anvil/impact/impact.state ]"),
            "PR group must not gate its mode on a runtime marker-file probe"
        );
        // Only pr-fast needs the PR title, which ADO does not expose as a
        // predefined variable.
        assert!(body.contains("PR_TITLE: $(PR_TITLE)"));
        assert!(body.contains("setvariable variable=PR_TITLE"));
        assert!(body.contains("Invoke-RestMethod"));
        assert!(!body.contains("PR_TITLE: $(System.PullRequest.Title)"));
    }

    #[test]
    fn render_pr_msrv_group_has_no_title_api_or_token_dependency() {
        let body = render_group_step("pr-msrv");
        for needle in ["PR_TITLE", "Invoke-RestMethod", "SYSTEM_ACCESSTOKEN", "System.AccessToken"] {
            assert!(!body.contains(needle), "pr-msrv must not contain '{needle}'");
        }
    }

    #[test]
    fn scheduled_group_step_forces_impact_off_unconditionally() {
        // The scheduled tier always validates the full workspace. Its impact
        // mode is fixed by group class at emit time, never probed at runtime from
        // target/anvil/impact/impact.state, so no leftover state on the agent
        // can wrongly enable scoping and skip the full-workspace backstop.
        let body = render_group_step("scheduled-test");
        assert!(body.contains(r#"ANVIL_IMPACT: "off""#));
        assert!(body.contains("just _anvil-scheduled-test"));
        assert!(!body.contains("just anvil-scheduled-test"));
        assert!(
            !body.contains(r#"ANVIL_IMPACT: "consume""#),
            "scheduled group must never consume the impact cache"
        );
        assert!(
            !body.contains("[ -f target/anvil/impact/impact.state ]"),
            "scheduled group must not gate its mode on a runtime marker-file probe"
        );
    }

    #[test]
    fn group_step_path_is_under_anvil() {
        assert_eq!(group_step_path("scheduled-test"), ".anvil/ado/steps/scheduled-test.yml");
    }

    #[test]
    fn custom_stages_artifacts_are_under_anvil_ado() {
        match custom_pr_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".anvil/ado/custom/pr-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::OwnedFileSection(_) | Artifact::Region(_) => panic!("expected owned file"),
        }
        match custom_scheduled_stages() {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".anvil/ado/custom/scheduled-stages.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
            }
            Artifact::OwnedFileSection(_) | Artifact::Region(_) => panic!("expected owned file"),
        }
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
