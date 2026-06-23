# Local Recipe Surface

This document describes the `justfiles/anvil/` tree that anvil writes into a repo, how the
recipes are organized, and how local invocations differ from cloud workflows invocations (spoiler: they
don't — that's the design).

See also:

- [design.md](./design.md) for the overall principles.
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
└── justfiles/anvil/                               owned (one checksum per file)
    ├── mod.just            entry point: imports the sibling files and defines
    │                       `alias anvil := anvil-pr`. The user's Justfile
    │                       region pulls in this single file; everything else is
    │                       reached transitively.
    ├── checks.just         per-check recipes (anvil-fmt, anvil-clippy, anvil-llvm-cov, …).
    │                       Starts with `set unstable` (needed for the `[script("pwsh")]`
    │                       attribute on `anvil-pr-title`). Each per-crate check depends on
    │                       `anvil-impact` and resolves its scope via `_anvil-impact-include`
    │                       (see §4).
    ├── impact.just         the `anvil-impact` recipe + `_anvil-impact-snapshot` and
    │                       `_anvil-impact-include` helpers. Computes the cargo-delta impact
    │                       set into `target/anvil/impact/` (durable artifacts) and resolves
    │                       per-tier include scope for the check recipes. See §4.
    ├── groups.just         group recipes (anvil-pr-fast, anvil-pr-test,
    │                       anvil-pr-runtime-analysis, anvil-pr-mutants, anvil-scheduled-test, …)
    │                       plus a convenience `anvil-pr-slow` umbrella that
    │                       invokes the three pr-slow* sub-recipes sequentially.
    ├── tiers.just          tier aggregators (anvil-pr, anvil-scheduled, anvil-full).
    ├── tools.just          tool/component/toolchain install + validate-prereqs recipes,
    │                       plus anvil-system-deps-check and anvil-validate-prereqs.
    └── versions.just       pinned nightly toolchains and pinned cargo-subcommand versions
                            as plain just variables (rust_nightly, cargo_nextest_version, …).
                            Read by recipes via `{{ var }}` interpolation.
                            Single source of truth for all version pins. See §3.
```

The Justfile region is the only file anvil adds to that the user co-owns, and it's
a single `import` line — everything anvil-specific lives inside `justfiles/anvil/`.
All files under that directory are tool-owned (tracked by full-file checksum in
the sidecar manifest). If the user wants to add project-specific recipes, they add them
to the top-level `Justfile` outside the managed region, or to their own additional
imported `.just` files. The alias `anvil := anvil-pr` lives in `mod.just`, not in
the user's `Justfile`, so renaming or retargeting the alias is a template update with
no managed-region churn.

Recipes in `groups.just`, `tiers.just`, and `checks.just` that actually *run* checks
are annotated with `[group("anvil")]`. The install/validate-prereqs/setup recipes
in `tools.just` (and the per-check/group/tier setup recipes appended to the other
files) are annotated with `[group("anvil-setup")]`. `just --groups` therefore shows
two clean clusters: one for "run checks", one for "install prereqs".

## 2. Recipe layers

`justfiles/anvil/` is structured to make all three levels (check, group, tier) addressable
from the command line.

### checks.just

One recipe per individual check, each named `anvil-<check>`. Recipes are usually a single
`cargo …` line; a handful (license-headers, ensure-no-cyclic-deps,
ensure-no-default-features, pr-title, the bench smoke loop) are short `[script]` blocks.
Every check recipe depends on its `*-validate-prereqs` recipe; per-crate checks additionally
depend on `anvil-impact` and resolve their scope from the impact cache (see §4):

```just
anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    # body resolves scope via `_anvil-impact-include affected`, then:
    # cargo clippy <include> --all-targets --all-features --locked -- -D warnings
