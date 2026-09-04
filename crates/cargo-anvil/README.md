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
just anvil-setup
just anvil-pr-fast-setup
```

Generated checks validate their prerequisites before executing and provide
an installation hint when something is missing.

### Safe updates and customization

cargo-anvil manages two kinds of content:

* **owned files**, such as recipes and workflow implementations;
* **managed regions** inside repository files such as `Cargo.toml`,
  `rustfmt.toml`, and `deny.toml`.

Checksums in `.anvil.lock` record the last generated state. Unmodified
content updates automatically. Repository-owned text outside managed
regions is preserved. If managed content was edited, cargo-anvil preserves
it and writes an `.anvil-proposed` sibling when the catalog changes, making
the conflict visible instead of overwriting the customization.

Common repository policy remains in source and configuration rather than
generated scripts. Examples include per-package coverage thresholds,
spelling dictionaries, tests ignored by a specific Miri profile, Loom test
targets, and examples excluded from automatic execution.

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
downstream tool can start from [`Catalog::anvil`][__link1], add, replace, or remove
[`Artifact`][__link2] values, select its own CLI identity, and pass the result to
[`run_app`][__link3]:

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

### Further reading

The design documentation under `docs/design/` covers:

* `checks.md` — every check, its scope, and its rationale;
* `local.md` — generated recipes and tool installation;
* `updates.md` — ownership, drift detection, and opt-outs;
* `github.md` and `ado.md` — cloud workflow architecture;
* `containers.md` — container execution;
* `extensibility.md` — downstream catalogs.

`docs/verification.md` describes how cargo-anvil itself is tested.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-anvil">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbLWH4W5BHNskbie4mnQoeuX4bATuTcxHMFGAbnkJHpis_GnRhYvRhcoQb5qM5xwlC-NIbNBdBq7fWV0gb5UdhRAr2q1Ubdt0Vc4ki7NphZIGDa2NhcmdvLWFudmlsZTAuOC4wa2NhcmdvX2Fudmls
 [__link0]: https://github.com/casey/just
 [__link1]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/?search=Catalog::anvil
 [__link2]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/?search=Artifact
 [__link3]: https://docs.rs/cargo-anvil/0.8.0/cargo_anvil/fn.run_app.html
