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
//! "deny"`) — see [design.md §6](../../../docs/design/design.md) for the
//! rationale (TOML forbids re-declaring a table header, so dotted keys
//! let users extend the scope outside the sentinels).

use std::path::Path;

use anyhow::Result;

use crate::manifest::Manifest;
use crate::plan::PlanItem;
use crate::region::CommentSyntax;
use crate::workspace::Workspace;

use super::managed_region::plan_managed_region;

/// Region id for the workspace-scope lints (multi-crate workspaces).
pub const WORKSPACE_LINTS_REGION_ID: &str = "ox-check-workspace-lints";

/// Region id for crate-scope lints — used both for single-crate repos
/// (full catalog) and for each member of a multi-crate workspace
/// (just `workspace = true`).
pub const CRATE_LINTS_REGION_ID: &str = "ox-check-lints";

/// Render the body of the workspace-scope lints region.
///
/// Emits a single `[workspace.lints]` table populated with the catalog
/// in dotted-key form.
#[must_use]
pub fn render_workspace_lints_body() -> String {
    let mut out = String::new();
    out.push_str("[workspace.lints]\n");
    push_catalog_lints(&mut out, "rust", RUST_LINTS);
    push_catalog_lints(&mut out, "rustdoc", RUSTDOC_LINTS);
    push_catalog_lints(&mut out, "clippy", CLIPPY_LINTS);
    out
}

/// Render the body of the single-crate lints region. Same catalog as
/// [`render_workspace_lints_body`] but rooted at `[lints]`.
#[must_use]
pub fn render_single_crate_lints_body() -> String {
    let mut out = String::new();
    out.push_str("[lints]\n");
    push_catalog_lints(&mut out, "rust", RUST_LINTS);
    push_catalog_lints(&mut out, "rustdoc", RUSTDOC_LINTS);
    push_catalog_lints(&mut out, "clippy", CLIPPY_LINTS);
    out
}

/// Render the body of a workspace-member lints region.
#[must_use]
pub fn render_member_lints_body() -> &'static str {
    "[lints]\nworkspace = true\n"
}

fn push_catalog_lints(out: &mut String, group: &str, lints: &[(&str, &str)]) {
    for (name, value) in lints {
        out.push_str(group);
        out.push('.');
        out.push_str(name);
        out.push_str(" = ");
        out.push_str(value);
        out.push('\n');
    }
}

const RUST_LINTS: &[(&str, &str)] = &[
    ("ambiguous_negative_literals", "\"warn\""),
    ("missing_debug_implementations", "\"warn\""),
    ("missing_docs", "\"warn\""),
    ("redundant_imports", "\"warn\""),
    ("redundant_lifetimes", "\"warn\""),
    ("trivial_numeric_casts", "\"warn\""),
    ("unsafe_op_in_unsafe_fn", "\"warn\""),
    ("unused_lifetimes", "\"warn\""),
];

const RUSTDOC_LINTS: &[(&str, &str)] = &[
    ("missing_crate_level_docs", "\"warn\""),
    ("unescaped_backticks", "\"warn\""),
];

