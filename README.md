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
- [`cargo-unique-target-names`](./crates/cargo-unique-target-names/README.md) - A cargo subcommand that detects workspace targets which uplift to the same build artifact
- [`cargo-unused-deps`](./crates/cargo-unused-deps/README.md) - A Cargo subcommand that finds unused and misplaced dependencies
- [`hunspell-dictgen`](./crates/hunspell-dictgen/README.md) - Generate deterministic Hunspell dictionaries from human-maintained word lists

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

3. Run `.\scripts\release-crate.ps1 <crate_name> -Version <new_version>` to set a crate's version and update the crate's
   `CHANGELOG.md` file.
   Omit `-Version` for a minor bump, or use `-Bump` with `major`, `minor`, or `patch`.
   To refresh notes for the current, untagged version, pass that same version to `-Version`.
   Run the script many times if you want to release several crates in the same PR.
   Generated notes prefer a PR-linked release entry over a duplicate unlinked
   preparation entry for the same crate and version.

   To reconstruct a missing changelog section for an existing tagged release without changing
   the crate version, run `.\scripts\release-crate.ps1 <crate_name> -BackfillVersion <version>`.
   The script derives the commit range from that release tag and its preceding semantic-version tag.
   For the first semantic-version tag, it includes the crate's complete history through that tag;
   no preceding tag is required. Run these commands from the repository root.
   Validate the backfill workflow with `pwsh -NoProfile -File .\scripts\test-release-crate.ps1`.

4. Create a PR like normal to push changes out.

Once your PR is merged, automation will kick in. It will tag the
commit and push the crate to crates.io.

### GitHub Release Notes

The `Publish GitHub Release` workflow runs once per pushed crate-version tag.
Immediately before creating the GitHub release, `scripts\publish-gh-release.rs`
generates its notes from Git commit messages. It does not read or update `CHANGELOG.md`.
The PowerShell release-preparation script still generates changelogs independently.

#### Previewing Locally

Run from the repository root with an existing crate-version tag:

```powershell
cargo +nightly -Zscript .\scripts\publish-gh-release.rs --dry-run sample-v1.3.0
```

Replace `sample-v1.3.0` with the tag to preview. The script prints the release title
and generated Markdown, followed by a notice that release creation was skipped,
without invoking `gh`. `--repo` is not required for a dry run; publishing still
requires `--repo <owner/name>`.
The same history selection, filtering, and errors apply in both modes.

#### Commit Selection

For example, publishing `sample-v1.3.0`:

1. Find the greatest version tag below `1.3.0` for `sample`. Versions are compared
   numerically, not by tag date or alphabetical order. If both `sample-v1.2.0` and
   `sample-v1.10.0` exist, only `sample-v1.2.0` can precede this release.
2. Read commits in `sample-v1.2.0..sample-v1.3.0`, limited to changes under
   `crates\sample`. The previous tag's history is excluded; the target tag is included.
3. If there is no preceding version tag, read all history affecting that directory
   through `sample-v1.3.0`. There is no special "Initial release" replacement.

The upper bound is always the requested release tag, not `HEAD`. Later commits do not
enter an older release's notes. Only `<crate>-v<major>.<minor>.<patch>` tags are supported;
prerelease tags, build-metadata tags, and tags for other crates are not predecessor candidates.
Full history and tags are required; the workflow checks out with `fetch-depth: 0`.

Selection is based on changed paths, not the conventional-commit scope. A commit scoped
to another crate can appear if it also changes files under `crates\sample`.

#### Formatting Example

Suppose the selected history contains these two commit messages:

```text
feat(sample): add configuration files (#123)
```

```text
fix(sample): reject invalid configuration

BREAKING CHANGE: invalid configurations now return an error instead of using defaults.
```

The generated Markdown is:

```markdown
- ⚠️ Breaking

  - reject invalid configuration

- ✨ Features

  - add configuration files (#123)
```

These are illustrative commit messages and a PR number, not an actual release.
`git-conventional` parses full messages. Both a header `!`, such as
`fix(sample)!: reject invalid configuration`, and a `BREAKING CHANGE` footer select
the Breaking category. Only the description becomes bullet text;
scopes, commit bodies, and footer explanations are not copied into the notes.

#### Categories and Filtering

The standard category order is Breaking, Features, Bug Fixes, Performance, Documentation,
Tasks, Code Refactoring, Build System, Continuous Integration, Styling, and Miscellaneous. Empty
categories are omitted. `chore` maps to Tasks, `doc` to Documentation, and `misc` to
Miscellaneous. Other conventional types get their own capitalized heading after the
standard categories, sorted alphabetically. Messages that cannot be parsed as conventional
commits retain their subject under Miscellaneous. Conventional commit types are matched
case-insensitively and custom category names are normalized to lowercase.

PR references such as `(#123)` are
retained verbatim for GitHub to link. Entries within each category retain Git's log order.

Conventional `test` commits are excluded, including ones marked breaking.
Notes-maintenance and release-preparation commits receive no special filtering or
deduplication.

Git failures stop publication. If the selected history is empty or filtering removes
every entry, publication also fails rather than creating a release with placeholder notes.

#### Limitations

**Package moves are not followed.** The publisher assumes `crates\<crate-name>` exists
in the checkout and uses only that path throughout the selected history. For example:

```text
sample-v1.2.0
  fix(sample): correct parsing     [old\sample]
  chore: move sample              [old\sample -> crates\sample]
  feat(sample): add validation    [crates\sample]
sample-v1.3.0
```

The parsing fix under `old\sample` is omitted even though it belongs to this release.
Moving the package away from `crates\sample` instead fails the directory check.
Package renames and historical directory discovery are not supported.

**Changes outside the package directory are not selected on their own.** For example,
a root `Cargo.toml`, `Cargo.lock`, or shared build-configuration update appears only if
the same commit also affects the package directory. The generator does not infer a
package's changes from its workspace dependency graph.

**Changelog-only information is not carried over.** Manually curated text, `Unreleased`
sections, and dependency-upgrade bullets stored in `CHANGELOG.md` do not become GitHub
release notes. Only the selected commit messages supply note content.

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
just anvil-examples
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

- **Mutation Testing**. Anvil uses [`cargo-gamma`](./crates/cargo-gamma/README.md)
  0.4.0 or newer for diff-scoped PR checks and full scheduled campaigns.
  Repository policy lives in `gamma.toml`, retaining all-feature testing and the
  existing `mutants` build profile. Campaigns must meet a 100% assertion-killed
  score with no unresolved flaky outcomes. Empty diff selections succeed;
  nonempty populations with no gradeable mutations and empty full campaigns
  fail the applicable gates. The former `.cargo/mutants.toml` is not used by Anvil.

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
