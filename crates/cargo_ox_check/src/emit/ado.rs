// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Azure DevOps Pipelines backend emitter.
//!
//! Emits three layers per [`ado.md`](../../../docs/design/ado.md):
//!
//! 1. Step templates under `.pipelines/ox-check/steps/*.yml`.
//! 2. Stages templates (`pr.yml`, `nightly.yml`).
//! 3. Root pipelines (`ox-check-pr.yml`, `ox-check-scheduled.yml`).

use std::path::Path;

use ohno::AppError;

use super::owned_file::plan_owned_file;
use crate::manifest::Manifest;
use crate::plan::PlanItem;

/// Embedded body of the shared setup step template.
pub const SETUP_STEP: &str = include_str!("../../templates/ado/steps/setup.yml");

/// Embedded body of the cargo-delta impact step template.
pub const IMPACT_STEP: &str = include_str!("../../templates/ado/steps/impact.yml");

/// Embedded body of the dirty-file job wrapper.
///
/// Every job in `pr.yml` / `nightly.yml` is rendered through this
/// wrapper; 1ESPT (and similar extension-template) users take ownership
/// of it to inject `templateContext:` blocks without forking the owned
/// stages templates. See [`ado.md §4`](../../../docs/design/ado.md#4-owned-stages-templates).
pub const JOB_WRAPPER: &str = include_str!("../../templates/ado/steps/job.yml");

/// Embedded body of the PR-tier stages template.
pub const PR_STAGES: &str = include_str!("../../templates/ado/pr-stages.yml");

/// Embedded body of the scheduled-tier stages template.
pub const SCHEDULED_STAGES: &str = include_str!("../../templates/ado/scheduled-stages.yml");

/// Embedded body of the PR root pipeline.
pub const PR_ROOT_PIPELINE: &str = include_str!("../../templates/ado/pr-root-pipeline.yml");

/// Embedded body of the scheduled root pipeline.
pub const SCHEDULED_ROOT_PIPELINE: &str = include_str!("../../templates/ado/scheduled-root-pipeline.yml");

/// All check groups that get a per-group step template.
pub const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-mutants",
    "scheduled-test",
    "scheduled-advisories",
    "scheduled-runtime",
    "scheduled-exhaustive",
];

/// Embedded template for one per-group step. `__GROUP__` is substituted
/// with the group name at emit time.
pub const GROUP_STEP_TEMPLATE: &str = include_str!("../../templates/ado/steps/group.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Render the step template for one group.
///
/// Substitutes the group name into [`GROUP_STEP_TEMPLATE`]. The
/// resulting template:
///
/// - Skips itself if the `skip` parameter is `'true'` (set from the
///   impact stage's `skip` output in the stages template).
/// - Sets `OX_CHECK_EXCLUDES` from the `excludes` parameter.
/// - Invokes `just ox-check-<group>` via bash.
#[must_use]
pub fn render_group_step(group: &str) -> String {
    GROUP_STEP_TEMPLATE.replace(GROUP_PLACEHOLDER, group)
}

/// Repo-root-relative path for one group's step template.
#[must_use]
pub fn group_step_path(group: &str) -> String {
    format!(".pipelines/ox-check/steps/{group}.yml")
}

/// Plan the two stages templates.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_stages_templates(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_owned_file(repo_root, manifest, ".pipelines/ox-check/pr.yml", PR_STAGES)?,
        plan_owned_file(repo_root, manifest, ".pipelines/ox-check/scheduled.yml", SCHEDULED_STAGES)?,
    ])
}

/// Plan the two root pipelines.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_root_pipelines(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_owned_file(repo_root, manifest, ".pipelines/ox-check-pr.yml", PR_ROOT_PIPELINE)?,
        plan_owned_file(repo_root, manifest, ".pipelines/ox-check-scheduled.yml", SCHEDULED_ROOT_PIPELINE)?,
    ])
}

/// Plan every file the ADO backend emits.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_ado_backend(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::new();
    items.extend(plan_step_templates(repo_root, manifest)?);
    items.extend(plan_stages_templates(repo_root, manifest)?);
    items.extend(plan_root_pipelines(repo_root, manifest)?);
    Ok(items)
}

