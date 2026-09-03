# Local Recipe Surface

This document describes the `justfiles/anvil/` tree that anvil writes into a repo, how the
recipes are organized, and how local invocations differ from cloud workflows invocations (spoiler: they
don't — that's the design).

See also:

- [README.md](./README.md) for the overall principles.
- [checks.md](./checks.md) for the catalog the recipes implement.
- [updates.md](./updates.md) for how these files are tracked / regenerated.

## 1. File layout

```text
repo/
├── Justfile                                       managed-region: anvil-imports
│   # >>> anvil-managed: anvil-imports
│   import 'justfiles/anvil/mod.just'
│   # <<< anvil-managed: anvil-imports
│   …user content…
│
├── justfiles/anvil/                               owned (one checksum per file)
│   ├── mod.just            entry point: imports the sibling files and defines
│   │                       `alias anvil := anvil-pr`. The user's Justfile
│   │                       region pulls in this single file; everything else is
│   │                       reached transitively.
│   ├── helpers.just        the shared helper recipe _anvil-base-ref (resolve the
│   │                       PR base ref), reused across the impact recipe, the
│   │                       cloud impact steps, and anvil-mutants-diff.
│   ├── impact.just         the single `anvil-impact` building block: snapshots the
│   │                       base ref and working tree (two independent cache keys),
│   │                       runs `cargo delta impact`, and writes the
│   │                       target/anvil/impact/ artifacts that scoped checks read
│   │                       via _anvil-impact-include. Also owns _anvil-impact-format
│   │                       (project a cargo-delta report into per-tier
│   │                       cargo-each selector tokens). Same recipe
│   │                       locally and in CI.
│   ├── checks/             one file per check: checks/<check>.just holds the
│   │                       `anvil-<check>` recipe plus its paired `*-setup` and
│   │                       `*-validate-prereqs` recipes (anvil-fmt, anvil-clippy,
│   │                       anvil-llvm-cov, anvil-miri, …).
│   ├── groups/             one file per group: groups/<group>.just holds the
│   │                       `anvil-<group>` recipe plus its `*-setup` /
│   │                       `*-validate-prereqs` (anvil-pr-fast, anvil-pr-test,
│   │                       anvil-pr-msrv, anvil-pr-runtime-analysis, anvil-pr-mutants,
│   │                       anvil-scheduled-test, …). `anvil-pr-slow` is a
│   │                       convenience umbrella over the pr-slow groups.
│   ├── container.just      containerized execution (`anvil-container`). See containers.md.
│   ├── tiers.just          tier aggregators (anvil-pr, anvil-scheduled, anvil-full).
│   ├── tools.just          tool/component/toolchain install + validate-prereqs recipes,
│   │                       plus the cargo-spellcheck source-deps check and
│   │                       anvil-validate-prereqs.
│   └── versions.just       catalog nightly toolchains and cargo-subcommand minimum versions,
│                           plus the lazy stable-toolchain argument selector, as
│                           non-exported just variables. See §3.
│
└── .anvil/container/                              the container image definition
    ├── Dockerfile                                 composed: anvil-managed regions, your content between
    ├── Dockerfile.dockerignore
    └── hooks.ps1                                  optional; credentials, not emitted by default
```

The Justfile region is not the only file anvil adds to that the user co-owns: the
container `Dockerfile` is composed the same way, from five managed regions with
the repository's own instructions in the gaps between them (see
[containers.md](./containers.md)). Generated recipes live inside `justfiles/anvil/`;
the container image definition lives inside `.anvil/container/`. Generated files in
both directories are tool-owned (tracked by full-file checksum in the sidecar
manifest), except the composed `Dockerfile`, whose regions are tracked
individually. If the user wants to add project-specific recipes, they add them to
the top-level `Justfile` outside the managed region, or to their own additional
imported `.just` files. The alias `anvil := anvil-pr` lives in `mod.just`, not in
the user's `Justfile`, so renaming or retargeting the alias is a template update
with no managed-region churn.

Recipes in the `groups/`, `tiers.just`, and `checks/` files that actually *run* checks
are annotated with `[group("anvil")]`. The install/validate-prereqs/setup recipes
in `tools.just` (and the per-check/group/tier setup recipes colocated in the same
files) are annotated with `[group("anvil-setup")]`. `just --groups` therefore shows
two clean clusters: one for "run checks", one for "install prereqs".

> **Containerized execution.** `justfiles/anvil/container.just` adds the
> `anvil-container <command…>` recipe, which runs the given argv inside a pinned
> Linux image instead of against the host toolchain (Linux-on-Windows parity,
> toolchain pinning). Anvil recipes are reached by naming `just`
> (`just anvil-container just anvil-pr`); with no argument it opens a shell. It
> is explicit: the tiers themselves always run natively.
> The recipe bodies are unchanged; see [containers.md](./containers.md).

## 2. Recipe layers

`justfiles/anvil/` is structured to make all three levels (check, group, tier) addressable
from the command line.

### checks/

