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

* The owned `justfiles/anvil/` recipe tree (`tools.just`, `checks/`,
  `groups/`, `tiers.just`).
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
  written or proposed, or if Anvil refuses to manage an artifact it
  cannot safely inspect.
* `--force` — override the single-tool guard and switch the repository to
  this tool, then run a normal update. A repo is managed by exactly one
  anvil-family tool (recorded as `tool` in `.anvil.lock`); without
  `--force`, a run refuses when that field names a different tool.

`--version` prints the build version plus, on a second line, the
`catalog:` checksum — a `sha256` over the whole compiled-in catalog — so
two builds at the same version but different catalogs are distinguishable.

Before running ordinary checks, provide a deterministic Rust selection:
a caller-set `RUSTUP_TOOLCHAIN`, either root `rust-toolchain` file spelling,
or a root `[workspace.package].rust-version` / `[package].rust-version`.
Anvil passes no explicit selector for the environment or file cases so
rustup handles them natively; only the root MSRV becomes `+<version>`.
Repositories with none of these sources fail instead of inheriting the
runner’s ambient compiler.

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
imported `.just` files, including impact scoping: a local `just anvil-pr`
runs `anvil-impact` (via [`cargo-delta`][__link0])
and scopes each check to the affected packages, exactly as a cloud-workflow
PR run does. The scheduled and full tiers deliberately opt out
(`ANVIL_IMPACT=off`) and run every check over the whole workspace.

The generated GitHub scheduled workflow publishes failures as GitHub
issues. On failure, it best-effort reuses an open marker-owned issue and
comments when later scheduled runs also fail. A maintainer closes the issue
after resolving the incident; successful runs do not close it automatically.
Repositories can disable this behavior by setting the
`ANVIL_PUBLISH_FAILURE_ISSUE` Actions repository variable to `false`.

### Containerized local checks

Any generated recipe can be executed inside a content-addressed Linux
image. The image installs the Rust toolchain and Cargo tools this
repository pins by running `just anvil-setup`, the same recipe the checks
use, reading the same generated pins, so the image and the host agree on
the toolset by construction, with no second tool list to keep in step.

Execution is opt-in per invocation: `just anvil-pr` and every other recipe
continue to run natively, and a container is entered only through
`anvil-container`, whose arguments are the argv executed inside the image.
Those arguments are whitespace-delimited tokens: `just` joins a variadic
parameter with spaces before the recipe sees it, so an argument that itself
contains a space cannot be recovered and does not survive the round trip.

```text
just anvil-container just anvil-clippy         # one check
just anvil-container just anvil-pr             # the whole PR tier
just anvil-container just anvil-setup binstall # a recipe with an argument
just anvil-container cargo build               # any other command
just anvil-container                           # interactive shell
```

The feature is three generated artifacts and one optional hook script, with
no configuration file: `justfiles/anvil/container.just` drives the engine,
`.anvil/container/Dockerfile` and its `Dockerfile.dockerignore` define what
the image contains, and `.anvil/container/hooks.ps1` supplies credentials
when a repository needs them.

One container is created per invocation, however many checks the requested
recipe runs. The repository is bind-mounted at `/workspace`, so `target/`
stays visible from the host. Cargo’s download caches are named volumes,
keeping that write-heavy path off the host boundary; `CARGO_HOME` and
`RUSTUP_HOME` themselves are deliberately not mounted, since a volume would
pin the first image’s tools over every later one.

#### Prerequisites

* A container engine callable from the shell that runs `just`: Docker, or
  Podman via `ANVIL_CONTAINER_ENGINE=podman`. On Windows that means Docker
  Desktop, Podman, a Windows `docker` CLI pointed at an engine in WSL, or
  Docker Engine installed only inside the default WSL distribution. No
  Windows CLI is needed in that last case, since anvil reaches the engine
  through `wsl.exe` when it finds none on `PATH` and translates repository
  paths with `wslpath`.
* `just` and `PowerShell` Core (`pwsh`) on the host.
* A repository-owned `rust-toolchain.toml`.

Docker is supported; Podman works on a best-effort basis, with two
documented gaps on Windows. The image is pinned to `linux/amd64`, so on
ARM64 hosts it is emulated and is substantially slower.

#### Image identity

