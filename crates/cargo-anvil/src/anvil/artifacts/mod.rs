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
pub mod github;
pub mod justfile;
pub mod region;

use crate::catalog::Artifact;

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
        justfile::checks(),
        justfile::groups(),
        justfile::tiers(),
        region::justfile_imports(),
        region::workspace_lints(),
        region::single_crate_lints(),
        region::member_lints(),
        region::deny(),
        region::rustfmt(),
        region::delta(),
        region::spellcheck(),
        region::clippy(),
    ];

    // Backend files (gated); both backends present, filtered by gate at plan time.
    out.extend(github::all());
    out.extend(ado::all());

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    #[test]
    fn every_registry_entry_is_in_the_anvil_catalog() {
        let catalog = Catalog::anvil();
        let present = |artifact: &Artifact| catalog.artifacts().iter().any(|a| a == artifact);

        let singletons = [
            justfile::entry(),
            justfile::versions(),
            justfile::tools(),
            justfile::checks(),
            justfile::groups(),
            justfile::tiers(),
            region::justfile_imports(),
            region::workspace_lints(),
            region::single_crate_lints(),
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
            assert!(present(artifact), "registry entry is not in Catalog::anvil(): {artifact:?}");
        }

        for artifact in github::all().iter().chain(ado::all().iter()) {
            assert!(present(artifact), "backend artifact is not in Catalog::anvil(): {artifact:?}");
        }
    }
}