One file per individual check, `checks/<check>.just`, defining `anvil-<check>` (plus its
paired `*-setup` / `*-validate-prereqs`). Recipes are usually a single
`cargo …` line; a handful (license-headers, ensure-no-cyclic-deps,
ensure-no-default-features, pr-title, the bench smoke loop) are short `[script]` blocks.
Every check recipe depends on its `*-validate-prereqs` recipe:

```just
anvil-clippy: anvil-clippy-validate-prereqs
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The per-check `*-validate-prereqs` recipe (in the `anvil-setup` group) chains the
relevant atomic validators -- e.g. `anvil-component-default-clippy-validate-prereqs`
for clippy, plus `anvil-tool-rustc-validate-prereqs` for the toolchain pin -- each of
which calls `cargo install --list` / `rustup component list` / `rustc --version` to
confirm the tool meets the catalog's pin. Missing or below-pin tools fail with a
one-line install hint pointing at the matching `anvil-tool-<name>-install` recipe.
The cost is a handful of cheap lookups per check, well under a second on a warm cache.

Setup and validation have separate dependency graphs. Every setup path that
uses stable Cargo/Rust or installs a Cargo tool reaches
`anvil-toolchain-stable-install` first. The shared Cargo-tool installer and
default-toolchain component installers own that prerequisite, while checks
that need only built-in stable Cargo (bench, doc-build, doc-test, examples,
and loom) depend on it directly. Just deduplicates the primitive when a group,
tier, or full setup fans out across many checks. The corresponding
`*-validate-prereqs` graph never depends on an install recipe and never mutates
the environment.

### groups/

One file per cloud-workflow-visible group, `groups/<group>.just`, defining
`anvil-<tier>-<group>`. The check-recipe and group-recipe
namespaces are kept disjoint by naming choice: no check is named `<tier>-<group>` for
any tier × group combination (e.g. the coverage-instrumented test check is named
`llvm-cov`, not `test`, so that group names like `anvil-pr-test` unambiguously refer to a group recipe).

The `pr-slow` work is split into independent cloud-workflow-visible groups
(`pr-test`, `pr-msrv`, `pr-runtime-analysis`, `pr-mutants`) so they run as parallel cloud-workflow jobs/stages.
A convenience umbrella `anvil-pr-slow` recipe is also provided for local
use; it invokes those groups sequentially. `pr-mutants` (mutants) is
diff-scoped against the PR base; `scheduled-exhaustive` runs the
full-workspace mutants recipe:

```just
anvil-pr-fast: anvil-fmt anvil-clippy anvil-cargo-sort anvil-license-headers \
               anvil-ensure-no-cyclic-deps anvil-ensure-no-default-features \
               anvil-doc-build anvil-readme-check anvil-spellcheck anvil-pr-title \
               anvil-deny anvil-audit anvil-udeps anvil-semver-check \
               anvil-external-types anvil-aprz

anvil-pr-slow: anvil-pr-test anvil-pr-msrv anvil-pr-runtime-analysis anvil-pr-mutants
anvil-pr-test: anvil-llvm-cov anvil-doc-test anvil-examples
anvil-pr-msrv: anvil-msrv-test
anvil-pr-runtime-analysis: anvil-miri anvil-careful anvil-loom anvil-bolero
anvil-pr-mutants: anvil-mutants-diff

anvil-scheduled-test: anvil-llvm-cov anvil-doc-test anvil-examples
anvil-scheduled-advisories: anvil-deny anvil-audit anvil-aprz anvil-clippy
anvil-scheduled-runtime-analysis: anvil-miri anvil-miri-tree-borrows \
                                  anvil-miri-strict-provenance anvil-miri-race-coverage
anvil-scheduled-exhaustive: anvil-mutants-full anvil-cargo-hack anvil-bench
```

### tiers.just

Three tier aggregators. Each tier is a recipe that depends on the appropriate set of groups
in a deterministic order:

```just
anvil-pr: anvil-pr-validate-prereqs anvil-pr-fast anvil-pr-slow
anvil-scheduled: anvil-scheduled-validate-prereqs anvil-scheduled-test \
                 anvil-scheduled-advisories anvil-scheduled-runtime-analysis \
                 anvil-scheduled-exhaustive
