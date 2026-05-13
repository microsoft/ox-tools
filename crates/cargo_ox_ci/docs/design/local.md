# Local Recipe Surface

This document describes the `justfiles/ox-ci/` tree that ox-ci writes into a repo, how the
recipes are organized, and how local invocations differ from CI invocations (spoiler: they
don't — that's the design).

See also:

- [design.md](./design.md) for the overall principles.
- [checks.md](./checks.md) for the catalog the recipes implement.
- [updates.md](./updates.md) for how these files are tracked / regenerated.

## 1. File layout

```text
repo/
├── Justfile                                       managed-region: ox-ci-imports
│   # >>> ox-ci-managed: ox-ci-imports
│   # checksum: sha256:…  rendered-by: cargo-ox-ci 0.4.1
│   import 'justfiles/ox-ci/checks.just'
│   import 'justfiles/ox-ci/groups.just'
│   import 'justfiles/ox-ci/tiers.just'
│   import 'justfiles/ox-ci/tools.just'
│   alias ox-ci := ox-ci-pr
│   # <<< ox-ci-managed: ox-ci-imports
│   …user content…
│
└── justfiles/ox-ci/                               owned (one checksum per file)
    ├── checks.just          per-check recipes (ox-ci-fmt, ox-ci-clippy, ox-ci-test, …)
    ├── groups.just          group recipes (ox-ci-pr-fast, ox-ci-pr-test, ox-ci-nightly-test, …)
    ├── tiers.just           tier aggregators (ox-ci-pr, ox-ci-nightly, ox-ci-full)
    └── tools.just           ox-ci-tools-check + ox-ci-tools-install + helpers
```

The Justfile region is the only file ox-ci adds to that the user co-owns. The four files
under `justfiles/ox-ci/` are tool-owned (full file checksums). If the user wants to add
project-specific recipes, they add them to the top-level `Justfile` outside the managed
region, or to their own additional imported `.just` files.

## 2. Recipe layers

`justfiles/ox-ci/` is structured to make all three levels (check, group, tier) addressable
from the command line.

### checks.just

One recipe per individual check, each named `ox-ci-<check>`. Recipes are usually a single
`cargo …` line; a handful (license-headers, ensure-no-cyclic-deps,
ensure-no-default-features, pr-title, the bench smoke loop) are short `[script]` blocks.
Every check recipe is prefixed with a quick version-gate dependency:

```just
ox-ci-clippy: (_ox-ci-require "cargo-clippy")
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

`_ox-ci-require` is a private recipe in `tools.just` that calls `cargo install --list` to
verify the tool meets the catalog's declared minimum version; missing or below-minimum
tools fail with a one-line `cargo install` hint. The cost is one cheap `cargo install
--list` invocation per check, well under a second on a warm cache.

### groups.just

One recipe per group, named `ox-ci-<tier>-<group>`. Where a check name and a group's nested
check name would collide (the `pr-test` group contains a check also called `test`), the
check recipe is suffixed `-only`: `ox-ci-test-only` is the single-check recipe. The
`pr-mutants` group runs the diff-scoped recipe; `nightly-exhaustive` runs the full-workspace
recipe:

```just
ox-ci-pr-fast: ox-ci-fmt ox-ci-clippy ox-ci-cargo-sort ox-ci-license-headers \
               ox-ci-ensure-no-cyclic-deps ox-ci-ensure-no-default-features \
               ox-ci-doc-build ox-ci-readme-check ox-ci-spellcheck ox-ci-pr-title \
               ox-ci-deny ox-ci-audit ox-ci-udeps ox-ci-semver-check \
               ox-ci-external-types ox-ci-aprz

ox-ci-pr-test: ox-ci-test-only ox-ci-doc-test ox-ci-examples
ox-ci-pr-mutants: ox-ci-mutants-diff

ox-ci-nightly-test: ox-ci-test-only ox-ci-doc-test ox-ci-examples
ox-ci-nightly-advisories: ox-ci-deny ox-ci-audit ox-ci-aprz ox-ci-clippy ox-ci-udeps
ox-ci-nightly-runtime: ox-ci-miri ox-ci-careful
ox-ci-nightly-exhaustive: ox-ci-mutants-full ox-ci-cargo-hack ox-ci-bench-only
```

### tiers.just

Three tier aggregators. Each tier is a recipe that depends on the appropriate set of groups
in a deterministic order:

```just
ox-ci-pr: ox-ci-tools-check ox-ci-pr-fast ox-ci-pr-test ox-ci-pr-mutants
ox-ci-nightly: ox-ci-tools-check ox-ci-nightly-test ox-ci-nightly-advisories \
               ox-ci-nightly-runtime ox-ci-nightly-exhaustive
