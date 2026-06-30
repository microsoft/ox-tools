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

cloud workflows invokes the same recipes. Local and cloud-workflow runs are bit-identical because
they share one implementation in the imported `.just` files.

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

### Per-crate check conventions

A few checks read source-level or `Cargo.toml` knobs in *your* crates.
These are stable conventions: set them in your own code, and the
`anvil-` recipes pick them up. None require editing the generated
`justfiles/anvil/` tree.

#### Coverage (`llvm-cov` + `cargo-coverage-gate`)

Per-package line-coverage thresholds live in `Cargo.toml` metadata. A
per-package value wins; otherwise the workspace value applies; the
built-in default is `100.0`:

```toml
# workspace root: the default threshold for every member
[workspace.metadata.coverage-gate]
min-lines-percent = 90.0

# a single crate: override (or opt out with 0)
[package.metadata.coverage-gate]
min-lines-percent = 0      # 0 == opt this crate out of the gate entirely
```

To exclude an individual item (an untestable error arm, a
process-shelling path) from coverage, use the standard attribute —
the `coverage`/`coverage_nightly` cfgs are pre-declared (see *Custom
cfg names* below), and coverage is measured on nightly so the
exclusion is live:

```text
#[cfg_attr(coverage_nightly, coverage(off))]
```

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

#### Fuzz smoke-testing (`bolero`)

The `bolero` check runs each [`bolero`][__link0]
harness for about 60 seconds as a crash/hang smoke test. It is **Linux-only**
(the libfuzzer engine and `bolero-afl` don’t build on
Windows/macOS); on other hosts the check self-skips, but harnesses
still compile and run as ordinary tests under `llvm-cov`. A crate
with no bolero harness is a no-op.

#### Custom cfg names

Every cfg the checks rely on — `coverage`, `coverage_nightly`,
`loom`, `miri_tree_borrows`, `miri_strict_provenance`,
`miri_race_coverage` — is pre-declared in the managed `[workspace.lints]`
`unexpected_cfgs.check-cfg` list, so the catalog’s `-D warnings` cloud
policy doesn’t reject the conventions above. Need another custom cfg?
Take ownership of that one `check-cfg` line; the drift detector
preserves your edit and emits a `.anvil-proposed` sibling on future
catalog bumps.

#### Note: `careful` self-cleans on a toolchain bump

Not a knob, but worth knowing: the `careful` check builds an
instrumented std into a version-stable cache that cargo’s fingerprint
can’t see, so on a pinned-nightly or cargo-careful bump it runs
`cargo clean` once (announced in the log) to avoid linking stale
artifacts against a freshly rebuilt std.

### Extensibility: shipping your own tool

Another team can ship its own cargo subcommand with its own catalog while
reusing this entire engine. The downstream binary’s `main` is one line:

```rust
use std::process::ExitCode;

fn main() -> ExitCode {
    cargo_anvil::run_app(myforge::catalog())
}
```

…plus a [`Catalog`][__link1] value that starts from [`Catalog::anvil`][__link2] and
customizes the CLI identity ([`CliMeta`][__link3]) and artifact set:

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
([`CatalogBuilder::with_artifact`][__link4], [`CatalogBuilder::replace_artifact`][__link5],
[`CatalogBuilder::without_artifact`][__link6]) over the public [`artifacts`][__link7]
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

 [__cargo_doc2readme_dependencies_info]: ggGkYW0CYXSEGxYc2fK81jTWG7kWg0hlspxYGx-DzHaE-xjXG1cDT7T4wIbxYXKEG4NZNOqjh6EvG_Q0K9EA8V3RG43-IYQ08b4wG0Q3wrD2FvnsYWSBg2tjYXJnby1hbnZpbGUwLjEuMGtjYXJnb19hbnZpbA
 [__link0]: https://crates.io/crates/bolero
 [__link1]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=Catalog
 [__link2]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=Catalog::anvil
 [__link3]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=CliMeta
 [__link4]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=CatalogBuilder::with_artifact
 [__link5]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=CatalogBuilder::replace_artifact
 [__link6]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=CatalogBuilder::without_artifact
 [__link7]: https://docs.rs/cargo-anvil/0.1.0/cargo_anvil/?search=artifacts