anvil-full: anvil-pr anvil-scheduled
```

### tools.just

`tools.just` houses six layers of recipes:

1. **`anvil-tool-cargo-spellcheck-source-deps-check`** — probe for `libclang`, which
   `cargo-spellcheck` needs when built from source. Best-effort presence check; on
   missing deps emits per-OS install hints and exits non-zero. No auto-install. See
   §3.3.1.
2. **Private helpers** (`_install-tool`, `_check-tool`, `_install-toolchain`,
   `_check-toolchain`, `_install-component`, `_check-component`) — the single
   implementation point for "install this thing at the pinned version" and
   "verify this thing is installed at >= the pinned version".
3. **Per-toolchain recipes** — `anvil-toolchain-<symbolic>-install` and
   `anvil-toolchain-<symbolic>-validate-prereqs`. Symbolic names are `nightly` and
   `nightly-external-types`, mapped to the pinned version strings in `versions.just`.
4. **Per-component recipes** — `anvil-component-<toolchain>-<component>-install`
   and `-validate-prereqs` (e.g. `anvil-component-nightly-miri-install`).
   Component installs depend on the matching toolchain install.
5. **Per-tool recipes** — `anvil-tool-<bin>-install installer="install"` and
   `anvil-tool-<bin>-validate-prereqs` for every cargo subcommand the catalog needs
   (`cargo-nextest`, `cargo-llvm-cov`, `cargo-mutants`, …) plus `rustc` and `pwsh`.
   `installer` selects `cargo install` vs `cargo binstall`.
6. **Per-check / per-group / per-tier / global setup** — composition layer; see §3.3.

All atomic install recipes are idempotent: they early-skip when the tool is already
present at or above the pinned version (`_install-tool` uses `cargo install --list`
plus a `[version]` comparison in pwsh). So calling any composition layer on every cloud workflows
run costs nothing on a cache hit.

The full tool-version policy these recipes implement is detailed in §3 below.

## 3. Tool versions, toolchains, and installation

### 3.1 Policy

The catalog records, for each cargo subcommand, a **catalog version** (e.g.
`cargo_nextest_version := "0.9.137"`). The pin is used two different ways:

- **On install** (`anvil-tool-<bin>-install` writing into `~/.cargo/bin`): the recipe
  installs *exactly* that version (`--version '={{ pin }}'`), never `>=`. Pulling
  latest-matching at install time is a cloud-workflow reproducibility risk -- an upstream release
  between yesterday's green build and today's PR can break things, even though the
  catalog hasn't moved. Exact pins let the catalog accommodate behavior changes before
  upgrading; for example, cargo-spellcheck 0.15.7 is paired with the explicit
  `tokenization_splitchars` boundary list in `spellcheck.toml` rather than being selected
  implicitly by a range.
- **On runtime check** (`anvil-tool-<bin>-validate-prereqs`): the recipe enforces
  `installed >= pin`. A local developer who has manually upgraded a tool for their own
  reasons (e.g. needing a bugfix the catalog hasn't pinned yet) is not downgraded by
  setup. Their newer version still satisfies the gate; recipes run against it.

This asymmetry -- "install exact, accept newer if already present" -- gives cloud workflows
reproducibility *and* leaves the user in control. Bumping a pin is a deliberate
catalog edit (changing a variable in `versions.just`), not an upstream-release-triggered
surprise.

`cargo-binstall` and `just` are bootstrap utilities rather than catalog checks.
When absent, setup installs the latest compatible release available at that time;
when present, setup accepts it. They are intentionally outside the catalog's exact
installation guarantee so the bootstrap does not recursively require a versioned
installer. Their versions can therefore vary across cold environments, while every
tool that determines a catalog check's verdict remains catalog-controlled.

### 3.2 Detecting installed versions

The atomic `_check-tool` helper (a private recipe in `tools.just`) uses
`cargo install --list` to enumerate currently-installed cargo subcommands and their
versions, then checks `installed >= pin` via pwsh's `[version]` cast. This avoids the
problem of tools without a stable `--version` flag, is fast, and works uniformly for
everything the catalog cares about. For non-cargo dependencies (`just` itself, `rustc`,
`pwsh`), there are dedicated `anvil-tool-<name>-validate-prereqs` recipes that fall
back to `tool --version` and a known parser.

### 3.3 Installing tools (and toolchains, and components)

Installation is layered. The bottom layer is a per-tool / per-component / per-toolchain
install recipe (one per atomic resource); composition layers chain those.

**Atomic layer** (in `tools.just`):

- `anvil-tool-<bin>-install installer="install"` — install one cargo subcommand
  (e.g. `cargo-nextest`) at its pinned version using either `cargo install --locked`
  (the default, `installer="install"`) or `cargo binstall --locked`
  (`installer="binstall"`).
- `anvil-toolchain-<symbolic>-install` — `rustup toolchain install` for a pinned
  nightly (e.g. `nightly-2026-02-10`).
- `anvil-component-<toolchain>-<component>-install` — `rustup component add`
  on a specific toolchain. Stable components explicitly target the toolchain
  selected by an override, repository toolchain file, or MSRV fallback. Depends
  on the matching toolchain-install recipe.

Each has a matching `*-validate-prereqs` recipe that exits 0 when the resource is
already present at or above its pin and fails with a one-line install hint otherwise.

**Composition layer** (per check, per group, per tier, global):

- `anvil-<check>-setup installer="install"` — depends on every atomic-layer
  install recipe that this check needs. So `anvil-clippy-setup` brings up
  `cargo-clippy` (a default-toolchain component) and `rustc`, and nothing else.
- `anvil-<group>-setup installer="install"` — depends on every per-check setup
  in the group. cloud workflows matrix jobs call this so a `pr-fast` leg never installs
  cargo-mutants.
- `anvil-<tier>-setup installer="install"` — depends on every per-group setup
  in the tier. Local "I want to run the whole PR tier" convenience.
- `anvil-setup installer="install"` — depends on every per-tier setup. The
  catch-all that brings an empty environment up to where any catalog recipe runs.
  This is what `cargo anvil` adopters get when they run "the global one".

Every composition recipe takes the same `installer` parameter and threads it
through to the atomic-layer installs.

Mirror `*-validate-prereqs` recipes exist at every composition layer
(`anvil-<x>-validate-prereqs`), so it's possible to verify a group's
prerequisites without installing them.

The atomic installs are fully idempotent (early-skip on installed >= pin), so calling
any composition layer on every cloud-workflow run is cheap on a cache hit. There is intentionally
no separate "install-missing" variant: every install recipe IS the install-missing
recipe.

The `installer` argument:

- `install` (default) -- `cargo install --locked <tool> --version '=<pin>'`. Pure
  source builds; works in any cargo environment with no extra runtime dependency.
  Slow on a cold runner (~30 min for the full catalog) because every tool
  re-compiles common deps (`clap`, `syn`, `quote`, ...) from scratch independently.
- `binstall` -- `cargo binstall --no-confirm --locked <tool> --version '=<pin>'`.
  This selects an ordered strategy, not a binary-only backend. Anvil first asks
  cargo-binstall to install the exact pin. Tools without a source prerequisite retain
  cargo-binstall's compile strategy. For tools that declare a source prerequisite,
  Anvil disables that compile strategy so compilation cannot bypass the check. Any
  nonzero binstall result then falls back to Anvil's exact-pin `cargo install`; the
  declared prerequisite, when present, runs immediately before that fallback.
  A successful binary path cuts the cold-runner install phase from ~30 min to ~1 min.
  `cargo-binstall` itself needs to be on PATH; the GH setup composite arranges this.

The GitHub composite setup action calls `just anvil-<group>-setup binstall`
(or just `anvil-setup binstall` when no group is scoped). The ADO setup step
template uses the default `install` backend because cargo-binstall has unresolved
compliance issues for internal ADO pipelines (the binary registry it pulls from
isn't on the standard allow-list), so the slower pure-cargo path is the
conservative choice there. Locally, users pick whichever matches their environment.

#### Version source of truth

All pins live in `justfiles/anvil/versions.just` as plain just variables:
`rust_nightly`, `rust_nightly_external_types`, `cargo_nextest_version`,
`cargo_spellcheck_version`, … There is intentionally **no** sidecar data file --
edits to versions are normal catalog edits, picked up by `cargo anvil`
like any other tool-owned change.

Two prerequisites are not cargo-installable and must be present before any
install recipe can run:

- **`just`** itself -- bootstrap with `cargo install just --locked` once, or use a
  system package. Every backend's setup composite/template installs it via cargo as
  a one-shot before calling any catalog recipe.
- **`pwsh`** (PowerShell Core) -- used by every
  `[script("pwsh", "-NoProfile")]` recipe in the catalog. Disabling profiles
  keeps machine-readable output and exit behavior independent of user or system
  startup scripts. PowerShell is preinstalled on every relevant cloud-workflow
  runner (GH-hosted Linux/Windows/macOS, Microsoft-hosted ADO agents). On a
  developer machine without pwsh, `anvil-tool-pwsh-validate-prereqs` fails with
  a per-OS install hint pointing at <https://github.com/PowerShell/PowerShell>.

Trade-off acknowledged: `cargo install --locked` is slow on a cold cache (several
minutes for the full catalog). It is also the most reliable mechanism in restricted
networks. Caching (via the GH cache action and the ADO pipeline workspace cache) is
configured by the setup action/template to key on platform, toolchain
configuration, Cargo configuration, and `versions.just`. See
[github.md](./github.md#caching) and [ado.md](./ado.md#caching).

#### 3.3.1 System-level prerequisites

A small set of catalog tools have non-Rust build dependencies that `cargo install`
can't satisfy on its own. Today the only entry is `libclang`, needed by
`cargo-spellcheck` (via `clang-sys` / `hunspell-rs`) at build time. A successful
prebuilt binstall sidesteps these; a failed binstall can reach the controlled source
fallback and therefore still requires them.

Scope policy: only check for system libs that an anvil catalog tool **directly**
requires. anvil is not a general-purpose dev-env doctor. Repository-specific
system deps (e.g. `openssl-devel`, `symcrypt` for the adopter's own crates) belong
in the adopter's `setup.yml` customization, not in the anvil catalog.

Detection (`anvil-tool-cargo-spellcheck-source-deps-check`) uses presence-only probes -- file existence
in standard install dirs plus the `LIBCLANG_PATH` env var override. No version
checks: system libs upgrade independently of the catalog and any reasonably modern
libclang satisfies clang-sys.

On a missing dep the recipe prints per-OS install hints (apt-get / tdnf / brew /
scoop / winget) and exits non-zero. **No auto-install** -- admin/sudo decisions and
package-manager choice stay with the user. The cargo-spellcheck install recipe passes
`anvil-tool-cargo-spellcheck-source-deps-check` to `_install-tool` as its source prerequisite.
The prerequisite runs for an explicit source-build `install` backend and when `binstall`
cannot provide a binary. Anvil disables binstall's own compile strategy and performs the
source fallback itself, so missing libclang surfaces as a clear hint before compilation
instead of a cryptic clang-sys build error 10 minutes into the install.

Each tool with a source-build system dependency owns a tool-specific prerequisite recipe and
wires it into `_install-tool`. Catalog changes propagate to adopters via `cargo anvil` like
any other template edit.

### 3.4 Per-check warnings

Every check recipe depends on `anvil-<check>-validate-prereqs` so even ad-hoc
invocations like `just anvil-miri` fail loudly if a required tool is missing or
predates the catalog minimum, with a one-line hint pointing at the matching
`anvil-tool-<name>-install` recipe.

### 3.5 The stable Rust toolchain

Anvil selects one deterministic toolchain for every check that otherwise uses the
ambient stable toolchain. The selection order is:

1. `RUSTUP_TOOLCHAIN`, when the caller already set it. This is rustup's standard
   override for selecting an already available toolchain.
2. A root `rust-toolchain` or `rust-toolchain.toml`, delegated to rustup without
   Anvil parsing or modifying the file.
3. The root package or `[workspace.package]` `rust-version`, treated as the
   repository MSRV.

There is no runner-default fallback. A repository with neither a root toolchain
file nor a root MSRV fails with an actionable setup error rather than inheriting a
compiler that can change with the machine image.

Repositories with one compatibility floor should declare root
`[workspace.package].rust-version` (or package
`rust-version`) and have every workspace member inherit it or declare an equal
or lower minimum. Repositories with missing package MSRVs, or a member minimum
newer than the root, correct the root floor or add a root toolchain file to
select one compiler for catalog checks. Callers can instead set
`RUSTUP_TOOLCHAIN` to an already provisioned toolchain.

A caller-provided `RUSTUP_TOOLCHAIN` and the presence of either root
toolchain-file spelling both produce no explicit `+toolchain` argument. The
environment value is inherited unchanged. In the file case, rustup owns all
parsing and applies its native lookup from each Cargo or Rust command's actual
working directory; Anvil does not parse the root file, replay options from it,
or promise isolation from nested toolchain files. Only the root MSRV fallback
produces an explicit `+<version>` argument.

Setup ensures that the selected stable compiler is available before stable
Cargo or Rust use. Repository toolchain files are processed by rustup; a
missing root-MSRV toolchain is installed explicitly. Prerequisite validation is
read-only: before running workspace metadata under the root MSRV, it requires
rustup and verifies that exact toolchain is already installed. A missing
toolchain fails with the stable-setup recipe as the corrective action instead
of allowing Cargo to auto-install it.

When selection falls back to the root MSRV, Anvil reads the root manifest just
far enough to bootstrap that compiler, then prerequisite validation reads
`cargo +<root-msrv> metadata`. Cargo performs the authoritative workspace
resolution; Anvil requires every workspace package to expose a `rust_version`
no newer than the root floor. Running metadata under that compiler avoids
depending on an ambient default during the fallback itself.
Lower member minima are valid because the root compiler satisfies them.
Workspaces with missing package MSRVs or member minima above the root must
correct the declarations or choose a single catalog toolchain explicitly with
a selecting toolchain file. Anvil does not build a per-package toolchain matrix
for this uncommon case.

Cargo-installed tools and stable analyzers use this same repository-selected
compiler. Anvil does not provision a separate tooling compiler; checks that
require nightly continue to use their catalog-pinned nightly.

When the root manifest declares an MSRV, `anvil-msrv-test` runs affected-package
`cargo test --all-targets` in all-features and default-features configurations
under that compiler. This includes library and binary unit tests, integration
tests, examples, and benches as test targets. It does not add a
`--no-default-features` pass. A root toolchain file does not suppress this
minimum-version run. Without a root MSRV the recipe is a no-op.

`anvil-tool-rustc-validate-prereqs` verifies that `rustc` is available and
enforces the workspace MSRV compatibility rule. Per-check toolchain
requirements (for example, miri, careful, and udeps need nightly) remain
enforced by their matching prerequisite validation.

### 3.6 Nightly pinning

A handful of catalog checks need nightly Rust: `fmt`, `udeps`, `miri`, `careful`, and
`check-external-types`. We **pin** the nightly snapshots used by these checks rather than
floating bare `+nightly`. Pinning eliminates "rustup update on Tuesday broke main on
Wednesday" — every cloud-workflow run uses the same nightly until we deliberately bump the pin.

`fmt` is on nightly because the catalog's `rustfmt.toml` opts into `unstable_features`
to get import grouping (`imports_granularity = "Module"`, `group_imports =
"StdExternalCrate"`) and `format_code_in_doc_comments`. Those are the high-value
opinions every surveyed Microsoft Rust repo reaches for; the stable rustfmt option set
doesn't include them. Pinning is what makes nightly fmt sustainable — formatting
churn happens on a pin bump, not on every `rustup update`.

The pins live in `justfiles/anvil/versions.just` as plain just variables:

```just
rust_nightly := "nightly-YYYY-MM-DD"
rust_nightly_external_types := "nightly-YYYY-MM-DD"
```

**One source of truth, two consumers.** Recipes read the pins by `{{ }}` interpolation
(`cargo +{{ rust_nightly }} udeps ...`). The `anvil-toolchain-<name>-install`
recipes read the same variables and pass them to `rustup toolchain install`. The
setup composites/templates call those install recipes (directly or transitively via
a group's `*-setup` recipe). There is no env-file duplicate.

**Two pins, not one.** `rust_nightly` is the general-purpose nightly used by udeps, miri,
careful. `rust_nightly_external_types` is intentionally narrower: it's tied to the rustdoc
JSON schema version that the currently-selected `cargo-check-external-types` release
accepts. Bump it alongside `cargo-check-external-types` upgrades, not on the general
cadence. When the two pins resolve to the same date the setup composite installs only one
toolchain.

**Bump policy.** The general `rust_nightly` is intended to move on a regular cadence
(monthly is a reasonable default) so adopters absorb nightly drift in predictable chunks.
`rust_nightly_external_types` moves only when `cargo-check-external-types` releases a new
version that targets a newer rustdoc JSON schema. Both bumps are normal `cargo anvil
update` operations: edit `versions.just`, regenerate, validate, commit. Adopters are free
to override either pin in their `versions.just` (it's an owned file) — the next run sees
the dirt and emits a `.anvil-proposed` sibling instead of overwriting.

**Why pin, not float?** We tried floating nightly once and immediately needed
regex-based tolerance code in the `check-external-types` recipe to absorb rustdoc JSON
schema bumps. That was a tell: any tool that depends on nightly internals will routinely
break on schema/lint/intrinsic drift, and the alternative to pinning is per-tool
tolerance shims accumulating in the recipes. Pinning is one mechanism that handles all
present and future cases; tolerance shims are bespoke and silently degrade what the
check actually validates.

## 4. Impact scoping via the `anvil-impact` recipe

Impact analysis lives in **one** place: the `anvil-impact` recipe (`impact.just`). It
runs `cargo delta impact` against the working tree vs. the resolved base ref and writes
durable artifacts under `target/anvil/impact/`. The **same** recipe runs locally and in
cloud workflows — a cloud impact job runs `just anvil-impact` and the scoped checks read
its output. It is a single building block that behaves identically everywhere: CI computes
impact once and publishes it as a downloadable artifact, rather than computing it inline in
a CI shell and threading the result between jobs as environment variables.

`anvil-impact` produces, under `target/anvil/impact/`:

| Artifact                     | Purpose                                                                 |
|------------------------------|-------------------------------------------------------------------------|
| `snapshots/baseline.json`    | cargo-delta snapshot of the base ref. Cached, keyed on the composite of the base commit sha **and** the effective `.delta.toml` identity (`baseline.key` stores `<base-sha> <config-hash>`) — the expensive throwaway-worktree snapshot is retaken when the base moves *or* the cargo-delta config changes, so the baseline is never diffed against a current snapshot captured under different rules. |
| `snapshots/current.json`     | cargo-delta snapshot of the working tree. Cached, keyed on the HEAD sha (`current.key`); the dirty-tree guard widens instead of snapshotting, so a snapshotted tree always corresponds exactly to HEAD. |
| `impact.json`                | the `cargo delta impact --format json` report (the durable source of truth; `{}` when nothing changed). |
| `include_<tier>.txt`         | the pre-projected per-tier selector tokens (see below), one token per line. |

Because the modified/current set comes from cargo-delta's **committed** git diff against
the base, local scoping reflects the commits your branch adds on top of the base ref, the
same way a PR does — not un-committed working-tree edits.

### 4.1 How checks consume it

Every **impact-scoped** check depends on `anvil-impact`. Ordinary checks pass their
category's selector tokens directly to `cargo-each`:

```just
[script("pwsh", "-NoProfile")]
anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    $ErrorActionPreference = 'Stop'
    $selection = @(& "{{ just_executable() }}" _anvil-impact-include affected)
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & cargo each @selection --once '--' cargo clippy '{packages}' --all-targets --all-features --locked '--' '-D' 'warnings'
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

