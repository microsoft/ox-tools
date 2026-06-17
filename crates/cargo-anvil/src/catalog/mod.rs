// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The reusable catalog engine API.
//!
//! A [`Catalog`] pairs a [`CliMeta`] identity with an ordered set of
//! [`Artifact`]s; [`CatalogBuilder`] customizes one. This module is
//! catalog-engine only — it knows nothing about the `anvil` base catalog,
//! which lives in [`crate::anvil`]. [`crate::run::build_plan`] iterates any
//! catalog's artifacts and dispatches each to the generic owned-file /
//! managed-region drivers, so a downstream tool ships its own catalog while
//! reusing the entire engine.
//!
//! See [`extensibility.md`](../../docs/design/extensibility.md).

pub mod artifact;
pub mod builder;
pub mod meta;

pub use artifact::{Artifact, HostSelector, OwnedFileSpec, RegionId, RegionSpec};
pub use builder::{Catalog, CatalogBuilder};
pub use meta::CliMeta;
