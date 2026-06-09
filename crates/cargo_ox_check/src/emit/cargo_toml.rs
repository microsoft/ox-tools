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

use ohno::AppError;

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

/// Embedded body of the lint catalog, in dotted-key form (no table header).
/// The header (`[workspace.lints]` or `[lints]`) is prepended per host.
pub const LINTS_BODY: &str = include_str!("../../templates/regions/cargo-lints-body.toml");

/// Embedded body of a workspace-member lints region.
pub const MEMBER_LINTS_BODY: &str =
    include_str!("../../templates/regions/cargo-member-lints.toml");

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
pub fn plan_cargo_lints(
    repo_root: &Path,
    workspace: &Workspace,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>, AppError> {
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
                assert_eq!(
                    trimmed, "[lints]",
                    "unexpected table header in single-crate body: {line}"
                );
            }
        }
    }

    #[test]
    fn member_body_is_workspace_inheritance_stub() {
        assert!(MEMBER_LINTS_BODY.contains("[lints]"));
        assert!(MEMBER_LINTS_BODY.contains("workspace = true"));
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
        assert_eq!(items.len(), 3);
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
        spliced.push_str("clippy.todo = \"warn\"\n");
        let _: toml_edit::DocumentMut =
            spliced.parse().expect("user extension keeps document valid");
    }

    #[test]
    fn member_relpaths_use_forward_slashes() {
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
        let _ = std::any::type_name::<WorkspaceMember>();
    }
}
