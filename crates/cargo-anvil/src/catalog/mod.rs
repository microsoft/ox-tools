// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The catalog: the set of artifacts a tool emits, plus its CLI identity.
//!
//! [`crate::run::build_plan`] iterates a catalog's artifacts and dispatches
//! each to the generic owned-file / managed-region drivers, instead of
//! calling a fixed list of hand-named emitters. This is what lets a
//! downstream tool ship its own catalog while reusing the entire engine.
//!
//! See [`extensibility.md`](../../docs/design/extensibility.md).

pub mod artifact;
pub mod artifacts;

mod anvil;

pub use artifact::{Artifact, HostSelector, OwnedFileSpec, RegionId, RegionSpec};
