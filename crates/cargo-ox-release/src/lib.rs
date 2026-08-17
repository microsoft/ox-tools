// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-ox-release`: a deterministic release planner for Oxidizer-style Cargo
//! workspaces.
//!
//! Releasing a workspace of many interdependent crates happens in three phases:
//!
//! 1. **facts** — gather a deterministic workspace snapshot: the dependency
//!    graph, public type exposure, macro publication, and modification state.
//! 2. **resolve** — [`resolve`] turns that snapshot plus the caller's classified
//!    decisions into an exact release plan: token parsing, version arithmetic,
//!    type- and macro-contract-aware cascades, pin reconciliation, ambiguity
//!    reporting, and topological ordering. This crate implements this phase.
//! 3. **apply** — write the new versions, changelogs, and READMEs atomically.
//!
//! The facts snapshot is consumed as JSON (see [`Facts`]); gathering it and
//! applying a plan are separate concerns outside this crate's current scope.
//!
//! The resolver performs only mechanical work — classifying source diffs and
//! reviewing proc-macro behavior are the caller's responsibility, supplied
//! through the [`Request`]. Given the same facts and request it always produces
//! the same plan.

mod cli;
pub mod model;
mod resolve;
mod version;

pub use cli::run_main;
pub use model::{
    Ambiguity, CascadeReasonOutput, ExposureProbe, ExternalDepChange, Facts, MacroCompileFixtureChange, MacroContractOutput, PackageFact,
    Plan, PlanStatus, RegressionEvidenceOutput, ReleaseOutput, ReleaseSource, Request, SelectionDecisionOutput,
};
pub use resolve::resolve;
pub use version::ChangeType;
