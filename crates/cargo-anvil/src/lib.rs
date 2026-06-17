// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "panic-on-failure idioms are appropriate in tests"
    )
)]

//! # cargo-anvil
//!
//! Opinionated, unified Rust build and cloud-workflow scaffolding for GitHub Actions and
//! Azure DevOps Pipelines. One opinionated check catalog, two cloud workflows
//! backends, generated from the same source of truth.
//!
//! ## What it does
//!
//! `cargo-anvil` writes files. `just` runs them. The repo composes
//! everything. The tool itself is not on the local-build hot path or in
//! the cloud-workflow graph at runtime — it is a code generator that you re-run when
//! you want to upgrade the opinionated baseline.
//!
//! Each run of `cargo anvil` writes:
//!
//! - The `justfiles/anvil/` recipe tree (`tools.just`, `checks.just`,
//!   `groups.just`, `tiers.just`) — owned files.
//! - A managed region in your `Justfile` that imports them.
//! - A managed region in your workspace `Cargo.toml` carrying
//!   `[workspace.lints]` in dotted-key form, plus a `[lints] workspace =
//!   true` region in each workspace member.
//! - Managed regions in `deny.toml`, `rustfmt.toml`, and `.delta.toml`.
//! - For each selected cloud-workflow backend (`github`, `ado`), the full set of
//!   composite actions / step templates, reusable workflows / stages
//!   templates, and root workflows / pipelines.
//!
//! Outside the managed regions, your content is preserved byte-for-byte.
//!
//! ## Installation
//!
//! ```bash
//! cargo install --locked cargo-anvil
//! ```
//!
//! Only the maintainer who runs updates needs the binary. Everyone else
//! uses `just` (or plain `cargo`).
//!
//! ## Usage
//!
//! ```text
//! cargo anvil [--backend <name>]... [--no-backends] [--dry-run]
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
//! - `--no-backends` — emit only local files; skip every cloud-workflow backend.
//!   Mutually exclusive with `--backend`.
//! - `--dry-run` — analyze without writing. Exits 1 if anything would be
//!   written or proposed.
//!
//! ## Daily driver
//!
//! After the first run, your daily workflow is plain `just`:
//!
//! ```text
//! $ just anvil          # alias for `just anvil-pr`
//! $ just anvil-pr       # the PR tier
//! $ just anvil-scheduled  # the scheduled tier
//! $ just anvil-full     # both, sequentially
//! ```
//!
//! cloud workflows invokes the same recipes. Local and cloud-workflow runs are bit-identical because
//! they share one implementation in the imported `.just` files.
//!
//! ## Customization
//!
//! Four escape valves, in increasing severity:
//!
//! 1. **Compose around the tool**: add your own `.just` files or
//!    workflows; the tool never touches anything not prefixed
//!    `anvil-`.
//! 2. **Extend managed regions** outside the sentinels — add lints,
//!    deny rules, etc. The tool preserves everything outside.
//! 3. **Opt out by emptying** a managed region or owned file. The tool
//!    will skip the item on every future `update` and only emit a
//!    `.anvil-proposed` sibling when the template actually changes.
//! 4. **Take ownership by editing inside** an owned file or managed
//!    region. The next `update` detects the dirt and writes a
//!    `.anvil-proposed` sibling instead of overwriting.
//!
//! ## Design docs
//!
//! See `docs/design/` for the full architecture:
//!
//! - `design.md` — overall principles and CLI shape.
//! - `checks.md` — the opinionated check catalog.
//! - `local.md` — the `justfiles/anvil/` tree.
//! - `updates.md` — the drift-detection algorithm.
//! - `extensibility.md` — how downstream tools ship their own brand + catalog.
//! - `github.md` — GitHub Actions emission.
//! - `ado.md` — Azure DevOps Pipelines emission.
//!
//! And `docs/verification.md` for the continuous-validation strategy.

#![deny(unsafe_code)]

pub mod backend;
pub mod catalog;
pub mod checksum;
pub mod cli;
pub mod decision;
pub mod emit;
pub mod io;
pub mod manifest;
pub mod plan;
pub mod region;
pub mod run;
pub mod workspace;

use std::process::ExitCode;

pub use backend::Backend;
pub use catalog::{Artifact, Catalog, CatalogBuilder, CliMeta, HostSelector, OwnedFileSpec, RegionId, RegionSpec, artifacts};
pub use checksum::{checksum_bytes, checksum_str};
pub use cli::Cli;
pub use decision::{Decision, DecisionInputs, decide};
pub use manifest::{Manifest, RegionKey};
pub use plan::{Plan, PlanItem, Target};
pub use region::{CommentSyntax, Region, find_region, render_region, upsert_region};
pub use run::run;
pub use workspace::{Workspace, WorkspaceMember};

/// One-call entry point for a tool built on the anvil engine.
///
/// This is the body of `cargo-anvil`'s own `main`, generalized over a
/// [`Catalog`]: it initializes tracing, parses argv against the catalog's
/// CLI identity, runs the update, and maps the result to an [`ExitCode`].
/// A downstream binary's entire `main` is therefore one line:
///
/// ```no_run
/// use std::process::ExitCode;
///
/// use cargo_anvil::Catalog;
///
/// fn main() -> ExitCode {
///     cargo_anvil::run_app(Catalog::anvil())
/// }
/// ```
#[must_use]
#[mutants::skip] // Entry point: tracing/clap setup + dispatch to run; behavior is integration-tested via run_update.
pub fn run_app(catalog: Catalog) -> ExitCode {
    use tracing_subscriber::fmt::format::FmtSpan;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let cli = match Cli::parse_from_cargo_args(&catalog, std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            // clap formats and prints the help/error itself.
            err.exit();
        }
    };

    match run::run(&catalog, &cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