```

Unscoped checks (`pr-title`, `deny`, `audit`, `aprz`, `mutants-full`) take neither the
`anvil-impact` dependency nor the scope preamble — they always run the full workspace.

The per-check `*-validate-prereqs` recipe (in the `anvil-setup` group) chains the
relevant atomic validators -- e.g. `anvil-component-default-clippy-validate-prereqs`
for clippy, plus `anvil-tool-rustc-validate-prereqs` for the toolchain pin -- each of
which calls `cargo install --list` / `rustup component list` / `rustc --version` to
confirm the tool meets the catalog's pin. Missing or below-pin tools fail with a
one-line install hint pointing at the matching `anvil-tool-<name>-install` recipe.
The cost is a handful of cheap lookups per check, well under a second on a warm cache.

### groups.just

One recipe per cloud-workflow-visible group, named `anvil-<tier>-<group>`. The check-recipe and group-recipe
namespaces are kept disjoint by naming choice: no check is named `<tier>-<group>` for
any tier × group combination (e.g. the coverage-instrumented test check is named
`llvm-cov`, not `test`, so that group names like `anvil-pr-test` unambiguously refer to a group recipe).

The `pr-slow` work is split into three independent cloud-workflow-visible sub-groups
(`pr-test`, `pr-runtime-analysis`, `pr-mutants`) so they run as parallel cloud-workflow jobs/stages.
A convenience umbrella `anvil-pr-slow` recipe is also provided for local
use; it invokes the three sub-recipes sequentially. `pr-mutants` (mutants) is
diff-scoped against the PR base; `scheduled-exhaustive` runs the
full-workspace mutants recipe:

```just
anvil-pr-fast: anvil-fmt anvil-clippy anvil-cargo-sort anvil-license-headers \
               anvil-ensure-no-cyclic-deps anvil-ensure-no-default-features \
               anvil-doc-build anvil-readme-check anvil-spellcheck anvil-pr-title \
               anvil-deny anvil-audit anvil-udeps anvil-semver-check \
               anvil-external-types anvil-aprz

anvil-pr-slow: anvil-pr-test anvil-pr-runtime-analysis anvil-pr-mutants
anvil-pr-test: anvil-llvm-cov anvil-doc-test anvil-examples
anvil-pr-runtime-analysis: anvil-miri anvil-careful
anvil-pr-mutants: anvil-mutants-diff

anvil-scheduled-test: anvil-llvm-cov anvil-doc-test anvil-examples
anvil-scheduled-advisories: anvil-deny anvil-audit anvil-aprz anvil-clippy
anvil-scheduled-exhaustive: anvil-mutants-full anvil-cargo-hack anvil-bench
```

### tiers.just

Three tier aggregators. Each tier is a recipe that depends on the appropriate set of groups
in a deterministic order:

```just
anvil-pr: anvil-pr-validate-prereqs anvil-pr-fast anvil-pr-slow
anvil-scheduled: anvil-scheduled-validate-prereqs anvil-scheduled-test anvil-scheduled-advisories \
               anvil-scheduled-exhaustive