`_anvil-impact-include` reads `target/anvil/impact/` — the cache produced by the
`: anvil-impact` dependency (which just ran with a cache hit). This one code path serves
both worlds:

* **Local** — the check reads the cache the local `anvil-impact` run produced. Scoping is
  therefore **on by default** for `just anvil-pr`.
* **Cloud** — the group job downloaded the `target/anvil/impact/` artifact the impact job
  uploaded, so the *same* `_anvil-impact-include` call reads the *same* cache — no scoping
  is threaded between jobs via environment variables.

Each check requests one cargo-delta **category** — the selector it passes to
`_anvil-impact-include`:

| Category   | What recipes do with it                                                                       |
|------------|------------------------------------------------------------------------------------------------|
| `modified` | `--none` makes `cargo-each` a successful no-op. Otherwise `--workspace` admits the recipe's command against its normal full input domain. |
| `affected` | `cargo-each` resolves the selected members and `{packages}` forwards them to the child command. |
| `required` | Same semantics as affected, but consumed by recipes that need the transitive dependency graph in scope (doc-build, cargo-hack, udeps). |

The helper emits one token per line: `--none`, `--workspace`, or repeated
`--package` / bare-name pairs. PowerShell captures that output as an array ready to splat
without string parsing. `cargo-each` resolves the names against live workspace metadata;
when `{packages}` is used, it forwards version-qualified specs so child Cargo commands
remain unambiguous.

