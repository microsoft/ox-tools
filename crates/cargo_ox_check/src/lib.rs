// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # cargo-ox-check
//!
//! Opinionated, unified Rust build/CI scaffolding for GitHub Actions and Azure
//! DevOps Pipelines.
//!
//! See the design documents under `docs/design/` for the full architecture.
//! `cargo-ox-check` writes files; `just` runs them; the repo composes everything.
//!
//! ## CLI surface
//!
//! ```text
//! cargo ox-check update [--backend <name>]... [--no-backends] [--dry-run]
//! ```
//!
//! `update` is the only subcommand. Repeat `--backend` to emit multiple backends
//! (`github`, `ado`). `--no-backends` skips CI emission entirely. `--dry-run`
//! performs the same analysis but writes nothing and returns a non-zero exit
//! code if anything is out of date.
//!
//! This crate is split into:
//!
//! - [`cli`] — argument parsing and validation.
//! - [`run`] — the top-level `update` driver (no-op in this commit).

#![deny(unsafe_code)]

pub mod backend;
pub mod checksum;
pub mod cli;
pub mod decision;
pub mod manifest;
pub mod plan;
pub mod region;
pub mod run;
pub mod workspace;

pub use backend::Backend;
pub use checksum::{checksum_bytes, checksum_str};
pub use cli::{Cli, Command, UpdateArgs};
pub use decision::{Decision, DecisionInputs, decide};
pub use manifest::{Manifest, RegionKey};
pub use plan::{Plan, PlanItem, Target};
pub use region::{CommentSyntax, Region, find_region, render_region, upsert_region};
pub use run::run;
pub use workspace::{Workspace, WorkspaceMember};
