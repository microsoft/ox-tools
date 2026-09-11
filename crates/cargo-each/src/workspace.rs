// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Workspace discovery via [`cargo_metadata`].
//!
//! Enumerates workspace members and captures, for each, the facts the
//! selection and filter layers need: package identity, publication state,
//! features, dependencies, targets, and the freeform `package.metadata`
//! block.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use cargo_metadata::semver::Version;
use cargo_metadata::{MetadataCommand, TargetKind};
use serde_json::Value;

use crate::error::{EachError, LoadMetadataError, WorkspaceManifestParseError, WorkspaceManifestReadError, WorkspaceRustVersionError};

/// A resolved view of the cargo workspace `cargo-each` is operating on.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    /// One entry per workspace member, in alphabetical order by name.
    pub(crate) members: Vec<Member>,
    /// Names of the workspace's default members (cargo's `default-members`,
    /// or every member when unset). Used to resolve a selection that names
    /// no packages.
    pub(crate) default_member_names: HashSet<String>,
    /// Absolute path to the workspace root manifest.
    pub(crate) root_manifest_path: PathBuf,
}

/// A single workspace member and the facts selection/filtering key on.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// Cargo package name (e.g. `cargo-anvil`).
    pub(crate) name: String,
    /// Package version, rendered (e.g. `0.3.0`).
    pub(crate) version: String,
    /// The member's resolved minimum supported Rust version.
    pub(crate) rust_version: Option<Version>,
    /// Absolute path to this member's `Cargo.toml`.
    pub(crate) manifest_path: PathBuf,
    /// Whether Cargo permits publishing this package.
    pub(crate) publishable: bool,
    /// Features declared by this package.
    pub(crate) features: BTreeSet<String>,
    /// Cargo targets declared by this package.
    pub(crate) targets: Vec<MemberTarget>,
    /// Names of this member's declared dependencies (any kind).
    pub(crate) dependencies: BTreeSet<String>,
    /// The member's `package.metadata` block, as freeform JSON.
    ///
    /// Crate-private: only the in-crate [`Predicate`](crate::filter::Predicate)
    /// evaluation reads it, so it stays out of the public API surface (and
    /// keeps `serde_json` off the public boundary).
    pub(crate) metadata: Value,
}

/// The target facts used by target-kind predicates and per-target execution.
#[derive(Debug, Clone)]
pub(crate) struct MemberTarget {
    /// Cargo target name.
    pub(crate) name: String,
    /// Cargo metadata target kinds.
    pub(crate) kinds: BTreeSet<TargetKind>,
    /// Features Cargo requires before this target is available.
    pub(crate) required_features: BTreeSet<String>,
}

impl Member {
    /// The version-qualified cargo spec, `name@version`.
    #[must_use]
    pub(crate) fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// The directory containing this member's `Cargo.toml` (its crate root).
    ///
    /// # Panics
    ///
    /// Never in practice: `cargo metadata` always reports a manifest *file*
    /// path, which necessarily has a parent directory. The `expect` documents
    /// that invariant.
    #[must_use]
    pub(crate) fn manifest_dir(&self) -> &Path {
        self.manifest_path
            .parent()
            .expect("cargo-metadata always reports a manifest file path with a parent directory")
    }
}

impl Workspace {
    /// Load the workspace enclosing `manifest_path` (or the current directory).
    ///
    /// Discovery is side-effect-free: it performs no network access and builds
    /// nothing, yet still reports each member's *declared* dependencies (what
    /// the `dep:` filter needs) alongside its targets and `package.metadata`.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] when the workspace cannot be enumerated — for
    /// example a missing or invalid manifest.
    #[ohno::enrich_err("failed to load cargo workspace metadata")]
    pub(crate) fn load(manifest_path: Option<&Path>) -> Result<Self, EachError> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd.exec().map_err(LoadMetadataError::caused_by)?;