anvil-full: anvil-pr anvil-scheduled
```

### tools.just

`tools.just` houses six layers of recipes:

1. **`anvil-system-deps-check`** — probe for system-level libs that catalog tools need to
   build from source (currently: `libclang` for `cargo-spellcheck`). Best-effort presence
   check; on missing deps emits per-OS install hints and exits non-zero. No auto-install.
   See §3.3.1.
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

The catalog records, for each cargo subcommand, a **pinned version** (e.g.
`cargo_nextest_version := "0.9.122"`). The pin is used two different ways:

- **On install** (`anvil-tool-<bin>-install` writing into `~/.cargo/bin`): the recipe
  installs *exactly* that version (`--version '={{ pin }}'`), never `>=`. Pulling
  latest-matching at install time is a cloud-workflow reproducibility risk -- an upstream release
  between yesterday's green build and today's PR can break things, even though the
  catalog hasn't moved. `cargo-spellcheck 0.15.7`'s em-dash word-boundary regression is
  the canonical example: with `>=0.15.1` the catalog would have silently picked it up,
  breaking every PR until the catalog was edited. With `=0.15.1` the catalog locks in
  the version it was validated against.
- **On runtime check** (`anvil-tool-<bin>-validate-prereqs`): the recipe enforces
  `installed >= pin`. A local developer who has manually upgraded a tool for their own
  reasons (e.g. needing a bugfix the catalog hasn't pinned yet) is not downgraded by
  setup. Their newer version still satisfies the gate; recipes run against it.

This asymmetry -- "install exact, accept newer if already present" -- gives cloud workflows
reproducibility *and* leaves the user in control. Bumping a pin is a deliberate
catalog edit (changing a variable in `versions.just`), not an upstream-release-triggered
surprise.

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
  on a specific toolchain. Depends on the matching toolchain-install recipe.

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
  Downloads a prebuilt binary from each tool's GitHub Releases when available.
  Cuts the cold-runner install phase from ~30 min to ~1 min. `cargo-binstall`
  itself needs to be on PATH; the GH setup composite arranges this.

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
- **`pwsh`** (PowerShell Core) -- used by every `[script("pwsh")]` recipe in the
  catalog. Preinstalled on every relevant cloud-workflow runner (GH-hosted
  Linux/Windows/macOS, Microsoft-hosted ADO agents). On a developer machine
  without pwsh, `anvil-tool-pwsh-validate-prereqs` fails with a per-OS install
  hint pointing at <https://github.com/PowerShell/PowerShell>.

Trade-off acknowledged: `cargo install --locked` is slow on a cold cache (several
minutes for the full catalog). It is also the most reliable mechanism in restricted
networks. Caching (via the GH cache action and the ADO pipeline workspace cache) is
configured by the setup action/template to key on `Cargo.lock`, the toolchain
channel, and `versions.just`. See
[github.md](./github.md#caching) and [ado.md](./ado.md#caching).

#### 3.3.1 System-level prerequisites

A small set of catalog tools have non-Rust build dependencies that `cargo install`
can't satisfy on its own. Today the only entry is `libclang`, needed by
`cargo-spellcheck` (via `clang-sys` / `hunspell-rs`) at build time. The `binstall`
install path sidesteps these entirely by downloading prebuilt binaries.

Scope policy: only check for system libs that an anvil catalog tool **directly**
requires. anvil is not a general-purpose dev-env doctor. Repository-specific
system deps (e.g. `openssl-devel`, `symcrypt` for the adopter's own crates) belong
in the adopter's `setup.yml` customization, not in the anvil catalog.

Detection (`anvil-system-deps-check`) uses presence-only probes -- file existence
in standard install dirs plus the `LIBCLANG_PATH` env var override. No version
checks: system libs upgrade independently of the catalog and any reasonably modern
libclang satisfies clang-sys.

On a missing dep the recipe prints per-OS install hints (apt-get / tdnf / brew /
scoop / winget) and exits non-zero. **No auto-install** -- admin/sudo decisions and
package-manager choice stay with the user. Tool-install recipes that need a system
lib depend on `anvil-system-deps-check` (only on the source-build `install`
backend), so missing system libs surface as a clear hint instead of a cryptic
clang-sys build error 10 minutes into the install.

Adding a new system dep is a one-block catalog change in `tools.just`; it
propagates to adopters via `cargo anvil` like any other catalog edit.

### 3.4 Per-check warnings

Every check recipe depends on `anvil-<check>-validate-prereqs` so even ad-hoc
invocations like `just anvil-miri` fail loudly if a required tool is missing or
predates the catalog minimum, with a one-line hint pointing at the matching
`anvil-tool-<name>-install` recipe.

### 3.5 The Rust toolchain

`rust-toolchain.toml` is read but never written, and anvil never installs the *project's*
Rust toolchain itself. Per-backend rationale lives in [github.md](./github.md#rust-toolchain)
and [ado.md](./ado.md#rust-toolchain); short version: msrustup owns it on ADO/1ESPT, the
runner image owns it on GH, the user owns it locally.

`anvil-tool-rustc-validate-prereqs` validates the installed `rustc` against the
catalog's minimum at recipe time; a below-minimum `rustc` produces a clean failure
message naming the version mismatch. Per-check toolchain requirements (e.g. miri,
careful, udeps need nightly) are enforced by the matching
`anvil-toolchain-<name>-validate-prereqs` recipe, which suggests the
user-environment-appropriate install command in the failure message
(`rustup install nightly-YYYY-MM-DD` or "ask your team's pipeline owner to add
nightly to msrustup").

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

## 4. Impact scoping

PR-tier checks only need to run against the crates a change can actually affect. anvil
computes that blast radius with [`cargo-delta`](https://crates.io/crates/cargo-delta)
and skips unaffected crates. The same computation runs **locally and in cloud workflows**
through a single recipe — `anvil-impact` — so a one-file PR runs the same narrow set of
crates whether the developer types `just anvil-pr` or the PR-tier workflow fires.

The previous design ran the impact analysis only in the cloud-workflow wiring (a composite
action / step template that emitted pre-built `--package …` strings as job outputs, threaded
forward as env vars). Local runs had no equivalent and always fell back to `--workspace`.
This section describes the recipe-owned model that replaces it: the analysis is a `just`
recipe that writes **durable artifacts** under `target/anvil/`, every per-crate check depends
on it, and the cloud-workflow wiring simply *shares those artifacts between jobs* instead of
recomputing or re-threading anything.

> **Guiding principle restated.** `cargo-anvil` writes files; `just` runs them. Impact
> analysis is no exception: it lives in `impact.just`, not in a backend's YAML. The cloud
> workflows transport the artifact between jobs; they do not own the logic.

### 4.1 The `anvil-impact` recipe and its artifacts

`justfiles/anvil/impact.just` defines one public recipe and two private helpers:

- **`_anvil-impact-snapshot`** (private) — produces the two cargo-delta snapshots the
  impact compare needs:
  - `target/anvil/impact/snapshots/current.json` — `cargo delta snapshot` of the working
    tree (HEAD plus any uncommitted edits).
  - `target/anvil/impact/snapshots/baseline.json` — `cargo delta snapshot` of the base ref.
    cargo-delta has no `--base` shortcut, so the baseline is snapshotted inside a throwaway
    `git worktree add --detach` at the base ref; the worktree is removed in a `finally` block
    so an interrupted run never leaves one behind.
- **`anvil-impact`** (public, `[group("anvil")]`) — the entry point. Its body, in order:
  short-circuits when `ANVIL_IMPACT=off`; otherwise validates that `cargo-delta` meets its
  pin, invokes `_anvil-impact-snapshot`, runs `cargo delta impact --baseline … --current …
  --format json` to produce `impact.json`, and projects each tier into a pre-built include
  string written to its cache file. The snapshot and the cargo-delta prereq check are invoked
  **from the body, not as `just` dependencies**, so the single `ANVIL_IMPACT=off` guard skips
  all of them — including the cargo-delta requirement — which is what lets a scheduled run
  that never installed cargo-delta fire this recipe as a dependency and cleanly no-op.
- **`_anvil-impact-include <tier>`** (private) — the resolver every per-crate check calls to
  learn its scope for a tier. See §4.2.

The full artifact set under `target/anvil/impact/`:

| File                          | Producer            | Contents                                                                              |
|-------------------------------|---------------------|---------------------------------------------------------------------------------------|
| `snapshots/baseline.json`     | `_anvil-impact-snapshot` | cargo-delta snapshot of the base ref.                                             |
| `snapshots/baseline.sha`      | `_anvil-impact-snapshot` | The base commit sha `baseline.json` was taken at. The baseline cache key.          |
| `snapshots/current.json`      | `_anvil-impact-snapshot` | cargo-delta snapshot of the working tree.                                         |
| `snapshots/current.state`     | `_anvil-impact-snapshot` | `<HEAD sha> <working-tree-diff hash>`. The current-snapshot cache key.             |
| `impact.json`                 | `anvil-impact`      | Raw `cargo delta impact` output (TitleCase `Modified` / `Affected` / `Required` sets). The durable source of truth. |
| `include_modified.txt`        | `anvil-impact`      | Projection of the `Modified` tier into `--package X --package Y …`, or the literal `--skip` when empty. |
| `include_affected.txt`        | `anvil-impact`      | Same projection for the `Affected` tier (modified ∪ workspace rev-deps).               |
| `include_required.txt`        | `anvil-impact`      | Same projection for the `Required` tier (affected ∪ workspace-internal transitive deps). |

`impact.json` is the durable representation; the three `include_*.txt` files are a
*projection* of it into the exact argument shape recipes splice. The `cargo-delta`-emitted
crate names are library names (snake_case); the projection maps them back to cargo package
names (which may be hyphenated, e.g. `cargo_anvil` → `cargo-anvil`) and drops any name that
isn't a real workspace package, exactly as the prior cloud-workflow formatter did — that
logic now lives in the recipe, in one place, instead of being duplicated across the GitHub
and ADO templates.

`target/` is cargo's build directory and is git-ignored, so these artifacts never enter the
working tree the way a sidecar metadata file would. Putting them under `target/anvil/` keeps
them next to the build outputs they describe and makes them trivially shareable as a
cloud-workflow pipeline artifact (§4.4).

#### Idempotency and the two independent cache keys

The two snapshots have **different, independent** cache keys, because they cost very
different amounts to produce and change at different rates:

- **`baseline.json`** is the expensive one: snapshotting the base ref requires creating and
  tearing down a throwaway `git worktree`. But it only depends on *where the base ref points*
  — not on anything the developer is editing. So it is keyed solely on the base commit sha
  (`snapshots/baseline.sha`). `_anvil-impact-snapshot` resolves the base sha
  (`git rev-parse <base>`, sub-second) and **only recreates the worktree and re-snapshots the
  baseline when that sha differs** from `baseline.sha`. On a normal edit-rebuild loop the base
  doesn't move, so the worktree is never recreated after the first run.
- **`current.json`** is cheap (an in-place snapshot of the working tree) and changes on every
  edit. It is keyed on `<HEAD sha> <working-tree-diff hash>` (`snapshots/current.state`) and
  re-taken whenever the working tree changes.

`anvil-impact` then re-runs `cargo delta impact` and rewrites the projection only when either
snapshot was regenerated (or `impact.json` / an `include_*.txt` is missing). Cache validity is
keyed on the **content** of these marker files, never on file mtimes — mtimes are not reliably
preserved when the artifacts are uploaded and re-downloaded as a cloud-workflow artifact
(§4.4), so a downstream job that restored a fresh cache must still recognise it as fresh. Each
marker is written *after* its snapshot completes, so a half-finished run never looks like a
cache hit.

So the first `just anvil-pr` after a code change re-snapshots the working tree and recomputes
impact (seconds), but pays the worktree/baseline cost only when the base actually moved.
A downstream cloud-workflow job that downloaded the artifact at the same commit sees both keys
match and no-ops entirely.

> **No-op when scoping is off.** `_anvil-impact-snapshot`, `anvil-impact`, and
> `_anvil-impact-include` each read the `ANVIL_IMPACT` environment variable at the top of
> their body and short-circuit when it is `off` (no git, no snapshot, no cargo-delta;
> `_anvil-impact-include` returns the tier default). Because the variable is read from the
> process environment, it is honored by these recipes even when they run as *dependencies* of
> another recipe — `just` runs a dependency in the same environment as the invocation, so a
> caller that exports `ANVIL_IMPACT=off` before invoking `just` disables scoping for the whole
> run, deps included. (A recipe cannot set it for its own dependencies from its body, because
> deps run before the body; see §4.3 for how the scheduled tier handles this.)

#### Base-ref resolution, fresh clones, shallow clones

The recipe resolves the base ref in this order:

1. `$BASE_REF` if set (adopter / wiring override). It is run through the same normalization
   as the branch names below — a bare branch (`release`) or a `refs/heads/`-qualified ref is
   resolved to `origin/<branch>`; an already-qualified remote ref (`origin/release`) is used
   as-is — so the ADO wiring can hand it `$(System.PullRequest.TargetBranch)` directly.
2. The PR target-branch name reported by the backend — `$GITHUB_BASE_REF` on GitHub,
   `$SYSTEM_PULLREQUEST_TARGETBRANCH` on ADO — **normalized** by stripping any leading
   `refs/heads/` and prefixing `origin/`. (ADO reports the target branch as
   `refs/heads/main` in some contexts and the short name in others; the normalization
   collapses both to `origin/main`.)
3. `origin/main`, then `origin/master`.

The recipe **never fetches on the developer's behalf** — mutating the local git state as a
side effect of a build check is surprising and can race with the user's own git operations.
If the resolved base ref isn't present locally (a fresh clone that never fetched the base
branch) it fails fast with a one-line `git fetch origin <branch>` hint. On a **shallow**
clone — where the base ref's history is truncated and the worktree snapshot can't be
materialized — it fails fast with a one-line `git fetch --unshallow` hint rather than
producing a silently-wrong impact set.

### 4.2 How checks consume the impact set

Every per-crate check recipe gains two things:

1. `anvil-impact` as a dependency, so the cache is fresh before the check reads it.
2. A call to `_anvil-impact-include <tier>` at the top of its body to resolve its scope.

`_anvil-impact-include <tier>` resolves a tier's scope with a simple rule and echoes the
result:

1. If `ANVIL_IMPACT=off`, return the **tier default** — `--workspace` for the
   affected/required tiers, empty (run unconditionally) for the modified tier.
2. Otherwise, if `target/anvil/impact/include_<tier>.txt` is present (the cache file
   `anvil-impact` just wrote, or that a cloud-workflow job downloaded; §4.4), return its
   contents.
3. Otherwise, return the tier default. (This only happens if `anvil-impact` itself decided
   not to write a cache — e.g. it was a no-op — so the safe behavior is to run wide.)

There is intentionally **no per-tier override env var**. The only knob is `ANVIL_IMPACT=off`
(§4.3), which forces the whole run to full-workspace; a per-tier "force this exact
`--package` set" override would be a footgun (it silently diverges local results from what
cargo-delta actually computed) with no real use case, so it isn't offered.

Putting the rule in one helper means each check recipe is a two-line preamble plus its
`cargo …` line. Because recipes in this tree are `[script("pwsh")]` (see
[mod.just](#1-file-layout) note), the helper and the checks are pwsh:

```just
# affected-tier check
[script("pwsh")]
anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    $include = (just _anvil-impact-include affected)
    if ($include -eq '--skip') { Write-Host 'anvil-clippy: no affected crates; skipping'; exit 0 }
    cargo clippy @($include -split ' ') --all-targets --all-features --locked -- -D warnings
