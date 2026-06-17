// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The catalog artifact model.
//!
//! An [`Artifact`] is one unit a catalog emits: either a fully tool-owned
//! file ([`OwnedFileSpec`]) or a sentinel-delimited managed region spliced
//! into a user-composed host file ([`RegionSpec`]). The engine iterates a
//! catalog's artifacts and dispatches each to the generic owned-file /
//! managed-region drivers in [`crate::emit`].
//!
//! See [`extensibility.md §4`](../../docs/design/extensibility.md) for the
//! design rationale. The on-disk vocabulary (`anvil-managed` sentinels,
//! `justfiles/anvil/`, `.anvil.lock`) is fixed engine format — an artifact
//! never parameterizes it.

use crate::backend::Backend;
use crate::region::CommentSyntax;

/// A managed-region identifier.
///
/// A newtype, not a bare string, so it can't be confused with a file path,
/// a recipe name, or any other string the API takes. It is the value placed
/// after `anvil-managed:` in the region sentinels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(&'static str);

impl RegionId {
    /// Construct a region id from a static string.
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    /// The underlying string slice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for RegionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Where a managed region's host file lives.
///
/// A region is not always anchored to one literal path: the crate-scope
/// `[lints]` region is spliced into *every* workspace member's manifest,
/// with the host set discovered at plan time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostSelector {
    /// A single literal repo-root-relative forward-slash path
    /// (`Justfile`, `deny.toml`, the root `Cargo.toml`).
    Path(String),
    /// Every workspace member's manifest — expands to one
    /// `<member>/Cargo.toml` host per member discovered at plan time.
    EachMemberManifest,
}

/// A fully tool-owned file.
///
/// The justfile tree members live here, and so does every cloud-workflow
/// backend file. An owned file may be **gated** on a backend so it is
/// emitted only when that backend is selected. Identity: its
/// repo-root-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFileSpec {
    /// Repo-root-relative forward-slash path. This is the artifact's identity.
    pub path: &'static str,
    /// The byte-exact content the artifact renders.
    pub body: String,
    /// `None` = emit always; `Some(b)` = emit only when backend `b` is selected.
    pub gate: Option<Backend>,
}

/// A sentinel-delimited managed region spliced into a host file.
///
/// Identity: `(host-selector, region_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionSpec {
    /// Where the region goes.
    pub host: HostSelector,
    /// The sentinel id.
    pub id: RegionId,
    /// The content rendered between the sentinels.
    pub body: String,
    /// The host's comment flavor.
    pub syntax: CommentSyntax,
}

/// One catalog artifact: an owned file or a managed region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Artifact {
    /// A fully tool-owned file.
    OwnedFile(OwnedFileSpec),
    /// A managed region spliced into a host file.
    Region(RegionSpec),
}

/// The engine-internal identity of an artifact, used for dedup and override.
///
/// Not part of the public surface: a fork never constructs or names a key;
/// it references the built-in artifacts themselves (see the `artifacts::`
/// registry) and the engine derives the key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ArtifactKey {
    /// An owned file, keyed by its path.
    OwnedFile(String),
    /// A managed region, keyed by its host selector and id.
    Region {
        /// The host selector.
        host: HostSelector,
        /// The region id.
        id: String,
    },
}

impl Artifact {
    /// Construct an ungated owned-file artifact.
    #[must_use]
    pub fn owned_file(path: &'static str, body: impl Into<String>) -> Self {
        Self::OwnedFile(OwnedFileSpec {
            path,
            body: body.into(),
            gate: None,
        })
    }

    /// Construct an owned-file artifact gated on a backend.
    ///
    /// Used to **add** a new backend file. It takes the closed [`Backend`]
    /// enum, so a fork can gate only on an existing backend — it cannot
    /// invent one.
    #[must_use]
    pub fn backend_file(backend: Backend, path: &'static str, body: impl Into<String>) -> Self {
        Self::OwnedFile(OwnedFileSpec {
            path,
            body: body.into(),
            gate: Some(backend),
        })
    }

    /// Construct a managed-region artifact from a full spec.
    #[must_use]
    pub fn region(spec: RegionSpec) -> Self {
        Self::Region(spec)
    }

    /// Construct a per-member managed region (`EachMemberManifest` + `Hash`
    /// syntax), the common case for a region replicated across every
    /// workspace member's `Cargo.toml`.
    #[must_use]
    pub fn member_region(id: RegionId, body: impl Into<String>) -> Self {
        Self::Region(RegionSpec {
            host: HostSelector::EachMemberManifest,
            id,
            body: body.into(),
            syntax: CommentSyntax::Hash,
        })
    }

