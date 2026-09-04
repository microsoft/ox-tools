// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-anvil/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-anvil/favicon.ico")]
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
//! `cargo-anvil` gives a Rust repository a complete, maintained set of build
//! checks for local development and continuous integration. It standardizes
//! common engineering work, including formatting, linting, tests, coverage, dependency
//! policy, minimum-Rust-version testing, undefined-behavior checks, and
//! mutation testing, without requiring each repository to design and maintain
//! that infrastructure independently.
//!
//! Run it once to add the build system to a repository. Run it again whenever
//! the repository should adopt a newer cargo-anvil release. The generated
//! files are committed with the source, so builds do not download or execute
//! cargo-anvil itself.
//!
//! ## What you get
//!
//! A generated setup includes:
//!
//! - commands for individual checks, groups of related checks, and complete
//!   pull-request or scheduled validation tiers;
//! - pinned Rust nightly and Cargo tool versions;
//! - impact analysis that limits pull-request work to affected crates while
//!   retaining full-workspace scheduled validation;
//! - GitHub Actions workflows, Azure DevOps pipelines, or both;
//! - shared formatting, lint, dependency, spelling, and impact-analysis
//!   configuration;
//! - safe update metadata that distinguishes generated content from
//!   repository-owned customization.
//!
//! The default catalog is opinionated. Its purpose is to provide one strong,
//! coherent baseline rather than a collection of switches that every
//! repository must assemble differently.
//!
//! ## Install and adopt
//!
//! Install the generator:
//!
//! ```console
//! cargo install --locked cargo-anvil
//! ```
//!
//! From the root of a Rust repository, generate the local commands and
//! autodetect the cloud provider from the `origin` remote:
//!
//! ```console
//! cargo anvil
//! ```
//!
//! A backend can also be selected explicitly:
//!
//! ```console
//! cargo anvil --backend github
//! cargo anvil --backend ado
//! cargo anvil --backend github --backend ado
//! cargo anvil --no-backends
//! ```
//!
//! Commit the generated files. Teammates and CI runners need the generated
//! files and their declared tools, but they do not need the cargo-anvil binary.
//!
//! `cargo anvil --dry-run` reports drift without writing. It exits unsuccessfully
//! when an update is pending, generated state is inconsistent, or content
//! cannot be inspected safely. `cargo anvil --force` permits an intentional
//! switch from another tool built on the cargo-anvil engine.
//!
//! ## Running checks
//!
//! The generated command interface uses
//! [`just`](https://github.com/casey/just), a cross-platform command runner.
//! A repository's `Justfile` imports the generated recipes under
//! `justfiles/anvil/`; each recipe invokes ordinary Rust and Cargo tools.
//!
//! Run the complete pull-request tier locally:
//!
//! ```console
//! just anvil-pr
//! ```
//!
//! Run only the fast checks when a change cannot affect executable behavior:
//!
//! ```console
//! just anvil-pr-fast
//! ```
//!
//! Other useful entry points are:
//!
//! ```console
//! just anvil-build
//! just anvil-build --package my-crate
//! just anvil-doc-build --open
//! just anvil-examples --run
//! just anvil-fmt --fix
//! just anvil-miri --package my-crate --example basic
//! just anvil-readme --fix
//! just anvil-scheduled
//! just anvil-full
//! ```
//!
//! Individual checks are also available as `anvil-<check>` recipes. Run
//! `just --list` to see the generated interface and `just --usage <recipe>` to
//! see a recipe's options.
//!
//! ## Pull-request and scheduled validation
//!
//! The pull-request tier is impact-scoped. It computes which workspace
//! packages changed and which packages depend on them, then gives each check
//! the scope required for correctness. Independent groups run in parallel in
//! the generated cloud workflow:
//!
//! - **fast checks** cover formatting, Clippy, documentation, dependency
//!   policy, spelling, README drift, API compatibility, and metadata;
//! - **tests and coverage** run tests, documentation tests, coverage gates,
//!   and example compilation;
//! - **MSRV tests** execute affected tests with the repository's declared
//!   minimum supported Rust version;
//! - **runtime analysis** runs Miri, cargo-careful, Loom, and a short Bolero
//!   fuzzing pass;
//! - **mutation testing** tests mutants in the pull-request diff.
//!
//! The scheduled tier runs full-workspace backstops and expensive checks that
//! do not belong on every pull request, including extended Miri profiles, full
//! mutation testing, feature-powerset checks, and benchmark compilation.
//! GitHub scheduled failures can be published to a durable issue.
//!
//! Local and cloud runs call the same generated recipes with the same check
//! arguments. Cloud workflows add orchestration for matrices, permissions,
//! artifacts, comments, and status reporting, but do not re-implement the
//! checks.
//!
//! ## Rust toolchains and tools
//!
//! Ordinary checks select Rust deterministically, in this order:
//!
//! 1. a caller-provided `RUSTUP_TOOLCHAIN`;
//! 2. a root `rust-toolchain` or `rust-toolchain.toml`;
//! 3. the root package or workspace `rust-version`.
//!
//! Repositories with none of these fail instead of silently using a runner's
//! ambient compiler. Nightly-only checks use catalog pins. The MSRV group
//! separately installs and tests the minimum version declared by the
//! repository.
//!
//! Tool installation is explicit:
//!
//! ```console
//! just anvil-setup
//! just anvil-pr-fast-setup
//! ```
//!
//! Generated checks validate their prerequisites before executing and provide
//! an installation hint when something is missing.
//!
//! ## Safe updates and customization
//!
//! cargo-anvil manages two kinds of content:
//!
//! - **owned files**, such as recipes and workflow implementations;
//! - **managed regions** inside repository files such as `Cargo.toml`,
//!   `rustfmt.toml`, and `deny.toml`.
//!
//! Checksums in `.anvil.lock` record the last generated state. Unmodified
//! content updates automatically. Repository-owned text outside managed
//! regions is preserved. If managed content was edited, cargo-anvil preserves
//! it and writes an `.anvil-proposed` sibling when the catalog changes, making
//! the conflict visible instead of overwriting the customization.
//!
//! Common repository policy remains in source and configuration rather than
//! generated scripts. Examples include per-package coverage thresholds,
//! spelling dictionaries, tests ignored by a specific Miri profile, Loom test
//! targets, and examples excluded from automatic execution.
//!
//! ## Containerized local execution
//!
//! Any generated command can run in an optional content-addressed Linux
//! container:
//!
//! ```console
//! just anvil-container just anvil-pr
//! just anvil-container just anvil-clippy
//! just anvil-container cargo build
//! ```
//!
//! The image contains the same tool versions used by the generated checks.
//! Its tag is derived from the Dockerfile, toolchain, and generated recipe
//! tree, so changes to those inputs select a different image. Docker is
//! supported; Podman is available on a best-effort basis. Repositories that
//! need private feeds can add a host-side credential hook without embedding
//! credentials in the image or command line. See
//! `docs/design/containers.md` for setup, security boundaries, and advanced
//! customization.
//!
//! ## Building another tool on the engine
//!
//! The crate also exposes the catalog engine used by `cargo-anvil`. A
//! downstream tool can start from [`Catalog::anvil`], add, replace, or remove
//! [`Artifact`] values, select its own CLI identity, and pass the result to
//! [`run_app`]:
//!
//! ```no_run
//! use std::process::ExitCode;
//!
//! use cargo_anvil::{Artifact, Catalog, artifacts};
//!
//! fn catalog() -> Catalog {
//!     Catalog::anvil()
//!         .into_builder()
//!         .subcommand("myforge")
//!         .with_artifact(Artifact::owned_file(
//!             "justfiles/anvil/extra.just",
//!             "# generated by myforge\n",
//!         ))
//!         .without_artifact(artifacts::region::clippy())
//!         .build()
//!         .expect("the customized catalog has one artifact per target")
//! }
//!
//! fn main() -> ExitCode {
//!     cargo_anvil::run_app(catalog())
//! }
//! ```
//!
//! The engine format remains `anvil`-named on disk so tools built on it can
//! coexist safely and share the same update rules.
//!
//! ## Further reading
//!
//! The design documentation under `docs/design/` covers:
//!
//! - `checks.md` — every check, its scope, and its rationale;
//! - `local.md` — generated recipes and tool installation;
//! - `updates.md` — ownership, drift detection, and opt-outs;
//! - `github.md` and `ado.md` — cloud workflow architecture;
//! - `containers.md` — container execution;
//! - `extensibility.md` — downstream catalogs.
//!
//! `docs/verification.md` describes how cargo-anvil itself is tested.

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
    pub use crate::checksum::checksum_str;
    pub use crate::cli::Cli;
    pub use crate::decision::Decision;
    pub use crate::manifest::{MANIFEST_FILE_NAME, Manifest, RegionKey};
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
