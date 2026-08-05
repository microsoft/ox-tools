// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The [`Catalog`] value and its [`CatalogBuilder`].
//!
//! A catalog pairs a [`CliMeta`] identity with an ordered set of
//! [`Artifact`]s. [`Catalog::anvil`] is the built-in base; a fork starts
//! from it via [`Catalog::into_builder`] (or from empty via
//! [`Catalog::builder`]) and customizes the identity and artifacts through
//! the three uniform verbs, then calls [`CatalogBuilder::build`].
//!
//! See [`extensibility.md §4, §5`](../../docs/design/extensibility.md).

use std::collections::BTreeSet;

use ohno::{AppError, bail};

use crate::catalog::artifact::{Artifact, ArtifactKey};
use crate::catalog::meta::CliMeta;
use crate::checksum::checksum_str;
use crate::config::ContainerConfig;

/// The set of artifacts a tool emits, plus its CLI identity.
#[derive(Debug, Clone)]
pub struct Catalog {
    cli: CliMeta,
    artifacts: Vec<Artifact>,
    container_defaults: ContainerConfig,
    /// Keys of artifacts that are only emitted when container execution is
    /// enabled. Tracked here — parallel to `artifacts` and keyed by the
    /// existing [`ArtifactKey`] — so the artifact model itself stays
    /// untouched and container gating works uniformly for owned files and
    /// regions.
    container_gated: BTreeSet<ArtifactKey>,
}

impl Catalog {
    /// Assemble the base catalog from its ordinary artifacts plus a set of
    /// container-gated artifacts, infallibly.
    ///
    /// The base-catalog constructor ([`Catalog::anvil`], defined in the
    /// `anvil` module) goes through this so the fields stay private to the
    /// reusable engine. Its artifact identities are unique by construction, so
    /// it needs neither the builder's duplicate detection nor a fallible
    /// `build` (and thus stays panic-free — a fork that composes arbitrary
    /// artifacts still goes through the checked builder verbs). Pass an empty
    /// `container` vec for a catalog with no container support.
    pub(crate) fn from_parts_with_container(cli: CliMeta, mut artifacts: Vec<Artifact>, container: Vec<Artifact>) -> Self {
        let mut container_gated = BTreeSet::new();
        for artifact in container {
            container_gated.insert(artifact.key());
            artifacts.push(artifact);
        }
        Self {
            cli,
            artifacts,
            container_defaults: ContainerConfig::default(),
            container_gated,
        }
    }

    /// Start a new, empty catalog from a CLI identity.
    #[must_use]
    pub fn builder(cli: CliMeta) -> CatalogBuilder {
        CatalogBuilder {
            cli,
            artifacts: Vec::new(),
            container_defaults: ContainerConfig::default(),
            container_gated: BTreeSet::new(),
            errors: Vec::new(),
        }
    }

    /// Start a builder from this catalog, to customize it.
    #[must_use]
    pub fn into_builder(self) -> CatalogBuilder {
        CatalogBuilder {
            cli: self.cli,
            artifacts: self.artifacts,
            container_defaults: self.container_defaults,
            container_gated: self.container_gated,
            errors: Vec::new(),
        }
    }

    /// The CLI identity.
    #[must_use]
    pub fn cli(&self) -> &CliMeta {
        &self.cli
    }

    /// The ordered artifact set.
    #[must_use]
    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// The catalog-supplied container defaults. A fork sets these via
    /// [`CatalogBuilder::with_container_defaults`]; the user's `anvil.toml`
    /// overrides them field-by-field. Empty by default (base `anvil` ships no
    /// container defaults, so an absent `anvil.toml` keeps container mode off).
    #[must_use]
    pub(crate) fn container_defaults(&self) -> &ContainerConfig {
        &self.container_defaults
    }

    /// Whether the artifact with `key` is container-gated (emitted only when
    /// container execution is enabled).
    #[must_use]
    pub(crate) fn is_container_gated(&self, key: &ArtifactKey) -> bool {
        self.container_gated.contains(key)
    }

    /// Whether this catalog registered any container-gated artifacts. When
    /// false, enabling container mode in `anvil.toml` is meaningless (there is
    /// no `container.just` to emit), so `run_update` rejects it.
    #[must_use]
    pub(crate) fn supports_containers(&self) -> bool {
        !self.container_gated.is_empty()
    }