    /// Derive a variant of this artifact with a new body, preserving every
    /// other field — path, gate, host, id, syntax.
    ///
    /// This is how a fork overrides a built-in without being able to alter
    /// its identity.
    #[must_use]
    pub fn with_body(self, body: impl Into<String>) -> Self {
        match self {
            Self::OwnedFile(spec) => Self::OwnedFile(OwnedFileSpec { body: body.into(), ..spec }),
            Self::Region(spec) => Self::Region(RegionSpec { body: body.into(), ..spec }),
        }
    }

    /// This artifact's engine-internal identity, used for dedup and override.
    pub(crate) fn key(&self) -> ArtifactKey {
        match self {
            Self::OwnedFile(spec) => ArtifactKey::OwnedFile(spec.path.to_owned()),
            Self::Region(spec) => ArtifactKey::Region {
                host: spec.host.clone(),
                id: spec.id.as_str().to_owned(),
            },
        }
    }

    /// The body this artifact renders.
    #[must_use]
    pub fn body(&self) -> &str {
        match self {
            Self::OwnedFile(spec) => &spec.body,
            Self::Region(spec) => &spec.body,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_id_round_trips() {
        let id = RegionId::new("anvil-imports");
        assert_eq!(id.as_str(), "anvil-imports");
        assert_eq!(id.to_string(), "anvil-imports");
    }

    #[test]
    fn owned_file_constructor_has_no_gate() {
        let a = Artifact::owned_file("justfiles/anvil/mod.just", "body\n");
        match &a {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, "justfiles/anvil/mod.just");
                assert_eq!(spec.body, "body\n");
                assert_eq!(spec.gate, None);
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
    }

    #[test]
    fn backend_file_constructor_sets_gate() {
        let a = Artifact::backend_file(Backend::GitHub, ".github/workflows/anvil-pr.yml", "x");
        match &a {
            Artifact::OwnedFile(spec) => assert_eq!(spec.gate, Some(Backend::GitHub)),
            Artifact::Region(_) => panic!("expected owned file"),
        }
    }

    #[test]
    fn member_region_sets_each_member_and_hash() {
        let a = Artifact::member_region(RegionId::new("anvil-lints"), "body\n");
        match &a {
            Artifact::Region(spec) => {
                assert_eq!(spec.host, HostSelector::EachMemberManifest);
                assert_eq!(spec.id, RegionId::new("anvil-lints"));
                assert_eq!(spec.syntax, CommentSyntax::Hash);
            }
            Artifact::OwnedFile(_) => panic!("expected region"),
        }
    }

    #[test]
    fn with_body_preserves_owned_file_path_and_gate() {
        let a = Artifact::backend_file(Backend::Ado, ".pipelines/anvil/pr.yml", "old");
        let b = a.with_body("new");
        match &b {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, ".pipelines/anvil/pr.yml");
                assert_eq!(spec.gate, Some(Backend::Ado));
                assert_eq!(spec.body, "new");
            }
            Artifact::Region(_) => panic!("expected owned file"),
        }
    }

    #[test]
    fn with_body_preserves_region_host_id_syntax() {
        let a = Artifact::region(RegionSpec {
            host: HostSelector::Path("Cargo.toml".to_owned()),
            id: RegionId::new("anvil-workspace-lints"),
            body: "old".to_owned(),
            syntax: CommentSyntax::Hash,
        });
        let b = a.with_body("new");
        match &b {
            Artifact::Region(spec) => {
                assert_eq!(spec.host, HostSelector::Path("Cargo.toml".to_owned()));
                assert_eq!(spec.id, RegionId::new("anvil-workspace-lints"));
                assert_eq!(spec.syntax, CommentSyntax::Hash);
                assert_eq!(spec.body, "new");
            }
            Artifact::OwnedFile(_) => panic!("expected region"),
        }
    }

    #[test]
    fn key_identifies_owned_file_by_path() {
        let a = Artifact::owned_file("a/b.just", "x");
        let b = Artifact::owned_file("a/b.just", "different body");
        assert_eq!(a.key(), b.key(), "key is body-independent");
        let c = Artifact::owned_file("a/c.just", "x");
        assert_ne!(a.key(), c.key());
    }

    #[test]
    fn key_identifies_region_by_host_and_id() {
        let a = Artifact::region(RegionSpec {
            host: HostSelector::Path("Cargo.toml".to_owned()),
            id: RegionId::new("anvil-lints"),
            body: "x".to_owned(),
            syntax: CommentSyntax::Hash,
        });
        let member = Artifact::member_region(RegionId::new("anvil-lints"), "x");
        // Same id, different host selector -> different identity.
        assert_ne!(a.key(), member.key());
    }

    #[test]
    fn gate_is_not_part_of_key() {
        // Identity is path-only; the gate is not part of the key.
        let plain = Artifact::owned_file("p", "x");
        let gated = Artifact::backend_file(Backend::GitHub, "p", "x");
        assert_eq!(plain.key(), gated.key());
    }
}
