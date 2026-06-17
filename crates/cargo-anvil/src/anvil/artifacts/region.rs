// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Managed regions spliced into user-composed host files.
//!
//! Covers the `Justfile` imports region, the `Cargo.toml` lint regions, and
//! the shared-config regions (`deny.toml`, `rustfmt.toml`, `.delta.toml`,
//! `spellcheck.toml`, `clippy.toml`).
//!
//! Holds the embedded region bodies, the region ids, the lint-body render
//! helpers, and the registry functions wrapping them as [`Artifact`]s.

use super::justfile;
use crate::catalog::{Artifact, HostSelector, RegionId, RegionSpec};
use crate::region::CommentSyntax;

/// Region id for the workspace-scope lints (multi-crate workspaces).
pub const WORKSPACE_LINTS_REGION_ID: &str = "anvil-workspace-lints";

/// Region id for crate-scope lints — used both for single-crate repos (full
/// catalog) and for each member of a multi-crate workspace (`workspace =
/// true`).
pub const CRATE_LINTS_REGION_ID: &str = "anvil-lints";

/// Embedded body of the lint catalog, in dotted-key form (no table header).
pub const LINTS_BODY: &str = include_str!("../../../templates/regions/cargo-lints-body.toml");

/// Embedded body of a workspace-member lints region.
pub const MEMBER_LINTS_BODY: &str = include_str!("../../../templates/regions/cargo-member-lints.toml");

/// Repo-root-relative path of the `cargo-deny` config.
pub const DENY_PATH: &str = "deny.toml";
/// Region id for the managed section of `deny.toml`.
pub const DENY_REGION_ID: &str = "anvil-deny";

/// Repo-root-relative path of the `rustfmt` config.
pub const RUSTFMT_PATH: &str = "rustfmt.toml";
/// Region id for the managed section of `rustfmt.toml`.
pub const RUSTFMT_REGION_ID: &str = "anvil-rustfmt";

/// Repo-root-relative path of the `cargo-delta` config.
pub const DELTA_PATH: &str = ".delta.toml";
/// Region id for the managed section of `.delta.toml`.
pub const DELTA_REGION_ID: &str = "anvil-delta";

/// Repo-root-relative path of the `cargo-spellcheck` config.
pub const SPELLCHECK_PATH: &str = "spellcheck.toml";
/// Region id for the managed section of `spellcheck.toml`.
pub const SPELLCHECK_REGION_ID: &str = "anvil-spellcheck";

/// Repo-root-relative path of the `clippy` lint-tuning config.
pub const CLIPPY_PATH: &str = "clippy.toml";
/// Region id for the managed section of `clippy.toml`.
pub const CLIPPY_REGION_ID: &str = "anvil-clippy";

/// Embedded body of the deny.toml managed region.
pub const DENY_BODY: &str = include_str!("../../../templates/regions/deny.toml");

/// Embedded body of the rustfmt.toml managed region.
pub const RUSTFMT_BODY: &str = include_str!("../../../templates/regions/rustfmt.toml");

/// Embedded body of the .delta.toml managed region.
pub const DELTA_BODY: &str = include_str!("../../../templates/regions/delta.toml");

/// Embedded body of the spellcheck.toml managed region.
pub const SPELLCHECK_BODY: &str = include_str!("../../../templates/regions/spellcheck.toml");

/// Embedded body of the clippy.toml managed region.
pub const CLIPPY_BODY: &str = include_str!("../../../templates/regions/clippy.toml");

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

/// Build a single-path `Hash`-syntax region artifact.
fn path_region(path: &str, id: &'static str, body: impl Into<String>) -> Artifact {
    Artifact::region(RegionSpec {
        host: HostSelector::Path(path.to_owned()),
        id: RegionId::new(id),
        body: body.into(),
        syntax: CommentSyntax::Hash,
    })
}

/// `Justfile` / `anvil-imports` — imports the `justfiles/anvil/` tree.
#[must_use]
pub fn justfile_imports() -> Artifact {
    path_region(
        justfile::JUSTFILE_PATH,
        justfile::JUSTFILE_REGION_ID,
        justfile::JUSTFILE_IMPORTS_BODY,
    )
}

/// Root `Cargo.toml` / `anvil-workspace-lints`.
///
/// The workspace-scope lint catalog under `[workspace.lints]`. Emitted only
/// in a multi-crate workspace; the [`HostSelector::WorkspaceCargoToml`] host
/// skips it in a single-crate repo.
#[must_use]
pub fn workspace_lints() -> Artifact {
    Artifact::region(RegionSpec {
        host: HostSelector::WorkspaceCargoToml,
        id: RegionId::new(WORKSPACE_LINTS_REGION_ID),
        body: render_workspace_lints_body(),
        syntax: CommentSyntax::Hash,
    })
}