const CLIPPY_LINTS: &[(&str, &str)] = &[
    // Category gates (priority -1 so per-lint allows below override them).
    ("cargo", "{ level = \"warn\", priority = -1 }"),
    ("complexity", "{ level = \"warn\", priority = -1 }"),
    ("correctness", "{ level = \"warn\", priority = -1 }"),
    ("nursery", "{ level = \"warn\", priority = -1 }"),
    ("pedantic", "{ level = \"warn\", priority = -1 }"),
    ("perf", "{ level = \"warn\", priority = -1 }"),
    ("style", "{ level = \"warn\", priority = -1 }"),
    ("suspicious", "{ level = \"warn\", priority = -1 }"),
    // Per-lint opinions.
    ("allow_attributes", "\"warn\""),
    ("allow_attributes_without_reason", "\"warn\""),
    ("clone_on_ref_ptr", "\"warn\""),
    ("disallowed_script_idents", "\"warn\""),
    ("map_err_ignore", "\"warn\""),
    ("panic", "\"warn\""),
    ("undocumented_unsafe_blocks", "\"warn\""),
    ("unnecessary_safety_comment", "\"warn\""),
    ("unwrap_used", "\"warn\""),
    // Allows that override category defaults that misfire too often.
    ("option_if_let_else", "\"allow\""),
    ("missing_const_for_fn", "\"allow\""),
    ("multiple_crate_versions", "\"allow\""),
    ("significant_drop_tightening", "\"allow\""),
    ("wildcard_imports", "\"allow\""),
];

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
pub fn plan_cargo_lints(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
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
                render_member_lints_body(),
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
    fn workspace_body_uses_dotted_keys() {
        let body = render_workspace_lints_body();
        assert!(body.starts_with("[workspace.lints]\n"));
        assert!(body.contains("rust.unsafe_op_in_unsafe_fn = \"warn\""));
        assert!(body.contains("clippy.unwrap_used = \"warn\""));
        // No bracketed sub-tables.
        assert!(!body.contains("[workspace.lints.rust]"));
        assert!(!body.contains("[workspace.lints.clippy]"));
        assert!(!body.contains("[workspace.lints.rustdoc]"));
    }

    #[test]
    fn workspace_body_inline_tables_for_categories() {
        let body = render_workspace_lints_body();
        assert!(body.contains("clippy.pedantic = { level = \"warn\", priority = -1 }"));
    }

    #[test]
    fn single_crate_body_uses_plain_lints_table() {
        let body = render_single_crate_lints_body();
        assert!(body.starts_with("[lints]\n"));
        assert!(body.contains("clippy.unwrap_used = \"warn\""));
        assert!(!body.contains("[workspace.lints]"));
    }

    #[test]
    fn member_body_is_workspace_inheritance_stub() {
        assert_eq!(render_member_lints_body(), "[lints]\nworkspace = true\n");
    }

    #[test]
    fn plan_multi_crate_workspace_emits_root_plus_members() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
        );
        write(&root.join("crates/a/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");
        write(&root.join("crates/b/Cargo.toml"), "[package]\nname='b'\nversion='0.1.0'\n");

        let ws = crate::workspace::load_workspace(root).unwrap();
        let items = plan_cargo_lints(root, &ws, &Manifest::default()).unwrap();
        assert_eq!(items.len(), 3); // 1 workspace region + 2 member regions
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

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
        // Sanity-check: splice the rendered body into a real Cargo.toml
        // and ensure the result still parses.
        let host = "[workspace]\nmembers = [\"crates/a\"]\n";
        let region_body = render_workspace_lints_body();
        let spliced = crate::region::upsert_region(
            host,
            WORKSPACE_LINTS_REGION_ID,
            &region_body,
            CommentSyntax::Hash,
        )
        .unwrap();
        let _: toml_edit::DocumentMut = spliced.parse().expect("spliced TOML must be valid");
    }

    #[test]
    fn user_extension_after_region_parses() {
        let host = "[workspace]\nmembers = [\"x\"]\n";
        let region_body = render_workspace_lints_body();
        let mut spliced = crate::region::upsert_region(
            host,
            WORKSPACE_LINTS_REGION_ID,
            &region_body,
            CommentSyntax::Hash,
        )
        .unwrap();
        // User adds another lint in the same scope right after the sentinel.
        spliced.push_str("clippy.todo = \"warn\"\n");
        let _: toml_edit::DocumentMut =
            spliced.parse().expect("user extension keeps document valid");
    }

    #[test]
    fn member_relpaths_use_forward_slashes() {
        // The Workspace contract: forward slashes in manifest_relpath. Make
        // sure the emitter consumes them as-is.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        );
        write(&root.join("crates/a/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");

        let ws = crate::workspace::load_workspace(root).unwrap();
        let items = plan_cargo_lints(root, &ws, &Manifest::default()).unwrap();
        let member_targets: Vec<_> = items
            .iter()
            .filter_map(|i| match &i.target {
                crate::plan::Target::Region { host, .. } if host != "Cargo.toml" => {
                    Some(host.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(member_targets, vec!["crates/a/Cargo.toml"]);

        // Suppress unused-import lint when WorkspaceMember isn't exercised
        // in this test (kept as a reminder).
        let _ = std::any::type_name::<WorkspaceMember>();
    }
}
