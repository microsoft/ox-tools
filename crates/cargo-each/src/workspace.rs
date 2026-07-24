// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Workspace discovery via [`cargo_metadata`].
//!
//! Enumerates workspace members and captures, for each, the facts the
//! selection and filter layers need: name, version, manifest path, whether
//! it has a `lib` / `bin` target, its declared dependency names, and its
//! freeform `package.metadata` block.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use cargo_metadata::MetadataCommand;
use serde_json::Value;

use crate::error::{EachError, LoadMetadataError};

/// A resolved view of the cargo workspace `cargo-each` is operating on.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// One entry per workspace member, in alphabetical order by name.
    pub members: Vec<Member>,
    /// Names of the workspace's default members (cargo's `default-members`,
    /// or every member when unset). Used to resolve a selection that names
    /// no packages.
    pub default_member_names: HashSet<String>,
}

/// A single workspace member and the facts selection/filtering key on.
#[derive(Debug, Clone)]
pub struct Member {
    /// Cargo package name (e.g. `cargo-anvil`).
    pub name: String,
    /// Package version, rendered (e.g. `0.3.0`).
    pub version: String,
    /// Absolute path to this member's `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Whether the member has a `lib` target.
    pub has_lib: bool,
    /// Whether the member has a `bin` target.
    pub has_bin: bool,
    /// Names of this member's declared dependencies (any kind).
    pub dependencies: BTreeSet<String>,
    /// The member's `package.metadata` block, as freeform JSON.
    ///
    /// Crate-private: only the in-crate [`Predicate`](crate::Predicate)
    /// evaluation reads it, so it stays out of the public API surface (and
    /// keeps `serde_json` off the public boundary).
    pub(crate) metadata: Value,
}

impl Member {
    /// The version-qualified cargo spec, `name@version`.
    #[must_use]
    pub fn spec(&self) -> String {
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
    pub fn manifest_dir(&self) -> &Path {
        self.manifest_path
            .parent()
            .expect("cargo-metadata always reports a manifest file path with a parent directory")
    }
}

impl Workspace {
    /// Load workspace metadata for the workspace enclosing `manifest_path`
    /// (or the current directory when `None`).
    ///
    /// Runs `cargo metadata --no-deps`, which does not fetch or build
    /// dependencies and is therefore fast and side-effect-free. The
    /// `--no-deps` mode still reports each member's *declared* dependencies,
    /// which is what the `dep:` predicate needs.
    #[ohno::enrich_err("failed to load cargo workspace metadata")]
    pub fn load(manifest_path: Option<&Path>) -> Result<Self, EachError> {
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
                let has_lib = pkg.targets.iter().any(|t| target_is(t, "lib"));
                let has_bin = pkg.targets.iter().any(|t| target_is(t, "bin"));
                let dependencies = pkg.dependencies.iter().map(|d| d.name.clone()).collect();
                Member {
                    name: pkg.name.to_string(),
                    version: pkg.version.to_string(),
                    manifest_path: pkg.manifest_path.clone().into_std_path_buf(),
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

/// Whether a target has the given kind (e.g. `"lib"`, `"bin"`).
///
/// `cargo_metadata` models target kinds as a typed enum; compare via its
/// `Display` form so this stays robust to the exact variant spelling.
fn target_is(target: &cargo_metadata::Target, kind: &str) -> bool {
    target.kind.iter().any(|k| k.to_string() == kind)
}
