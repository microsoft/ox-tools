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
    │                       attribute on `anvil-pr-title`).
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

**msrustup-swappable toolchain recipes.** The private `_install-toolchain` /
`_check-toolchain` helpers are written so a downstream catalog targeting an
**msrustup-only** environment (no OSS `rustup`; e.g. SubstratePT / 1ES) can reuse
this `tools.just` verbatim via a pure `s/rustup/msrustup` body transform — no
`if`-branching on toolchain name, no flag-stripping. Two constraints make the swap
clean: (1) the presence probe is `rustup toolchain list` (a no-network check both
rustup and msrustup support; `msrustup` has no `which` subcommand), matched as a
substring because rustup suffixes the host triple while msrustup lists the bare
build name; and (2) `toolchain install` is called **flag-free** (`msrustup
toolchain install` rejects `--profile` / `--no-self-update`). The component
recipes already swap cleanly (`rustup component add` → `msrustup component add`)
and their probes use `cargo +<tc>` / `rustc +<tc>` multiplexing, which msrustup
provides. Preserve both constraints when editing these recipes.

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

## 4. Impact-scoping pass-through env vars

Every check recipe whose work is per-crate accepts an optional pass-through env var
that the cloud-workflow wiring populates from the `anvil-impact` building block. There are three
such env vars, one per cargo-delta tier:

| Env var                      | Bucket    | What recipes do with it                                                                       |
|------------------------------|-----------|------------------------------------------------------------------------------------------------|
| `ANVIL_INCLUDE_MODIFIED`  | modified  | `--skip` → recipe exits 0. Otherwise: run unconditionally (modified-tier tools are workspace-wide). |
| `ANVIL_INCLUDE_AFFECTED`  | affected  | `--skip` → recipe exits 0. Otherwise: splice the value into the cargo invocation, defaulting to `--workspace` when unset. |
| `ANVIL_INCLUDE_REQUIRED`  | required  | Same semantics as `ANVIL_INCLUDE_AFFECTED`, but consumed by recipes that need transitive dep graph in scope (doc-build, cargo-hack, udeps). |

Each var holds either the literal sentinel `--skip` (the tier is empty for this PR), or
a pre-built argument string like `--package alpha --package beta`. The cloud-workflow wiring sets
exactly one form; local invocations leave the vars unset, and recipes fall back to
`--workspace`.

A typical affected-tier recipe:

```just
anvil-clippy:
    @if [ "$ANVIL_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "anvil-clippy: no affected packages; skipping"; exit 0; \
    fi; \
    cargo clippy ${ANVIL_INCLUDE_AFFECTED:---workspace} --all-targets --all-features --locked -- -D warnings
```

A typical modified-tier recipe (the tool is workspace-wide, so there's nothing to
splice — only the skip guard matters):

```just
anvil-fmt:
    @if [ "$ANVIL_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "anvil-fmt: no modified packages; skipping"; exit 0; \
    fi; \
    cargo fmt --all --check
```

The mapping from check to bucket is fixed in the catalog (see
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping)). Unscoped checks
(`pr-title`, `deny`, `audit`, `aprz`, `mutants-full`) ignore the vars entirely — they
always run. Group recipes do not interpolate the vars themselves; each underlying check
recipe reads what it needs, so a group recipe is just a dependency list and nothing
changes when scoping is disabled.

### 4.1 The `--skip` sentinel

`--skip` is a magic string the impact step emits when a tier is empty for the PR
(typically a docs-only PR or a PR touching only files cargo-delta's
`file_exclude_patterns` ignore). It is not a valid cargo argument, so there is no risk
of collision with a real package name. Recipes test for it with `[ "$VAR" = "--skip" ]`
and exit 0 cleanly, keeping the cloud-workflow job green while signalling that nothing in that tier
needed to run.

This separation is what makes the wiring layer durably structural: "which checks can
no-op when nothing in the relevant tier is affected" is a per-check property living in
the catalog/recipe, not in the wiring layer. Moving a check between buckets is a pure
catalog change; the cloud workflow templates always thread all three vars and never gate jobs on
their values.

### 4.2 Local impact-scoped runs

Not the default. To preview what cloud workflows would skip, run cargo-delta manually and export the
env vars:

```sh
# Compute the affected-tier include list (--package … form) against origin/main.
export ANVIL_INCLUDE_AFFECTED="$(cargo delta impact --base origin/main --format cargo-args --affected)"
just anvil-pr-test
```

A wrapper recipe to compute and export all three vars in one shot is left to v2: it has
subtle git-state interactions and the manual flow is good enough for the rare case a
developer actually wants to reproduce cloud workflows scoping locally.

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
