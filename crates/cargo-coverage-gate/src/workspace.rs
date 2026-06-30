// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Workspace discovery and per-package threshold metadata extraction.
//!
//! Wraps [`cargo_metadata`] to enumerate workspace members and reads
//! the optional `[package.metadata.coverage-gate]` block from each
//! member, plus the optional `[workspace.metadata.coverage-gate]`
//! block at the root. Threshold resolution itself (per-package →
//! workspace default → built-in `100.0`) lives in [`crate::threshold`]
//! and consumes the values surfaced here.

use std::path::{Path, PathBuf};

use cargo_metadata::MetadataCommand;
use serde_json::Value;

use crate::error::{
    ConflictingCoverageMetadataError, CoverageGateError, InvalidNoCoverableLinesValueError, InvalidThresholdValueError, LoadMetadataError,
    ThresholdOutOfRangeError, WorkspaceScopedNoCoverableLinesError,
};

/// Lower bound on `min-lines-percent` values.
const MIN_LINES_LOWER: f64 = 0.0;
/// Upper bound on `min-lines-percent` values.
const MIN_LINES_UPPER: f64 = 100.0;

/// A resolved view of the cargo workspace the gate is operating on.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    /// One entry per workspace member, in alphabetical order by name.
    pub(crate) members: Vec<Member>,
    /// `min-lines-percent` value from `[workspace.metadata.coverage-gate]`, if set.
    pub(crate) default_min_lines_percent: Option<f64>,
}

/// A single workspace member.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// Cargo package name.
    pub(crate) name: String,
    /// Absolute directory containing this member's `Cargo.toml`.
    pub(crate) manifest_dir: PathBuf,
    /// `min-lines-percent` value from this member's
    /// `[package.metadata.coverage-gate]`, if set.
    pub(crate) min_lines_percent: Option<f64>,
    /// `expect-no-coverable-lines = true` from this member's
    /// `[package.metadata.coverage-gate]`. When set, the package asserts
    /// it contains no coverable lines; the gate passes only if that holds
    /// and fails (as a regression) if coverable lines appear. Mutually
    /// exclusive with [`Member::min_lines_percent`].
    pub(crate) expect_no_coverable_lines: bool,
}

impl Workspace {
    /// Load workspace metadata for the workspace enclosing
    /// `manifest_path` (or `CWD` if `None`), capturing each member's
    /// threshold-related metadata.
    ///
    /// Runs `cargo metadata --no-deps`, which does not fetch or build
    /// dependencies and is therefore fast and side-effect-free.
    #[ohno::enrich_err("failed to load cargo workspace metadata")]
    pub(crate) fn load(manifest_path: Option<&Path>) -> Result<Self, CoverageGateError> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd.exec().map_err(LoadMetadataError::caused_by)?;

        // The workspace scope may carry a `min-lines-percent` default but
        // must not carry the per-package `expect-no-coverable-lines`
        // assertion.
        let workspace_default = extract_coverage_gate(&metadata.workspace_metadata, "workspace", Scope::Workspace)?.min_lines_percent;

        let mut members: Vec<Member> = metadata
            .workspace_packages()
            .iter()
            .map(|pkg| {
                let manifest_dir = pkg
                    .manifest_path
                    .parent()
                    .expect("cargo-metadata always reports a manifest file path with a parent directory")
                    .as_std_path()
                    .to_path_buf();
                let gate = extract_coverage_gate(&pkg.metadata, &pkg.name, Scope::Package)?;
                Ok::<Member, CoverageGateError>(Member {
                    name: pkg.name.to_string(),
                    manifest_dir,
                    min_lines_percent: gate.min_lines_percent,
                    expect_no_coverable_lines: gate.expect_no_coverable_lines,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        members.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self {
            members,
            default_min_lines_percent: workspace_default,
        })
    }
}

/// Whether a `[*.metadata.coverage-gate]` block is being read from a
/// package or from the workspace root. Some keys are only valid in the
/// package scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `[package.metadata.coverage-gate]`.
    Package,
    /// `[workspace.metadata.coverage-gate]`.
    Workspace,
}

/// The `coverage-gate` metadata extracted from a single scope.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct CoverageGateMetadata {
    /// `min-lines-percent`, validated to `[0.0, 100.0]`, if present.
    min_lines_percent: Option<f64>,
    /// `expect-no-coverable-lines`, normalized so `false`/absent are
    /// indistinguishable. Always `false` in the workspace scope (a
    /// `true` value there is rejected).
    expect_no_coverable_lines: bool,
}