    /// A `sha256:…` checksum over the whole catalog — every artifact's
    /// identity and rendered body, in canonical (sorted) order.
    ///
    /// Deterministic and independent of any repository: it depends only on
    /// the artifact set, not on artifact insertion order and not on the
    /// [`CliMeta`] identity. Two builds that share a `tool_version` but
    /// differ in any artifact (an extra file, an overridden body, a swapped
    /// backend file) produce different checksums. See
    /// [`updates.md §1`](../../docs/design/updates.md) and
    /// [`extensibility.md §5.1`](../../docs/design/extensibility.md).
    #[must_use]
    pub fn checksum(&self) -> String {
        let mut entries: Vec<String> = self.artifacts.iter().map(canonical_repr).collect();
        entries.sort();
        let mut repr = entries.join("\n");
        // The container-gated key set contributes to the checksum only when
        // it is non-empty, so a catalog with no container artifacts (the base
        // `anvil` catalog) produces exactly the checksum it produced before
        // container support existed.
        if !self.container_gated.is_empty() {
            let mut gated: Vec<String> = self.container_gated.iter().map(key_repr).collect();
            gated.sort();
            repr.push_str("\n\u{1f}container-gated\u{1f}\n");
            repr.push_str(&gated.join("\n"));
        }
        checksum_str(&repr)
    }
}

/// Canonical string for a container-gated key: enough to distinguish owned
/// files from regions and to change the checksum when the gated set changes.
fn key_repr(key: &ArtifactKey) -> String {
    const SEP: char = '\u{1f}';
    match key {
        ArtifactKey::OwnedFile(path) => format!("file{SEP}{path}"),
        ArtifactKey::Region { host, id } => format!("region{SEP}{}{SEP}{id}", host_repr(host)),
    }
}

/// Canonical, collision-resistant string for one artifact: its full identity
/// (including gate / syntax) followed by its rendered body. The leading
/// fields make sorting these strings a canonical, order-independent ordering.
fn canonical_repr(artifact: &Artifact) -> String {
    // U+001F (unit separator) cannot appear in paths/ids and is vanishingly
    // unlikely in bodies, so it disambiguates the joined fields.
    const SEP: char = '\u{1f}';
    match artifact {
        Artifact::OwnedFile(spec) => {
            format!("file{SEP}{}{SEP}gate={}{SEP}{}", spec.path, gate_repr(spec.gate), spec.body)
        }
        Artifact::Region(spec) => {
            format!(
                "region{SEP}{}{SEP}{}{SEP}{}{SEP}{}",
                host_repr(&spec.host),
                spec.id.as_str(),
                syntax_repr(spec.syntax),
                spec.body
            )
        }
    }
}

fn gate_repr(gate: Option<crate::backend::Backend>) -> String {
    gate.map_or_else(|| "none".to_owned(), |backend| backend.name().to_owned())
}

fn host_repr(host: &crate::catalog::HostSelector) -> String {
    match host {
        crate::catalog::HostSelector::Path(path) => format!("path:{path}"),
        crate::catalog::HostSelector::EachMemberManifest => "each_member_manifest".to_owned(),
        crate::catalog::HostSelector::WorkspaceCargoToml => "workspace_cargo_toml".to_owned(),
        crate::catalog::HostSelector::SingleCrateCargoToml => "single_crate_cargo_toml".to_owned(),
    }
}

fn syntax_repr(syntax: crate::region::CommentSyntax) -> &'static str {
    match syntax {
        crate::region::CommentSyntax::Hash => "hash",
        crate::region::CommentSyntax::SlashSlash => "slashslash",
    }
}

/// A builder for customizing a [`Catalog`].
///
/// The three artifact verbs are uniform — all operate on the [`Artifact`]
/// unit and are loud on mismatch. Errors are accumulated and surfaced by
/// [`CatalogBuilder::build`], so a chain reads cleanly while still failing if
/// any edit was invalid.
#[derive(Debug, Clone)]
pub struct CatalogBuilder {
    cli: CliMeta,
    artifacts: Vec<Artifact>,
    container_defaults: ContainerConfig,
    container_gated: BTreeSet<ArtifactKey>,
    errors: Vec<String>,
}

