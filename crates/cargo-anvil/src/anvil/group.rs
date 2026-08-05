// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structural classification of base-catalog artifacts into
//! [`ArtifactGroup`]s.
//!
//! The `[anvil] artifacts` allow-list (see [`crate::config`]) is expressed in
//! group names; [`group_of`] maps each concrete [`Artifact`] the base catalog
//! emits onto exactly one group. Membership is derived from the artifact's
//! *structure* rather than a hand-maintained list, so a newly-registered
//! artifact is grouped automatically:
//!
//! * container-gated artifacts → [`ArtifactGroup::Container`];
//! * backend-gated owned files → [`ArtifactGroup::Backends`];
//! * every other owned file (the `justfiles/anvil/` recipe tree) →
//!   [`ArtifactGroup::Recipes`];
//! * the `Justfile` imports region → [`ArtifactGroup::Recipes`] (the one
//!   region that belongs with the recipe tree it makes reachable);
//! * every other managed region (the user-config regions) →
//!   [`ArtifactGroup::Config`].
//!
//! Because [`group_of`] is total, every artifact belongs to exactly one group
//! — a future artifact can never be silently ungrouped.

use super::artifacts::justfile::JUSTFILE_REGION_ID;
use crate::catalog::{Artifact, Catalog};
use crate::config::ArtifactGroup;

/// The [`ArtifactGroup`] the given base-catalog artifact belongs to.
///
/// `catalog` is consulted only to learn whether the artifact is
/// container-gated (that fact lives beside the catalog, not on the artifact).
pub(crate) fn group_of(catalog: &Catalog, artifact: &Artifact) -> ArtifactGroup {
    if catalog.is_container_gated(&artifact.key()) {
        return ArtifactGroup::Container;
    }
    match artifact {
        // Owned files carrying a backend gate are the cloud-workflow CI files;
        // ungated owned files are the `justfiles/anvil/` recipe tree.
        Artifact::OwnedFile(spec) => {
            if spec.gate.is_some() {
                ArtifactGroup::Backends
            } else {
                ArtifactGroup::Recipes
            }
        }
        // The `Justfile` imports region belongs with the recipe tree it makes
        // reachable; every other region is a user-config region.
        Artifact::Region(spec) => {
            if spec.id.as_str() == JUSTFILE_REGION_ID {
                ArtifactGroup::Recipes
            } else {
                ArtifactGroup::Config
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::anvil::artifacts;

    /// Every base-catalog artifact is classified into exactly one group, and
    /// the classification matches the artifact's structure. Asserting the
    /// structural invariant per group means a future artifact cannot be
    /// silently mis-grouped: e.g. an ungated owned file placed outside
    /// `justfiles/anvil/` would trip the `Recipes` path assertion and force a
    /// deliberate grouping decision.
    #[test]
    fn every_artifact_is_grouped_exactly_once_by_structure() {
        let catalog = Catalog::anvil();
        let mut seen = std::collections::BTreeSet::new();
        for artifact in catalog.artifacts() {
            let group = group_of(&catalog, artifact);
            seen.insert(group);
            match group {
                ArtifactGroup::Container => {
                    assert!(
                        catalog.is_container_gated(&artifact.key()),
                        "Container group must be container-gated: {artifact:?}"
                    );
                }
                ArtifactGroup::Backends => match artifact {
                    Artifact::OwnedFile(spec) => assert!(
                        spec.gate.is_some(),
                        "Backends group owned files must carry a backend gate: {artifact:?}"
                    ),
                    Artifact::Region(_) => panic!("Backends group must be an owned file: {artifact:?}"),
                },
                ArtifactGroup::Recipes => match artifact {
                    Artifact::OwnedFile(spec) => assert!(
                        spec.path.starts_with("justfiles/anvil/"),
                        "Recipes owned files must live under justfiles/anvil/: {artifact:?}"
                    ),
                    Artifact::Region(spec) => assert_eq!(
                        spec.id.as_str(),
                        JUSTFILE_REGION_ID,
                        "the only Recipes region is the Justfile imports region: {artifact:?}"
                    ),
                },
                ArtifactGroup::Config => match artifact {
                    Artifact::Region(spec) => assert_ne!(
                        spec.id.as_str(),
                        JUSTFILE_REGION_ID,
                        "Config regions are the user-config regions, not the imports region: {artifact:?}"
                    ),
                    Artifact::OwnedFile(_) => panic!("Config group must be a managed region: {artifact:?}"),
                },
            }
        }
        // The base catalog exercises all four groups.
        assert_eq!(
            seen,
            ArtifactGroup::all_set(),
            "the base catalog must populate every artifact group"
        );
    }

    #[test]
    fn representative_artifacts_land_in_expected_groups() {
        let catalog = Catalog::anvil();
        let g = |a: &Artifact| group_of(&catalog, a);
        assert_eq!(g(&artifacts::justfile::entry()), ArtifactGroup::Recipes);
        assert_eq!(g(&artifacts::justfile::tiers()), ArtifactGroup::Recipes);
        assert_eq!(g(&artifacts::region::justfile_imports()), ArtifactGroup::Recipes);
        assert_eq!(g(&artifacts::region::rustfmt()), ArtifactGroup::Config);
        assert_eq!(g(&artifacts::region::workspace_lints()), ArtifactGroup::Config);
        assert_eq!(g(&artifacts::region::deny_bans()), ArtifactGroup::Config);
        assert_eq!(g(&artifacts::region::gitattributes()), ArtifactGroup::Config);
        assert_eq!(g(&artifacts::github::pr_root_workflow()), ArtifactGroup::Backends);
        assert_eq!(g(&artifacts::ado::pr_root_pipeline()), ArtifactGroup::Backends);
        assert_eq!(g(&artifacts::container::container_just()), ArtifactGroup::Container);
        assert_eq!(g(&artifacts::container::cluster_just()), ArtifactGroup::Container);
    }
}
