// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The built-in (`anvil`) artifacts, exposed as a registry of functions.
//!
//! Each function returns the [`Artifact`] for one built-in catalog entry —
//! a `justfiles/anvil/` recipe file, a managed region, or a backend file.
//! A fork overrides a built-in by deriving from the corresponding function
//! via [`Artifact::with_body`], so the artifact's identity and gate are
//! preserved by construction:
//!
//! ```ignore
//! catalog.replace_artifact(artifacts::region::rustfmt().with_body(my_body));
//! ```
//!
//! This is the single source of truth for the base catalog: both
//! [`crate::catalog::anvil_artifacts`] and downstream forks build on these
//! functions, so there is no separate key/content split to keep in sync.
//!
//! See [`extensibility.md §4.1`](../../docs/design/extensibility.md).

use crate::backend::Backend;
use crate::catalog::artifact::{Artifact, HostSelector, RegionId, RegionSpec};
use crate::region::CommentSyntax;

/// The `justfiles/anvil/` recipe tree — every member is an owned `.just` file.
pub mod justfile {
    use super::Artifact;
    use crate::emit::local;

    /// `justfiles/anvil/mod.just` — the single-import entry point.
    #[must_use]
    pub fn entry() -> Artifact {
        Artifact::owned_file(local::MOD_JUST_PATH, local::MOD_JUST)
    }

    /// `justfiles/anvil/versions.just` — pinned toolchain versions.
    #[must_use]
    pub fn versions() -> Artifact {
        Artifact::owned_file(local::VERSIONS_JUST_PATH, local::VERSIONS_JUST)
    }

    /// `justfiles/anvil/tools.just` — tool install / prereq recipes.
    #[must_use]
    pub fn tools() -> Artifact {
        Artifact::owned_file(local::TOOLS_JUST_PATH, local::TOOLS_JUST)
    }

    /// `justfiles/anvil/checks.just` — the per-check recipes.
    #[must_use]
    pub fn checks() -> Artifact {
        Artifact::owned_file(local::CHECKS_JUST_PATH, local::CHECKS_JUST)
    }

    /// `justfiles/anvil/groups.just` — the group recipes.
    #[must_use]
    pub fn groups() -> Artifact {
        Artifact::owned_file(local::GROUPS_JUST_PATH, local::GROUPS_JUST)
    }

    /// `justfiles/anvil/tiers.just` — the tier aggregators.
    #[must_use]
    pub fn tiers() -> Artifact {
        Artifact::owned_file(local::TIERS_JUST_PATH, local::TIERS_JUST)
    }
}

/// Managed regions spliced into user-composed host files.
pub mod region {
    use super::{Artifact, CommentSyntax, HostSelector, RegionId, RegionSpec};
    use crate::emit::{cargo_toml, local, shared_configs};

    /// Build a single-path `Hash`-syntax region artifact.
    fn path_region(path: &str, id: &'static str, body: impl Into<String>) -> Artifact {
        Artifact::region(RegionSpec {
            host: HostSelector::Path(path.to_owned()),
            id: RegionId::new(id),
            body: body.into(),
            syntax: CommentSyntax::Hash,
        })
    }

    /// `Justfile` / `anvil-imports` — imports the `justfiles/anvil/` tree.
    #[must_use]
    pub fn justfile_imports() -> Artifact {
        path_region(local::JUSTFILE_PATH, local::JUSTFILE_REGION_ID, local::JUSTFILE_IMPORTS_BODY)
    }

    /// Root `Cargo.toml` / `anvil-workspace-lints` — the workspace-scope
    /// lint catalog (multi-crate workspaces only).
    #[must_use]
    pub fn workspace_lints() -> Artifact {
        path_region(
            "Cargo.toml",
            cargo_toml::WORKSPACE_LINTS_REGION_ID,
            cargo_toml::render_workspace_lints_body(),
        )
    }

    /// `<member>/Cargo.toml` / `anvil-lints` — the per-member
    /// `workspace = true` inheritance stub, replicated across every member.
    #[must_use]
    pub fn member_lints() -> Artifact {
        Artifact::member_region(RegionId::new(cargo_toml::CRATE_LINTS_REGION_ID), cargo_toml::MEMBER_LINTS_BODY)
    }

    /// `deny.toml` / `anvil-deny`.
    #[must_use]
    pub fn deny() -> Artifact {
        path_region(shared_configs::DENY_PATH, shared_configs::DENY_REGION_ID, shared_configs::DENY_BODY)
    }

    /// `rustfmt.toml` / `anvil-rustfmt`.
    #[must_use]
    pub fn rustfmt() -> Artifact {
        path_region(
            shared_configs::RUSTFMT_PATH,
            shared_configs::RUSTFMT_REGION_ID,
            shared_configs::RUSTFMT_BODY,
        )
    }

    /// `.delta.toml` / `anvil-delta`.
    #[must_use]
    pub fn delta() -> Artifact {
        path_region(
            shared_configs::DELTA_PATH,
            shared_configs::DELTA_REGION_ID,
            shared_configs::DELTA_BODY,
        )
    }

