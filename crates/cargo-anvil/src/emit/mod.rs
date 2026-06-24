// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic plan drivers.
//!
//! Two reusable engine drivers turn a catalog artifact into a
//! [`crate::plan::PlanItem`] for a single repository: [`plan_owned_file`] for
//! a fully tool-owned file and [`plan_managed_region`] for a sentinel-
//! delimited region spliced into a host file. They are catalog-agnostic — the
//! engine's plan builder feeds them the bodies the catalog supplies. The
//! anvil base catalog's templates live in [`crate::anvil`].

pub mod managed_region;
pub mod owned_file;

pub use managed_region::plan_managed_region;
pub use owned_file::plan_owned_file;
