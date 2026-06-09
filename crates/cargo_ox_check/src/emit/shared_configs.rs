// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regions for the small shared-config files: `deny.toml`, `rustfmt.toml`,
//! `.delta.toml`.
//!
//! Each is a TOML file with a single `ox-check-*` region near the end.
//! The body content is small and opinionated; emptying the region is the
//! opt-out path.

use std::path::Path;

use ohno::AppError;

use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::CommentSyntax;

use super::managed_region::plan_managed_region;

/// Repo-root-relative path of the `cargo-deny` config.
pub const DENY_PATH: &str = "deny.toml";
/// Region id for the managed section of `deny.toml`.
pub const DENY_REGION_ID: &str = "ox-check-deny";

/// Repo-root-relative path of the `rustfmt` config.
pub const RUSTFMT_PATH: &str = "rustfmt.toml";
/// Region id for the managed section of `rustfmt.toml`.
pub const RUSTFMT_REGION_ID: &str = "ox-check-rustfmt";

/// Repo-root-relative path of the `cargo-delta` config.
pub const DELTA_PATH: &str = ".delta.toml";
/// Region id for the managed section of `.delta.toml`.
pub const DELTA_REGION_ID: &str = "ox-check-delta";

/// Embedded body of the deny.toml managed region — a permissive license
/// allow-list, deny-yanked advisories, and a sources-allowlist baseline.
pub const DENY_BODY: &str = include_str!("../../templates/regions/deny.toml");

/// Embedded body of the rustfmt.toml managed region.
///
/// A minimal opinion set. Contested choices stay at rustfmt defaults to
/// keep adoption friction low; users who want different formatting
/// empty the region.
pub const RUSTFMT_BODY: &str = include_str!("../../templates/regions/rustfmt.toml");

/// Embedded body of the .delta.toml managed region.
///
/// Minimum cargo-delta config covering the impact-scoping inputs used
/// by the CI emitter.
pub const DELTA_BODY: &str = include_str!("../../templates/regions/delta.toml");

/// Plan all three shared-config regions.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_shared_configs(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_managed_region(
            repo_root,
            manifest,
            DENY_PATH,
            DENY_REGION_ID,
            DENY_BODY,
            CommentSyntax::Hash,
        )?,
        plan_managed_region(
            repo_root,
            manifest,
            RUSTFMT_PATH,
            RUSTFMT_REGION_ID,
            RUSTFMT_BODY,
            CommentSyntax::Hash,
        )?,
        plan_managed_region(
            repo_root,
            manifest,
            DELTA_PATH,
            DELTA_REGION_ID,
            DELTA_BODY,
            CommentSyntax::Hash,
        )?,
    ])
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;
    use crate::region::upsert_region;

    #[test]
    fn deny_body_includes_allowlist_and_advisories() {
        assert!(DENY_BODY.contains("[licenses]"));
        assert!(DENY_BODY.contains("\"MIT\""));
        assert!(DENY_BODY.contains("\"Apache-2.0\""));
        assert!(DENY_BODY.contains("[advisories]"));
        assert!(DENY_BODY.contains("yanked = \"deny\""));
    }

    #[test]
    fn rustfmt_body_sets_edition_and_width() {
        assert!(RUSTFMT_BODY.contains("edition = \"2024\""));
        // Catalog default is 140, matching the explicit choice in
        // oxidizer-github and ox-tools (the two surveyed repos that
        // opted to set a width). oxidizer/assistants-oxide/ox-docs
        // didn't set one, defaulting to rustfmt's 100; they can
        // override outside the managed region if they prefer that.
        assert!(RUSTFMT_BODY.contains("max_width = 140"));
    }

    #[test]
    fn delta_body_has_root_files() {
        assert!(DELTA_BODY.contains("root-files"));
        assert!(DELTA_BODY.contains("Cargo.lock"));
    }

    #[test]
    fn bodies_round_trip_through_toml_parser() {
        for (id, body) in [
            (DENY_REGION_ID, DENY_BODY),
            (RUSTFMT_REGION_ID, RUSTFMT_BODY),
            (DELTA_REGION_ID, DELTA_BODY),
        ] {
            let spliced = upsert_region("", id, body, CommentSyntax::Hash).unwrap();
            let _: toml_edit::DocumentMut = spliced
                .parse()
                .unwrap_or_else(|e| panic!("body for region '{id}' did not parse: {e}"));
        }
    }

    #[test]
    fn plan_shared_configs_emits_three_items() {
        let tmp = TempDir::new().unwrap();
        let items = plan_shared_configs(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 3);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }
}
