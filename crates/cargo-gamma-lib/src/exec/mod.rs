// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Building the mutant schema once and running the test suite against each mutant.
//!
//! A run copies the workspace to a scratch tree, instruments every mutated file, builds the test
//! binaries once, measures a baseline with no mutant active, then runs the suite once per mutant
//! with `GAMMA_ACTIVE` naming the one that is live. Every test process, baseline included, gets
//! `CARGO_GAMMA=1` so a suite that drives cargo itself can opt out of a nested build.
//!
//! Mutants that cannot compile — replacing a return value with `Default::default()` when the type
//! is not `Default`, for instance — are attributed back to the guards that caused them, withdrawn,
//! and the build is retried; a handful of rollback rounds converges.

mod baseline;
mod build;
mod cargo_options;
mod census;
mod config;
mod copy;
mod events;
#[cfg(test)]
mod faults;
mod harness_filters;
mod incremental_mode;
mod killers;
mod loader;
mod manifest;
// Exposed to integration tests, which need to ask whether this host can bound memory at all before
// they can say what a run should have done. See `declare_modules!` in `lib.rs` for the convention.
mod measure;
#[cfg(feature = "internals")]
pub mod memory;
#[cfg(not(feature = "internals"))]
mod memory;
mod nextest;
mod progress;
#[cfg(target_os = "linux")]
pub(crate) mod relaunch;
mod session;
mod stall;
mod sweep;
mod sync;
mod test_binary;
mod verdict;
mod workspace;

pub use build::{OrderingHints, Round, Withdrawal};
pub use cargo_options::{BuildLimits, CargoOptions, DEFAULT_ROLLBACK_ROUNDS};
pub use config::Config;
pub(crate) use config::{available_parallelism, resolve_jobs};
pub use events::Events;
pub use incremental_mode::IncrementalMode;
pub use loader::UNDER_GAMMA_VAR;
pub(crate) use manifest::RUNTIME_CRATE;
pub use measure::{Built, Measured, Oracle, measure, run};
pub use memory::{DEFAULT_HEADROOM, DEFAULT_MULTIPLIER, Demand, MemoryControl, MemoryPolicy};
// Named for its subject at this level, where `support` alone would say nothing about what is
// supported. The module itself is private unless the `internals` feature exposes it.
pub(crate) use memory::{implied_memory_control, support as memory_support};
pub use session::{CensusCost, Phases, Session, SweepCost};
pub use test_binary::TestBinary;
pub(crate) use verdict::CONFIRM_FACTOR;
pub use verdict::READERS;
#[cfg(loom)]
pub(crate) use verdict::run_loom_models;
pub use workspace::{Workspace, clean_cache, footprint, gamma_base, scratch_tree};
pub(crate) use workspace::{claim_cache, claim_workspace};
