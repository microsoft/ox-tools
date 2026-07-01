// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
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
//! cargo anvil [--backend <name>]... [--no-backends] [--dry-run] [--force]
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
//! - `--force` — override the single-tool guard and switch the repository to
//!   this tool, then run a normal update. A repo is managed by exactly one
//!   anvil-family tool (recorded as `tool` in `.anvil.lock`); without
//!   `--force`, a run refuses when that field names a different tool.
//!
//! `--version` prints the build version plus, on a second line, the
//! `catalog:` checksum — a `sha256` over the whole compiled-in catalog — so
//! two builds at the same version but different catalogs are distinguishable.
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
//! cloud workflows invoke the same recipes, so a check behaves identically
//! locally and in cloud workflows — they share one implementation in the
//! imported `.just` files. The one difference is scope: cloud-workflow PR
//! runs perform impact analysis (via [`cargo-delta`](https://crates.io/crates/cargo-delta))
//! and run each check only over the affected packages, whereas a local
//! `just anvil-pr` runs every check over the whole workspace.
//!
//! ## Checks and tiers
//!
//! Checks are grouped into **tiers** (`anvil-pr`, `anvil-scheduled`) that
//! fan out to **groups** (one cloud-workflow job each), which in turn run
//! individual checks sequentially. `anvil-full` runs both tiers.
//!
//! The catalog and per-check rationale live in `docs/design/checks.md`;
//! the table below maps each check to the group(s) that run it.
//!
//! **PR tier** (`anvil-pr`) — runs on every pull request, impact-scoped in
//! cloud workflows:
//!
//! | Group | Checks |
//! |-------|--------|
//! | `pr-fast` | `fmt`, `clippy`, `cargo-sort`, `license-headers`, `ensure-no-cyclic-deps`, `ensure-no-default-features`, `doc-build`, `readme-check`, `spellcheck`, `pr-title`, `deny`, `audit`, `udeps`, `semver-check`, `external-types`, `aprz` |
//! | `pr-test` | `llvm-cov` (coverage), `doc-test`, `examples` |
//! | `pr-runtime-analysis` | `miri`, `careful`, `loom`, `bolero` |
//! | `pr-mutants` | `mutants-diff` (diff-scoped mutation testing) |
//!
//! (`pr-test`, `pr-runtime-analysis`, and `pr-mutants` are sub-recipes of a
//! single `pr-slow` job, run sequentially per OS leg.)
//!
//! **Scheduled tier** (`anvil-scheduled`) — full-workspace, runs on a
//! schedule against the default branch, not on PRs:
//!
//! | Group | Checks |
//! |-------|--------|
//! | `scheduled-test` | `llvm-cov`, `doc-test`, `examples` |
//! | `scheduled-advisories` | `deny`, `audit`, `aprz`, `clippy` (re-run to catch newly-published advisories / lints) |
//! | `scheduled-runtime-analysis` | `miri` and the three stricter miri profiles: `miri-tree-borrows`, `miri-strict-provenance`, `miri-race-coverage` |
//! | `scheduled-exhaustive` | `mutants-full`, `cargo-hack` (feature-powerset), `bench` (compile-only) |
//!
//! What each tool does:
//!
//! - **Formatting / hygiene**: `fmt` (rustfmt), `cargo-sort` (sorted
//!   `Cargo.toml`), `license-headers`, `spellcheck`, `readme-check`
//!   (READMEs match crate docs), `pr-title` (conventional-commit title).
//! - **Linting / API**: `clippy`, `doc-build` (intra-doc links),
//!   `semver-check` (advisory API-break detection), `external-types`
//!   (public API doesn't leak un-approved external types), `udeps`
//!   (unused dependencies), `cargo-hack` (feature-powerset compile).
//! - **Dependency health**: `deny` (licenses / bans / advisories),
//!   `audit` (RUSTSEC), `aprz` (supply-chain risk appraisal),
//!   `ensure-no-cyclic-deps`, `ensure-no-default-features`.
//! - **Tests / coverage**: `doc-test`, `examples`, `llvm-cov` (line
//!   coverage, gated by [`cargo-coverage-gate`](https://crates.io/crates/cargo-coverage-gate)).
//! - **Runtime correctness**: `miri` (UB detection), `careful`
//!   (debug-instrumented std), `loom` (concurrency model checking),
//!   `bolero` (fuzz smoke test, Linux-only).
//! - **Mutation testing**: `mutants-diff` (PR, diff-scoped) and
//!   `mutants-full` (scheduled, whole workspace).
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
//! ## In-tree tool customization
//!
//! anvil follows a few source-level and `Cargo.toml` conventions so you
//! can customize how some of the executed tools behave from within your
//! own crates — without editing the generated `justfiles/anvil/` tree.
//!
//! ### Coverage (`llvm-cov`)
//!
//! Coverage is gated by [`cargo-coverage-gate`](https://crates.io/crates/cargo-coverage-gate);
//! per-package and per-workspace thresholds, the coverage-exclusion
//! attribute, and opt-out are all configured through its `Cargo.toml`
//! metadata conventions — see its documentation.
//!
//! ### Undefined-behavior checking (`miri`)
//!
//! The PR-tier `miri` check runs `cargo miri test --all-features --tests`
//! (libtest, not nextest — process-per-test is roughly twice as slow under miri).
//! Opt a test out of miri when it touches the filesystem, spawns
//! subprocesses, or otherwise can't run under the interpreter:
//!
//! ```text
//! #[cfg_attr(miri, ignore)]
//! ```
//!
//! The **scheduled** tier adds three stricter miri profiles, each of
//! which sets a distinct cfg so you can quarantine a test from one
//! profile without affecting the others (e.g. a test that OOMs only
//! under tree-borrows):
//!
//! ```text
//! #[cfg_attr(miri_tree_borrows,      ignore = "OOMs under -Zmiri-tree-borrows")]
//! #[cfg_attr(miri_strict_provenance, ignore = "int-to-ptr cast by design")]
//! #[cfg_attr(miri_race_coverage,     ignore = "nondeterministic across seeds")]
//! ```
//!
//! ### Concurrency model checking (`loom`)
//!
//! The `loom` check runs only the test targets that opt in, detected
//! **structurally** (no filename/comment heuristic). A crate opts in by
//! declaring a `loom` feature, a dedicated `[[test]]` target that
//! requires it, and a `cfg(loom)`-gated `loom` dependency:
//!
//! ```toml
//! [features]
//! loom = []
//!
//! [[test]]
//! name = "loom"               # tests/loom.rs
//! required-features = ["loom"]
//!
//! [target.'cfg(loom)'.dependencies]
//! loom = "0.7"
//! ```
//!
//! In source, swap std atomics for loom's under the cfg
//! (`#[cfg(loom)] use loom::sync::atomic::...`). The recipe builds those
//! targets with `--cfg loom`, per-package so the cfg never leaks into
//! other members' dependencies. It is **fail-loud**: a crate that
//! declares loom support (a `loom` feature or a `cfg(loom)` dependency)
//! but ships no such test target errors out rather than silently
//! skipping. When no crate ships a loom target the check is a no-op.
//!
//! ### Note: `careful` self-cleans on a toolchain bump
//!
//! Not a knob, but worth knowing: the `careful` check builds an
//! instrumented std into a version-stable cache that cargo's fingerprint
//! can't see, so on a pinned-nightly or cargo-careful bump it runs
//! `cargo clean` once (announced in the log) to avoid linking stale
//! artifacts against a freshly rebuilt std.
//!
//! ## Extensibility: shipping your own tool
//!
//! Another team can ship its own cargo subcommand with its own catalog while
//! reusing this entire engine. The downstream binary's `main` is one line:
//!
//! ```no_run
//! use std::process::ExitCode;
//!
//! fn main() -> ExitCode {
//!     cargo_anvil::run_app(myforge::catalog())
//! }
//! # mod myforge { pub fn catalog() -> cargo_anvil::Catalog { cargo_anvil::Catalog::anvil() } }
//! ```
//!
//! …plus a [`Catalog`] value that starts from [`Catalog::anvil`] and
//! customizes the CLI identity ([`CliMeta`]) and artifact set:
//!
//! ```no_run
//! use cargo_anvil::{Artifact, Catalog, artifacts};
//!
//! pub fn catalog() -> Catalog {
//!     Catalog::anvil()
//!         .into_builder()
//!         .subcommand("myforge")
//!         .with_artifact(Artifact::owned_file(
//!             "justfiles/anvil/extra.just",
//!             "# ...\n",
//!         ))
//!         .replace_artifact(artifacts::region::rustfmt().with_body("max_width = 80\n"))
//!         .without_artifact(artifacts::region::clippy())
//!         .build()
//!         .expect("valid catalog")
//! }
//! ```
//!
//! The on-disk vocabulary (`.anvil.lock`, `anvil-managed` sentinels,
//! `justfiles/anvil/`, `anvil-` recipes) is the fixed engine format and is
//! never rebranded. A fork customizes only its CLI identity and which
//! artifacts it emits, via the three uniform builder verbs
//! ([`CatalogBuilder::with_artifact`], [`CatalogBuilder::replace_artifact`],
//! [`CatalogBuilder::without_artifact`]) over the public [`artifacts`]
//! registry. The `tool` field recorded in `.anvil.lock` keeps two
//! anvil-family tools from clobbering one another in a shared repo (see `--force`).
//! See `docs/design/extensibility.md`.
//!
//! ## Design docs
//!
//! See `docs/design/` for the full architecture:
//!
//! - `design.md` — overall principles and CLI shape.
//! - `checks.md` — the opinionated check catalog.
//! - `local.md` — the `justfiles/anvil/` tree.
//! - `updates.md` — the drift-detection algorithm.
//! - `extensibility.md` — how downstream tools ship their own catalog.
//! - `github.md` — GitHub Actions emission.
//! - `ado.md` — Azure DevOps Pipelines emission.
//!
//! And `docs/verification.md` for the continuous-validation strategy.

