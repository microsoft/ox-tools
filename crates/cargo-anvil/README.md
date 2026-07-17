<div align="center">
 <img src="./logo.png" alt="Cargo-Anvil Logo" width="96">

# Cargo-Anvil

[![crates.io](https://img.shields.io/crates/v/cargo-anvil.svg)](https://crates.io/crates/cargo-anvil)
[![docs.rs](https://docs.rs/cargo-anvil/badge.svg)](https://docs.rs/cargo-anvil)
[![MSRV](https://img.shields.io/crates/msrv/cargo-anvil)](https://crates.io/crates/cargo-anvil)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-anvil

Opinionated, unified Rust build and cloud-workflow scaffolding for GitHub Actions and
Azure DevOps Pipelines. One opinionated check catalog, two cloud workflows
backends, generated from the same source of truth.

### What it does

`cargo-anvil` writes files. `just` runs them. The repo composes
everything. The tool itself is not on the local-build hot path or in
the cloud-workflow graph at runtime — it is a code generator that you re-run when
you want to upgrade the opinionated baseline.

Each run of `cargo anvil` writes:

* The `justfiles/anvil/` recipe tree (`tools.just`, `checks.just`,
  `groups.just`, `tiers.just`) — owned files.
* A managed region in your `Justfile` that imports them.
* A managed region in your workspace `Cargo.toml` carrying
  `[workspace.lints]` in dotted-key form, plus a `[lints] workspace = true` region in each workspace member.
* Managed regions in `deny.toml`, `rustfmt.toml`, and `.delta.toml`.
* For each selected cloud-workflow backend (`github`, `ado`), the full set of
  composite actions / step templates, reusable workflows / stages
  templates, and root workflows / pipelines.

Outside the managed regions, your content is preserved byte-for-byte.

### Installation

```bash
cargo install --locked cargo-anvil
```

Only the maintainer who runs updates needs the binary. Everyone else
uses `just` (or plain `cargo`).

### Usage

```text
cargo anvil [--backend <name>]... [--no-backends] [--dry-run] [--force]
```

`update` is the only subcommand. There is no separate `init`,
`migrate`, `check`, `enable`, or `disable`. The algorithm is uniform
— first runs and subsequent runs go through the same decision table.

Flags:

* `--backend <name>` — repeatable. Valid values: `github`, `ado`. If
  omitted, the backend is autodetected from the `origin` git remote.
* `--no-backends` — emit only local files; skip every cloud-workflow backend.
  Mutually exclusive with `--backend`.
* `--dry-run` — analyze without writing. Exits 1 if anything would be
  written or proposed.
* `--force` — override the single-tool guard and switch the repository to
  this tool, then run a normal update. A repo is managed by exactly one
  anvil-family tool (recorded as `tool` in `.anvil.lock`); without
  `--force`, a run refuses when that field names a different tool.

`--version` prints the build version plus, on a second line, the
`catalog:` checksum — a `sha256` over the whole compiled-in catalog — so
two builds at the same version but different catalogs are distinguishable.

### Daily driver

After the first run, your daily workflow is plain `just`:

```text
$ just anvil          # alias for `just anvil-pr`
$ just anvil-pr       # the PR tier
$ just anvil-scheduled  # the scheduled tier
$ just anvil-full     # both, sequentially
```

cloud workflows invoke the same recipes, so a check behaves identically
locally and in cloud workflows — they share one implementation in the
imported `.just` files. The one difference is scope: cloud-workflow PR
runs perform impact analysis (via [`cargo-delta`][__link0])
and run each check only over the affected packages, whereas a local
`just anvil-pr` runs every check over the whole workspace.

### Containerized local checks

Anvil can run any generated recipe in a content-addressed Linux container.
This is useful for Linux-on-Windows checks and for matching a pinned CI
distribution without installing the full Rust/tool catalog on the host.

#### Prerequisites

* Podman 4.3 or newer.
* `git`, `just`, and PowerShell Core (`pwsh`) on the host.
* A repository-owned `rust-toolchain.toml`.
* On Windows, a running Podman machine:

```powershell
podman machine init   # once
podman machine start
```

#### Run a recipe

```text
just anvil-container anvil-clippy
just anvil-container anvil-pr
just anvil-container
```

The no-argument form opens an interactive shell. The first invocation builds
the matching image locally; later invocations reuse it. Changes to the Rust
toolchain, generated Anvil files, Containerfile, or downstream build helpers
produce a different image tag and trigger a new build.

Cargo registry, Cargo Git, and `target/` data use named Podman volumes. The
repository is mounted at `/workspace`, while build output stays off the
Windows bind-mount hot path.

#### Make tiers use the container

Native execution remains the default. Enable container execution for the
current shell:

```powershell
$env:ANVIL_RUNNER = "container"
just anvil-pr
```

On Unix:

```sh
ANVIL_RUNNER=container just anvil-pr
```

A one-off override is also supported:

```text
just anvil_runner=container anvil-pr
```

To make containers the project default, change the generated
`anvil-runner` region in the repository `Justfile` from `"native"` to
`"container"` and commit that user-owned policy change. Set
`ANVIL_RUNNER=native` to override it for one shell.

#### Controls

|Variable|Effect|
|--------|------|
|`ANVIL_CONTAINER_IMAGE`|Override the local image name. The content hash remains the tag.|
|`ANVIL_CONTAINER_NO_REBUILD=1`|Fail when the matching image is missing instead of building it.|
|`ANVIL_CONTAINER_FORWARD_GITHUB_TOKEN=1`|Forward an existing `GITHUB_TOKEN` to checks that require authenticated GitHub API access.|

The public driver never pulls `ANVIL_CONTAINER_IMAGE` remotely. Downstream
catalogs can add private image-build or dependency-preparation hooks without
changing the public command surface.

#### Troubleshooting

* A first-run image build is expected and may take several minutes.
* `podman images anvil-dev` lists locally cached Anvil images.
* `ANVIL_CONTAINER_NO_REBUILD=1` distinguishes a cache miss from a build
  failure.
* Regenerate managed files with `cargo anvil`; do not hand-edit
  `justfiles/anvil/container/`.

### Checks and tiers

Checks are grouped into **tiers** (`anvil-pr`, `anvil-scheduled`) that
fan out to **groups** (one cloud-workflow job each), which in turn run
individual checks sequentially. `anvil-full` runs both tiers.

The catalog and per-check rationale live in `docs/design/checks.md`;
the tables below map each check to the group that runs it, link each
check to its tool’s documentation, and note anything anvil-specific.

**PR tier** (`anvil-pr`) — runs on every pull request, impact-scoped in
cloud workflows. Two jobs: `pr-fast`, and `pr-slow` (whose three
sub-groups run sequentially within the one job per OS leg):

<table>
  <thead><tr><th>Job</th><th>Sub-group</th><th>Check</th><th>Notes</th></tr></thead>
  <tbody>
    <tr><td rowspan="16"><code>pr-fast</code></td><td rowspan="16">—</td><td><a href="https://rust-lang.github.io/rustfmt/">fmt</a></td><td>predefined configuration with nightly features</td></tr>
    <tr><td><a href="https://doc.rust-lang.org/clippy/">clippy</a></td><td>predefined lints</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-sort">cargo-sort</a></td><td>keeps blank-line groups (<code>--grouped</code>)</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-heather">license-headers</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-ensure-no-cyclic-deps">ensure-no-cyclic-deps</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-ensure-no-default-features">ensure-no-default-features</a></td><td></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/cargo/commands/cargo-doc.html">doc-build</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-doc2readme">readme-check</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-spellcheck">spellcheck</a></td><td>custom dictionary: <code>.spelling</code></td></tr>
    <tr><td><a href="https://www.conventionalcommits.org/">pr-title</a></td><td>cloud-only; skipped locally</td></tr>
    <tr><td><a href="https://embarkstudios.github.io/cargo-deny/">deny</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-audit">audit</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-udeps">udeps</a></td><td>runs twice: with and without <code>--all-targets</code></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-semver-checks">semver-check</a></td><td>advisory-only; never fails the build (posts a PR comment)</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-check-external-types">external-types</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-aprz">aprz</a></td><td>fails on a high-risk crate</td></tr>
    <tr><td rowspan="8"><code>pr-slow</code></td><td rowspan="3"><code>pr-test</code></td><td><a href="https://crates.io/crates/cargo-llvm-cov">llvm-cov</a></td><td>dual feature-config; gated by <a href="https://crates.io/crates/cargo-coverage-gate">cargo-coverage-gate</a></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html">doc-test</a></td><td>runs both feature configs</td></tr>
    <tr><td><a href="https://doc.rust-lang.org/cargo/commands/cargo-build.html">examples</a></td><td>compile-only</td></tr>
    <tr><td rowspan="4"><code>pr-runtime-analysis</code></td><td><a href="https://github.com/rust-lang/miri">miri</a></td><td>libtest, not nextest</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-careful">careful</a></td><td>self-cleans on a toolchain bump</td></tr>
    <tr><td><a href="https://crates.io/crates/loom">loom</a></td><td>opt-in targets only</td></tr>
    <tr><td><a href="https://crates.io/crates/bolero">bolero</a></td><td>60s smoke only; Linux-only</td></tr>
    <tr><td><code>pr-mutants</code></td><td><a href="https://mutants.rs/">mutants-diff</a></td><td>diff-scoped (<code>--in-diff</code>)</td></tr>
  </tbody>
</table>

**Scheduled tier** (`anvil-scheduled`) — full-workspace, runs on a
schedule against the default branch, not on PRs:

<table>
  <thead><tr><th>Group</th><th>Check</th><th>Notes</th></tr></thead>
  <tbody>
    <tr><td rowspan="3"><code>scheduled-test</code></td><td><a href="https://crates.io/crates/cargo-llvm-cov">llvm-cov</a></td><td></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html">doc-test</a></td><td></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/cargo/commands/cargo-build.html">examples</a></td><td></td></tr>
    <tr><td rowspan="4"><code>scheduled-advisories</code></td><td><a href="https://embarkstudios.github.io/cargo-deny/">deny</a></td><td rowspan="4">re-run to catch newly-published advisories / lints</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-audit">audit</a></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-aprz">aprz</a></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/clippy/">clippy</a></td></tr>
    <tr><td rowspan="4"><code>scheduled-runtime-analysis</code></td><td><a href="https://github.com/rust-lang/miri">miri</a></td><td></td></tr>
    <tr><td><a href="https://github.com/rust-lang/miri">miri-tree-borrows</a></td><td><code>-Zmiri-tree-borrows</code></td></tr>
    <tr><td><a href="https://github.com/rust-lang/miri">miri-strict-provenance</a></td><td><code>-Zmiri-strict-provenance</code></td></tr>
    <tr><td><a href="https://github.com/rust-lang/miri">miri-race-coverage</a></td><td>day-rotated seed window</td></tr>
    <tr><td rowspan="3"><code>scheduled-exhaustive</code></td><td><a href="https://mutants.rs/">mutants-full</a></td><td></td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-hack">cargo-hack</a></td><td>feature powerset</td></tr>
    <tr><td><a href="https://doc.rust-lang.org/cargo/commands/cargo-bench.html">bench</a></td><td>compile-only</td></tr>
  </tbody>
</table>

### Customization

Four escape valves, in increasing severity:

1. **Compose around the tool**: add your own `.just` files or
   workflows; the tool never touches anything not prefixed
   `anvil-`.
1. **Extend managed regions** outside the sentinels — add lints,
   deny rules, etc. The tool preserves everything outside.
1. **Opt out by emptying** a managed region or owned file. The tool
   will skip the item on every future `update` and only emit a
   `.anvil-proposed` sibling when the template actually changes.
1. **Take ownership by editing inside** an owned file or managed
   region. The next `update` detects the dirt and writes a
   `.anvil-proposed` sibling instead of overwriting.

### In-tree tool customization

anvil follows a few source-level and `Cargo.toml` conventions so you
can customize how some of the executed tools behave from within your
own crates — without editing the generated `justfiles/anvil/` tree.

#### Spelling dictionary (`spellcheck`)

The `spellcheck` check ([`cargo-spellcheck`][__link1])
reads a repo-root `.spelling` file — one word per line — as its custom
dictionary. Add project-specific terms (crate names, acronyms,
identifiers) there to silence false positives; the `anvil-spellcheck`
recipe sorts and filters it into the dictionary cargo-spellcheck
consumes. Keep the file `LF`-terminated.

#### Coverage (`llvm-cov`)

Coverage is gated by [`cargo-coverage-gate`][__link2];
per-package and per-workspace thresholds, the coverage-exclusion
attribute, and opt-out are all configured through its `Cargo.toml`
metadata conventions — see its documentation.

#### Undefined-behavior checking (`miri`)

The PR-tier `miri` check runs `cargo miri test --all-features --tests`
(libtest, not nextest — process-per-test is roughly twice as slow under miri).
Opt a test out of miri when it touches the filesystem, spawns
subprocesses, or otherwise can’t run under the interpreter:

```text
#[cfg_attr(miri, ignore)]
```

The **scheduled** tier adds three stricter miri profiles, each of
which sets a distinct cfg so you can quarantine a test from one
profile without affecting the others (e.g. a test that OOMs only
under tree-borrows):

```text
#[cfg_attr(miri_tree_borrows,      ignore = "OOMs under -Zmiri-tree-borrows")]
#[cfg_attr(miri_strict_provenance, ignore = "int-to-ptr cast by design")]
#[cfg_attr(miri_race_coverage,     ignore = "nondeterministic across seeds")]
```

#### Concurrency model checking (`loom`)

The `loom` check runs only the test targets that opt in, detected
**structurally** (no filename/comment heuristic). A crate opts in by
declaring a `loom` feature, a dedicated `[[test]]` target that
requires it, and a `cfg(loom)`-gated `loom` dependency:

```toml
[features]
loom = []

[[test]]
name = "loom"               # tests/loom.rs
required-features = ["loom"]

[target.'cfg(loom)'.dependencies]
loom = "0.7"
```

In source, swap std atomics for loom’s under the cfg
(`#[cfg(loom)] use loom::sync::atomic::...`). The recipe builds those
targets with `--cfg loom`, per-package so the cfg never leaks into
other members’ dependencies. It is **fail-loud**: a crate that
declares loom support (a `loom` feature or a `cfg(loom)` dependency)
but ships no such test target errors out rather than silently
skipping. When no crate ships a loom target the check is a no-op.

### Extensibility: shipping your own tool

Another team can ship its own cargo subcommand with its own catalog while
reusing this entire engine. The downstream binary’s `main` is one line:

```rust
use std::process::ExitCode;

fn main() -> ExitCode {
    cargo_anvil::run_app(myforge::catalog())
}
```

…plus a [`Catalog`][__link3] value that starts from [`Catalog::anvil`][__link4] and
customizes the CLI identity ([`CliMeta`][__link5]) and artifact set:

```rust
use cargo_anvil::{Artifact, Catalog, artifacts};

pub fn catalog() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .subcommand("myforge")
        .with_artifact(Artifact::owned_file(
            "justfiles/anvil/extra.just",
            "# ...\n",
        ))
        .replace_artifact(artifacts::region::rustfmt().with_body("max_width = 80\n"))
        .without_artifact(artifacts::region::clippy())
        .build()
        .expect("valid catalog")
}
```

The on-disk vocabulary (`.anvil.lock`, `anvil-managed` sentinels,
`justfiles/anvil/`, `anvil-` recipes) is the fixed engine format and is
never rebranded. A fork customizes only its CLI identity and which
artifacts it emits, via the three uniform builder verbs
([`CatalogBuilder::with_artifact`][__link6], [`CatalogBuilder::replace_artifact`][__link7],
[`CatalogBuilder::without_artifact`][__link8]) over the public [`artifacts`][__link9]
registry. The `tool` field recorded in `.anvil.lock` keeps two
anvil-family tools from clobbering one another in a shared repo (see `--force`).
See `docs/design/extensibility.md`.

### Design docs

See `docs/design/` for the full architecture:

* `design.md` — overall principles and CLI shape.
* `checks.md` — the opinionated check catalog.
* `local.md` — the `justfiles/anvil/` tree.
* `updates.md` — the drift-detection algorithm.
* `extensibility.md` — how downstream tools ship their own catalog.
* `github.md` — GitHub Actions emission.
* `ado.md` — Azure DevOps Pipelines emission.

And `docs/verification.md` for the continuous-validation strategy.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-anvil">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGkYW0CYXSEGxYc2fK81jTWG7kWg0hlspxYGx-DzHaE-xjXG1cDT7T4wIbxYXKEG4V8DpYlvEgcG-cCCsIYpKfiG47pVjigUns3G6ytm5mkV4LpYWSBg2tjYXJnby1hbnZpbGUwLjIuMWtjYXJnb19hbnZpbA
 [__link0]: https://crates.io/crates/cargo-delta
 [__link1]: https://crates.io/crates/cargo-spellcheck
 [__link2]: https://crates.io/crates/cargo-coverage-gate
 [__link3]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=Catalog
 [__link4]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=Catalog::anvil
 [__link5]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=CliMeta
 [__link6]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=CatalogBuilder::with_artifact
 [__link7]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=CatalogBuilder::replace_artifact
 [__link8]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=CatalogBuilder::without_artifact
 [__link9]: https://docs.rs/cargo-anvil/0.2.1/cargo_anvil/?search=artifacts