```

```just
# modified-tier check (tool is workspace-wide; only the skip guard matters)
[script("pwsh")]
anvil-fmt: anvil-fmt-validate-prereqs anvil-impact
    if ((just _anvil-impact-include modified) -eq '--skip') { Write-Host 'anvil-fmt: no modified crates; skipping'; exit 0 }
    cargo fmt --all --check
```

The check → tier (bucket) mapping is fixed in the catalog; the full table and the rationale
for each assignment live in
[checks.md §5](./checks.md#5-impact-scoping-check--tier-mapping). Unscoped checks
(`pr-title`, `deny`, `audit`, `aprz`, `mutants-full`) take neither the dependency nor the
preamble — they always run. Group recipes and the `anvil-pr` tier remain plain dependency
lists that never read the include values themselves (the scheduled/full tiers are the one
exception — thin wrappers; see §4.3), so moving a check between groups or tiers changes
nothing in the wiring.

#### The `--skip` sentinel

`--skip` is the magic value `anvil-impact` writes into a tier's cache file when that tier is
empty (a docs-only PR, or a PR touching only files cargo-delta's `file_exclude_patterns`
ignore). It is not a valid cargo argument, so it can never collide with a real package name.
Recipes test for it explicitly and exit 0, keeping the run green while signalling that
nothing in that tier needed to run. This is what makes "which checks can no-op when their
tier is empty" a per-check property living in the recipe, not in the wiring.

### 4.3 Turning scoping off (forcing a full-workspace run)

There is exactly one lever: export `ANVIL_IMPACT=off` *in the environment that invokes
`just`*. Because `just` runs dependencies in that same environment, the guard at the top of
`anvil-impact` / `_anvil-impact-snapshot` / `_anvil-impact-include` is honored even when
those recipes run as dependencies of a check: `anvil-impact` no-ops without computing
anything (no git, no snapshot, no cargo-delta), and `_anvil-impact-include` returns the tier
default (`--workspace` / run). So `ANVIL_IMPACT=off just anvil-clippy` runs clippy over the
whole workspace, and `ANVIL_IMPACT=off just anvil-pr` runs the entire PR tier unscoped. This
is also how the **scheduled tier stays full-workspace**.

The scheduled cloud-workflow jobs set `ANVIL_IMPACT=off` at the job/stage level, so the
whole invocation — deps included — runs unscoped. For local exhaustive runs the catch is
that a *dependency-only* tier recipe cannot set the variable for the check deps it pulls in
(deps run before any body). So `anvil-scheduled` and `anvil-full` are **thin two-stage
wrappers**: their body exports `ANVIL_IMPACT=off` and then re-invokes `just` on the actual
group aggregator (`just _anvil-scheduled-impl` / `… anvil-pr _anvil-scheduled-impl`). That
nested invocation inherits the variable, so its check deps no-op the impact recipe and run
the whole workspace — and a PR-shaped cache left in `target/` by an earlier `anvil-pr` run
never scopes them. (`anvil-pr` stays a plain dependency list, leaves `ANVIL_IMPACT` at its
default, and is scoped.) Running a single scheduled check directly without the wrapper is
scoped like any other check unless the developer exports `ANVIL_IMPACT=off` themselves.

This keeps the catch-all property of the scheduled tier intact: scheduled always runs the
whole workspace, so anything PR-scoping skipped is caught within the schedule window.

### 4.4 Sharing the artifacts across cloud-workflow jobs

Because the impact set is a set of files under `target/anvil/impact/`, the cloud-workflow
backends do not recompute it per job and no longer thread `--package` strings as job
outputs. Instead:

1. A dedicated **impact job/stage** runs `just anvil-impact` and uploads
   `target/anvil/impact/` as a pipeline artifact.
2. Each downstream group job **downloads** that artifact into `target/anvil/impact/`
   before running its group recipe. When the group recipe's checks fire their
   `: anvil-impact` dependency, the recomputed cache keys match the marker files in the
   downloaded artifact (`baseline.sha` and `current.state`), so the recipe no-ops (a
   content-keyed cache hit) and the checks read the downloaded `include_*.txt` files. Both
   keys are OS-independent on a clean CI checkout — they are commit shas and the hash of an
   empty working-tree diff — so a Windows or aarch64 group job recognises the cache produced
   by the Linux impact job.

The artifact share is purely an **optimization, not a correctness requirement**. If the
share wiring is removed, each group job's `: anvil-impact` dependency simply recomputes the
impact set locally (the recipe handles both paths identically) — the only cost is repeating
the snapshot work per job. anvil therefore includes `cargo-delta` in every PR-tier group's
setup so the recompute fallback works even when no artifact is present.

The backend-specific mechanics — which composite action / step template uploads and
downloads, and how the fallback setup is wired — are in
[github.md §6](./github.md#6-impact-scoping) and
[ado.md §5](./ado.md#5-per-group-step-templates).

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

The same commands appear as the body of the corresponding `just` recipes in
`justfiles/anvil/checks.just`, so they are discoverable by reading that file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective
tools.

## 7. Customization at the recipe level

Per the four customization tiers in [design.md §7](./design.md#7-customization):

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
