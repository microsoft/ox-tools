// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The engine's mutator registry, under the path this crate's callers use.
//!
//! Named one item at a time rather than glob-re-exported. A glob would make every future public
//! item in the engine's registry part of this crate's surface the moment it was added there, which
//! is a decision taken in the wrong crate: adding a helper for the engine's own use would silently
//! publish it here.

pub use cargo_gamma_engine::ops::registry::{Mutator, PRESETS, Preset, REGISTRY, Selection, families, find, find_preset, resolve};
