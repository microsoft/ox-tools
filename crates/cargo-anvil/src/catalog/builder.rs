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

use ohno::{AppError, bail};

use crate::catalog::artifact::Artifact;
use crate::catalog::meta::CliMeta;
use crate::checksum::checksum_str;

/// The set of artifacts a tool emits, plus its CLI identity.
#[derive(Debug, Clone)]
pub struct Catalog {
    cli: CliMeta,
    artifacts: Vec<Artifact>,
}

impl Catalog {
    /// Assemble a catalog from its parts. The base-catalog constructor
    /// ([`Catalog::anvil`], defined in the `anvil` module) and the builder go
    /// through this so the fields stay private to the reusable engine.
    pub(crate) fn from_parts(cli: CliMeta, artifacts: Vec<Artifact>) -> Self {
        Self { cli, artifacts }
    }

    /// Start a new, empty catalog from a CLI identity.
    #[must_use]
    pub fn builder(cli: CliMeta) -> CatalogBuilder {
        CatalogBuilder {
            cli,
            artifacts: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Start a builder from this catalog, to customize it.
    #[must_use]
    pub fn into_builder(self) -> CatalogBuilder {
        CatalogBuilder {
            cli: self.cli,
            artifacts: self.artifacts,
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
        checksum_str(&entries.join("\n"))
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
        })
    }
}

#[cfg(test)]
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