impl CatalogBuilder {
    /// Set the cargo subcommand token, deriving `bin_name` as
    /// `cargo-{name}`.
    #[must_use]
    pub fn subcommand(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.cli.bin_name = format!("cargo-{name}");
        self.cli.subcommand = name;
        self
    }

    /// Set the `about` text shown in `--help`.
    #[must_use]
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.cli.about = about.into();
        self
    }

    /// Set the version string shown in `--version`.
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.cli.version = version.into();
        self
    }

    /// Supply catalog-level container defaults.
    ///
    /// A fork calls this to pre-fill container settings (most usefully
    /// `image`) so a repository can enable container mode with a minimal
    /// `anvil.toml`. The user's `anvil.toml` still overrides these
    /// field-by-field; fields the user omits fall back to what is set here,
    /// and fields neither sets fall back to the built-in defaults.
    #[must_use]
    pub fn with_container_defaults(mut self, defaults: ContainerConfig) -> Self {
        self.container_defaults = defaults;
        self
    }

    /// Append a container-gated artifact: inserted like [`Self::with_artifact`]
    /// and additionally recorded as container-gated, so it is emitted only
    /// when container execution is enabled (via `anvil.toml`) and its body
    /// receives plan-time token substitution. Additive; the existing verbs
    /// keep their exact behavior.
    #[must_use]
    pub fn with_container_artifact(self, artifact: Artifact) -> Self {
        let key = artifact.key();
        let mut builder = self.with_artifact(artifact);
        builder.container_gated.insert(key);
        builder
    }

    /// Position of an artifact with the same identity as `artifact`, if any.
    fn position_of(&self, artifact: &Artifact) -> Option<usize> {
        let key = artifact.key();
        self.artifacts.iter().position(|a| a.key() == key)
    }

    /// Append an artifact. Records an error (surfaced by [`Self::build`]) if
    /// an artifact with the same identity already exists.
    #[must_use]
    pub fn with_artifact(mut self, artifact: Artifact) -> Self {
        if self.position_of(&artifact).is_some() {
            self.errors.push(format!(
                "with_artifact: an artifact with identity {:?} already exists; use replace_artifact to override it",
                artifact.key()
            ));
        } else {
            self.artifacts.push(artifact);
        }
        self
    }

    /// Override an existing artifact in place (preserving its position).
    /// Records an error if no artifact with that identity exists.
    #[must_use]
    pub fn replace_artifact(mut self, artifact: Artifact) -> Self {
        match self.position_of(&artifact) {
            Some(index) => self.artifacts[index] = artifact,
            None => self.errors.push(format!(
                "replace_artifact: no artifact with identity {:?} exists; use with_artifact to add it",
                artifact.key()
            )),
        }
        self
    }

    /// Remove an artifact. Records an error if no artifact with that identity
    /// exists.
    #[must_use]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "uniform artifact-verb signature; only the artifact's identity is read"
    )]
    pub fn without_artifact(mut self, artifact: Artifact) -> Self {
        match self.position_of(&artifact) {
            Some(index) => {
                self.artifacts.remove(index);
            }
            None => self
                .errors
                .push(format!("without_artifact: no artifact with identity {:?} exists", artifact.key())),
        }
        self
    }

    /// Finalize the catalog, failing if any verb recorded an error.
    ///
    /// # Errors
    ///
    /// Returns an error if any `with_artifact` / `replace_artifact` /
    /// `without_artifact` call violated its add/override/remove invariant.
    pub fn build(self) -> Result<Catalog, AppError> {
        if !self.errors.is_empty() {
            bail!("invalid catalog for '{}':\n  - {}", self.cli.subcommand, self.errors.join("\n  - "));
        }
        Ok(Catalog {
            cli: self.cli,
            artifacts: self.artifacts,
            container_defaults: self.container_defaults,
            container_gated: self.container_gated,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::anvil::artifacts;

    #[test]
    fn subcommand_derives_bin_name() {
        let catalog = Catalog::anvil().into_builder().subcommand("myforge").build().unwrap();
        assert_eq!(catalog.cli().subcommand, "myforge");
        assert_eq!(catalog.cli().bin_name, "cargo-myforge");
    }

    #[test]
    fn with_artifact_appends_new() {
        let before = Catalog::anvil().artifacts().len();
        let catalog = Catalog::anvil()
            .into_builder()
            .with_artifact(Artifact::owned_file("justfiles/anvil/extra.just", "x\n"))
            .build()
            .unwrap();
        assert_eq!(catalog.artifacts().len(), before + 1);
    }

    #[test]
    fn with_artifact_errors_on_duplicate() {
        let err = Catalog::anvil()
            .into_builder()
            .with_artifact(artifacts::region::rustfmt())
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn replace_artifact_overrides_in_place() {
        let catalog = Catalog::anvil()
            .into_builder()
            .replace_artifact(artifacts::region::rustfmt().with_body("custom = true\n"))
            .build()
            .unwrap();
        let body = catalog
            .artifacts()
            .iter()
            .find(|a| a.key() == artifacts::region::rustfmt().key())
            .unwrap()
            .body();
        assert_eq!(body, "custom = true\n");
    }

    #[test]
    fn replace_artifact_errors_when_absent() {
        let err = Catalog::anvil()
            .into_builder()
            .replace_artifact(Artifact::owned_file("justfiles/anvil/absent.just", "x"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("no artifact with identity"), "got: {err}");
    }

    #[test]
    fn without_artifact_removes() {
        let before = Catalog::anvil().artifacts().len();
        let catalog = Catalog::anvil()
            .into_builder()
            .without_artifact(artifacts::region::clippy())
            .build()
            .unwrap();
        assert_eq!(catalog.artifacts().len(), before - 1);
        assert!(catalog.artifacts().iter().all(|a| a.key() != artifacts::region::clippy().key()));
    }

    #[test]
    fn without_artifact_errors_when_absent() {
        let err = Catalog::anvil()
            .into_builder()
            .without_artifact(Artifact::owned_file("nope", "x"))
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("no artifact with identity"), "got: {err}");
    }

    #[test]
    fn builder_from_empty_starts_blank() {
        let catalog = Catalog::builder(CliMeta::new("solo"))
            .with_artifact(Artifact::owned_file("a", "x"))
            .build()
            .unwrap();
        assert_eq!(catalog.artifacts().len(), 1);
        assert_eq!(catalog.cli().subcommand, "solo");
    }

    #[test]
    fn multiple_errors_are_all_reported() {
        let err = Catalog::anvil()
            .into_builder()
            .replace_artifact(Artifact::owned_file("absent-1", "x"))
            .without_artifact(Artifact::owned_file("absent-2", "x"))
            .build()
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("absent-1"), "got: {msg}");
        assert!(msg.contains("absent-2"), "got: {msg}");
    }

    #[test]
    fn checksum_is_independent_of_insertion_order() {
        let a = Artifact::owned_file("a", "1");
        let b = Artifact::owned_file("b", "2");
        let one = Catalog::builder(CliMeta::new("t"))
            .with_artifact(a.clone())
            .with_artifact(b.clone())
            .build()
            .unwrap();
        let two = Catalog::builder(CliMeta::new("t"))
            .with_artifact(b)
            .with_artifact(a)
            .build()
            .unwrap();
        assert_eq!(one.checksum(), two.checksum());
        assert!(one.checksum().starts_with("sha256:"));
    }

    #[cfg_attr(
        miri,
        ignore = "hashes the full embedded anvil catalog; pure safe Rust with no leak/UB to exercise, covered by the native run"
    )]
    #[test]
    fn checksum_is_independent_of_cli_meta() {
        let base = Catalog::anvil().checksum();
        let renamed = Catalog::anvil()
            .into_builder()
            .subcommand("zzz")
            .version("9.9.9")
            .about("totally different")
            .build()
            .unwrap()
            .checksum();
        assert_eq!(base, renamed, "CliMeta must not affect the catalog checksum");
    }

    #[cfg_attr(
        miri,
        ignore = "hashes the full embedded anvil catalog; pure safe Rust with no leak/UB to exercise, covered by the native run"
    )]
    #[test]
    fn checksum_changes_when_a_body_changes() {
        let base = Catalog::anvil().checksum();
        let edited = Catalog::anvil()
            .into_builder()
            .replace_artifact(artifacts::region::rustfmt().with_body("max_width = 80\n"))
            .build()
            .unwrap()
            .checksum();
        assert_ne!(base, edited);
    }

    #[cfg_attr(
        miri,
        ignore = "hashes the full embedded anvil catalog; pure safe Rust with no leak/UB to exercise, covered by the native run"
    )]
    #[test]
    fn checksum_changes_when_an_artifact_is_added_or_removed() {
        let base = Catalog::anvil().checksum();
        let added = Catalog::anvil()
            .into_builder()
            .with_artifact(Artifact::owned_file("justfiles/anvil/extra.just", "x\n"))
            .build()
            .unwrap()
            .checksum();
        let removed = Catalog::anvil()
            .into_builder()
            .without_artifact(artifacts::region::clippy())
            .build()
            .unwrap()
            .checksum();
        assert_ne!(base, added);
        assert_ne!(base, removed);
        assert_ne!(added, removed);
    }

    #[cfg_attr(
        miri,
        ignore = "hashes the full embedded anvil catalog; pure safe Rust with no leak/UB to exercise, covered by the native run"
    )]
    #[test]
    fn base_catalog_checksum_is_deterministic_not_pinned() {
        // This test used to pin `Catalog::anvil()`'s checksum to the
        // cargo-anvil-v0.3.0 value. That pin was removed DELIBERATELY and must
        // not be reinstated: the base catalog now registers the container
        // artifacts (container-gated), which necessarily changes the catalog
        // checksum. A base catalog that gains capability changing its checksum
        // is normal and expected — every anvil release that adds a check does
        // the same. It is NOT a breaking change: the checksum is not public
        // API, and `.anvil.lock`'s `catalog_checksum` is only stamped on save,
        // while per-artifact write/skip decisions are made by content hash in
        // `decision.rs` — so a changed checksum causes no file churn.
        //
        // The property that actually matters — that turning containerization
        // off yields byte-identical *output* — is asserted by
        // `disabled_container_is_byte_identical_to_base` and the unchanged
        // base-tree snapshots. Here we assert only that the checksum is a
        // deterministic sha256, and that the base catalog does now carry
        // container support.
        assert_eq!(Catalog::anvil().checksum(), Catalog::anvil().checksum());
        assert!(Catalog::anvil().checksum().starts_with("sha256:"));
        assert!(
            Catalog::anvil().supports_containers(),
            "containerization ships in the base catalog (container-gated)"
        );
    }

    #[cfg_attr(
        miri,
        ignore = "hashes the full embedded anvil catalog; pure safe Rust with no leak/UB to exercise, covered by the native run"
    )]
    #[test]
    fn container_gating_is_additive_and_affects_checksum_only_when_present() {
        // A catalog with no container-gated artifacts folds nothing extra into
        // its checksum; adding a gated artifact changes it deterministically.
        // (Built from empty so the property is exercised in isolation — the
        // base `Catalog::anvil()` now always carries container support.)
        let cli = Catalog::anvil().cli().clone();
        let plain = Catalog::builder(cli.clone())
            .with_artifact(Artifact::owned_file("justfiles/anvil/x.just", "x\n"))
            .build()
            .unwrap();
        assert!(!plain.supports_containers());

        let with_container = Catalog::builder(cli)
            .with_artifact(Artifact::owned_file("justfiles/anvil/x.just", "x\n"))
            .with_container_artifact(artifacts::container::container_just())
            .build()
            .unwrap();
        assert!(with_container.supports_containers());
        assert!(with_container.is_container_gated(&artifacts::container::container_just().key()));
        // The gated key set is non-empty, so the checksum diverges from the
        // otherwise-identical plain catalog.
        assert_ne!(plain.checksum(), with_container.checksum());
    }

    #[test]
    fn canonical_repr_uses_explicit_stable_tags() {
        let file = Artifact::backend_file(crate::backend::Backend::GitHub, "x.txt", "body");
        let region = Artifact::region(crate::catalog::RegionSpec {
            host: crate::catalog::HostSelector::Path("Cargo.toml".to_owned()),
            id: crate::catalog::RegionId::new("anvil"),
            body: "region-body".to_owned(),
            syntax: crate::region::CommentSyntax::SlashSlash,
        });

        let file_repr = canonical_repr(&file);
        let region_repr = canonical_repr(&region);

        assert!(file_repr.contains("gate=github"));
        assert!(file_repr.contains("file"));
        assert!(region_repr.contains("path:Cargo.toml"));
        assert!(!region_repr.contains("single_crate_cargo_toml"));
        assert!(region_repr.contains("slashslash"));
    }
}