The helper validates this grammar before returning any cached selector. Empty files,
mixed single-value and package forms, incomplete pairs, whitespace-bearing names, and
unknown flag positions fail closed. `ANVIL_IMPACT=consume` validates all three downloaded
selector files before accepting the artifact, so a malformed cache cannot fall through
to cargo-each's default-member selection.

`_anvil-impact-format` **fails the recipe** (non-zero exit, aborting `anvil-impact`)
when at least one name cargo-delta reported cannot be resolved to exactly one workspace
package. The helper resolves each reported name against three namespaces: the exact cargo
package name, the library/proc-macro target name (snake_case, for crates whose package and
lib names differ), and the manifest-directory leaf (for deeply nested workspaces where
cargo-delta emits a directory name). A name is unresolvable when it maps to **no** package
*or* to **more than one** distinct package (a genuinely ambiguous identifier). Because
cargo-delta reports *library* identifiers rather than unique cargo specs, an unresolvable
name signals a gap in this reverse-mapping — not a scoping decision — so the helper logs
`cargo-delta returned unknown package '<name>'` or `cargo-delta returned ambiguous package
identifier '<name>'` to stderr and exits non-zero rather than guessing (and risking a
silently under-scoped tier that skips a check). Failing hard surfaces the mapping gap so it
gets fixed, instead of masking it behind a silently full-workspace run.

