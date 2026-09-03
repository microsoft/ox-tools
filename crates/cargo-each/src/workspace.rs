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

use cargo_metadata::{MetadataCommand, TargetKind};
use serde_json::Value;

use crate::error::{EachError, LoadMetadataError};

/// A resolved view of the cargo workspace `cargo-each` is operating on.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    /// One entry per workspace member, in alphabetical order by name.
    pub(crate) members: Vec<Member>,
    /// Names of the workspace's default members (cargo's `default-members`,
    /// or every member when unset). Used to resolve a selection that names
    /// no packages.
    pub(crate) default_member_names: HashSet<String>,
}

/// A single workspace member and the facts selection/filtering key on.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// Cargo package name (e.g. `cargo-anvil`).
    pub(crate) name: String,
    /// Package version, rendered (e.g. `0.3.0`).
    pub(crate) version: String,
    /// Absolute path to this member's `Cargo.toml`.
    pub(crate) manifest_path: PathBuf,
    /// Whether Cargo permits publishing this package.
    pub(crate) publishable: bool,
    /// Features declared by this package.
    pub(crate) features: BTreeSet<String>,
    /// Cargo targets declared by this package.
    pub(crate) targets: Vec<MemberTarget>,
    /// Whether the member has a `lib` target.
    pub(crate) has_lib: bool,
    /// Whether the member has a `bin` target.
    pub(crate) has_bin: bool,
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
    /// Cargo metadata target-kind spellings.
    pub(crate) kinds: BTreeSet<String>,
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
                let has_lib = pkg.targets.iter().any(is_lib_target);
                let has_bin = pkg.targets.iter().any(is_bin_target);
                let dependencies = pkg.dependencies.iter().map(|d| d.name.clone()).collect();
                let mut targets: Vec<MemberTarget> = pkg
                    .targets
                    .iter()
                    .map(|target| MemberTarget {
                        name: target.name.clone(),
                        kinds: target.kind.iter().map(target_kind_name).collect(),
                        required_features: target.required_features.iter().cloned().collect(),
                    })
                    .collect();
                targets.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.kinds.cmp(&b.kinds)));
                Member {
                    name: pkg.name.to_string(),
                    version: pkg.version.to_string(),
                    manifest_path: pkg.manifest_path.clone().into_std_path_buf(),
                    publishable: pkg.publish.as_ref().is_none_or(|registries| !registries.is_empty()),
                    features: pkg.features.keys().cloned().collect(),
                    targets,
                    has_lib,
                    has_bin,
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

        Ok(Self {
            members,
            default_member_names,
        })
    }
}

/// Whether a target is a plain Rust library (`lib`).
///
/// Matches the typed target-kind enum directly rather than its `Display`
/// string — no per-target allocation and no spelling-drift risk. Proc-macro,
/// `cdylib`, and `staticlib` library kinds are deliberately *not* counted:
/// the `lib` filter means the plain `lib` kind only (see the design doc).
fn is_lib_target(target: &cargo_metadata::Target) -> bool {
    target.kind.iter().any(|k| matches!(k, TargetKind::Lib))
}

/// Whether a target is a binary (`bin`).
fn is_bin_target(target: &cargo_metadata::Target) -> bool {
    target.kind.iter().any(|k| matches!(k, TargetKind::Bin))
}

/// Convert cargo-metadata's typed target kind into its Cargo JSON spelling.
fn target_kind_name(kind: &TargetKind) -> String {
    match kind {
        TargetKind::Bench => "bench",
        TargetKind::Bin => "bin",
        TargetKind::CustomBuild => "custom-build",
        TargetKind::CDyLib => "cdylib",
        TargetKind::DyLib => "dylib",
        TargetKind::Example => "example",
        TargetKind::Lib => "lib",
        TargetKind::ProcMacro => "proc-macro",
        TargetKind::RLib => "rlib",
        TargetKind::StaticLib => "staticlib",
        TargetKind::Test => "test",
        TargetKind::Unknown(value) => value,
        _ => "unknown",
    }
    .to_owned()
}

/// Whether a CLI target-kind spelling is one cargo-each supports.
#[must_use]
pub(crate) fn is_known_target_kind(kind: &str) -> bool {
    matches!(
        kind,
        "bench" | "bin" | "custom-build" | "cdylib" | "dylib" | "example" | "lib" | "proc-macro" | "rlib" | "staticlib" | "test"
    )
}
