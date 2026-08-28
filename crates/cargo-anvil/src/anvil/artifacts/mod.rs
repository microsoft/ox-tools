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
//! `anvil_artifacts` and downstream forks build on these functions, so the
//! template content and its identity live together with no separate
//! key/content split. See [`extensibility.md §4.1`](../../../docs/design/extensibility.md).

pub mod ado;
pub mod container;
pub mod github;
pub mod justfile;
pub mod region;

use crate::catalog::Artifact;

/// The impact-scoping mode a group's CI job runs under -- the word emitted into
/// `export ANVIL_IMPACT=<mode>` in the group's action/step template.
///
/// PR groups download the `target/anvil/impact` artifact and trust it verbatim
/// (`consume`); scheduled groups force `off` so every tier runs full-workspace.
/// This is the single source of truth for the tier-to-mode policy, shared by
/// both the GitHub and ADO backends (the per-backend `GROUPS` lists remain
/// render inventories, but the *policy* answer lives here once). The match is
/// exhaustive by group and panics on an unrecognized group, so adding a group
/// forces an explicit classification here rather than silently defaulting into
/// `consume` -- which, since `consume` now hard-errors on a missing cache
/// (impact.just), would fail the pipeline for an unclassified group instead of
/// scoping it; either way, a missing classification is caught up front, not silent.
#[must_use]
pub(crate) fn impact_mode(group: &str) -> &'static str {
    match group {
        "pr-fast" | "pr-test" | "pr-msrv" | "pr-runtime-analysis" | "pr-mutants" => "consume",
        "scheduled-test" | "scheduled-advisories" | "scheduled-runtime-analysis" | "scheduled-exhaustive" => "off",
        other => {
            panic!("impact_mode: unclassified group '{other}'; add it to the pr/scheduled arms in artifacts::impact_mode")
        }
    }
}

/// The full built-in artifact set, in emission order.
#[must_use]
pub(crate) fn anvil_artifacts() -> Vec<Artifact> {
    // The justfiles/anvil/ owned-file tree, the Justfile imports region, the
    // Cargo.toml lint regions (build_plan reconciles the single-crate shape),
    // and the shared-config regions.
    let mut out = vec![
        justfile::entry(),
        justfile::tools(),
        justfile::versions(),
        justfile::helpers(),
        justfile::impact(),
        justfile::runner(),
        justfile::tiers(),
        region::justfile_imports(),
        region::justfile_runner(),
        region::workspace_lints(),
        region::single_crate_lints(),
        region::member_lints(),
        region::deny_advisories(),
        region::deny_licenses(),
        region::deny_bans(),
        region::deny_sources(),
        region::rustfmt(),
        region::delta(),
        region::spellcheck(),
        region::clippy(),
        region::gitattributes(),
    ];

    // One owned file per check and per group (the split recipe tree).
    out.extend(justfile::check_files());
    out.extend(justfile::group_files());
    out.extend(container::all());

    // Backend files (gated); both backends present, filtered by gate at plan time.
    out.extend(github::all());
    out.extend(ado::all());

    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    #[test]
    fn impact_mode_classifies_pr_and_scheduled_groups() {
        assert_eq!(impact_mode("pr-fast"), "consume");
        assert_eq!(impact_mode("pr-msrv"), "consume");
        assert_eq!(impact_mode("pr-mutants"), "consume");
        assert_eq!(impact_mode("scheduled-test"), "off");
        assert_eq!(impact_mode("scheduled-exhaustive"), "off");
    }

    #[test]
    #[should_panic(expected = "unclassified group")]
    fn impact_mode_panics_on_an_unclassified_group() {
        let _ = impact_mode("mystery-group");
    }

    #[test]
    fn every_registry_entry_is_in_the_anvil_catalog() {
        let catalog = Catalog::anvil();
        let present = |artifact: &Artifact| catalog.artifacts().iter().any(|a| a == artifact);

        let singletons = [
            justfile::entry(),
            justfile::versions(),
            justfile::tools(),
            justfile::helpers(),
            justfile::impact(),
            justfile::runner(),
            justfile::tiers(),
            region::justfile_imports(),
            region::justfile_runner(),
            region::workspace_lints(),
            region::single_crate_lints(),
            region::member_lints(),
            region::deny_advisories(),
            region::deny_licenses(),
            region::deny_bans(),
            region::deny_sources(),
            region::rustfmt(),
            region::delta(),
            region::spellcheck(),
            region::clippy(),
            region::gitattributes(),
            github::setup_action(),
            github::just_problem_matcher(),
            github::run_group_action(),
            github::report_status_action(),
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
            ado::custom_pr_stages(),
            ado::custom_scheduled_stages(),
            ado::pr_root_pipeline(),
            ado::scheduled_root_pipeline(),
        ];
        for artifact in &singletons {
            assert!(present(artifact), "registry entry is not in Catalog::anvil(): {artifact:?}");
        }

        for artifact in github::all().iter().chain(ado::all().iter()) {
            assert!(present(artifact), "backend artifact is not in Catalog::anvil(): {artifact:?}");
        }

        for artifact in justfile::check_files().iter().chain(justfile::group_files().iter()) {
            assert!(present(artifact), "split recipe file is not in Catalog::anvil(): {artifact:?}");
        }
        for artifact in container::all() {
            assert!(present(&artifact), "container artifact is not in Catalog::anvil(): {artifact:?}");
        }
    }

    #[test]
    fn every_owned_file_identifies_cargo_anvil_as_its_generator() {
        for artifact in anvil_artifacts() {
            if let Artifact::OwnedFile(spec) = artifact {
                let generated_marker = spec.body.contains("GENERATED BY cargo-anvil. DO NOT EDIT DIRECTLY.");
                let customizable_wrapper =
                    spec.path == ".pipelines/anvil/steps/job.yml" && spec.body.contains("Default job wrapper emitted by cargo-anvil.");
                assert!(
                    generated_marker || customizable_wrapper,
                    "owned file '{}' lacks the generated-content marker",
                    spec.path
                );
            }
        }
    }

    #[test]
    fn justfiles_only_contains_just_recipes() {
        // `CatalogBuilder::build` enforces this for every derived catalog;
        // `Catalog::anvil` is assembled directly from parts, so assert the
        // built-in set satisfies the same invariant.
        for artifact in anvil_artifacts() {
            if let Artifact::OwnedFile(spec) = artifact
                && spec.path.starts_with("justfiles/")
            {
                assert!(
                    std::path::Path::new(spec.path).extension().and_then(|extension| extension.to_str()) == Some("just"),
                    "non-Just artifact must live outside justfiles/: {}",
                    spec.path
                );
            }
        }
        Catalog::anvil()
            .into_builder()
            .build()
            .expect("the built-in catalog must pass the builder's justfiles/ invariant");
    }
}
