// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regions for the small shared-config files: `deny.toml`, `rustfmt.toml`,
//! `.delta.toml`, `spellcheck.toml`, `clippy.toml`.
//!
//! Each is a TOML file with a single `ox-check-*` region near the end.
//! The body content is small and opinionated; emptying the region is the
//! opt-out path.

use std::path::Path;

use ohno::AppError;

use super::managed_region::plan_managed_region;
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::CommentSyntax;

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

/// Repo-root-relative path of the `cargo-spellcheck` config.
pub const SPELLCHECK_PATH: &str = "spellcheck.toml";
/// Region id for the managed section of `spellcheck.toml`.
pub const SPELLCHECK_REGION_ID: &str = "ox-check-spellcheck";

/// Repo-root-relative path of the `clippy` lint-tuning config.
pub const CLIPPY_PATH: &str = "clippy.toml";
/// Region id for the managed section of `clippy.toml`.
pub const CLIPPY_REGION_ID: &str = "ox-check-clippy";

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

/// Embedded body of the spellcheck.toml managed region.
///
/// Defaults aligned with the `ox-check-spellcheck` recipe's behavior:
/// `hunspell` with `en_US`, project-local `target/spelling.dic` derived
/// from `.spelling`, no OS dictionary lookups (cross-platform
/// determinism), `CamelCase` concatenation enabled.
pub const SPELLCHECK_BODY: &str = include_str!("../../templates/regions/spellcheck.toml");

/// Embedded body of the clippy.toml managed region.
///
/// Fine-tuning settings (not lint levels — those live in Cargo.toml
/// `[workspace.lints]`) that pair with the catalog's enabled lints.
/// `allow-panic-in-tests` / `allow-unwrap-in-tests` are required
/// companions to `clippy.unwrap_used`; `semicolon-outside-block-
/// ignore-multiline` tames `clippy.semicolon_outside_block` for
/// common multiline-block style.
pub const CLIPPY_BODY: &str = include_str!("../../templates/regions/clippy.toml");

/// Plan all five shared-config regions.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_shared_configs(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_managed_region(repo_root, manifest, DENY_PATH, DENY_REGION_ID, DENY_BODY, CommentSyntax::Hash)?,
        plan_managed_region(
            repo_root,
            manifest,
            RUSTFMT_PATH,
            RUSTFMT_REGION_ID,
            RUSTFMT_BODY,
            CommentSyntax::Hash,
        )?,
        plan_managed_region(repo_root, manifest, DELTA_PATH, DELTA_REGION_ID, DELTA_BODY, CommentSyntax::Hash)?,
        plan_managed_region(
            repo_root,
            manifest,
            SPELLCHECK_PATH,
            SPELLCHECK_REGION_ID,
            SPELLCHECK_BODY,
            CommentSyntax::Hash,
        )?,
        plan_managed_region(repo_root, manifest, CLIPPY_PATH, CLIPPY_REGION_ID, CLIPPY_BODY, CommentSyntax::Hash)?,
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
        // Nightly-only opinions: import grouping + granularity, doc
        // comment code formatting. ox-check-fmt invokes nightly
        // rustfmt via the pinned `rust_nightly` so these never go
        // stale on a `rustup update`.
        assert!(RUSTFMT_BODY.contains("unstable_features = true"));
        assert!(RUSTFMT_BODY.contains("imports_granularity = \"Module\""));
        assert!(RUSTFMT_BODY.contains("group_imports = \"StdExternalCrate\""));
        assert!(RUSTFMT_BODY.contains("format_code_in_doc_comments = true"));
    }

    #[test]
    fn delta_body_has_root_files() {
        assert!(DELTA_BODY.contains("root-files"));
        assert!(DELTA_BODY.contains("Cargo.lock"));
    }

    #[test]
    fn spellcheck_body_configures_hunspell_with_extra_dictionary() {
        assert!(SPELLCHECK_BODY.contains("[Hunspell]"));
        assert!(SPELLCHECK_BODY.contains("lang = \"en_US\""));
        // Generated by ox-check-spellcheck from .spelling — the recipe
        // and the config must agree on this path.
        assert!(SPELLCHECK_BODY.contains("\"target/spelling.dic\""));
        // Required for cross-platform reproducibility (no system dict).
        assert!(SPELLCHECK_BODY.contains("skip_os_lookups = true"));
        assert!(SPELLCHECK_BODY.contains("use_builtin = true"));
        // CamelCase concatenation handling — major false-positive reducer
        // on Rust codebases.
        assert!(SPELLCHECK_BODY.contains("[Hunspell.quirks]"));
        assert!(SPELLCHECK_BODY.contains("allow_concatenation = true"));
    }

    #[test]
    fn clippy_body_carries_companion_tunings_for_catalog_lints() {
        // Required companions for clippy.unwrap_used — tests should be
        // free to unwrap/panic without the catalog lint firing.
        assert!(CLIPPY_BODY.contains("allow-panic-in-tests = true"));
        assert!(CLIPPY_BODY.contains("allow-unwrap-in-tests = true"));
        // Required tuning for clippy.semicolon_outside_block.
        assert!(CLIPPY_BODY.contains("semicolon-outside-block-ignore-multiline = true"));
        // Workspace-internal code: prefer the correct fix over
        // exported-API stability.
        assert!(CLIPPY_BODY.contains("avoid-breaking-exported-api = false"));
        // Path-length tuning shared across all four surveyed repos.
        assert!(CLIPPY_BODY.contains("absolute-paths-max-segments = 3"));
        // Aspirational: when wildcard_imports is flipped back to warn
        // (clippy bug #15036 fix), this tuning is already in place.
        assert!(CLIPPY_BODY.contains("warn-on-all-wildcard-imports = true"));
    }

    #[test]
    fn bodies_round_trip_through_toml_parser() {
        for (id, body) in [
            (DENY_REGION_ID, DENY_BODY),
            (RUSTFMT_REGION_ID, RUSTFMT_BODY),
            (DELTA_REGION_ID, DELTA_BODY),
            (SPELLCHECK_REGION_ID, SPELLCHECK_BODY),
            (CLIPPY_REGION_ID, CLIPPY_BODY),
        ] {
            let spliced = upsert_region("", id, body, CommentSyntax::Hash).unwrap();
            let _: toml_edit::DocumentMut = spliced
                .parse()
                .unwrap_or_else(|e| panic!("body for region '{id}' did not parse: {e}"));
        }
    }

    #[test]
    fn plan_shared_configs_emits_five_items() {
        let tmp = TempDir::new().unwrap();
        let items = plan_shared_configs(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 5);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }
}