/// Pull the `coverage-gate` block out of a freeform metadata `Value` and
/// validate it.
///
/// Validates that `min-lines-percent`, when present, is a number in
/// `[0.0, 100.0]`, and that `expect-no-coverable-lines`, when present, is
/// a boolean. The two keys are mutually exclusive on a package, and
/// `expect-no-coverable-lines` is rejected entirely in the workspace
/// scope.
fn extract_coverage_gate(metadata: &Value, source: &str, scope: Scope) -> Result<CoverageGateMetadata, CoverageGateError> {
    let Some(gate) = metadata.get("coverage-gate") else {
        return Ok(CoverageGateMetadata::default());
    };

    let min_lines_percent = extract_min_lines_percent(gate, source)?;
    let expect_no_coverable_lines = extract_expect_no_coverable_lines(gate, source, scope)?;

    if min_lines_percent.is_some() && expect_no_coverable_lines {
        return Err(ConflictingCoverageMetadataError::new(source.to_owned()).into());
    }

    Ok(CoverageGateMetadata {
        min_lines_percent,
        expect_no_coverable_lines,
    })
}

/// Pull `min-lines-percent` out of a `coverage-gate` block and validate
/// that it falls in `[0.0, 100.0]`.
///
/// Accepts either integer or float JSON numbers (the TOML
/// representation may have used either form).
fn extract_min_lines_percent(gate: &Value, source: &str) -> Result<Option<f64>, CoverageGateError> {
    let Some(min) = gate.get("min-lines-percent") else {
        return Ok(None);
    };
    let value = min
        .as_f64()
        .ok_or_else(|| InvalidThresholdValueError::new(source.to_owned(), min.clone()))?;
    if !(MIN_LINES_LOWER..=MIN_LINES_UPPER).contains(&value) {
        return Err(ThresholdOutOfRangeError::new(source.to_owned(), value, MIN_LINES_LOWER, MIN_LINES_UPPER).into());
    }
    Ok(Some(value))
}

