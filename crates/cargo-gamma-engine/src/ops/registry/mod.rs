// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The mutator registry: stable names, families, presets, and selector resolution.
//!
//! Every mutator has one stable, well-known name of the form `family.transform`. That name is the
//! single vocabulary used by all three suppression channels, by `--mutators`, by the report, and by
//! configuration. Nothing anywhere refers to a mutator by index, by description string, or by
//! position in a list, because all three of those change when the catalog grows.

mod catalog;
mod lookup;
mod mutator;
mod preset;
mod selection;

pub use catalog::{PRESETS, REGISTRY};
pub use lookup::{families, find, find_preset, resolve};
pub use mutator::Mutator;
pub use preset::Preset;
pub use selection::Selection;
