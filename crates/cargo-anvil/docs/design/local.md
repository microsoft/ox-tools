# Local Recipe Surface

Anvil generates `just` recipes used both locally and in CI. Start with
`just anvil-pr` for PR checks, or select a group or individual check.
See [checks.md](./checks.md) for the catalog and
[updates.md](./updates.md) for generated-file ownership.

## Requirements

The supported floor for repositories using the recipes is **Rust and Cargo
1.95 or newer**. The selected compiler must also satisfy the repository and
installed tools' own requirements. Building cargo-anvil from this source tree
requires the workspace's declared Rust version, currently 1.97.

The recipe validators currently check toolchain availability and workspace MSRV
compatibility, not a universal Rust/Cargo 1.95 minimum. Passing prerequisite
validation with an older compiler does not make that configuration supported.

Local execution also requires rustup and **just 1.47.0+**. PowerShell 7+
(`pwsh`) is needed only for container commands. Non-container recipes use
portable Just logic and Cargo tools. Impact analysis uses cargo-delta. Spellcheck
dictionary preparation and source-build prerequisite checks use
`cargo-spellcheck-helper` without PowerShell. Setup installs catalog tools and
Rust components; install the prerequisites needed by the selected recipes
before invoking them.

## 1. File layout

| Path | Purpose |
|------|---------|
| `Justfile` | Imports `.anvil/anvil.just` through a managed region; other content belongs to the repository. |
| `.anvil/anvil.just` | Recipe entry point and `anvil` alias for `anvil-pr`. |
| `.anvil/checks.just` | Individual checks, impact analysis, and developer commands. |
| `.anvil/groups.just` | Check groups and PR/scheduled/full tiers. |
| `.anvil/helpers.just` | Private recipes and shared definitions. |
| `.anvil/setup.just` | Tool pins, setup recipes, and prerequisite validation. |
| `.anvil/container.just` | Container commands and helpers; optional when a derived catalog removes container support. |
| `.anvil/container/` | Container image definition. |

Generated recipe files are tool-owned. The top-level `Justfile` and container
`Dockerfile` have managed regions, leaving room for repository-specific content.
See [containers.md](./containers.md) for container configuration.

## 2. Recipe layers

```console
just --list
just anvil-clippy
just anvil-pr-fast
just anvil-pr-slow
just anvil-pr
just anvil-scheduled
just anvil-full
```

Individual checks compose into groups, then tiers. `anvil-pr-slow` runs the
test, MSRV, runtime-analysis, fuzzing, and mutation groups sequentially.
CI can run those groups in parallel. Scheduled groups and the scheduled/full
tiers always cover the full workspace; expensive PR checks use impact scoping.
Lightweight formatting, manifest sorting, and license-header checks always run
over their full workspace/input domain, including for an empty impact set.

`just --list` shows checks, groups, tiers, development commands, setup, and
container commands. Scoped setup and validation recipes are hidden from the
listing but callable:

```console
just anvil-clippy-setup
just anvil-pr-fast-setup binstall
just anvil-pr-validate-prereqs
just --show anvil-clippy-setup
```

Checks validate prerequisites before execution. Groups and tiers validate their
prerequisites up front. Validation is read-only and reports how to install
missing tools; it does not run setup automatically.

Tiers execute natively. To use the pinned Linux container explicitly:

```console
just anvil-container just anvil-pr
```

## 3. Tool versions, toolchains, and installation

### Setup and version policy

```console
just anvil-setup
just anvil-setup binstall
just anvil-validate-prereqs
```

Setup can target a check, group, tier, or the whole catalog. The default
`install` backend builds tools from source; `binstall` tries prebuilt binaries
and falls back to source builds. Setup bootstraps cargo-binstall if needed.
GitHub setup uses `binstall`; ADO uses source installation.

Cargo tools are installed at their exact catalog versions. Already-installed
versions at or above the catalog minimum are accepted and are not downgraded.
Pins are recorded in `.anvil/setup.just`. Bootstrap utilities such as cargo-binstall
are not covered by the catalog's exact-version guarantee.

### System prerequisites

Spellcheck setup installs the pinned `cargo-spellcheck-helper` before installing
cargo-spellcheck. Dictionary preparation uses the helper to turn `.spelling`
into `target/spelling.dic`: missing input yields an empty dictionary; empty
and ASCII-decimal-only lines are removed; entries are sorted ordinally with
duplicates retained. Output is atomic UTF-8 with LF line endings.

Building cargo-spellcheck from source requires discoverable libclang and native
C/C++ build tools, checked by `cargo spellcheck-helper check-source-deps`. On
Windows, its directory must also be on `PATH`; `LIBCLANG_PATH` alone is
insufficient. Missing prerequisites produce installation guidance rather than
an automatic system-package installation. A successful prebuilt installation
avoids this source dependency, but a source fallback still needs it.

Repository-specific native libraries and services must be provisioned by the
repository's own setup. Generated GitHub Linux spellcheck setup installs
`libclang-dev`; local and self-hosted environments remain responsible for their
system packages.

