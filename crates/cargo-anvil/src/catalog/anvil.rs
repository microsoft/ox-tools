// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assembly of the built-in (`anvil`) catalog as data.
//!
//! [`anvil_artifacts`] returns the full base artifact set — the
//! `justfiles/anvil/` tree, the managed regions, and the gated backend
//! files for both `github` and `ado` — built from the public
//! [`crate::catalog::artifacts`] registry (one source of truth).
//!
//! Backend files for *both* backends are always present in the set; they
//! carry a `gate` and [`crate::run::build_plan`] emits each only when its
//! backend is in the resolved set.

use crate::catalog::artifact::Artifact;
use crate::catalog::artifacts;

/// The full built-in artifact set, in emission order.
#[must_use]
pub(crate) fn anvil_artifacts() -> Vec<Artifact> {
    // The justfiles/anvil/ owned-file tree, the Justfile imports region, the
    // Cargo.toml lint regions (build_plan reconciles the single-crate shape),
    // and the shared-config regions.
    let mut out = vec![
        artifacts::justfile::entry(),
        artifacts::justfile::tools(),
        artifacts::justfile::versions(),
        artifacts::justfile::checks(),
        artifacts::justfile::groups(),
        artifacts::justfile::tiers(),
        artifacts::region::justfile_imports(),
        artifacts::region::workspace_lints(),
        artifacts::region::member_lints(),
        artifacts::region::deny(),
        artifacts::region::rustfmt(),
        artifacts::region::delta(),
        artifacts::region::spellcheck(),
        artifacts::region::clippy(),
    ];

    // Backend files (gated); both backends present, filtered by gate at plan time.
    out.extend(artifacts::github::all());
    out.extend(artifacts::ado::all());

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;
    use crate::backend::Backend;
    use crate::catalog::artifact::{HostSelector, OwnedFileSpec, RegionSpec};
    use crate::emit::{ado, github, local, shared_configs};
    use crate::manifest::Manifest;
    use crate::plan::Target;

    /// Map every owned-file artifact to `(path -> (body, gate))`.
    fn owned_files(artifacts: &[Artifact]) -> BTreeMap<&str, (&str, Option<Backend>)> {
        artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::OwnedFile(OwnedFileSpec { path, body, gate }) => Some((*path, (body.as_str(), *gate))),
                Artifact::Region(_) => None,
            })
            .collect()
    }

    /// Map every region artifact to `((host, id) -> body)`.
    fn regions(artifacts: &[Artifact]) -> BTreeMap<(String, String), String> {
        artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::Region(RegionSpec { host, id, body, .. }) => {
                    let host_key = match host {
                        HostSelector::Path(p) => p.clone(),
                        HostSelector::EachMemberManifest => "<each-member>".to_owned(),
                    };
                    Some(((host_key, id.as_str().to_owned()), body.clone()))
                }
                Artifact::OwnedFile(_) => None,
            })
            .collect()
    }

    /// Render an emitter's plan items into `(path -> body)` on an empty repo.
    fn rendered_owned(items: &[crate::plan::PlanItem]) -> BTreeMap<String, String> {
        items
            .iter()
            .filter_map(|i| match &i.target {
                Target::File { path } => Some((path.clone(), i.rendered.clone().expect("Write item carries rendered"))),
                Target::Region { .. } => None,
            })
            .collect()
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn owned_files_match_legacy_emitters() {
        let tmp = TempDir::new().unwrap();
        let manifest = Manifest::default();
        let artifacts = anvil_artifacts();
        let catalog = owned_files(&artifacts);

        // justfiles/anvil/ tree.
        let local = rendered_owned(&local::plan_local_just_tree(tmp.path(), &manifest).unwrap());
        for (path, body) in &local {
            let (cat_body, gate) = catalog.get(path.as_str()).unwrap_or_else(|| panic!("catalog missing {path}"));
            assert_eq!(cat_body, body, "body mismatch for {path}");
            assert_eq!(*gate, None, "justfile tree must be ungated: {path}");
        }

        // GitHub backend files (gated on GitHub).
        let gh = rendered_owned(&github::plan_github_backend(tmp.path(), &manifest).unwrap());
        for (path, body) in &gh {
            let (cat_body, gate) = catalog.get(path.as_str()).unwrap_or_else(|| panic!("catalog missing {path}"));
            assert_eq!(cat_body, body, "github body mismatch for {path}");
            assert_eq!(*gate, Some(Backend::GitHub), "github file must be GitHub-gated: {path}");
        }

        // ADO backend files (gated on Ado).
        let az = rendered_owned(&ado::plan_ado_backend(tmp.path(), &manifest).unwrap());
        for (path, body) in &az {
            let (cat_body, gate) = catalog.get(path.as_str()).unwrap_or_else(|| panic!("catalog missing {path}"));
            assert_eq!(cat_body, body, "ado body mismatch for {path}");
            assert_eq!(*gate, Some(Backend::Ado), "ado file must be Ado-gated: {path}");
        }

        // The catalog has exactly these owned files and no more.
        let expected = local.len() + gh.len() + az.len();
        assert_eq!(
            catalog.len(),
            expected,
            "catalog owned-file count must equal the union of the emitters"
        );
    }

    #[test]
    fn fixed_path_regions_match_legacy_bodies() {
        let r = regions(&anvil_artifacts());

        let cases: &[(&str, &str, &str)] = &[
            (local::JUSTFILE_PATH, local::JUSTFILE_REGION_ID, local::JUSTFILE_IMPORTS_BODY),
            (shared_configs::DENY_PATH, shared_configs::DENY_REGION_ID, shared_configs::DENY_BODY),
            (
                shared_configs::RUSTFMT_PATH,
                shared_configs::RUSTFMT_REGION_ID,
                shared_configs::RUSTFMT_BODY,
            ),
            (
                shared_configs::DELTA_PATH,
                shared_configs::DELTA_REGION_ID,
                shared_configs::DELTA_BODY,
            ),
            (
                shared_configs::SPELLCHECK_PATH,
                shared_configs::SPELLCHECK_REGION_ID,
                shared_configs::SPELLCHECK_BODY,
            ),
            (
                shared_configs::CLIPPY_PATH,
                shared_configs::CLIPPY_REGION_ID,
                shared_configs::CLIPPY_BODY,
            ),
        ];
        for (host, id, body) in cases {
            let got = r
                .get(&((*host).to_owned(), (*id).to_owned()))
                .unwrap_or_else(|| panic!("catalog missing region {host}#{id}"));
            assert_eq!(got, body, "region body mismatch for {host}#{id}");
        }

        // Workspace lints region body.
        let ws = r
            .get(&(
                "Cargo.toml".to_owned(),
                crate::emit::cargo_toml::WORKSPACE_LINTS_REGION_ID.to_owned(),
            ))
            .expect("catalog missing workspace lints region");
        assert_eq!(*ws, crate::emit::cargo_toml::render_workspace_lints_body());

        // Member lints region: EachMemberManifest with the stub body.
        let member = r
            .get(&(
                "<each-member>".to_owned(),
                crate::emit::cargo_toml::CRATE_LINTS_REGION_ID.to_owned(),
            ))
            .expect("catalog missing member lints region");
        assert_eq!(*member, crate::emit::cargo_toml::MEMBER_LINTS_BODY);
    }
}
