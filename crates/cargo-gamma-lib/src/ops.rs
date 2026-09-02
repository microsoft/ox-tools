// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coordinator access to engine-owned mutation operators and collection.

pub mod collect;
pub mod registry;

#[doc(inline)]
pub use collect::{Candidate, collect as collect_candidates, into_mutants};
#[doc(inline)]
pub use registry::{Mutator, Preset, Selection, families, find, find_preset, resolve};
