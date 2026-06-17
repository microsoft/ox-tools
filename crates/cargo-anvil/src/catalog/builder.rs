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

use crate::catalog::anvil::anvil_artifacts;
use crate::catalog::artifact::Artifact;
use crate::catalog::meta::CliMeta;

/// The set of artifacts a tool emits, plus its CLI identity.
#[derive(Debug, Clone)]
pub struct Catalog {
    cli: CliMeta,
    artifacts: Vec<Artifact>,
}

impl Catalog {
    /// The built-in base catalog: the `anvil` CLI identity and the full
    /// built-in artifact set.
    #[must_use]
    pub fn anvil() -> Self {
        Self {
            cli: CliMeta::anvil(),
            artifacts: anvil_artifacts(),
        }
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
    use crate::catalog::artifacts;

    #[test]
    fn anvil_catalog_has_identity_and_artifacts() {
        let catalog = Catalog::anvil();
        assert_eq!(catalog.cli().subcommand, "anvil");
        assert_eq!(catalog.cli().bin_name, "cargo-anvil");
        assert!(!catalog.artifacts().is_empty());
    }

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
}
