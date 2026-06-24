// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The **anvil base catalog** — the concrete catalog the `cargo-anvil` binary
//! ships, kept separate from the reusable [`crate::catalog`] engine API.
//!
//! This module owns everything anvil-specific: the embedded templates and the
//! [`artifacts`] registry that wraps them, and the assembled
//! [`Catalog::anvil`] base catalog. The engine consumes a catalog generically
//! — there is no anvil-specific reconciliation in the plan path.

pub mod artifacts;

pub(crate) use artifacts::anvil_artifacts;

use crate::catalog::{Catalog, CliMeta};

impl Catalog {
    /// The built-in base catalog: the `anvil` CLI identity and the full
    /// built-in artifact set.
    #[must_use]
    pub fn anvil() -> Self {
        Self::from_parts(anvil_cli_meta(), anvil_artifacts())
    }
}

/// The CLI identity of the built-in `anvil` tool.
fn anvil_cli_meta() -> CliMeta {
    CliMeta {
        subcommand: "anvil".to_owned(),
        bin_name: "cargo-anvil".to_owned(),
        about: "Update local recipes, cloud-workflow building blocks, and managed regions for the anvil unified build setup".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anvil_catalog_has_identity_and_artifacts() {
        let catalog = Catalog::anvil();
        assert_eq!(catalog.cli().subcommand, "anvil");
        assert_eq!(catalog.cli().bin_name, "cargo-anvil");
        assert!(!catalog.artifacts().is_empty());
    }
}
