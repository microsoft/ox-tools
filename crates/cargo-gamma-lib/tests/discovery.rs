// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Physical module identities, package discovery, and declaration-source generations.

#[path = "discovery/module_aliases.rs"]
mod module_aliases;
#[path = "discovery/source_generations.rs"]
mod source_generations;
#[path = "discovery/type_identities.rs"]
mod type_identities;
