// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-level mutation identity and storage.

mod identity;
mod interner;
mod mutant_definition;
mod mutation_site;

pub(crate) use identity::mutant_id_with_discriminator;
pub use identity::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, MutantId, mutant_id, normalize_site_text, site_key};
pub use interner::Interner;
pub use mutant_definition::MutantDefinition;
pub use mutation_site::MutationSite;