    /// `spellcheck.toml` / `anvil-spellcheck`.
    #[must_use]
    pub fn spellcheck() -> Artifact {
        path_region(
            shared_configs::SPELLCHECK_PATH,
            shared_configs::SPELLCHECK_REGION_ID,
            shared_configs::SPELLCHECK_BODY,
        )
    }

    /// `clippy.toml` / `anvil-clippy`.
    #[must_use]
    pub fn clippy() -> Artifact {
        path_region(
            shared_configs::CLIPPY_PATH,
            shared_configs::CLIPPY_REGION_ID,
            shared_configs::CLIPPY_BODY,
        )
    }
}

/// GitHub Actions backend files (owned files gated on [`Backend::GitHub`]).
pub mod github {
    use super::{Artifact, Backend};
    use crate::emit::github as gh;

    /// `.github/actions/anvil-setup/action.yml`.
    #[must_use]
    pub fn setup_action() -> Artifact {
        Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-setup/action.yml", gh::SETUP_ACTION)
    }

    /// `.github/actions/anvil-impact/action.yml`.
    #[must_use]
    pub fn impact_action() -> Artifact {
        Artifact::backend_file(Backend::GitHub, ".github/actions/anvil-impact/action.yml", gh::IMPACT_ACTION)
    }

    /// `.github/workflows/anvil-pr-impl.yml` — the PR reusable workflow.
    #[must_use]
    pub fn pr_impl_workflow() -> Artifact {
        Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-pr-impl.yml", gh::PR_IMPL_WORKFLOW)
    }

    /// `.github/workflows/anvil-scheduled-impl.yml` — the scheduled reusable workflow.
    #[must_use]
    pub fn scheduled_impl_workflow() -> Artifact {
        Artifact::backend_file(
            Backend::GitHub,
            ".github/workflows/anvil-scheduled-impl.yml",
            gh::SCHEDULED_IMPL_WORKFLOW,
        )
    }

    /// `.github/workflows/anvil-pr.yml` — the PR root workflow.
    #[must_use]
    pub fn pr_root_workflow() -> Artifact {
        Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-pr.yml", gh::PR_ROOT_WORKFLOW)
    }

    /// `.github/workflows/anvil-scheduled.yml` — the scheduled root workflow.
    #[must_use]
    pub fn scheduled_root_workflow() -> Artifact {
        Artifact::backend_file(
            Backend::GitHub,
            ".github/workflows/anvil-scheduled.yml",
            gh::SCHEDULED_ROOT_WORKFLOW,
        )
    }

    /// The per-group composite actions, one concrete owned file per group.
    ///
    /// Each `(group, path)` pair's `path` must equal
    /// [`gh::group_action_path`] for its group (asserted in tests); the
    /// body is [`gh::render_group_action`] expanded for that group.
    pub(crate) const GROUP_ACTIONS: &[(&str, &str)] = &[
        ("pr-fast", ".github/actions/anvil-pr-fast/action.yml"),
        ("pr-test", ".github/actions/anvil-pr-test/action.yml"),
        ("pr-runtime-analysis", ".github/actions/anvil-pr-runtime-analysis/action.yml"),
        ("pr-mutants", ".github/actions/anvil-pr-mutants/action.yml"),
        ("scheduled-test", ".github/actions/anvil-scheduled-test/action.yml"),
        ("scheduled-advisories", ".github/actions/anvil-scheduled-advisories/action.yml"),
        ("scheduled-exhaustive", ".github/actions/anvil-scheduled-exhaustive/action.yml"),
    ];

    /// All GitHub backend artifacts in emission order.
    #[must_use]
    pub(crate) fn all() -> Vec<Artifact> {
        let mut out = vec![setup_action(), impact_action()];
        for (group, path) in GROUP_ACTIONS {
            out.push(Artifact::backend_file(Backend::GitHub, path, gh::render_group_action(group)));
        }
        out.push(pr_impl_workflow());
        out.push(scheduled_impl_workflow());
        out.push(pr_root_workflow());
        out.push(scheduled_root_workflow());
        out
    }
}

/// Azure DevOps Pipelines backend files (owned files gated on [`Backend::Ado`]).
pub mod ado {
    use super::{Artifact, Backend};
    use crate::emit::ado as az;