The tag *is* a SHA-256 digest over the inputs that define the image:
everything under `.anvil/container/`, `rust-toolchain.toml`, and the whole
generated `justfiles/anvil/` tree. The container directory is walked rather
than named file by file, because the Dockerfile is composed and a
repository can `COPY` a certificate or an install script it places there.
The recipe tree is included in
full because the image installs its tools by running `just anvil-setup`,
whose dependency chain runs through the tier, group and check recipes
before it reaches the install recipes – so the routing decides *whether* a
tool is installed just as surely as `tools.just` decides *how*.
A changed tool pin names a tag that cannot already exist, so a build
follows. There is no staleness check because there is no staleness to
detect: a locally built image that is present was built from the inputs
that name it. An image *fetched* by the resolve hook only claims as much,
since the digest is over source files and cannot be re-derived from layers,
so that claim is only as strong as the registry it came from, which should
have immutable tags and restricted push.

`anvil-container-tag` prints the reference without building it, and is the
single place the digest is computed, so a publisher can tag an image with
exactly the reference a consumer will later look up.

#### Controls

|Variable|Effect|
|--------|------|
|`ANVIL_CONTAINER_ENGINE`|`docker` (default) or `podman`. Read at run time.|
|`ANVIL_CONTAINER_NO_REBUILD=1`|Fail when the image is missing instead of building it, which distinguishes a cache miss from a build failure.|
|`ANVIL_CONTAINER_NO_RESOLVE=1`|Skip the resolve hook, so a query never pulls.|
|`ANVIL_CONTAINER_NO_CACHE=1`|Rebuild a tag that already resolves, ignoring the hook.|
|`ANVIL_IN_CONTAINER=1`|Set inside the image; makes a nested invocation run natively.|
|`GITHUB_TOKEN`|Forwarded when set on the host. When it is not, one is derived from `gh auth token` — but only for a target whose plan reads the variable, or for the interactive shell.|

Supporting recipes: `anvil-container-tag`, `anvil-container-status`
(reports the engine and image without building or pulling), and
`anvil-container-down` (removes this repository’s cache volumes). To rebuild
a tag that already resolves, scope `ANVIL_CONTAINER_NO_CACHE` to the one
invocation — an exported value is read by *every* later container command,
so a forgotten one rebuilds from scratch each time:

```text
$env:ANVIL_CONTAINER_NO_CACHE = '1'
try { just anvil-container just anvil-fmt } finally { Remove-Item Env:ANVIL_CONTAINER_NO_CACHE }
```

#### The hook

crates.io needs no credentials, so no hook is emitted by default. A
repository or a downstream catalog that needs one adds
`.anvil/container/hooks.ps1`, which the recipe loads by path whenever the
file is present, whoever wrote it:

```powershell
function Anvil-BuildSecrets { @{ Secrets = @{ feed = (mint-a-token) } } }
function Anvil-RunEnv       { @{ Env     = @{ FEED_TOKEN = (mint-a-token) } } }
function Anvil-ResolveImage { param($tag) (fetch-a-published-image $tag) }
```

All three are optional. Build secrets are passed to `BuildKit` by
environment variable name, so a value never reaches a process argument and
never reaches an image layer; run-time values are forwarded into the
container by name for the same reason. An empty value is a hard error,
because a build that quietly proceeded without its credential would install
a reduced tool set and then be tagged with the digest a credentialed build
produces.

`Anvil-ResolveImage` is offered the tag when nothing local matches, and
returns the reference it made available: a registry reference, used as-is
rather than re-tagged locally, so the run stays honest about where the
image came from. Its presence is checked before use – which proves
something carries that reference, not that the contents match the digest –
and every failure falls through to a local build: a publisher that has not
caught up must not block the change it has not caught up with.

The hook executes on the host, with the invoking user’s permissions, before
any container isolation exists. Only use one from a repository or catalog
you trust.

#### Customizing the image

`.anvil/container/Dockerfile` is a **user-composed file with managed
regions**: anvil owns five regions inside it and keeps them current, and the
gaps between them are the repository’s. Add to the gap that matches when the
addition is needed – re-declare `ARG BASE_IMAGE` to build on another base,
a root CA or proxy before the first download, libraries a catalog tool
compiles against before `anvil-setup`, run-time tools after it. Adding in a
gap leaves anvil’s content alone, so base and tool-pin bumps keep landing;
editing inside a region is preserved rather than overwritten, but freezes
those pins at the moment of the edit, which is why the gaps exist.

A downstream catalog that needs a different base OS for every repository it
manages replaces the base and tool regions instead, inheriting the catalog
install and the entry contract. A replacement that copies more of the tree
must replace the ignore file with it, since the build context admits only
`justfiles/anvil/`, `.anvil/container/` and `rust-toolchain.toml`. See
[`artifacts::container`][__link1] and the design document for the full contract, the
host setup for each engine, and the known limitations.

### Checks and tiers

Checks are grouped into **tiers** (`anvil-pr`, `anvil-scheduled`) that
fan out to **groups** (one cloud-workflow job each), which in turn run
individual checks sequentially. `anvil-full` runs both tiers.

