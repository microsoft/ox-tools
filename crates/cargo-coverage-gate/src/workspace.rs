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

use crate::error::CoverageGateError;

/// Lower bound on `min-lines` values.
const MIN_LINES_LOWER: f64 = 0.0;
/// Upper bound on `min-lines` values.
const MIN_LINES_UPPER: f64 = 100.0;

/// A resolved view of the cargo workspace the gate is operating on.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    /// One entry per workspace member, in alphabetical order by name.
    pub(crate) members: Vec<Member>,
    /// `min-lines` value from `[workspace.metadata.coverage-gate]`, if set.
    pub(crate) default_min_lines_percent: Option<f64>,
}

/// A single workspace member.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// Cargo package name.
    pub(crate) name: String,
    /// Directory containing the member's `Cargo.toml`.
    pub(crate) manifest_dir: PathBuf,
    /// `min-lines` value from this member's
    /// `[package.metadata.coverage-gate]`, if set.
    pub(crate) min_lines_percent: Option<f64>,
}

impl Workspace {
    /// Discover the workspace enclosing `manifest_path` (or `CWD` if
    /// `None`) and load every member's threshold metadata.
    ///
    /// Does not resolve dependencies — `cargo metadata --no-deps` is
    /// invoked, which is fast and side-effect-free.
    pub(crate) fn load(manifest_path: Option<&Path>) -> Result<Self, CoverageGateError> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd
            .exec()
            .map_err(|source| CoverageGateError::caused_by("failed to load workspace metadata".to_owned(), source))?;

        let workspace_default = extract_min_lines_percent(&metadata.workspace_metadata, "workspace")?;

        let mut members: Vec<Member> = metadata
            .workspace_packages()
            .iter()
            .map(|pkg| {
                let manifest_dir = pkg
                    .manifest_path
                    .parent()
                    .map_or_else(|| PathBuf::from(pkg.manifest_path.as_str()), |p| PathBuf::from(p.as_str()));
                let min_lines_percent = extract_min_lines_percent(&pkg.metadata, &pkg.name)?;
                Ok::<Member, CoverageGateError>(Member {
                    name: pkg.name.to_string(),
                    manifest_dir,
                    min_lines_percent,
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

/// Pull `coverage-gate.min-lines-percent` out of a freeform metadata `Value`
/// and validate that it falls in `[0.0, 100.0]`.
///
/// Accepts either integer or float JSON numbers (the TOML
/// representation may have used either form).
fn extract_min_lines_percent(metadata: &Value, source: &str) -> Result<Option<f64>, CoverageGateError> {
    let Some(min) = metadata.get("coverage-gate").and_then(|v| v.get("min-lines-percent")) else {
        return Ok(None);
    };
    let value = min
        .as_f64()
        .ok_or_else(|| CoverageGateError::new(format!("{source}: `coverage-gate.min-lines-percent` must be a number, got {min}")))?;
    if !(MIN_LINES_LOWER..=MIN_LINES_UPPER).contains(&value) {
        return Err(CoverageGateError::new(format!(
            "invalid coverage-gate min-lines-percent value `{value}` for {source}: \
             expected a number in {MIN_LINES_LOWER}.0..={MIN_LINES_UPPER}.0"
        )));
    }
    Ok(Some(value))
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata subprocess")]
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
}
