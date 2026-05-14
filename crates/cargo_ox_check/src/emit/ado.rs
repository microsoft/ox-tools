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

use anyhow::Result;

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

/// Render the step template for one group.
///
/// The step template:
///
/// - Skips itself if the `skip` parameter is `'true'` (set from the
///   impact stage's `skip` output in the stages template).
/// - Sets `OX_CHECK_EXCLUDES` from the `excludes` parameter.
/// - Invokes `just ox-check-<group>` via bash.
#[must_use]
pub fn render_group_step(group: &str) -> String {
    format!(
        "# Copyright (c) Microsoft Corporation.\n\
         # Licensed under the MIT License.\n\
         # Owned by cargo-ox-check; edit via `cargo ox-check update`.\n\
         parameters:\n  \
           - name: excludes\n    \
             type: string\n    \
             default: ''\n  \
           - name: skip\n    \
             type: string\n    \
             default: 'false'\n\
         steps:\n  \
           - template: setup.yml\n    \
             condition: ne(parameters.skip, 'true')\n  \
           - bash: just ox-check-{group}\n    \
             condition: ne(parameters.skip, 'true')\n    \
             displayName: ox-check-{group}\n    \
             env:\n      \
               OX_CHECK_EXCLUDES: ${{{{ parameters.excludes }}}}\n"
    )
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
) -> Result<Vec<PlanItem>> {
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

/// Plan every step template: setup, impact, and the seven per-group steps.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_step_templates(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
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
