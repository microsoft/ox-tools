// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Source-level mutation identity and storage.

mod identity;
mod interner;
mod mutant_definition;
mod mutation_site;

pub(crate) use identity::mutant_id_with_discriminator;
#[doc(inline)]
pub use identity::{MUTANT_ID_HEX_LEN, MUTANT_ID_VERSION, MutantId, SiteIndex, mutant_id, normalize_site_text, site_key};
#[doc(inline)]
pub use interner::Interner;
#[doc(inline)]
pub use mutant_definition::MutantDefinition;
#[doc(inline)]
pub use mutation_site::MutationSite;