### The stable Rust toolchain

Checks without a dedicated nightly or MSRV select their compiler in this order:

1. Caller-provided `RUSTUP_TOOLCHAIN`.
2. A root `rust-toolchain` or `rust-toolchain.toml`, interpreted by rustup.
3. Root `[workspace.package].rust-version`, or root `[package].rust-version`.

No selection means Cargo uses its default toolchain. Toolchain files retain
rustup's native behavior, including lookup from each command's working
directory; nested toolchain files can therefore affect commands.

With the root-MSRV fallback, every workspace member must declare or inherit a
`rust-version` no higher than the root value. Correct inconsistent declarations
or select the compiler explicitly. Cargo tools are independent applications
and install with the Cargo available to the setup caller; workspace toolchain
selection applies when checks compile or analyze workspace code.

`anvil-msrv-test` separately tests the root MSRV even when a toolchain file
selects a newer compiler. Without a root MSRV, setup, validation, and execution
are successful no-ops. Environments substituting an internally built compiler
must expose it through rustup under the declared public version.

### Nightly pinning

Nightly checks use catalog pins from `.anvil/setup.just`. The general nightly
supports formatting, coverage, unused-dependency checks, Miri, careful, and
Bolero. External-types has a separate nightly matched to its rustdoc JSON schema.
A newer installed external-types tool can pass version validation yet fail
execution if its schema is incompatible with that pin.

## 4. Impact scoping via the `anvil-impact` recipe

