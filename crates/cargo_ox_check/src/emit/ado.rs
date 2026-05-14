// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Azure DevOps Pipelines backend emitter.
//!
//! Emits three layers per [ado.md](../../../docs/design/ado.md):
//!
//! 1. Step templates under `.pipelines/ox-check/steps/*.yml`.
//! 2. Stages templates (`pr.yml`, `nightly.yml`).
//! 3. Root pipelines (`ox-check-pr.yml`, `ox-check-nightly.yml`).

use std::path::Path;

use ohno::AppError;

use crate::manifest::Manifest;
use crate::plan::PlanItem;

use super::owned_file::plan_owned_file;

/// Embedded body of the shared setup step template.
pub const SETUP_STEP: &str = include_str!("../../templates/ado/steps/setup.yml");

/// Embedded body of the cargo-delta impact step template.
pub const IMPACT_STEP: &str = include_str!("../../templates/ado/steps/impact.yml");

/// Embedded body of the PR-tier stages template.
pub const PR_STAGES: &str = include_str!("../../templates/ado/pr-stages.yml");

/// Embedded body of the nightly-tier stages template.
pub const NIGHTLY_STAGES: &str =
    include_str!("../../templates/ado/nightly-stages.yml");

/// Embedded body of the PR root pipeline.
pub const PR_ROOT_PIPELINE: &str =
    include_str!("../../templates/ado/pr-root-pipeline.yml");

/// Embedded body of the nightly root pipeline.
pub const NIGHTLY_ROOT_PIPELINE: &str =
    include_str!("../../templates/ado/nightly-root-pipeline.yml");

/// All check groups that get a per-group step template.
pub const GROUPS: &[&str] = &[
    "pr-fast",
    "pr-test",
    "pr-mutants",
    "nightly-test",
    "nightly-advisories",
    "nightly-runtime",
    "nightly-exhaustive",
];

/// Embedded template for one per-group step. `__GROUP__` is substituted
/// with the group name at emit time.
pub const GROUP_STEP_TEMPLATE: &str =
    include_str!("../../templates/ado/steps/group.yml");

/// Placeholder token the per-group template uses for the group name.
const GROUP_PLACEHOLDER: &str = "__GROUP__";

/// Placeholder token in root pipelines for the repo's default branch.
const DEFAULT_BRANCH_PLACEHOLDER: &str = "__DEFAULT_BRANCH__";

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
pub fn plan_stages_templates(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_owned_file(
            repo_root,
            manifest,
            ".pipelines/ox-check/pr.yml",
            PR_STAGES,
        )?,
        plan_owned_file(
            repo_root,
            manifest,
            ".pipelines/ox-check/nightly.yml",
            NIGHTLY_STAGES,
        )?,
    ])
}

/// Plan the two root pipelines.
///
/// `default_branch` is substituted into the PR pipeline's `branches.include:`
/// list and the nightly pipeline's schedule `branches.include:` list.
///
/// # Errors
///
/// Propagates I/O errors from the owned-file driver.
pub fn plan_root_pipelines(
    repo_root: &Path,
    manifest: &Manifest,
    default_branch: &str,
) -> Result<Vec<PlanItem>, AppError> {
    let pr = PR_ROOT_PIPELINE.replace(DEFAULT_BRANCH_PLACEHOLDER, default_branch);
    let nightly = NIGHTLY_ROOT_PIPELINE.replace(DEFAULT_BRANCH_PLACEHOLDER, default_branch);
    Ok(vec![
        plan_owned_file(repo_root, manifest, ".pipelines/ox-check-pr.yml", &pr)?,
        plan_owned_file(
            repo_root,
            manifest,
            ".pipelines/ox-check-nightly.yml",
            &nightly,
        )?,
    ])
}

/// Plan every file the ADO backend emits.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_ado_backend(
    repo_root: &Path,
    manifest: &Manifest,
    default_branch: &str,
) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::new();
    items.extend(plan_step_templates(repo_root, manifest)?);
    items.extend(plan_stages_templates(repo_root, manifest)?);
    items.extend(plan_root_pipelines(repo_root, manifest, default_branch)?);
    Ok(items)
}

/// Plan every step template: setup, impact, and the seven per-group steps.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_step_templates(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::with_capacity(GROUPS.len() + 2);
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
    for group in GROUPS {
        let body = render_group_step(group);
        items.push(plan_owned_file(
            repo_root,
            manifest,
            &group_step_path(group),
            &body,
        )?);
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
    fn render_group_step_has_skip_and_excludes() {
        let body = render_group_step("pr-fast");
        assert!(body.contains("parameters:"));
        assert!(body.contains("name: skip"));
        assert!(body.contains("name: excludes"));
        assert!(body.contains("just ox-check-pr-fast"));
        assert!(body.contains("ne(parameters.skip, 'true')"));
        assert!(body.contains("OX_CHECK_EXCLUDES"));
    }

    #[test]
    fn group_step_path_is_under_pipelines() {
        assert_eq!(
            group_step_path("nightly-test"),
            ".pipelines/ox-check/steps/nightly-test.yml"
        );
    }

    #[test]
    fn pr_stages_has_impact_and_group_stages() {
        for needle in ["stage: impact", "stage: pr_fast", "stage: pr_test", "stage: pr_mutants"] {
            assert!(PR_STAGES.contains(needle), "PR stages missing '{needle}'");
        }
        assert!(PR_STAGES.contains("stageDependencies.impact.compute.outputs"));
    }

    #[test]
    fn nightly_stages_has_four_groups() {
        for needle in [
            "stage: nightly_test",
            "stage: nightly_advisories",
            "stage: nightly_runtime",
            "stage: nightly_exhaustive",
        ] {
            assert!(NIGHTLY_STAGES.contains(needle), "nightly stages missing '{needle}'");
        }
        assert!(NIGHTLY_STAGES.contains("artifact: nightly-coverage-lcov"));
    }

    #[test]
    fn plan_stages_templates_emits_two() {
        let tmp = TempDir::new().unwrap();
        let items = plan_stages_templates(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn plan_step_templates_emits_setup_impact_plus_seven_groups() {
        let tmp = TempDir::new().unwrap();
        let items = plan_step_templates(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), GROUPS.len() + 2);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }
}
