// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # cargo-ox-check
//!
//! Opinionated, unified Rust build/CI scaffolding for GitHub Actions and
//! Azure DevOps Pipelines. One opinionated check catalog, two CI
//! backends, generated from the same source of truth.
//!
//! ## What it does
//!
//! `cargo-ox-check` writes files. `just` runs them. The repo composes
//! everything. The tool itself is not on the local-build hot path or in
//! the CI graph at runtime — it is a code generator that you re-run when
//! you want to upgrade the opinionated baseline.
//!
//! Each run of `cargo ox-check update` writes:
//!
//! - The `justfiles/ox-check/` recipe tree (`tools.just`, `checks.just`,
//!   `groups.just`, `tiers.just`) — owned files.
//! - A managed region in your `Justfile` that imports them.
//! - A managed region in your workspace `Cargo.toml` carrying
//!   `[workspace.lints]` in dotted-key form, plus a `[lints] workspace =
//!   true` region in each workspace member.
//! - Managed regions in `deny.toml`, `rustfmt.toml`, and `.delta.toml`.
//! - For each selected CI backend (`github`, `ado`), the full set of
//!   composite actions / step templates, reusable workflows / stages
//!   templates, and root workflows / pipelines.
//!
//! Outside the managed regions, your content is preserved byte-for-byte.
//!
//! ## Installation
//!
//! ```bash
//! cargo install --locked cargo-ox-check
//! ```
//!
//! Only the maintainer who runs updates needs the binary. Everyone else
//! uses `just` (or plain `cargo`).
//!
//! ## Usage
//!
//! ```text
//! cargo ox-check update [--backend <name>]... [--no-backends] [--dry-run]
//! ```
//!
//! `update` is the only subcommand. There is no separate `init`,
//! `migrate`, `check`, `enable`, or `disable`. The algorithm is uniform
//! — first runs and subsequent runs go through the same decision table.
//!
//! Flags:
//!
//! - `--backend <name>` — repeatable. Valid values: `github`, `ado`. If
//!   omitted, the backend is autodetected from the `origin` git remote.
//! - `--no-backends` — emit only local files; skip every CI backend.
//!   Mutually exclusive with `--backend`.
//! - `--dry-run` — analyze without writing. Exits 1 if anything would be
//!   written or proposed.
//!
//! ## Daily driver
//!
//! After the first run, your daily workflow is plain `just`:
//!
//! ```text
//! $ just ox-check          # alias for `just ox-check-pr`
//! $ just ox-check-pr       # the PR tier
//! $ just ox-check-nightly  # the nightly tier
//! $ just ox-check-full     # both, sequentially
//! ```
//!
//! CI invokes the same recipes. Local and CI are bit-identical because
//! they share one implementation in the imported `.just` files.
//!
//! ## Customization
//!
//! Four escape valves, in increasing severity:
//!
//! 1. **Compose around the tool**: add your own `.just` files or
//!    workflows; the tool never touches anything not prefixed
//!    `ox-check-`.
//! 2. **Extend managed regions** outside the sentinels — add lints,
//!    deny rules, etc. The tool preserves everything outside.
//! 3. **Opt out by emptying** a managed region or owned file. The tool
//!    will skip the item on every future `update` and only emit a
//!    `.ox-check-proposed` sibling when the template actually changes.
//! 4. **Take ownership by editing inside** an owned file or managed
//!    region. The next `update` detects the dirt and writes a
//!    `.ox-check-proposed` sibling instead of overwriting.
//!
//! ## Design docs
//!
//! See `docs/design/` for the full architecture:
//!
//! - `design.md` — overall principles and CLI shape.
//! - `checks.md` — the opinionated check catalog.
//! - `local.md` — the `justfiles/ox-check/` tree.
//! - `updates.md` — the drift-detection algorithm.
//! - `github.md` — GitHub Actions emission.
//! - `ado.md` — Azure DevOps Pipelines emission.
//!
//! And `docs/verification.md` for the continuous-validation strategy.

#![deny(unsafe_code)]

pub mod backend;
pub mod checksum;
pub mod cli;
pub mod decision;
pub mod emit;
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
