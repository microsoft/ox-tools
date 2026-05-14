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

pub mod cli;
pub mod run;

pub use cli::{Cli, Command, UpdateArgs};
pub use run::run;
