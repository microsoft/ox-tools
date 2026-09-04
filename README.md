<div align="center">
 <img src="./logo.svg" alt="Oxidizer Logo" width="96">

# The Oxidizer Tools Project

[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

</div>

This repository contains a set of tools  that help you build robust highly scalable services in Rust.

- [Crates](#crates)
- [About this Repo](#about-this-repo)
    - [Adding New Crates](#adding-new-crates)
    - [Publishing Crates](#publishing-crates)
    - [Documenting Crates](#documenting-crates)
    - [CI Workflows](#ci-workflows)
    - [Pull Request Gates](#pull-request-gates)
    - [Tool Versions](#tool-versions)
  - [Trademarks](#trademarks)

## Crates

These are the crates built out of this repo:

- [`cargo-anvil`](./crates/cargo-anvil/README.md) - Opinionated, unified Rust build and cloud-workflow scaffolding for GitHub Actions and Azure DevOps
- [`cargo-aprz`](./crates/cargo-aprz/README.md) - A cargo subcommand that appraises the quality of Rust dependencies
- [`cargo-coverage-gate`](./crates/cargo-coverage-gate/README.md) - A cargo subcommand that gates pull requests on per-package line coverage measured by cargo-llvm-cov
- [`cargo-each`](./crates/cargo-each/README.md) - A cargo subcommand that runs a command over a cargo-style selection of workspace members
- [`cargo-ensure-no-cyclic-deps`](./crates/cargo_ensure_no_cyclic_deps/README.md) - A cargo subcommand to detect cyclic dependencies in workspace crates
- [`cargo-ensure-no-default-features`](./crates/cargo-ensure-no-default-features/README.md) - A cargo subcommand that ensures dependencies are declared with default-features = false
- [`cargo-gamma`](./crates/cargo-gamma/README.md) - Fast mutation testing for Rust
- [`cargo-heather`](./crates/cargo-heather/README.md) - A cargo subcommand to validate license headers in Rust, TOML, PowerShell, Just, and env source files

## About this Repo

The following sections explain the overall engineering process we use
in this repo.

To set up a local PC environment capable of exercising all the tooling used by this repo's development processes,
you can follow the guide in [DEVELOPMENT.md](./DEVELOPMENT.md).

### Adding New Crates

Adding a new crate to this repo is done by running the `scripts\add-crate.ps1` script.
It will prompt you for a few bits of state, and then will get everything wired up that
needs to be.

The `add-crate` script does the following:

- Adds an entry for the crate to the [Crates](#crates) section in this README file.

- Adds an entry for the crate to the top-level [CHANGELOG.md](./CHANGELOG.md) file.

- Prepares a `README.md` file for the crate, setup for use with [
  `cargo-doc2readme`](https://crates.io/crates/cargo-doc2readme)
  with a set of appropriate CI badges.

- Creates an empty `CHANGELOG.md` file for the crate, which will later get populated by the `scripts\release-crate.ps1`
  script.

- Creates placeholder `logo.png` and `favicon.ico` files for the crate, which you're expected to replace with legit
  crab-themed
  logo and icon.

### Publishing Crates

Releasing new versions of crates to [crates.io](https://crates.io) is handled by
an internal Microsoft automation process. To release a new version of any crate, follow
this simple process:

1. Make sure the changes you want to release have all been committed to the repo.

2. Create a branch off of main.

3. Run `./scripts/release-crate.ps1 <crate_name> [new_version]` to bump a crate's version and update the crate's
   `CHANGELOG.md` file.
   Run the script many times if you want to release several crates in the same PR.

4. Create a PR like normal to push changes out.

Once your PR is merged, automation will kick in. It will tag the
commit and push the crate to crates.io.

### Documenting Crates

We want our crates to have world-class documentation such that our customers can enjoy discovering and using our
features. We expect our Rust code to be fully documented in the normal Rust way, and we introduce two doc-related
automation processes:

- The `README.md` file in each crate's directory is auto-generated from the crate-level documentation.
  We use the [`cargo-doc2readme`](https://crates.io/crates/cargo-doc2readme) tool which reads the crate docs, resolves intra-doc links, and
  generates the `README.md` file using a shared template. A pull request gate ensures the `README.md` file
  always reflects the latest crate documentation.

- The `CHANGELOG.md` file in each crate's directory is auto-generated from the commits to a crate's directory by the
  `scripts/release-crate.ps1` script.

To generate and open documentation locally with all features enabled, run:

```shell
just anvil-doc-build --open
```

The recipe generates documentation and opens it in your default browser.

### CI Workflows

We use the workflows generated and maintained by `cargo-anvil`:

- `Anvil`. Runs impact-scoped validation on pull requests and merge-queue
  commits. Its aggregate `PR Job / Required Anvil checks` context blocks a
  merge when impact analysis or any check-group matrix does not succeed.

- `anvil-scheduled`. Runs full-workspace tests, advisory checks, runtime
  analysis, mutation testing, feature-powerset checks, and benchmark
  compilation. Failures are published to a durable tracking issue.

### Pull Request Gates

We strive to deliver high-quality code and as such, we've put in place a number of PR gates, described here:

Before submitting source, build, configuration, or CI changes, run the complete
local PR tier:

```shell
just anvil-pr
```

For documentation-only changes that cannot affect executable behavior, run the
fast tier instead:

```shell
just anvil-pr-fast
```

The fast tier includes formatting, generated README, spelling, metadata,
dependency-policy, and static-analysis checks. It deliberately omits tests and
coverage, runtime analysis, and mutation testing. Generic developer operations
are also provided by Anvil:

```shell
just anvil-build
just anvil-build --package cargo-anvil
just anvil-doc-build --open
just anvil-examples --run
just anvil-fmt --fix
just anvil-miri --package cargo-anvil --example basic
just anvil-readme --fix
```

These focused operations are not substitutes for either verification tier.

- **Build**. We build affected crates for Windows and Linux on x86_64 and
  aarch64. The scheduled tier uses
  [`cargo-hack`](https://crates.io/crates/cargo-hack) to check the feature
  powerset across the full workspace.

- **Testing**. We run affected unit and integration tests through
  [`cargo-nextest`](https://nexte.st/) in both `--all-features` and
  `--no-default-features` configurations. Documentation tests run separately
  through `cargo test --doc` with all features and default features.

- **Code Coverage**. We collect coverage using
  [`cargo-llvm-cov`](https://crates.io/crates/cargo-llvm-cov) on Windows and
  Linux under the same all-features and no-default-features configurations.
  Pull requests measure affected packages; the scheduled tier measures the
  full workspace. The nightly compiler enables `coverage(off)` annotations,
  and the local cargo-coverage-gate verdict enforces each package's configured
  threshold.

- **Mutation Testing**. We use [`cargo-mutants`](https://crates.io/crates/cargo-mutants) to help maintain
  high test quality.

- **Source Linting**. We run Clippy with most warnings enabled and all treated as errors.

- **Doc Linting**. We lint documentation to help find bad links and other anti-patterns.

- **Source Formatting**. We ensure the source code complies with the Rust standard format.

- **Cargo.toml Formatting**. We use [`cargo-sort`](https://crates.io/crates/cargo-sort) to keep Cargo.toml
  files in a consistent format and layout.

- **Unsafe Verification**. We use Miri and [`cargo-careful`](https://crates.io/crates/cargo-careful) to verify that our
  unsafe code doesn't induce undefined behaviors.

- **External Type Exposure**. We use [`cargo-check-external-types`](https://crates.io/crates/cargo-check-external-types) to track
  which external types our crates depend on. Exposing a 3P type from a crate creates a coupling between the crate and
  the exporter
  of the type which can be problematic over time. This check is there to prevent unintentional exposure. If the exposure
  is intentional,
  it's a simple matter of adding an exclusion for it to the crate's `Cargo.toml` file.

- **Default Features**. We use [
  `cargo-ensure-no-default-features`](https://crates.io/crates/cargo-ensure-no-default-features) to make
  sure the dependencies pulled in by the top-level Cargo.toml are all annotated with `default-features = false`.
  Individual crates that use
  these dependencies are then responsible for stating exactly which features they need. This is designed to minimize
  build times for
  our customers.

- **Cyclic Dependencies**. We use [`cargo-ensure-no-cyclic-deps`](https://crates.io/crates/cargo-ensure-no-cyclic-deps)
  to ensure the
  crates in the repo don't create funny referential cycles using `dev-dependencies`. Things break or get difficult when
  these cycles exist.

- **Unneeded Dependencies**. We use [`cargo-udeps`](https://crates.io/crates/cargo-udeps) to ensure our crates don't
  have superfluous
  dependencies.

- **Dependency Validation**. We use [`cargo-deny`](https://crates.io/crates/cargo-deny) to ensure our dependencies
  have acceptable licenses and don't contain known vulnerabilities.

- **Semantic Version Compatibility**. We use [`cargo-semver-checks`](https://crates.io/crates/cargo-semver-checks) to report
  advisory findings about API compatibility against the pull request target.

- **PR Title**. Every PR submitted to this repo must follow
  the [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)
  specification. We use these PR titles as part of our automatic change log generation logic.

- **License Headers**. We ensure all source files have the requisite license header. The expected header is described in
  the [`.cargo-heather.toml`](./.cargo-heather.toml) file at the repo root and validated by the in-repo
  [`cargo-heather`](./crates/cargo-heather) tool.

- **Spell Checking**. We use [cargo-spellcheck](https://crates.io/crates/cargo-spellcheck) to help our docs have fewer typos.

- **README Content**. We use [`cargo-doc2readme`](https://crates.io/crates/cargo-doc2readme) to ensure each crate's `README.md`
  file matches the crate's current crate-level documentation.

### Tool Versions

Anvil's Rust nightly and cargo-tool pins live exclusively in
[`justfiles/anvil/versions.just`](./justfiles/anvil/versions.just). Because this
repository develops cargo-anvil itself, catalog updates start in
`crates/cargo-anvil/templates/justfiles/anvil/versions.just` and are applied by
running `cargo anvil`. The default stable toolchain remains in
[`rust-toolchain.toml`](./rust-toolchain.toml), while the workspace MSRV remains
the `rust-version` in [`Cargo.toml`](./Cargo.toml).

## Trademarks

This project may contain trademarks or logos for projects, products, or services. Authorized use of Microsoft
trademarks or logos is subject to and must follow
[Microsoft's Trademark & Brand Guidelines](https://www.microsoft.com/en-us/legal/intellectualproperty/trademarks/usage/general).
Use of Microsoft trademarks or logos in modified versions of this project must not cause confusion or imply Microsoft
sponsorship.
Any use of third-party trademarks or logos are subject to those third-party's policies.