        let mut members: Vec<Member> = metadata
            .workspace_packages()
            .iter()
            .map(|pkg| {
                let dependencies = pkg.dependencies.iter().map(|d| d.name.clone()).collect();
                let mut targets: Vec<MemberTarget> = pkg
                    .targets
                    .iter()
                    .map(|target| MemberTarget {
                        name: target.name.clone(),
                        kinds: target.kind.iter().cloned().collect(),
                        required_features: target.required_features.iter().cloned().collect(),
                    })
                    .collect();
                targets.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.kinds.cmp(&b.kinds)));
                Member {
                    name: pkg.name.to_string(),
                    version: pkg.version.to_string(),
                    rust_version: pkg.rust_version.clone(),
                    manifest_path: pkg.manifest_path.clone().into_std_path_buf(),
                    publishable: pkg.publish.as_ref().is_none_or(|registries| !registries.is_empty()),
                    features: pkg.features.keys().cloned().collect(),
                    targets,
                    dependencies,
                    metadata: pkg.metadata.clone(),
                }
            })
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));

        let default_member_names = metadata
            .workspace_default_packages()
            .iter()
            .map(|pkg| pkg.name.to_string())
            .collect();
        let root_manifest_path = metadata.workspace_root.join("Cargo.toml").into_std_path_buf();

        Ok(Self {
            members,
            default_member_names,
            root_manifest_path,
        })
    }

    /// Resolve and validate the workspace-wide Rust compatibility floor.
    ///
    /// This deliberately reads the root manifest only when the corresponding
    /// placeholder is used. Ordinary selection and execution therefore do not
    /// require a workspace Rust-version declaration.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if the root declaration is absent or invalid, or
    /// if any workspace member omits `rust-version` or requires a newer
    /// compiler than the root floor.
    pub(crate) fn workspace_rust_version(&self) -> Result<String, EachError> {
        let path = self.root_manifest_path.display().to_string();
        let text = std::fs::read_to_string(&self.root_manifest_path)
            .map_err(|error| WorkspaceManifestReadError::caused_by(path.clone(), error))?;
        let manifest: toml::Value = toml::from_str(&text).map_err(|error| WorkspaceManifestParseError::caused_by(path, error))?;

        let workspace_floor = manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("package"))
            .and_then(|package| package.get("rust-version"));
        let root_is_only_member = self.members.len() == 1 && self.members[0].manifest_path == self.root_manifest_path;
        let package_floor = root_is_only_member
            .then(|| manifest.get("package").and_then(|package| package.get("rust-version")))
            .flatten();
        let floor = workspace_floor.or(package_floor).ok_or_else(|| {
            WorkspaceRustVersionError::new(
                "the root manifest must declare `[workspace.package].rust-version`, or `[package].rust-version` for a single-package repository"
                    .to_owned(),
            )
        })?;
        let Some(floor) = floor.as_str() else {
            return Err(WorkspaceRustVersionError::new("the root Rust version must be a string".to_owned()).into());
        };
        let parsed_floor = parse_rust_version(floor)
            .map_err(|reason| WorkspaceRustVersionError::new(format!("root Rust version `{floor}` is invalid: {reason}")))?;

        for member in &self.members {
            let Some(member_floor) = member.rust_version.as_ref() else {
                return Err(WorkspaceRustVersionError::new(format!(
                    "workspace member `{}` does not expose a resolved `rust-version`",
                    member.name
                ))
                .into());
            };
            if member_floor.major != 1 || !member_floor.pre.is_empty() || !member_floor.build.is_empty() {
                return Err(WorkspaceRustVersionError::new(format!(
                    "workspace member `{}` exposes invalid Rust version `{member_floor}`; expected a Rust 1.x toolchain version",
                    member.name
                ))
                .into());
            }
            if member_floor > &parsed_floor {
                return Err(WorkspaceRustVersionError::new(format!(
                    "workspace member `{}` requires Rust {}, newer than the root floor {floor}",
                    member.name, member_floor
                ))
                .into());
            }
        }

        Ok(floor.to_owned())
    }
}

fn parse_rust_version(value: &str) -> Result<Version, String> {
    if value.contains('-') || value.contains('+') {
        return Err("pre-release and build metadata are not valid Rust toolchain versions".to_owned());
    }
    let components: Vec<&str> = value.split('.').collect();
    if !(2..=3).contains(&components.len())
        || components
            .iter()
            .any(|component| component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("expected `major.minor` or `major.minor.patch`".to_owned());
    }
    if components.iter().any(|component| component.len() > 1 && component.starts_with('0')) {
        return Err("numeric components must not contain leading zeroes".to_owned());
    }
    let normalized = if components.len() == 2 {
        format!("{value}.0")
    } else {
        value.to_owned()
    };
    let parsed: Version = normalized
        .parse()
        .map_err(|error| format!("expected `major.minor` or `major.minor.patch`: {error}"))?;
    if parsed.major != 1 {
        return Err("expected a Rust 1.x toolchain version".to_owned());
    }
    Ok(parsed)
}

/// Parse a supported Cargo target-kind spelling.
#[must_use]
pub(crate) fn parse_target_kind(kind: &str) -> Option<TargetKind> {
    match TargetKind::from(kind) {
        TargetKind::Unknown(_) => None,
        known => Some(known),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_target_kind() {
        for kind in [
            "bench",
            "bin",
            "custom-build",
            "cdylib",
            "dylib",
            "example",
            "lib",
            "proc-macro",
            "rlib",
            "staticlib",
            "test",
        ] {
            assert!(parse_target_kind(kind).is_some(), "{kind}");
        }
    }

    #[test]
    fn rejects_unknown_target_kind() {
        assert_eq!(parse_target_kind("future-kind"), None);
    }

    #[test]
    fn parses_cargo_rust_version_forms() {
        assert_eq!(parse_rust_version("1.80").expect("minor form"), Version::new(1, 80, 0));
        assert_eq!(parse_rust_version("1.80.1").expect("patch form"), Version::new(1, 80, 1));
        for value in ["1", "1.80.0-beta", "1.80+build", "1.080", "2.0", "one.80"] {
            assert!(parse_rust_version(value).is_err(), "{value}");
        }
    }
}
