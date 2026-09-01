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

#[doc(inline)]
pub use catalog::{PRESETS, REGISTRY};
#[doc(inline)]
pub use lookup::{families, find, find_preset, resolve};
#[doc(inline)]
pub use mutator::Mutator;
#[doc(inline)]
pub use preset::Preset;
#[doc(inline)]
pub use selection::Selection;
