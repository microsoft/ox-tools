// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The core data model: mutants, their identity, and their verdicts.

mod identity;
mod interner;
mod mutant;
mod outcome;
mod scoring;
mod summary;
mod suppression;

pub use identity::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, MutantId, mutant_id, normalize_site_text, site_key};
pub use interner::Interner;
pub use mutant::{Expectation, Mutant, one_line};
pub use outcome::Outcome;
pub use scoring::Scoring;
pub use summary::Summary;
pub use suppression::{Channel, Suppression};