Formatting illustrates the modified-tier contract. The tier decides only whether
the recipe runs. Once admitted, the recipe validates its pinned tools and uses
`cargo each --workspace` to launch one bounded rustfmt command per workspace
member. The script propagates the aggregate child status, and unlike
`cargo fmt --all`, it intentionally does not discover non-member local path
dependencies.

The mapping from check to bucket is fixed in the catalog (see
[checks.md §5](./checks.md#5-impact-scoping-check--include-mapping)). Unscoped checks —
`pr-title`, `deny`, `audit`, `aprz`, `mutants-full`, and the repo-level-input checks
`readme-check` and `spellcheck` (whose inputs cargo-delta maps to no package) — take no
`anvil-impact` dependency and never resolve a scope; they always run. Group recipes do not
resolve scope themselves; each underlying check reads what it needs.

### 4.2 Empty tiers

`_anvil-impact-format` emits cargo-each's native `--none` selector when a tier is empty
(typically a docs-only PR, or a PR touching only files cargo-delta's
`file_exclude_patterns` ignore). Ordinary recipes pass it directly to cargo-each, which
exits successfully without spawning the child command. Orchestration-heavy recipes
detect it before performing setup that would otherwise have side effects or, for `fmt`,
before starting its separate full-workspace per-manifest cargo-each fan-out.

### 4.3 Disabling scoping and the escape hatch

Set `ANVIL_IMPACT=off` in the environment to disable scoping entirely: `anvil-impact`
(and its snapshot dependency) no-op without touching git or cargo-delta, and
`_anvil-impact-include` returns `--workspace` for every tier. Because `just` runs a recipe's dependencies in the
same environment, the guard is honored even when `anvil-impact` fires as a check
dependency. This is exactly how the **scheduled** and **full** tiers stay full-workspace:
`anvil-scheduled` / `anvil-full` wrap their private recipe in `_anvil-unscoped`
(`anvil-scheduled: (_anvil-unscoped "scheduled")`), which exports
`ANVIL_IMPACT=off` before invoking the private `_anvil-<tier>` recipe, so the whole
dependency tree runs unscoped. The export lives in the wrapper because a
dependency-only tier recipe cannot set env for its own dependencies — they run before its
body. The scheduled *groups* (`anvil-scheduled-test`, …) wrap the same way, so a
scheduled group invoked directly is full-workspace too.

`ANVIL_IMPACT` is a strict tri-state — `off`, `consume`, or unset. Any other value makes
all three read sites exit 2 with an actionable error, so a typo like `ANVIL_IMPACT=on`
fails loudly rather than silently leaving scoping on. `consume` is CI-only: a group job
that already downloaded the `target/anvil/impact` artifact sets it to trust that cache
verbatim — `anvil-impact` no-ops after asserting the cache is present — and it is never set
locally.

The two-key cache means the expensive baseline snapshot is only retaken when the base ref
moves, and the working-tree snapshot only when the tree changes; an unchanged repo yields
a full cache hit (`anvil-impact: impact set up to date`). To force a recompute, delete
`target/anvil/impact/`.

### 4.4 Uncommitted changes widen to the full workspace

cargo-delta scopes on the **committed** diff of `HEAD` against the base ref, so an
uncommitted change — a crate you are actively editing but have not committed — is invisible
to it and would otherwise be silently scoped out, skipping the checks for the very crate you
are working on. To make local runs safe by default, `anvil-impact` detects an uncommitted
(dirty) working tree — any tracked edit or new, non-ignored untracked file — and widens
**every** tier to the full workspace, printing a one-line notice. Work-in-progress is
therefore never skipped; committing your changes restores impact scoping for the next run.

This only affects local runs: cloud-workflow checkouts are clean (the PR head is committed),
so CI always gets the scoped, committed-diff result. It is deliberately conservative over
fast — a dirty tree runs everything. Commit to scope by impact, or use `ANVIL_IMPACT=off`
(which also runs the full workspace, and additionally skips cargo-delta entirely).

### 4.5 Base-ref resolution and failure modes

`anvil-impact` resolves the base ref through `_anvil-base-ref` and computes the committed
diff of `HEAD` against it. It deliberately **does not** run `git fetch`: mutating git state
as a side effect of a build check is surprising and can race the user's own git operations.
The base is therefore a prerequisite the caller must satisfy, which yields three distinct
outcomes rather than one catch-all fallback:

- **Base ref not present locally** — the recipe fails fast, naming the missing ref and the
  recovery command (`git fetch origin <branch>`, then retry), rather than silently scoping
  against nothing.
- **Shallow clone** — a shallow history cannot produce a trustworthy diff, so the recipe
  fails with unshallow guidance (`git fetch --unshallow`) rather than under-scoping.
- **Base predates the workspace** — when the base commit has no root `Cargo.toml` (first
  adoption of anvil, or a base older than the workspace itself), the baseline snapshot
  records a `baseline-no-workspace` marker and `anvil-impact` widens **every** category to
  its full-workspace/run default. First adoption therefore validates broadly instead of
  failing.

The missing-base and shallow-clone cases are hard errors because a clean checkout with no
resolvable full-history base has no diff to trust; the workspace-less-base case still
carries a meaningful broad-validation signal, so it widens instead. A warm offline cache
can still hit these paths outside `consume` mode, because a cache miss recomputes the
baseline — so an environment without the base ref must either provide it, run with
`ANVIL_IMPACT=off`, or (in CI) download the cache and set `ANVIL_IMPACT=consume`.


## 5. Daily driver

```text
$ just anvil
[just] running anvil-pr-validate-prereqs
[just] running anvil-pr-fast
[just] running anvil-pr-slow
anvil OK
```

`anvil` is an alias for `anvil-pr` (set in the managed `Justfile` region). All three tiers
(`anvil-pr`, `anvil-scheduled`, `anvil-full`) are first-class -- locally reproducible with
exactly the same arguments cloud workflows uses, because cloud workflows invokes the same `just` recipes.

## 6. No-tooling fallback

A user with only `cargo` (no `just`, no `cargo-anvil`) can still run the basics:

```sh
cargo test   --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The same commands appear as the body of the corresponding `just` recipes under
`justfiles/anvil/checks/`, so they are discoverable by reading that check's file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective
tools.

## 7. Customization at the recipe level

Per the four customization tiers in [README.md §7](./README.md#7-customization):

- **Add your own recipes** to the top-level `Justfile` outside the managed region. The
  Justfile's managed region only contains `import` lines and an alias — your recipes never
  collide with it.
- **Add your own `.just` files** and `import` them after the managed region's closing
  sentinel.
- **Override a single anvil recipe**: the `just` import-and-override rules make this awkward
  (just doesn't have a "the most specific definition wins" rule). The recommended way is to
  copy the recipe you want to change into your top-level Justfile with a different name
  (e.g. `my-clippy`) and reference *that* from your own group/tier recipes. Don't fight the
  anvil-* names; just compose around them.
- **Disable a recipe wholesale**: opt out of the managed `Justfile` region per
  [updates.md §opt-out](./updates.md#6-opting-out-in-file-stubs). This stops the imports from
  happening at all, so all `anvil-*` recipes vanish. Use this only when anvil is no longer
  the right tool for your repo.

Customizing the *contents* of `justfiles/anvil/*.just` is supported — they're owned files,
so editing them flips them to "dirty" and the next `update` writes a `.anvil-proposed`
sibling instead of overwriting. See [updates.md](./updates.md) for the lifecycle.