/// Pull `expect-no-coverable-lines` out of a `coverage-gate` block.
///
/// Returns `false` when the key is absent or explicitly `false`. A
/// non-boolean value, or a `true` value in the workspace scope, is an
/// error.
fn extract_expect_no_coverable_lines(gate: &Value, source: &str, scope: Scope) -> Result<bool, CoverageGateError> {
    let Some(raw) = gate.get("expect-no-coverable-lines") else {
        return Ok(false);
    };
    let value = raw
        .as_bool()
        .ok_or_else(|| InvalidNoCoverableLinesValueError::new(source.to_owned(), raw.clone()))?;
    if value && scope == Scope::Workspace {
        return Err(WorkspaceScopedNoCoverableLinesError::new().into());
    }
    Ok(value)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use super::*;

    /// Write a minimal workspace with the given root `Cargo.toml` body
    /// and per-member specs.
    fn write_workspace(dir: &Path, root_body: &str, members: &[(&str, &str)]) {
        fs::write(dir.join("Cargo.toml"), root_body).expect("write root Cargo.toml");
        for (name, body) in members {
            let member_dir = dir.join(name);
            fs::create_dir_all(member_dir.join("src")).expect("mkdir member src");
            fs::write(member_dir.join("Cargo.toml"), body).expect("write member Cargo.toml");
            fs::write(member_dir.join("src/lib.rs"), "// empty\n").expect("write lib.rs");
        }
    }

    const ROOT_NO_DEFAULT: &str = r#"
[workspace]
resolver = "2"
members = ["alpha", "beta", "gamma"]
"#;

    const ROOT_WITH_DEFAULT: &str = r#"
[workspace]
resolver = "2"
members = ["alpha", "beta"]

[workspace.metadata.coverage-gate]
min-lines-percent = 80
"#;

    fn member(name: &str, min_lines_percent: Option<&str>) -> String {
        let extra = min_lines_percent.map_or(String::new(), |m| {
            format!("\n[package.metadata.coverage-gate]\nmin-lines-percent = {m}\n")
        });
        format!(
            r#"
[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
{extra}
"#
        )
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn loads_workspace_with_no_metadata_anywhere() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_NO_DEFAULT,
            &[
                ("alpha", &member("alpha", None)),
                ("beta", &member("beta", None)),
                ("gamma", &member("gamma", None)),
            ],
        );
        let ws = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect("workspace load should succeed");
        assert!(ws.default_min_lines_percent.is_none());
        assert_eq!(ws.members.len(), 3);
        let names: Vec<&str> = ws.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
        for m in &ws.members {
            assert!(m.min_lines_percent.is_none());
            assert!(m.manifest_dir.is_dir());
        }
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn picks_up_workspace_level_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_WITH_DEFAULT,
            &[("alpha", &member("alpha", None)), ("beta", &member("beta", None))],
        );
        let ws = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect("workspace load should succeed");
        assert_eq!(ws.default_min_lines_percent, Some(80.0));
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn picks_up_per_crate_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_WITH_DEFAULT,
            &[("alpha", &member("alpha", Some("90.5"))), ("beta", &member("beta", Some("0")))],
        );
        let ws = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect("workspace load should succeed");
        let alpha = ws.members.iter().find(|m| m.name == "alpha").expect("alpha");
        let beta = ws.members.iter().find(|m| m.name == "beta").expect("beta");
        assert_eq!(alpha.min_lines_percent, Some(90.5));
        assert_eq!(beta.min_lines_percent, Some(0.0));
        assert_eq!(ws.default_min_lines_percent, Some(80.0));
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_out_of_range_per_crate_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_NO_DEFAULT,
            &[
                ("alpha", &member("alpha", Some("120"))),
                ("beta", &member("beta", None)),
                ("gamma", &member("gamma", None)),
            ],
        );
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("out-of-range value must error");
        let rendered = err.to_string();
        assert!(rendered.contains("alpha"), "rendered: {rendered}");
        assert!(rendered.contains("120"), "rendered: {rendered}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_negative_workspace_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = r#"
[workspace]
resolver = "2"
members = ["alpha"]

[workspace.metadata.coverage-gate]
min-lines-percent = -1
"#;
        write_workspace(tmp.path(), root, &[("alpha", &member("alpha", None))]);
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("negative workspace value must error");
        let rendered = err.to_string();
        assert!(rendered.contains("workspace"), "rendered: {rendered}");
        assert!(rendered.contains("-1"), "rendered: {rendered}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_non_numeric_threshold() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = r#"
[workspace]
resolver = "2"
members = ["alpha"]

[workspace.metadata.coverage-gate]
min-lines-percent = "ninety"
"#;
        write_workspace(tmp.path(), root, &[("alpha", &member("alpha", None))]);
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("string threshold must error");
        assert!(err.to_string().contains("must be a number"));
    }

    /// A package body with an explicit `[package.metadata.coverage-gate]`
    /// block body (the caller supplies the inner key/value lines).
    fn member_with_gate(name: &str, gate_body: &str) -> String {
        format!(
            r#"
[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[package.metadata.coverage-gate]
{gate_body}
"#
        )
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn picks_up_expect_no_coverable_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_NO_DEFAULT,
            &[
                ("alpha", &member_with_gate("alpha", "expect-no-coverable-lines = true")),
                ("beta", &member_with_gate("beta", "expect-no-coverable-lines = false")),
                ("gamma", &member("gamma", None)),
            ],
        );
        let ws = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect("workspace load should succeed");
        let alpha = ws.members.iter().find(|m| m.name == "alpha").expect("alpha");
        let beta = ws.members.iter().find(|m| m.name == "beta").expect("beta");
        let gamma = ws.members.iter().find(|m| m.name == "gamma").expect("gamma");
        assert!(alpha.expect_no_coverable_lines);
        assert!(alpha.min_lines_percent.is_none());
        // `false` is indistinguishable from absent.
        assert!(!beta.expect_no_coverable_lines);
        assert!(!gamma.expect_no_coverable_lines);
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_both_min_lines_and_expect_no_coverable_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_NO_DEFAULT,
            &[
                (
                    "alpha",
                    &member_with_gate("alpha", "min-lines-percent = 50\nexpect-no-coverable-lines = true"),
                ),
                ("beta", &member("beta", None)),
                ("gamma", &member("gamma", None)),
            ],
        );
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("conflicting keys must error");
        let rendered = err.to_string();
        assert!(rendered.contains("alpha"), "rendered: {rendered}");
        assert!(rendered.contains("cannot set both"), "rendered: {rendered}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_workspace_scoped_expect_no_coverable_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = r#"
[workspace]
resolver = "2"
members = ["alpha"]

[workspace.metadata.coverage-gate]
expect-no-coverable-lines = true
"#;
        write_workspace(tmp.path(), root, &[("alpha", &member("alpha", None))]);
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("workspace-scoped assertion must error");
        let rendered = err.to_string();
        assert!(rendered.contains("package-level"), "rendered: {rendered}");
        assert!(rendered.contains("expect-no-coverable-lines"), "rendered: {rendered}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn rejects_non_boolean_expect_no_coverable_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_workspace(
            tmp.path(),
            ROOT_NO_DEFAULT,
            &[
                ("alpha", &member_with_gate("alpha", "expect-no-coverable-lines = \"yes\"")),
                ("beta", &member("beta", None)),
                ("gamma", &member("gamma", None)),
            ],
        );
        let err = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect_err("non-boolean value must error");
        let rendered = err.to_string();
        assert!(rendered.contains("must be a boolean"), "rendered: {rendered}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata subprocess; miri allows neither")]
    #[test]
    fn workspace_scoped_expect_no_coverable_lines_false_is_accepted() {
        // An explicit `false` at the workspace scope is harmless and must
        // not trip the package-level-only guard.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = r#"
[workspace]
resolver = "2"
members = ["alpha"]

[workspace.metadata.coverage-gate]
min-lines-percent = 80
expect-no-coverable-lines = false
"#;
        write_workspace(tmp.path(), root, &[("alpha", &member("alpha", None))]);
        let ws = Workspace::load(Some(&tmp.path().join("Cargo.toml"))).expect("workspace load should succeed");
        assert_eq!(ws.default_min_lines_percent, Some(80.0));
    }
}