ox-ci-full: ox-ci-pr ox-ci-nightly
```

### tools.just

- `ox-ci-tools-check` — print a status table of every tool's installed version vs. minimum.
- `ox-ci-tools-install` — install every catalog tool at the minimum version (or skip if
  already satisfied). Used as a one-shot in CI setup and locally on first use.
- `ox-ci-tools-install-missing` — install only the tools that are missing or below minimum.
- `_ox-ci-require <tool>` — internal helper called by each check.

The full tool-version policy these recipes implement is detailed in §3 below.

## 3. Tool versions and installation

### 3.1 Policy

The tool **never pins exact versions** for the user. The catalog records, for each tool, a
*minimum required version* (e.g. `cargo-nextest >= 0.9.122`). Users are free to install
newer versions, use `mise`/`asdf`, install via package manager, etc.

### 3.2 Detecting installed versions

`_ox-ci-require <tool>` (a private `just` recipe in `tools.just`) uses
`cargo install --list` to enumerate currently-installed cargo subcommands and their
versions, then compares against the catalog minimum. This avoids the problem of tools
without a stable `--version` flag, is fast, and works uniformly for everything the tool
cares about (all the cargo-* checks). For the small number of non-cargo dependencies
(`just` itself and `pwsh`), the recipe falls back to `tool --version` and a known parser.

### 3.3 Installing tools

`ox-ci-tools-install` and `ox-ci-tools-install-missing` are plain `just` recipes that loop
over the catalog and run `cargo install --locked <tool> --version >=<min>`. They are the
*only* mechanism the tool uses to install cargo-managed tools — there is no separate code
path for CI. CI setup just calls the recipes. Locally, the user runs the recipes once
when `ox-ci-tools-check` complains.

Two prerequisites are not cargo-installable and must be present before the recipes can
run:

- **`just`** itself — bootstrap with `cargo install just --locked` once, or use a system
  package. Every backend's setup composite/template installs it via cargo as a one-shot.
- **`pwsh`** (PowerShell Core) — used by four `[script]` recipes
  (`license-headers`, `ensure-no-cyclic-deps`, `ensure-no-default-features`, `pr-title`)
  for cross-platform shell logic. Preinstalled on every relevant CI runner (GH-hosted
  Linux/Windows/macOS, Microsoft-hosted ADO agents). On a developer machine without
  pwsh, `_ox-ci-require pwsh` fails with a per-OS install hint pointing at
  <https://github.com/PowerShell/PowerShell>. We use pwsh-everywhere rather than a
  bash/pwsh split because (a) it's already a hard dep on Windows, (b) it's pre-staged in
  CI, and (c) maintaining one helper implementation is materially simpler than two.

Trade-off acknowledged: `cargo install --locked` is slow on a cold cache (several minutes
for the full catalog). It is also the most reliable mechanism in restricted networks.
Caching (via the GH cache action and the ADO pipeline workspace cache) is configured by the
setup action/template to key on `Cargo.lock`, the toolchain channel, and the binary's
catalog hash. See [github.md](./github.md#caching) and [ado.md](./ado.md#caching).

### 3.4 Per-check warnings

Every check recipe depends on `_ox-ci-require <its-tool>` so even ad-hoc invocations like
`just ox-ci-miri` warn loudly if the installed tool predates the catalog minimum. The full
tier invocations additionally print a one-line tools summary at the top.

### 3.5 The Rust toolchain

`rust-toolchain.toml` is read but never written, and ox-ci never installs a Rust toolchain
itself. Per-backend rationale lives in [github.md](./github.md#rust-toolchain) and
[ado.md](./ado.md#rust-toolchain); short version: msrustup owns it on ADO/1ESPT, the runner
image owns it on GH, the user owns it locally.

`_ox-ci-require` validates the installed `rustc` against the catalog's minimum at recipe
time; missing or below-minimum `rustc` produces a clean failure message naming the version
mismatch. Per-check toolchain requirements (e.g. miri, careful, udeps need nightly) are
also enforced by `_ox-ci-require`, which suggests the user-environment-appropriate install
command in the failure message (`rustup install nightly` or "ask your team's pipeline
owner to add `nightly` to msrustup").

## 4. Impact-scoping pass-through env vars

Every check recipe whose work is per-crate accepts three optional pass-through env vars
and forwards them verbatim as `--workspace`-compatible exclude flags. Empty (the local
default) means full workspace; CI populates them from the `ox-ci-impact` building block:

```just
ox-ci-clippy: (_ox-ci-require "cargo-clippy")
    cargo clippy --workspace ${OX_CI_EXCLUDE_NOT_MODIFIED:-} --all-targets --all-features --locked -- -D warnings

ox-ci-test-only: (_ox-ci-require "cargo-llvm-cov") (_ox-ci-require "cargo-nextest")
    cargo llvm-cov nextest --workspace ${OX_CI_EXCLUDE_NOT_AFFECTED:-} --all-features --locked --lcov --output-path target/coverage/lcov.info
