// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Owned-file and managed-region emitters.
//!
//! Each emitter produces a [`PlanItem`] given the workspace and the
//! previous manifest. The driver in [`crate::run`] collects them into a
//! [`Plan`] and either applies or summarizes it.

pub mod local;
pub mod owned_file;

pub use owned_file::plan_owned_file;