/// Plan every step template: setup, impact, and the seven per-group steps.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_step_templates(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::with_capacity(GROUPS.len() + 3);
    items.push(plan_owned_file(
        repo_root,
        manifest,
        ".pipelines/ox-check/steps/setup.yml",
        SETUP_STEP,
    )?);
    items.push(plan_owned_file(
        repo_root,
        manifest,
        ".pipelines/ox-check/steps/impact.yml",
        IMPACT_STEP,
    )?);
    items.push(plan_owned_file(
        repo_root,
        manifest,
        ".pipelines/ox-check/steps/job.yml",
        JOB_WRAPPER,
    )?);
    for group in GROUPS {
        let body = render_group_step(group);
        items.push(plan_owned_file(repo_root, manifest, &group_step_path(group), &body)?);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;

    #[test]
    fn setup_and_impact_step_templates_are_non_empty() {
        assert!(SETUP_STEP.contains("just ox-check-tools-install"));
        assert!(IMPACT_STEP.contains("cargo-delta"));
        assert!(IMPACT_STEP.contains("##vso[task.setvariable"));
    }

    #[test]
    fn job_wrapper_declares_expected_contract() {
        // Contract is intentionally small and stable: name, pool, steps,
        // artifacts. Anything more elaborate is the user's responsibility
        // once they take ownership of the wrapper.
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
        assert!(body.contains("just ox-check-pr-fast"));
        assert!(body.contains("OX_CHECK_INCLUDE_MODIFIED"));
        assert!(body.contains("OX_CHECK_INCLUDE_AFFECTED"));
        assert!(body.contains("OX_CHECK_INCLUDE_REQUIRED"));
    }

    #[test]
    fn group_step_path_is_under_pipelines() {
        assert_eq!(group_step_path("scheduled-test"), ".pipelines/ox-check/steps/scheduled-test.yml");
    }

    #[test]
    fn pr_stages_has_impact_and_group_stages() {
        for needle in [
            "stage: impact_linux",
            "stage: impact_windows",
            "stage: pr_fast",
            "stage: pr_test",
            "stage: pr_mutants",
        ] {
            assert!(PR_STAGES.contains(needle), "PR stages missing '{needle}'");
        }
        assert!(PR_STAGES.contains("stageDependencies.impact_linux.compute.outputs"));
        assert!(PR_STAGES.contains("stageDependencies.impact_windows.compute.outputs"));
        // Every job is rendered through the dirty-file wrapper.
        assert!(PR_STAGES.contains("- template: steps/job.yml"));
        // No bare `- job:` keys — they must all go through the wrapper.
        assert!(
            !PR_STAGES.contains("\n      - job: "),
            "PR stages defines a bare `- job:` instead of going through steps/job.yml"
        );
    }

    #[test]
    fn scheduled_stages_has_four_groups() {
        for needle in [
            "stage: scheduled_test",
            "stage: scheduled_advisories",
            "stage: scheduled_runtime",
            "stage: scheduled_exhaustive",
        ] {
            assert!(SCHEDULED_STAGES.contains(needle), "scheduled stages missing '{needle}'");
        }
        // Scheduled tier publishes coverage via PublishCodeCoverageResults@2.
        assert!(SCHEDULED_STAGES.contains("PublishCodeCoverageResults@2"));
        // Every job is rendered through the dirty-file wrapper.
        assert!(SCHEDULED_STAGES.contains("- template: steps/job.yml"));
        assert!(
            !SCHEDULED_STAGES.contains("\n      - job: "),
            "Scheduled stages defines a bare `- job:` instead of going through steps/job.yml"
        );
    }

    #[test]
    fn plan_stages_templates_emits_two() {
        let tmp = TempDir::new().unwrap();
        let items = plan_stages_templates(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn plan_step_templates_emits_setup_impact_job_wrapper_plus_seven_groups() {
        let tmp = TempDir::new().unwrap();
        let items = plan_step_templates(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), GROUPS.len() + 3);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }
}