```

The mapping from check to env var is fixed in the catalog (see
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping)). Checks with no
per-crate scope (`pr-title`, `aprz`, `deny`, `audit`, `spellcheck`, `fmt`, `cargo-sort`,
`license-headers`, `ensure-no-cyclic-deps`, `ensure-no-default-features`) ignore the
vars. Group recipes do not interpolate the vars themselves — each underlying check
recipe reads what it needs, so a group recipe is just a dependency list and nothing
changes when scoping is disabled.

### 4.1 `OX_CI_IMPACT_SKIP` early-return hint

A fourth env var, `OX_CI_IMPACT_SKIP`, is set to `"true"` by the CI wiring when
cargo-delta reports that no workspace member is in any impact tier (typically a
docs-only PR or a PR touching only files cargo-delta's `file_exclude_patterns` ignore).
It is **advisory**, not a kill switch:

- The CI wiring **never** uses it to skip whole jobs. Every group runs on every PR.
- Recipes that scope to workspace members **may** check it and early-return — for
  example, `ox-ci-clippy` skips the cargo invocation when `OX_CI_IMPACT_SKIP=true`,
  saving the cargo-delta-computed exclude list from being parsed and the workspace from
  being touched.
- Recipes that don't scope to workspace members **ignore** it. `fmt` still runs (the
  source tree may have non-Rust files affected by the PR), `deny`/`audit`/`aprz` still
  run (their outcome doesn't depend on what was changed), `pr-title` still runs.

This separation is what makes the wiring layer durably structural. "Which checks can
no-op when nothing in the workspace is affected?" is a per-check property and lives in
the catalog/recipe, not in the wiring layer. Moving a check between groups never
requires touching the stages template / reusable workflow.

A typical skip-aware recipe looks like:

```just
ox-ci-clippy: (_ox-ci-require "cargo-clippy")
    @[ "${OX_CI_IMPACT_SKIP:-false}" = "true" ] && echo 'no affected crates; skipping clippy' && exit 0; \
        cargo clippy --workspace ${OX_CI_EXCLUDE_NOT_MODIFIED:-} --all-targets --all-features --locked -- -D warnings
```

(On Windows, with `set shell := ["pwsh", "-NoProfile", "-Command"]`, the equivalent
short-circuit uses `if ($env:OX_CI_IMPACT_SKIP -eq 'true') { exit 0 }`.)

### 4.2 Local impact-scoped runs

Not the default. To preview what CI would skip, run cargo-delta manually and export the
env vars:

```sh
git stash; git checkout origin/main
cargo delta -c .delta.toml snapshot > /tmp/base.json
git stash pop
cargo delta -c .delta.toml snapshot > /tmp/head.json
export OX_CI_EXCLUDE_NOT_AFFECTED="$(cargo delta -c .delta.toml impact \
    --baseline /tmp/base.json --current /tmp/head.json -f cargo-excludes --affected)"
just ox-ci-pr-test
```

A wrapper recipe (`ox-ci-impact-set base=origin/main`) is left to v2: it has subtle
git-state interactions and the manual flow is good enough for the rare case a developer
actually wants to reproduce CI scoping locally.

## 5. Daily driver

```text
$ just ox-ci
[just] running ox-ci-tools-check
[just] running ox-ci-pr-fast
[just] running ox-ci-pr-test
[just] running ox-ci-pr-mutants
ox-ci OK
```

`ox-ci` is an alias for `ox-ci-pr` (set in the managed `Justfile` region). All three tiers
(`ox-ci-pr`, `ox-ci-nightly`, `ox-ci-full`) are first-class — locally reproducible with
exactly the same arguments CI uses, because CI invokes the same `just` recipes.

## 6. No-tooling fallback

A user with only `cargo` (no `just`, no `cargo-ox-ci`) can still run the basics:

```sh
cargo test   --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The same commands appear as the body of the corresponding `just` recipes in
`justfiles/ox-ci/checks.just`, so they are discoverable by reading that file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective
tools.

## 7. Customization at the recipe level

Per the four customization tiers in [design.md §7](./design.md#7-customization):

- **Add your own recipes** to the top-level `Justfile` outside the managed region. The
  Justfile's managed region only contains `import` lines and an alias — your recipes never
  collide with it.
- **Add your own `.just` files** and `import` them after the managed region's closing
  sentinel.
- **Override a single ox-ci recipe**: the `just` import-and-override rules make this awkward
  (just doesn't have a "the most specific definition wins" rule). The recommended way is to
  copy the recipe you want to change into your top-level Justfile with a different name
  (e.g. `my-clippy`) and reference *that* from your own group/tier recipes. Don't fight the
  ox-ci-* names; just compose around them.
- **Disable a recipe wholesale**: opt out of the managed `Justfile` region per
  [updates.md §opt-out](./updates.md#opting-out-in-file-stubs). This stops the imports from
  happening at all, so all `ox-ci-*` recipes vanish. Use this only when ox-ci is no longer
  the right tool for your repo.

Customizing the *contents* of `justfiles/ox-ci/*.just` is supported — they're owned files,
so editing them flips them to "dirty" and the next `update` writes a `.ox-ci-proposed`
sibling instead of overwriting. See [updates.md](./updates.md) for the lifecycle.