The catalog and per-check rationale live in `docs/design/checks.md`;
the tables below map each check to the group that runs it, link each
check to its tool’s documentation, and note anything anvil-specific.

**PR tier** (`anvil-pr`) — runs on every pull request, impact-scoped
both locally and in cloud workflows. `pr-fast` is one job, while the
`pr-slow` groups run as independent parallel jobs per OS leg:

<table>
  <thead><tr><th>Umbrella</th><th>Group</th><th>Check</th><th>Notes</th></tr></thead>
  <tbody>
    <tr><td rowspan="15"><code>pr-fast</code></td><td rowspan="15">—</td><td><a href="https://rust-lang.github.io/rustfmt/">fmt</a></td><td>predefined configuration with nightly features</td></tr>
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
    <tr><td><a href="https://crates.io/crates/cargo-semver-checks">semver-check</a></td><td>findings and inconclusive comparisons are advisory (posts a PR comment); Anvil preflight failures remain enforcing</td></tr>
    <tr><td><a href="https://crates.io/crates/cargo-check-external-types">external-types</a></td><td></td></tr>
    <tr><td rowspan="9"><code>pr-slow</code></td><td rowspan="3"><code>pr-test</code></td><td><a href="https://crates.io/crates/cargo-llvm-cov">llvm-cov</a></td><td>dual feature-config; gated by <a href="https://crates.io/crates/cargo-coverage-gate">cargo-coverage-gate</a></td></tr>
    <tr><td><a href="https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html">doc-test</a></td><td>runs both feature configs</td></tr>
    <tr><td><a href="https://doc.rust-lang.org/cargo/commands/cargo-build.html">examples</a></td><td>compile-only</td></tr>
    <tr><td><code>pr-msrv</code></td><td>msrv-test</td><td>dual feature-config, all-target tests under the declared MSRV</td></tr>
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

#### Scheduled failure issue publication (GitHub)

The generated GitHub scheduled workflow creates or updates
`[Anvil] Scheduled checks failed` when a scheduled group fails.
To disable this behavior without editing an Anvil-owned workflow,
set the Actions repository variable `ANVIL_PUBLISH_FAILURE_ISSUE`
to `false` under **Settings → Secrets and variables → Actions →
Variables**. Removing the variable or setting any other value
restores the default publication behavior.

### In-tree tool customization

anvil follows a few source-level and `Cargo.toml` conventions so you
can customize how some of the executed tools behave from within your
own crates — without editing the generated `justfiles/anvil/` tree.

#### Spelling dictionary (`spellcheck`)

The `spellcheck` check ([`cargo-spellcheck`][__link2])
reads a repo-root `.spelling` file — one word per line — as its custom
dictionary. Add project-specific terms (crate names, acronyms,
identifiers) there to silence false positives; the `anvil-spellcheck`
recipe sorts and filters it into the dictionary cargo-spellcheck
consumes. Keep the file `LF`-terminated.

#### Coverage (`llvm-cov`)

Coverage is gated by [`cargo-coverage-gate`][__link3];
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

…plus a [`Catalog`][__link4] value that starts from [`Catalog::anvil`][__link5] and
customizes the CLI identity ([`CliMeta`][__link6]) and artifact set:

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
([`CatalogBuilder::with_artifact`][__link7], [`CatalogBuilder::replace_artifact`][__link8],
[`CatalogBuilder::without_artifact`][__link9]) over the public [`artifacts`][__link10]
registry. The `tool` field recorded in `.anvil.lock` keeps two
anvil-family tools from clobbering one another in a shared repo (see `--force`).
See `docs/design/extensibility.md`.

### Design docs

See `docs/design/` for the full architecture:

* `README.md` — overall principles and CLI shape.
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

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjNhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbLvVGTNtetQUbnp9vX0Ew7_gbkZEyxfXZXyMbltL72AXa-o1hZIGDa2NhcmdvLWFudmlsZTAuNi4wa2NhcmdvX2Fudmls
 [__link0]: https://crates.io/crates/cargo-delta
 [__link1]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=artifacts::container
 [__link10]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=artifacts
 [__link2]: https://crates.io/crates/cargo-spellcheck
 [__link3]: https://crates.io/crates/cargo-coverage-gate
 [__link4]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=Catalog
 [__link5]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=Catalog::anvil
 [__link6]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=CliMeta
 [__link7]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=CatalogBuilder::with_artifact
 [__link8]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=CatalogBuilder::replace_artifact
 [__link9]: https://docs.rs/cargo-anvil/0.6.0/cargo_anvil/?search=CatalogBuilder::without_artifact
