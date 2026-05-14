// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Owned-file and managed-region emitters.
//!
//! Each emitter produces a [`PlanItem`] given the workspace and the
//! previous manifest. The driver in [`crate::run`] collects them into a
//! [`Plan`] and either applies or summarizes it.

pub mod cargo_toml;
pub mod github;
pub mod local;
pub mod managed_region;
pub mod owned_file;
pub mod shared_configs;

pub use managed_region::plan_managed_region;
pub use owned_file::plan_owned_file;
