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
//! - repository-wide agent instructions for setup, complete PR validation,
//!   and efficient iteration on individual failures;
//! - a repository skill for completing adoption in an existing Rust codebase,
//!   including policy migration and removal of duplicate build infrastructure;
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
//! just anvil-setup         # install every tool and toolchain in the catalog
//! just anvil-pr-fast-setup # install only prerequisites for the fast PR group
//! ```
//!
//! Generated checks validate their prerequisites before executing and provide
//! an installation hint when something is missing.
//!
//! ## Check catalog
//!
//! Every check is available independently as `just anvil-<name>`. The tiers
//! above compose these recipes into the default policy:
//!
//! - **Source and package hygiene:** [`rustfmt`](https://rust-lang.github.io/rustfmt/)
//!   (`fmt`), [`Clippy`](https://doc.rust-lang.org/clippy/) (`clippy`),
//!   [`cargo-sort`](https://crates.io/crates/cargo-sort), license headers with
//!   [`cargo-heather`](https://crates.io/crates/cargo-heather),
//!   [`cargo-ensure-no-cyclic-deps`](https://crates.io/crates/cargo-ensure-no-cyclic-deps),
//!   and [`cargo-ensure-no-default-features`](https://crates.io/crates/cargo-ensure-no-default-features).
//! - **Documentation and repository policy:** Cargo documentation
//!   (`doc-build`), documentation tests (`doc-test`), generated README checks
//!   with [`cargo-doc2readme`](https://crates.io/crates/cargo-doc2readme),
//!   [`cargo-spellcheck`](https://crates.io/crates/cargo-spellcheck), and
//!   [Conventional Commits](https://www.conventionalcommits.org/) pull-request
//!   titles.
//! - **Dependencies and public API:** [`cargo-deny`](https://embarkstudios.github.io/cargo-deny/),
//!   [`cargo-audit`](https://crates.io/crates/cargo-audit),
//!   [`cargo-aprz`](https://crates.io/crates/cargo-aprz),
//!   [`cargo-udeps`](https://crates.io/crates/cargo-udeps),
//!   [`cargo-semver-checks`](https://crates.io/crates/cargo-semver-checks), and
//!   [`cargo-check-external-types`](https://crates.io/crates/cargo-check-external-types).
//! - **Tests and coverage:** tests under the declared MSRV, coverage with
//!   [`cargo-llvm-cov`](https://crates.io/crates/cargo-llvm-cov) and
//!   [`cargo-coverage-gate`](https://crates.io/crates/cargo-coverage-gate),
//!   documentation tests, and example compilation or optional execution.
//! - **Runtime analysis:** [`Miri`](https://github.com/rust-lang/miri),
//!   [`cargo-careful`](https://crates.io/crates/cargo-careful),
//!   [`Loom`](https://crates.io/crates/loom), and
//!   [`Bolero`](https://crates.io/crates/bolero).
//! - **Broader validation:** diff-scoped and full
//!   [`cargo-mutants`](https://mutants.rs/) runs,
//!   [`cargo-hack`](https://crates.io/crates/cargo-hack) feature-powerset
//!   checks, and benchmark compilation.
//!
//! `docs/design/checks.md` records the exact commands, impact scope, feature
//! configurations, operating-system matrix, tier placement, and rationale for
//! each check.
//!
//! ## Configuring checks
//!
//! Customize behavior through the configuration understood by each underlying
//! tool, package metadata, and source attributes. This keeps policy close to
//! the code it governs and lets the same configuration work with Anvil,
//! direct Cargo commands, and editor integrations.
//!
//! cargo-anvil updates owned recipe and workflow files, plus marked regions in
//! shared configuration files. Checksums in `.anvil.lock` record the generated
//! state. Repository configuration outside `anvil-managed` regions is
//! preserved. If generated content is edited directly, cargo-anvil preserves
//! the edit and writes changed catalog content to an `.anvil-proposed` sibling
//! rather than overwriting it.
//!
//! For normal repository policy, configure the tool instead of editing the
//! generated recipe.
//!
//! ### Formatting and linting
//!
//! Rust formatting follows `rustfmt.toml`. Rust and Clippy lint policy lives in
//! workspace and package `Cargo.toml` lint tables, with Clippy-specific
//! configuration in `clippy.toml`. Add repository rules outside the generated
//! regions so catalog updates can continue to maintain the shared baseline.
//!
//! ### Dependencies and public API
//!
//! License, source, advisory, and duplicate-dependency policy lives in
//! `deny.toml`. Dependency features and version requirements remain ordinary
//! `Cargo.toml` declarations. Intentional public exposure of third-party types
//! is recorded with `cargo-check-external-types` package metadata, next to the
//! API surface that requires the exception.
//!
//! ### Spelling
//!
//! Add project names, acronyms, and domain terms to the repository's
//! `.spelling` file, one term per line. `anvil-spellcheck` converts it to the
//! dictionary format expected by cargo-spellcheck.
//!
//! ### Coverage
//!
//! [`cargo-coverage-gate`](https://crates.io/crates/cargo-coverage-gate)
//! metadata controls workspace and package thresholds, target-specific policy,
//! and intentional exclusions. Coverage enforcement is local to the generated
//! check; a hosted coverage service is optional reporting rather than the
//! source of the verdict.
//!
//! ### Miri
//!
//! Tests that cannot run in the interpreter can carry an ordinary ignore:
//!
//! ```text
//! #[cfg_attr(miri, ignore = "spawns a process")]
//! ```
//!
//! The scheduled Tree Borrows, strict-provenance, and race-coverage profiles
//! each define their own cfg so a test can opt out of one profile without
//! disappearing from the others:
//!
//! ```text
//! #[cfg_attr(miri_tree_borrows, ignore = "exceeds the runner memory limit")]
//! #[cfg_attr(miri_strict_provenance, ignore = "uses an intentional integer-to-pointer cast")]
//! #[cfg_attr(miri_race_coverage, ignore = "not deterministic across seeds")]
//! ```
//!
//! ### Loom
//!
//! A crate opts into concurrency model checking with a `loom` feature, a
//! dedicated test target, and a cfg-gated dependency:
//!
//! ```toml
//! [features]
//! loom = []
//!
//! [[test]]
//! name = "loom"
//! required-features = ["loom"]
//!
//! [target.'cfg(loom)'.dependencies]
//! loom = "0.7"
//! ```
//!
//! Source can then select Loom synchronization primitives with `#[cfg(loom)]`.
//! Anvil detects the target from Cargo metadata and fails loudly when a crate
//! declares Loom support but exposes no matching test target.
//!
//! ### Examples
//!
//! `anvil-examples` always compiles selected examples. An unfiltered
//! `--run` skips interactive, credentialed, or otherwise unsuitable examples
//! declared by their package:
//!
//! ```toml
//! [package.metadata.anvil.examples]
//! no-run = ["interactive-demo", "needs-production-credentials"]
//! ```
//!
//! Explicit `--package` and `--example` selection overrides the default
//! exclusion because the caller has deliberately chosen that example.
//!
//! ### Scheduled failure reporting
//!
//! The generated GitHub scheduled workflow creates or updates an
//! `[Anvil] Scheduled checks failed` issue. Set the Actions repository
//! variable `ANVIL_PUBLISH_FAILURE_ISSUE` to `false` to disable publication
//! without taking ownership of the workflow.
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
//! More detailed design and operational guidance is available in the
//! `docs/design/` folder.

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