/// Root `Cargo.toml` / `anvil-lints`.
///
/// The full lint catalog under `[lints]`. Emitted only in a single-crate
/// repo; the [`HostSelector::SingleCrateCargoToml`] host skips it in a
/// workspace, where the catalog lives under `[workspace.lints]` and members
/// inherit it.
#[must_use]
pub fn single_crate_lints() -> Artifact {
    Artifact::region(RegionSpec {
        host: HostSelector::SingleCrateCargoToml,
        id: RegionId::new(CRATE_LINTS_REGION_ID),
        body: render_single_crate_lints_body(),
        syntax: CommentSyntax::Hash,
    })
}

/// `<member>/Cargo.toml` / `anvil-lints` — the per-member `workspace = true`
/// inheritance stub, replicated across every member of a workspace.
#[must_use]
pub fn member_lints() -> Artifact {
    Artifact::member_region(RegionId::new(CRATE_LINTS_REGION_ID), MEMBER_LINTS_BODY)
}

/// `deny.toml` / `anvil-deny`.
#[must_use]
pub fn deny() -> Artifact {
    path_region(DENY_PATH, DENY_REGION_ID, DENY_BODY)
}

/// `rustfmt.toml` / `anvil-rustfmt`.
#[must_use]
pub fn rustfmt() -> Artifact {
    path_region(RUSTFMT_PATH, RUSTFMT_REGION_ID, RUSTFMT_BODY)
}

/// `.delta.toml` / `anvil-delta`.
#[must_use]
pub fn delta() -> Artifact {
    path_region(DELTA_PATH, DELTA_REGION_ID, DELTA_BODY)
}

/// `spellcheck.toml` / `anvil-spellcheck`.
#[must_use]
pub fn spellcheck() -> Artifact {
    path_region(SPELLCHECK_PATH, SPELLCHECK_REGION_ID, SPELLCHECK_BODY)
}

/// `clippy.toml` / `anvil-clippy`.
#[must_use]
pub fn clippy() -> Artifact {
    path_region(CLIPPY_PATH, CLIPPY_REGION_ID, CLIPPY_BODY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::upsert_region;

    #[test]
    fn embedded_catalog_uses_dotted_keys() {
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
    fn catalog_intentionally_omits_contested_lints() {
        for needle in ["rust.missing_docs", "clippy.expect_used", "clippy.panic "] {
            assert!(
                !LINTS_BODY.contains(needle),
                "catalog now contains '{needle}'; if intentional, update the catalog-omission test"
            );
        }
    }

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

    #[test]
    fn dotted_key_body_parses_as_valid_toml_when_appended_to_workspace() {
        let host = "[workspace]\nmembers = [\"crates/a\"]\n";
        let region_body = render_workspace_lints_body();
        let spliced = upsert_region(host, WORKSPACE_LINTS_REGION_ID, &region_body, CommentSyntax::Hash).unwrap();
        let _: toml_edit::DocumentMut = spliced.parse().expect("spliced TOML must be valid");
    }

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
        assert!(RUSTFMT_BODY.contains("max_width = 140"));
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
        assert!(SPELLCHECK_BODY.contains("\"target/spelling.dic\""));
        assert!(SPELLCHECK_BODY.contains("skip_os_lookups = true"));
        assert!(SPELLCHECK_BODY.contains("use_builtin = true"));
        assert!(SPELLCHECK_BODY.contains("[Hunspell.quirks]"));
        assert!(SPELLCHECK_BODY.contains("allow_concatenation = true"));
    }

    #[test]
    fn clippy_body_carries_companion_tunings_for_catalog_lints() {
        assert!(CLIPPY_BODY.contains("allow-panic-in-tests = true"));
        assert!(CLIPPY_BODY.contains("allow-unwrap-in-tests = true"));
        assert!(CLIPPY_BODY.contains("semicolon-outside-block-ignore-multiline = true"));
        assert!(CLIPPY_BODY.contains("avoid-breaking-exported-api = false"));
        assert!(CLIPPY_BODY.contains("absolute-paths-max-segments = 3"));
        assert!(CLIPPY_BODY.contains("warn-on-all-wildcard-imports = true"));
    }

    #[test]
    fn shared_config_bodies_round_trip_through_toml_parser() {
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
}