    /// `.pipelines/anvil/steps/setup.yml`.
    #[must_use]
    pub fn setup_step() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/setup.yml", az::SETUP_STEP)
    }

    /// `.pipelines/anvil/steps/impact.yml`.
    #[must_use]
    pub fn impact_step() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/impact.yml", az::IMPACT_STEP)
    }

    /// `.pipelines/anvil/steps/advisory-comments.yml`.
    #[must_use]
    pub fn advisory_comments() -> Artifact {
        Artifact::backend_file(
            Backend::Ado,
            ".pipelines/anvil/steps/advisory-comments.yml",
            az::ADVISORY_COMMENTS_STEP,
        )
    }

    /// `.pipelines/anvil/steps/job.yml` — the dirty-file job wrapper.
    #[must_use]
    pub fn job_wrapper() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil/steps/job.yml", az::JOB_WRAPPER)
    }

    /// `.pipelines/anvil/pr.yml` — the PR-tier stages template.
    #[must_use]
    pub fn pr_stages() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil/pr.yml", az::PR_STAGES)
    }

    /// `.pipelines/anvil/scheduled.yml` — the scheduled-tier stages template.
    #[must_use]
    pub fn scheduled_stages() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil/scheduled.yml", az::SCHEDULED_STAGES)
    }

    /// `.pipelines/anvil-pr.yml` — the PR root pipeline.
    #[must_use]
    pub fn pr_root_pipeline() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil-pr.yml", az::PR_ROOT_PIPELINE)
    }

    /// `.pipelines/anvil-scheduled.yml` — the scheduled root pipeline.
    #[must_use]
    pub fn scheduled_root_pipeline() -> Artifact {
        Artifact::backend_file(Backend::Ado, ".pipelines/anvil-scheduled.yml", az::SCHEDULED_ROOT_PIPELINE)
    }

    /// The per-group step templates, one concrete owned file per group.
    ///
    /// Each `(group, path)` pair's `path` must equal [`az::group_step_path`]
    /// for its group (asserted in tests); the body is
    /// [`az::render_group_step`] expanded for that group.
    pub(crate) const GROUP_STEPS: &[(&str, &str)] = &[
        ("pr-fast", ".pipelines/anvil/steps/pr-fast.yml"),
        ("pr-test", ".pipelines/anvil/steps/pr-test.yml"),
        ("pr-runtime-analysis", ".pipelines/anvil/steps/pr-runtime-analysis.yml"),
        ("pr-mutants", ".pipelines/anvil/steps/pr-mutants.yml"),
        ("scheduled-test", ".pipelines/anvil/steps/scheduled-test.yml"),
        ("scheduled-advisories", ".pipelines/anvil/steps/scheduled-advisories.yml"),
        ("scheduled-exhaustive", ".pipelines/anvil/steps/scheduled-exhaustive.yml"),
    ];

    /// All ADO backend artifacts in emission order.
    #[must_use]
    pub(crate) fn all() -> Vec<Artifact> {
        let mut out = vec![setup_step(), impact_step(), advisory_comments(), job_wrapper()];
        for (group, path) in GROUP_STEPS {
            out.push(Artifact::backend_file(Backend::Ado, path, az::render_group_step(group)));
        }
        out.push(pr_stages());
        out.push(scheduled_stages());
        out.push(pr_root_pipeline());
        out.push(scheduled_root_pipeline());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::{ado as az, github as gh};

    #[test]
    fn github_group_action_paths_match_emitter() {
        assert_eq!(github::GROUP_ACTIONS.len(), gh::GROUPS.len());
        for ((group, path), expected_group) in github::GROUP_ACTIONS.iter().zip(gh::GROUPS) {
            assert_eq!(group, expected_group, "group order must match the emitter's GROUPS");
            assert_eq!(
                *path,
                gh::group_action_path(group),
                "registry path must match emitter path for {group}"
            );
        }
    }

    #[test]
    fn ado_group_step_paths_match_emitter() {
        assert_eq!(ado::GROUP_STEPS.len(), az::GROUPS.len());
        for ((group, path), expected_group) in ado::GROUP_STEPS.iter().zip(az::GROUPS) {
            assert_eq!(group, expected_group, "group order must match the emitter's GROUPS");
            assert_eq!(
                *path,
                az::group_step_path(group),
                "registry path must match emitter path for {group}"
            );
        }
    }

    #[test]
    fn every_registry_entry_is_in_the_anvil_catalog() {
        use crate::catalog::Catalog;

        let catalog = Catalog::anvil();
        let present = |artifact: &Artifact| catalog.artifacts().iter().any(|a| a.key() == artifact.key());

        let singletons = [
            justfile::entry(),
            justfile::versions(),
            justfile::tools(),
            justfile::checks(),
            justfile::groups(),
            justfile::tiers(),
            region::justfile_imports(),
            region::workspace_lints(),
            region::member_lints(),
            region::deny(),
            region::rustfmt(),
            region::delta(),
            region::spellcheck(),
            region::clippy(),
            github::setup_action(),
            github::impact_action(),
            github::pr_impl_workflow(),
            github::scheduled_impl_workflow(),
            github::pr_root_workflow(),
            github::scheduled_root_workflow(),
            ado::setup_step(),
            ado::impact_step(),
            ado::advisory_comments(),
            ado::job_wrapper(),
            ado::pr_stages(),
            ado::scheduled_stages(),
            ado::pr_root_pipeline(),
            ado::scheduled_root_pipeline(),
        ];
        for artifact in &singletons {
            assert!(present(artifact), "registry entry {:?} is not in Catalog::anvil()", artifact.key());
        }

        // Per-group backend files (exposed via the all() helpers).
        for artifact in github::all().iter().chain(ado::all().iter()) {
            assert!(
                present(artifact),
                "backend artifact {:?} is not in Catalog::anvil()",
                artifact.key()
            );
        }
    }
}