`just anvil-impact` computes package impact against the merge base of `HEAD`
and the selected base ref. Scoped checks invoke it automatically. CI publishes
its results for group jobs to consume.
[Check buckets](./checks.md#5-impact-scoping-check--include-mapping)
determine which package set each check uses.

### Scope controls

| `ANVIL_IMPACT` | Behavior |
|----------------|----------|
| Unset | Compute or reuse impact for the current checkout. |
| `off` | Run full-workspace checks without impact analysis. |
| `consume` | Read a previously produced impact cache; missing required files fail. |

Other nonempty values fail before checks run. Scheduled groups and the
scheduled/full tiers set `off` automatically.

Local runs with tracked edits, staged edits, deletions, renames, or non-ignored
untracked files snapshot the current filesystem and compute fresh package
selections through cargo-delta's managed impact mode. An empty impact set skips
scoped package work successfully; unscoped checks still run, including the
unused-deps workspace dependency inheritance check.

GitHub Actions and Azure Pipelines impact jobs require clean checkouts and fail
if tracked or non-ignored untracked content is present. `consume` mode continues
to trust only the downloaded clean cache.

### Base ref and failures

The shared base resolver uses `BASE_REF`, then `SYSTEM_PULLREQUEST_TARGETBRANCH`
(ADO), then `GITHUB_BASE_REF`, then `origin/main`.
CI target branch names resolve to remote-tracking refs under `origin`.
The ref and enough history to resolve its merge base must exist locally;
recipes do not fetch. Git and cargo-delta failures stop the check. A merge base
without a root workspace manifest selects all current packages.

The current checkout's selected Rust toolchain is also used for metadata in
delta's temporary baseline worktree, so an older baseline toolchain file does
not trigger a separate toolchain installation. Baseline checkouts skip Git LFS
smudging. Cargo-delta owns worktree creation, snapshot generation, cache
freshness, and cleanup; Anvil does not maintain another snapshot cache.

Malformed impact data, unknown package names, and failed metadata or discovery
commands are errors, not empty selections.

### Package files and snapshot cache

CI computation writes package files under `target/anvil/impact/` for artifact
publication. Local computation writes the same files directly under
`target/anvil/impact-invocations/<invocation-id>/`, isolated from other Just
invocations and the CI artifact path:

| File | Meaning |
|------|---------|
| `modified.packages` | Modified workspace package specifications. |
| `affected.packages` | Modified packages plus their workspace dependents. |
| `required.packages` | Affected packages plus their workspace dependencies. |

Each file is directly consumable by cargo-each. An empty file is an explicit
successful no-op for scoped checks. Dependent recipes start only after all
three outputs succeed. The local invocation path is known while Just evaluates
recipe arguments; no selector files, publication copies, or runtime wrappers
are needed. The invocation id defaults to the Just process id and is inherited
by nested invocations. Local package files remain until `target/` is cleaned;
they are not an impact-result cache and are recomputed on each invocation.

Cargo-delta caches baseline and current snapshots under the Cargo target
directory's `cargo-delta/`. Its embedded keys include checkout content,
configuration, workspace identity, and tool version. Repeated tier calculations
reuse matching snapshots, including for unchanged dirty trees. Compute mode
requires cargo-delta even on a snapshot cache hit; `off` and `consume` do not.
Delete that snapshot directory to force snapshot regeneration.

In consume mode, `ANVIL_IMPACT_INPUT_DIR` selects another read-only cache
directory, using an absolute path or a path relative to the recipe's working
directory. All three package files are required. The override is ignored
outside consume mode. Container execution rejects a consume-mode directory
override because it names a host path.

## 5. Miri execution customization

Miri setup installs its pinned nightly components and prepares the sysroot.
Initial preparation can require registry access. A setup failure fails the
calling check/group/tier setup as well.

Miri runs all-features library and binary unit tests and integration tests.
The selected package scope compiles together once, preserving workspace feature
unification, and Cargo's JSON artifact stream is handed to cargo-each.
Executables run from their owning package roots concurrently, with one worker
per logical processor by default, limited by the executable count. Set
`ANVIL_MIRI_JOBS` to a positive integer to limit memory use. Each executable's
output is labelled and replayed in deterministic artifact order; failures are
aggregated after all executables finish. Successful runs remove the PID-scoped
artifact handoff; failed runs retain it under `target/` for diagnosis.

Package-wide exclusions apply to all Miri profiles:

```toml
[package.metadata.anvil.miri]
exclude = true
```

An excluded package can still compile as another package's dependency.
Individual tests can use `#[cfg_attr(miri, ignore = "<reason>")]` or the
[profile-specific cfgs](./checks.md#scheduled-runtime-analysis).

## 6. Daily driver

```console
just anvil
just anvil-build
just anvil-build --package my-crate
just anvil-doc-build --open
just anvil-examples --package my-crate --example basic
just anvil-fmt --fix
just anvil-unused-deps --fix
just anvil-miri --package my-crate --test test_name
just anvil-miri --package my-crate --example basic
just anvil-readme --fix
```

`anvil` aliases `anvil-pr`. `anvil-build` builds all workspace targets with all
features and the lockfile; it is a development command, not a check-group member.
`anvil-readme` checks by default and regenerates with `--fix`.

### Unused dependencies

`anvil-unused-deps` is check-only by default, including in groups and CI.
Explicit `--fix` removes uninherited workspace catalog entries, never package
dependency declarations. Compiler-backed package analysis uses the required
impact set; workspace dependency inheritance is checked even when that set is
empty. Disabling impact analyzes every workspace package.

### Examples

`anvil-examples` builds and executes affected examples, or all examples in
scheduled/full runs. `--package` overrides impact selection; `--example`
requires `--package` and selects one example.

Examples execute sequentially with all features, `ANVIL_EXAMPLE=1`, no
application arguments, and live stdout/stderr. The per-example timeout defaults
to 30 seconds; `--timeout <seconds>` accepts 1–3600. Failures and timeouts fail
the check after the remaining selected examples run.

Exclude examples needing interaction, credentials, or unprovisioned services:

```toml
[package.metadata.anvil.examples]
no-run = ["interactive-demo", "needs-production-credentials"]
```

Excluded examples still compile. Explicit `--package` and `--example` selection
runs an excluded example.

### Targeted Miri

For `anvil-miri`, `--package` overrides impact scope, `--test` filters libtest
names, and `--example` selects one example instead of the test suite.
`--example` requires `--package`.

## 7. No-tooling fallback

Basic checks remain available without Anvil's additional tooling:

```console
cargo test --workspace --tests --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

These are partial checks, not substitutes for the Anvil PR tier.

## 8. Customization at the recipe level

Add repository recipes to `Justfile` outside the managed import region, or import
your own `.just` files there. Use distinct names, such as `my-clippy`, and compose
custom groups around them.

Editing an owned recipe file prevents automatic replacement: the next update
writes a `.anvil-proposed` sibling for reconciliation. Empty owned files and
managed import regions have different update behavior; see
[updates.md](./updates.md).

## 9. Mutation testing with cargo-gamma

`anvil-mutants-diff` and `anvil-mutants-full` use cargo-gamma 0.7.0+ with a
100% assertion-killed score requirement and zero unresolved flaky outcomes.
Timeouts and out-of-memory outcomes do not count as assertion kills.

The PR recipe selects mutation sites from changed lines against the resolved
base, including uncommitted changes. The full recipe covers the workspace.
An empty impact set skips the PR campaign. Gamma 0.7.0 accepts an empty diff
selection, but an empty full campaign fails its score gate. The Gamma changes
in this source tree also accept empty full campaigns; that behavior requires
a release containing those changes and is not guaranteed by Anvil's 0.7.0 pin.
Wholly ungradeable nonempty populations fail their gates. Infrastructure and
baseline failures still fail.

Gamma reads native `gamma.toml` configuration. Mutators, Cargo features, build
profile, tests, concurrency, and memory policy follow Gamma's settings and
defaults. Anvil supplies workspace selection, diff scope, and strict gates.
On GitHub Actions it also fixes the report directory to `target/cargo-gamma`;
local and ADO runs honor Gamma's configured directory.

The recipes pass `--update-hints`, so an invoked campaign promotes learned hints
into `gamma-hints.yaml` even if the campaign fails. Campaign failures take
precedence over promotion failures; a promotion failure fails an otherwise
successful run. Skipped campaigns do not update hints.

See [GitHub reports](./github.md#gamma-reports-and-code-scanning) for artifact
and SARIF publication.
