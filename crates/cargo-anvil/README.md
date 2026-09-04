<div align="center">
 <img src="./logo.png" alt="Cargo-Anvil Logo" width="96">

# Cargo-Anvil

[![crates.io](https://img.shields.io/crates/v/cargo-anvil.svg)](https://crates.io/crates/cargo-anvil)
[![docs.rs](https://docs.rs/cargo-anvil/badge.svg)](https://docs.rs/cargo-anvil)
[![MSRV](https://img.shields.io/crates/msrv/cargo-anvil)](https://crates.io/crates/cargo-anvil)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml/badge.svg?event=pull_request)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-pr.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-anvil

`cargo-anvil` gives a Rust repository a complete, maintained set of build
checks for local development and continuous integration. It standardizes
common engineering work, including formatting, linting, tests, coverage, dependency
policy, minimum-Rust-version testing, undefined-behavior checks, and
mutation testing, without requiring each repository to design and maintain
that infrastructure independently.

Run it once to add the build system to a repository. Run it again whenever
the repository should adopt a newer cargo-anvil release. The generated
files are committed with the source, so builds do not download or execute
cargo-anvil itself.

### What you get

A generated setup includes:

* commands for individual checks, groups of related checks, and complete
  pull-request or scheduled validation tiers;
* pinned Rust nightly and Cargo tool versions;
* impact analysis that limits pull-request work to affected crates while
  retaining full-workspace scheduled validation;
* GitHub Actions workflows, Azure DevOps pipelines, or both;
* shared formatting, lint, dependency, spelling, and impact-analysis
  configuration;
* repository-wide agent instructions for setup, complete PR validation,
  and efficient iteration on individual failures;
* safe update metadata that distinguishes generated content from
  repository-owned customization.

The default catalog is opinionated. Its purpose is to provide one strong,
coherent baseline rather than a collection of switches that every
repository must assemble differently.

### Install and adopt

Install the generator:

```console
cargo install --locked cargo-anvil
```

From the root of a Rust repository, generate the local commands and
autodetect the cloud provider from the `origin` remote:

```console
cargo anvil
```

A backend can also be selected explicitly:

```console
cargo anvil --backend github
cargo anvil --backend ado
cargo anvil --backend github --backend ado
cargo anvil --no-backends
```

Commit the generated files. Teammates and CI runners need the generated
files and their declared tools, but they do not need the cargo-anvil binary.

`cargo anvil --dry-run` reports drift without writing. It exits unsuccessfully
when an update is pending, generated state is inconsistent, or content
cannot be inspected safely. `cargo anvil --force` permits an intentional
switch from another tool built on the cargo-anvil engine.

### Running checks

The generated command interface uses
[`just`][__link0], a cross-platform command runner.
A repository’s `Justfile` imports the generated recipes under
`justfiles/anvil/`; each recipe invokes ordinary Rust and Cargo tools.

Run the complete pull-request tier locally:

```console
just anvil-pr
```

Run only the fast checks when a change cannot affect executable behavior:

```console
just anvil-pr-fast
```

Other useful entry points are:

```console
just anvil-build
just anvil-build --package my-crate
just anvil-doc-build --open
just anvil-examples --run
just anvil-fmt --fix
just anvil-miri --package my-crate --example basic
just anvil-readme --fix
just anvil-scheduled
just anvil-full
```

Individual checks are also available as `anvil-<check>` recipes. Run
`just --list` to see the generated interface and `just --usage <recipe>` to
see a recipe’s options.

### Pull-request and scheduled validation

The pull-request tier is impact-scoped. It computes which workspace
packages changed and which packages depend on them, then gives each check
the scope required for correctness. Independent groups run in parallel in
the generated cloud workflow:

* **fast checks** cover formatting, Clippy, documentation, dependency
  policy, spelling, README drift, API compatibility, and metadata;
* **tests and coverage** run tests, documentation tests, coverage gates,
  and example compilation;
* **MSRV tests** execute affected tests with the repository’s declared
  minimum supported Rust version;
* **runtime analysis** runs Miri, cargo-careful, Loom, and a short Bolero
  fuzzing pass;
* **mutation testing** tests mutants in the pull-request diff.

The scheduled tier runs full-workspace backstops and expensive checks that
do not belong on every pull request, including extended Miri profiles, full
mutation testing, feature-powerset checks, and benchmark compilation.
GitHub scheduled failures can be published to a durable issue.

Local and cloud runs call the same generated recipes with the same check
arguments. Cloud workflows add orchestration for matrices, permissions,
artifacts, comments, and status reporting, but do not re-implement the
checks.

### Rust toolchains and tools

Ordinary checks select Rust deterministically, in this order:

1. a caller-provided `RUSTUP_TOOLCHAIN`;
1. a root `rust-toolchain` or `rust-toolchain.toml`;
1. the root package or workspace `rust-version`.

Repositories with none of these fail instead of silently using a runner’s
ambient compiler. Nightly-only checks use catalog pins. The MSRV group
separately installs and tests the minimum version declared by the
repository.

Tool installation is explicit:

```console
just anvil-setup         # install every tool and toolchain in the catalog
just anvil-pr-fast-setup # install only prerequisites for the fast PR group
```

Generated checks validate their prerequisites before executing and provide
an installation hint when something is missing.

### Check catalog

Every check is available independently as `just anvil-<name>`. The tiers
above compose these recipes into the default policy:

* **Source and package hygiene:** [`rustfmt`][__link1]
  (`fmt`), [`Clippy`][__link2] (`clippy`),
  [`cargo-sort`][__link3], license headers with
  [`cargo-heather`][__link4],
  [`cargo-ensure-no-cyclic-deps`][__link5],
  and [`cargo-ensure-no-default-features`][__link6].
* **Documentation and repository policy:** Cargo documentation
  (`doc-build`), documentation tests (`doc-test`), generated README checks
  with [`cargo-doc2readme`][__link7],
  [`cargo-spellcheck`][__link8], and
  [Conventional Commits][__link9] pull-request
  titles.
* **Dependencies and public API:** [`cargo-deny`][__link10],
  [`cargo-audit`][__link11],
  [`cargo-aprz`][__link12],
  [`cargo-udeps`][__link13],
  [`cargo-semver-checks`][__link14], and
  [`cargo-check-external-types`][__link15].
* **Tests and coverage:** tests under the declared MSRV, coverage with
  [`cargo-llvm-cov`][__link16] and
  [`cargo-coverage-gate`][__link17],
  documentation tests, and example compilation or optional execution.
* **Runtime analysis:** [`Miri`][__link18],
  [`cargo-careful`][__link19],
  [`Loom`][__link20], and
  [`Bolero`][__link21].
* **Broader validation:** diff-scoped and full
  [`cargo-mutants`][__link22] runs,
  [`cargo-hack`][__link23] feature-powerset
  checks, and benchmark compilation.

`docs/design/checks.md` records the exact commands, impact scope, feature
configurations, operating-system matrix, tier placement, and rationale for
each check.

### Configuring checks

Customize behavior through the configuration understood by each underlying
tool, package metadata, and source attributes. This keeps policy close to
the code it governs and lets the same configuration work with Anvil,
direct Cargo commands, and editor integrations.

cargo-anvil updates owned recipe and workflow files, plus marked regions in
shared configuration files. Checksums in `.anvil.lock` record the generated
state. Repository configuration outside `anvil-managed` regions is
preserved. If generated content is edited directly, cargo-anvil preserves
the edit and writes changed catalog content to an `.anvil-proposed` sibling
rather than overwriting it.

For normal repository policy, configure the tool instead of editing the
generated recipe.

#### Formatting and linting

Rust formatting follows `rustfmt.toml`. Rust and Clippy lint policy lives in
workspace and package `Cargo.toml` lint tables, with Clippy-specific
configuration in `clippy.toml`. Add repository rules outside the generated
regions so catalog updates can continue to maintain the shared baseline.

#### Dependencies and public API

License, source, advisory, and duplicate-dependency policy lives in
`deny.toml`. Dependency features and version requirements remain ordinary
`Cargo.toml` declarations. Intentional public exposure of third-party types
is recorded with `cargo-check-external-types` package metadata, next to the
API surface that requires the exception.

#### Spelling

Add project names, acronyms, and domain terms to the repository’s
`.spelling` file, one term per line. `anvil-spellcheck` converts it to the
dictionary format expected by cargo-spellcheck.

#### Coverage

[`cargo-coverage-gate`][__link24]
metadata controls workspace and package thresholds, target-specific policy,
and intentional exclusions. Coverage enforcement is local to the generated
check; a hosted coverage service is optional reporting rather than the
source of the verdict.

#### Miri

Tests that cannot run in the interpreter can carry an ordinary ignore:

```text
#[cfg_attr(miri, ignore = "spawns a process")]
```

The scheduled Tree Borrows, strict-provenance, and race-coverage profiles
each define their own cfg so a test can opt out of one profile without
disappearing from the others:

```text
#[cfg_attr(miri_tree_borrows, ignore = "exceeds the runner memory limit")]
#[cfg_attr(miri_strict_provenance, ignore = "uses an intentional integer-to-pointer cast")]
#[cfg_attr(miri_race_coverage, ignore = "not deterministic across seeds")]
```

#### Loom

A crate opts into concurrency model checking with a `loom` feature, a
dedicated test target, and a cfg-gated dependency:

```toml
[features]
loom = []

[[test]]
name = "loom"
required-features = ["loom"]

[target.'cfg(loom)'.dependencies]
loom = "0.7"
```

Source can then select Loom synchronization primitives with `#[cfg(loom)]`.
Anvil detects the target from Cargo metadata and fails loudly when a crate
declares Loom support but exposes no matching test target.

#### Examples

`anvil-examples` always compiles selected examples. An unfiltered
`--run` skips interactive, credentialed, or otherwise unsuitable examples
declared by their package:

```toml
[package.metadata.anvil.examples]
no-run = ["interactive-demo", "needs-production-credentials"]
```

Explicit `--package` and `--example` selection overrides the default
exclusion because the caller has deliberately chosen that example.

#### Scheduled failure reporting

The generated GitHub scheduled workflow creates or updates an
`[Anvil] Scheduled checks failed` issue. Set the Actions repository
variable `ANVIL_PUBLISH_FAILURE_ISSUE` to `false` to disable publication
without taking ownership of the workflow.

### Containerized local execution

Any generated command can run in an optional content-addressed Linux
container:

```console
just anvil-container just anvil-pr
just anvil-container just anvil-clippy
just anvil-container cargo build
```

The image contains the same tool versions used by the generated checks.
Its tag is derived from the Dockerfile, toolchain, and generated recipe
tree, so changes to those inputs select a different image. Docker is
supported; Podman is available on a best-effort basis. Repositories that
need private feeds can add a host-side credential hook without embedding
credentials in the image or command line. See
`docs/design/containers.md` for setup, security boundaries, and advanced
customization.

### Building another tool on the engine

The crate also exposes the catalog engine used by `cargo-anvil`. A
downstream tool can start from [`Catalog::anvil`][__link25], add, replace, or remove
[`Artifact`][__link26] values, select its own CLI identity, and pass the result to
[`run_app`][__link27]:

```rust
use std::process::ExitCode;

use cargo_anvil::{Artifact, Catalog, artifacts};

fn catalog() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .subcommand("myforge")
        .with_artifact(Artifact::owned_file(
            "justfiles/anvil/extra.just",
            "# generated by myforge\n",
        ))
        .without_artifact(artifacts::region::clippy())
        .build()
        .expect("the customized catalog has one artifact per target")
}

fn main() -> ExitCode {
    cargo_anvil::run_app(catalog())
}
```

The engine format remains `anvil`-named on disk so tools built on it can
coexist safely and share the same update rules.

More detailed design and operational guidance is available in the
`docs/design/` folder.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-anvil">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbLWH4W5BHNskbie4mnQoeuX4bATuTcxHMFGAbnkJHpis_GnRhYvRhcoQbfwKnpCfYspUb4NWI9bKnpi8bM8O_p0ovp3EbxjyK4cgWbiphZIGDa2NhcmdvLWFudmlsZTAuOC4wa2NhcmdvX2Fudmls
 [__link0]: https://github.com/casey/just
 [__link1]: https://rust-lang.github.io/rustfmt/
 [__link10]: https://embarkstudios.github.io/cargo-deny/
 [__link11]: https://crates.io/crates/cargo-audit
 [__link12]: https://crates.io/crates/cargo-aprz
 [__link13]: https://crates.io/crates/cargo-udeps
 [__link14]: https://crates.io/crates/cargo-semver-checks
 [__link15]: https://crates.io/crates/cargo-check-external-types
 [__link16]: https://crates.io/crates/cargo-llvm-cov
 [__link17]: https://crates.io/crates/cargo-coverage-gate
 [__link18]: https://github.com/rust-lang/miri
 [__link19]: https://crates.io/crates/cargo-careful
 [__link2]: https://doc.rust-lang.org/clippy/
 [__link20]: https://crates.io/crates/loom
 [__link21]: https://crates.io/crates/bolero
 [__link22]: https://mutants.rs/
 [__link23]: https://crates.io/crates/cargo-hack
 [__link24]: https://crates.io/crates/cargo-coverage-gate
 [__link25]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/?search=Catalog::anvil
 [__link26]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/?search=Artifact
 [__link27]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/fn.run_app.html
 [__link3]: https://crates.io/crates/cargo-sort
 [__link4]: https://crates.io/crates/cargo-heather
 [__link5]: https://crates.io/crates/cargo-ensure-no-cyclic-deps
 [__link6]: https://crates.io/crates/cargo-ensure-no-default-features
 [__link7]: https://crates.io/crates/cargo-doc2readme
 [__link8]: https://crates.io/crates/cargo-spellcheck
 [__link9]: https://www.conventionalcommits.org/
