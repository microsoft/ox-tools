// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Cargo.toml` lint-region emitters.
//!
//! The workspace `Cargo.toml` (or a single-crate `Cargo.toml`) carries the
//! catalog of `rust`/`clippy`/`rustdoc` lints inside a managed region.
//! Each workspace member also carries a tiny region asserting
//! `workspace = true` so the member inherits the catalog.
//!
//! All lints are emitted in *dotted-key form* (`clippy.unwrap_used =
//! "deny"`) — see [`design.md §6`](../../../docs/design/design.md) for the
//! rationale (TOML forbids re-declaring a table header, so dotted keys
//! let users extend the scope outside the sentinels).

use std::path::Path;

use ohno::AppError;

use super::managed_region::plan_managed_region;
use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::CommentSyntax;
use crate::workspace::Workspace;

/// Region id for the workspace-scope lints (multi-crate workspaces).
pub const WORKSPACE_LINTS_REGION_ID: &str = "anvil-workspace-lints";

/// Region id for crate-scope lints — used both for single-crate repos
/// (full catalog) and for each member of a multi-crate workspace
/// (just `workspace = true`).
pub const CRATE_LINTS_REGION_ID: &str = "anvil-lints";

/// Embedded body of the lint catalog, in dotted-key form (no table header).
/// The header (`[workspace.lints]` or `[lints]`) is prepended per host.
pub const LINTS_BODY: &str = include_str!("../../templates/regions/cargo-lints-body.toml");

/// Embedded body of a workspace-member lints region.
pub const MEMBER_LINTS_BODY: &str = include_str!("../../templates/regions/cargo-member-lints.toml");

/// Render the body of the workspace-scope lints region: `[workspace.lints]`
/// header followed by the embedded catalog.
#[must_use]
pub fn render_workspace_lints_body() -> String {
    let mut out = String::with_capacity(LINTS_BODY.len() + 32);
    out.push_str("[workspace.lints]\n");
    out.push_str(LINTS_BODY);
    out
}

/// Render the body of the single-crate lints region: `[lints]` header
/// followed by the embedded catalog.
#[must_use]
pub fn render_single_crate_lints_body() -> String {
    let mut out = String::with_capacity(LINTS_BODY.len() + 16);
    out.push_str("[lints]\n");
    out.push_str(LINTS_BODY);
    out
}

