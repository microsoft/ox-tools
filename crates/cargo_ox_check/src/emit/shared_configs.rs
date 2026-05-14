// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regions for the small shared-config files: `deny.toml`, `rustfmt.toml`,
//! `.delta.toml`.
//!
//! Each is a TOML file with a single `ox-check-*` region near the end.
//! The body content is small and opinionated; emptying the region is the
//! opt-out path.

use std::path::Path;

use anyhow::Result;

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

/// Render the body of the deny.toml managed region.
///
/// Opinionated baseline that covers the union of policies observed in
/// the surveyed repos: SPDX license allow-list rooted at the standard
/// permissive cluster + a denylist for advisories and yanked crates.
#[must_use]
pub fn render_deny_body() -> &'static str {
    r#"[advisories]
yanked = "deny"
unmaintained = "warn"

[licenses]
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "MPL-2.0",
    "Unicode-DFS-2016",
    "Unicode-3.0",
    "Zlib",
]
confidence-threshold = 0.93

[bans]
multiple-versions = "warn"
wildcards = "deny"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
"#
}

/// Render the body of the rustfmt.toml managed region.
///
/// The opinions here are deliberately minimal — the most contested
/// formatting choices are left at rustfmt defaults so adoption friction
/// is low. Users who want different formatting empty the region.
#[must_use]
pub fn render_rustfmt_body() -> &'static str {
    "edition = \"2024\"\n\
     max_width = 110\n\
     newline_style = \"Unix\"\n\
     use_field_init_shorthand = true\n\
     use_try_shorthand = true\n"
}

/// Render the body of the .delta.toml managed region.
///
/// Minimal cargo-delta configuration covering the impact-scoping inputs
/// used by the CI emitter. Most repos won't need to customize this.
#[must_use]
pub fn render_delta_body() -> &'static str {
    "[delta]\n\
     # Include the workspace root files that should invalidate every member's\n\
     # impact analysis when changed (lockfile, root manifest, toolchain).\n\
     root-files = [\n\
         \"Cargo.lock\",\n\
         \"Cargo.toml\",\n\
         \"rust-toolchain.toml\",\n\
     ]\n"
}

/// Plan all three shared-config regions.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_shared_configs(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>> {
    Ok(vec![
        plan_managed_region(
            repo_root,
            manifest,
            DENY_PATH,
            DENY_REGION_ID,
            render_deny_body(),
            CommentSyntax::Hash,
        )?,
        plan_managed_region(
            repo_root,
            manifest,
            RUSTFMT_PATH,
            RUSTFMT_REGION_ID,
            render_rustfmt_body(),
            CommentSyntax::Hash,
        )?,
        plan_managed_region(
            repo_root,
            manifest,
            DELTA_PATH,
            DELTA_REGION_ID,
            render_delta_body(),
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
        let b = render_deny_body();
        assert!(b.contains("[licenses]"));
        assert!(b.contains("\"MIT\""));
        assert!(b.contains("\"Apache-2.0\""));
        assert!(b.contains("[advisories]"));
        assert!(b.contains("yanked = \"deny\""));
    }

    #[test]
    fn rustfmt_body_sets_edition_and_width() {
        let b = render_rustfmt_body();
        assert!(b.contains("edition = \"2024\""));
        assert!(b.contains("max_width = 110"));
    }

    #[test]
    fn delta_body_has_root_files() {
        let b = render_delta_body();
        assert!(b.contains("root-files"));
        assert!(b.contains("Cargo.lock"));
    }

    #[test]
    fn bodies_round_trip_through_toml_parser() {
        // The spliced files (in a fresh repo, no host content) must parse
        // as valid TOML — the regions are toml-shaped after all.
        for (id, body) in [
            (DENY_REGION_ID, render_deny_body()),
            (RUSTFMT_REGION_ID, render_rustfmt_body()),
            (DELTA_REGION_ID, render_delta_body()),
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