#![deny(unsafe_code)]

pub(crate) mod anvil;
pub(crate) mod backend;
pub(crate) mod catalog;
pub(crate) mod checksum;
pub(crate) mod cli;
pub(crate) mod decision;
pub(crate) mod emit;
pub(crate) mod io;
pub(crate) mod manifest;
pub(crate) mod plan;
pub(crate) mod region;
pub(crate) mod run;
pub(crate) mod workspace;

/// Engine internals exposed **only** for this crate's own integration tests
/// (under `tests/`). This is not part of the public API: it is hidden from
/// the docs and may change or disappear at any time. Downstream tool authors
/// must not depend on it — use the crate-root surface (`Catalog`, `Artifact`,
/// `artifacts`, `run_app`, …) instead.
#[doc(hidden)]
pub mod test_support {
    pub use crate::cli::Cli;
    pub use crate::decision::Decision;
    pub use crate::manifest::{MANIFEST_FILE_NAME, Manifest};
    pub use crate::plan::Target;
    pub use crate::region::upsert_region;
    pub use crate::run::{RunOutcome, run_update};

    /// The rustfmt managed-region id, for integration tests.
    ///
    /// Tests use it to exercise the region opt-out / leave-alone behavior.
    /// Exposed as an accessor (rather than a re-export) so the underlying
    /// constant stays `pub(crate)`.
    #[must_use]
    pub fn rustfmt_region_id() -> &'static str {
        crate::anvil::artifacts::region::RUSTFMT_REGION_ID
    }
}

use std::process::ExitCode;

// Crate-root re-exports are limited to what a downstream tool author needs to
// describe a catalog and run it (see `docs/design/extensibility.md`):
// `run_app` (below), the catalog builder surface, the artifact model, the
// backend enum, and the `artifacts` registry. Everything else — the manifest,
// plan, decision table, region splicing, workspace discovery, the CLI parser,
// and the checksum helpers — is engine internals; it stays crate-private and
// is deliberately not surfaced at the crate root.
pub use anvil::artifacts;
pub use backend::Backend;
pub use catalog::{Artifact, Catalog, CatalogBuilder, CliMeta, HostSelector, OwnedFileSpec, RegionId, RegionSpec};
pub use region::CommentSyntax;

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
#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Entry point: tracing/clap setup + dispatch to run; behavior is integration-tested via run_update.
#[expect(
    clippy::needless_pass_by_value,
    reason = "public one-call entry point that owns the catalog for the process lifetime by design"
)]
pub fn run_app(catalog: Catalog) -> ExitCode {
    use tracing_subscriber::fmt::format::FmtSpan;

    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let cli = match cli::Cli::parse_from_cargo_args(&catalog, std::env::args_os()) {
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
