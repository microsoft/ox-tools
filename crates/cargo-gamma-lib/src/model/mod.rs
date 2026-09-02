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

#[doc(inline)]
pub use identity::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, MutantId, SiteIndex, mutant_id, normalize_site_text, site_key};
#[doc(inline)]
pub use interner::Interner;
#[doc(inline)]
pub use mutant::{Expectation, Mutant, one_line};
#[doc(inline)]
pub use outcome::Outcome;
#[doc(inline)]
pub use scoring::Scoring;
#[doc(inline)]
pub use summary::Summary;
#[doc(inline)]
pub use suppression::{Channel, Suppression};