/// Emit lint-region plan items for every appropriate `Cargo.toml` in the
/// workspace.
///
/// - Multi-crate workspace: one workspace-scope region in the root
///   `Cargo.toml` + one member region per workspace member.
/// - Single-crate repo: one crate-scope region in the root `Cargo.toml`.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_cargo_lints(repo_root: &Path, workspace: &Workspace, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    let mut items = Vec::new();
    if workspace.has_workspace_table {
        let body = render_workspace_lints_body();
        items.push(plan_managed_region(
            repo_root,
            manifest,
            "Cargo.toml",
            WORKSPACE_LINTS_REGION_ID,
            &body,
            CommentSyntax::Hash,
        )?);
        for member in &workspace.members {
            items.push(plan_managed_region(
                repo_root,
                manifest,
                &member.manifest_relpath,
                CRATE_LINTS_REGION_ID,
                MEMBER_LINTS_BODY,
                CommentSyntax::Hash,
            )?);
        }
    } else {
        let body = render_single_crate_lints_body();
        items.push(plan_managed_region(
            repo_root,
            manifest,
            "Cargo.toml",
            CRATE_LINTS_REGION_ID,
            &body,
            CommentSyntax::Hash,
        )?);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;
    use crate::workspace::WorkspaceMember;

    fn write(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn embedded_catalog_uses_dotted_keys() {
        // The catalog body has no TOML table headers — only dotted keys.
        // (A reference to "[workspace.lints]" can appear in a comment.)
        for line in LINTS_BODY.lines() {
            let trimmed = line.trim_start();
            assert!(
                !trimmed.starts_with('['),
                "unexpected table header in cargo-lints-body.toml: {line}"
            );
        }
        assert!(LINTS_BODY.contains("rust.unsafe_op_in_unsafe_fn = \"warn\""));
        assert!(LINTS_BODY.contains("clippy.unwrap_used = \"warn\""));
    }

    /// Locks in the deliberate decisions to omit these from the catalog:
    /// `missing_docs` (large-workspace noise), `expect_used` and
    /// `panic` (over-strict for tools/libraries with legitimate panic
    /// paths). Adopters who want any of them add them outside the
    /// managed region with no conflict. If we change our mind, this
    /// test goes; until then, accidentally re-adding any of them
    /// fires here instead of in the next adopter's cloud workflows.
    #[test]
    fn catalog_intentionally_omits_contested_lints() {
        for needle in ["rust.missing_docs", "clippy.expect_used", "clippy.panic "] {
            assert!(
                !LINTS_BODY.contains(needle),
                "catalog now contains '{needle}'; if intentional, update the catalog-omission test"
            );
        }
    }

    /// Pins the Bucket A folding (restriction-group consensus from
    /// oxidizer + oxidizer-github). If any of these get dropped from
    /// the catalog the test fires; if a maintainer intends to drop
    /// them they update this list too.
    #[test]
    fn catalog_includes_consensus_restriction_lints() {
        for needle in [
            "clippy.as_pointer_underscore = \"warn\"",
            "clippy.assertions_on_result_states = \"warn\"",
            "clippy.deref_by_slicing = \"warn\"",
            "clippy.empty_drop = \"warn\"",
            "clippy.empty_enum_variants_with_brackets = \"warn\"",
            "clippy.fn_to_numeric_cast_any = \"warn\"",
            "clippy.if_then_some_else_none = \"warn\"",
            "clippy.multiple_unsafe_ops_per_block = \"warn\"",
            "clippy.redundant_type_annotations = \"warn\"",
            "clippy.renamed_function_params = \"warn\"",
            "clippy.semicolon_outside_block = \"warn\"",
            "clippy.unnecessary_safety_doc = \"warn\"",
            "clippy.unneeded_field_pattern = \"warn\"",
            "clippy.unused_result_ok = \"warn\"",
            "clippy.redundant_pub_crate = \"allow\"",
            "clippy.should_panic_without_expect = \"allow\"",
        ] {
            assert!(LINTS_BODY.contains(needle), "catalog missing consensus lint '{needle}'");
        }
    }

    /// `unexpected_cfgs` is on-by-default at warn since Rust 1.80;
    /// combined with the catalog's `-D warnings` cloud-workflow policy, an
    /// undeclared cfg is a hard build failure. The catalog pre-declares
    /// the `coverage`/`coverage_nightly` cfgs that anvil's own
    /// `llvm-cov` recipe sets so the recommended
    /// `#[cfg_attr(coverage_nightly, coverage(off))]` pattern works
    /// out of the box. If this line moves out of the catalog,
    /// adopters using that pattern silently break.
    #[test]
    fn catalog_declares_llvm_cov_cfgs_for_unexpected_cfgs_lint() {
        assert!(
            LINTS_BODY.contains("rust.unexpected_cfgs"),
            "catalog must declare rust.unexpected_cfgs to pre-allow llvm-cov's coverage cfgs"
        );
        assert!(
            LINTS_BODY.contains("'cfg(coverage,coverage_nightly)'"),
            "catalog's unexpected_cfgs check-cfg list must include coverage,coverage_nightly"
        );
    }

    #[test]
    fn workspace_body_prepends_workspace_lints_header() {
        let body = render_workspace_lints_body();
        assert!(body.starts_with("[workspace.lints]\n"));
        assert!(body.contains("clippy.pedantic = { level = \"warn\", priority = -1 }"));
    }

    #[test]
    fn single_crate_body_prepends_lints_header() {
        let body = render_single_crate_lints_body();
        assert!(body.starts_with("[lints]\n"));
        assert!(body.contains("clippy.unwrap_used = \"warn\""));
        // No second header would appear (no `[workspace.lints]` line).
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                assert_eq!(trimmed, "[lints]", "unexpected table header in single-crate body: {line}");
            }
        }
    }

    #[test]
    fn member_body_is_workspace_inheritance_stub() {
        assert!(MEMBER_LINTS_BODY.contains("[lints]"));
        assert!(MEMBER_LINTS_BODY.contains("workspace = true"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_multi_crate_workspace_emits_root_plus_members() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n");
        write(&root.join("crates/a/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");
        write(&root.join("crates/b/Cargo.toml"), "[package]\nname='b'\nversion='0.1.0'\n");

        let ws = crate::workspace::load_workspace(root).unwrap();
        let items = plan_cargo_lints(root, &ws, &Manifest::default()).unwrap();
        assert_eq!(items.len(), 3);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn plan_single_crate_emits_one_region() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[package]\nname='solo'\nversion='0.1.0'\n");

        let ws = crate::workspace::load_workspace(root).unwrap();
        let items = plan_cargo_lints(root, &ws, &Manifest::default()).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn dotted_key_body_parses_as_valid_toml_when_appended_to_workspace() {
        let host = "[workspace]\nmembers = [\"crates/a\"]\n";
        let region_body = render_workspace_lints_body();
        let spliced = crate::region::upsert_region(host, WORKSPACE_LINTS_REGION_ID, &region_body, CommentSyntax::Hash).unwrap();
        let _: toml_edit::DocumentMut = spliced.parse().expect("spliced TOML must be valid");
    }

    #[test]
    fn user_extension_after_region_parses() {
        let host = "[workspace]\nmembers = [\"x\"]\n";
        let region_body = render_workspace_lints_body();
        let mut spliced = crate::region::upsert_region(host, WORKSPACE_LINTS_REGION_ID, &region_body, CommentSyntax::Hash).unwrap();
        spliced.push_str("clippy.todo = \"warn\"\n");
        let _: toml_edit::DocumentMut = spliced.parse().expect("user extension keeps document valid");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn member_relpaths_use_forward_slashes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\"]\n");
        write(&root.join("crates/a/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");

        let ws = crate::workspace::load_workspace(root).unwrap();
        let items = plan_cargo_lints(root, &ws, &Manifest::default()).unwrap();
        let member_targets: Vec<_> = items
            .iter()
            .filter_map(|i| match &i.target {
                crate::plan::Target::Region { host, .. } if host != "Cargo.toml" => Some(host.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(member_targets, vec!["crates/a/Cargo.toml"]);
        let _ = std::any::type_name::<WorkspaceMember>();
    }
}
