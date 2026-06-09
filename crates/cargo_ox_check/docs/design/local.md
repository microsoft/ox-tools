# Local Recipe Surface

This document describes the `justfiles/ox-check/` tree that ox-check writes into a repo, how the
recipes are organized, and how local invocations differ from CI invocations (spoiler: they
don't — that's the design).

See also:

- [design.md](./design.md) for the overall principles.
- [checks.md](./checks.md) for the catalog the recipes implement.
- [updates.md](./updates.md) for how these files are tracked / regenerated.

## 1. File layout

```text
repo/
├── Justfile                                       managed-region: ox-check-imports
│   # >>> ox-check-managed: ox-check-imports
│   import 'justfiles/ox-check/mod.just'
│   # <<< ox-check-managed: ox-check-imports
│   …user content…
│
└── justfiles/ox-check/                               owned (one checksum per file)
    ├── mod.just            entry point: imports the sibling files and defines
    │                       `alias ox-check := ox-check-pr`. The user's Justfile
    │                       region pulls in this single file; everything else is
    │                       reached transitively.
    ├── checks.just         per-check recipes (ox-check-fmt, ox-check-clippy, ox-check-llvm-cov, …).
    │                       Starts with `set unstable` (needed for the `[script("pwsh")]`
    │                       attribute on `ox-check-pr-title`).
    ├── groups.just         group recipes (ox-check-pr-fast, ox-check-pr-slow1,
    │                       ox-check-pr-slow2, ox-check-pr-slow3, ox-check-scheduled-test, …)
    │                       plus a convenience `ox-check-pr-slow` umbrella that
    │                       invokes the three pr-slow* sub-recipes sequentially.
    ├── tiers.just          tier aggregators (ox-check-pr, ox-check-scheduled, ox-check-full).
    ├── tools.just          ox-check-tools-check + ox-check-setup + helpers.
    ├── tool-minimums.txt   data file: <cargo-subcommand>=<pinned-version> per line.
    ├── rustup-components.txt data file: <toolchain-key>:<component> per line.
    └── versions.just       pinned nightly toolchains (rust_nightly, rust_nightly_external_types).
                            Read by recipes via `{{ var }}` interpolation and by the
                            setup composites via `just --evaluate`. See §3.6.
```

The Justfile region is the only file ox-check adds to that the user co-owns, and it's
a single `import` line — everything ox-check-specific lives inside `justfiles/ox-check/`.
All five files under that directory are tool-owned (tracked by full-file checksum in
the sidecar manifest). If the user wants to add project-specific recipes, they add them
to the top-level `Justfile` outside the managed region, or to their own additional
imported `.just` files. The alias `ox-check := ox-check-pr` lives in `mod.just`, not in
the user's `Justfile`, so renaming or retargeting the alias is a template update with
no managed-region churn.

Every recipe in `groups.just`, `tiers.just`, and `checks.just` is annotated with
`[group("ox-check")]` so `just --groups` and `just --list --unsorted` cluster them
cleanly in tooling output.

## 2. Recipe layers

`justfiles/ox-check/` is structured to make all three levels (check, group, tier) addressable
from the command line.

### checks.just

One recipe per individual check, each named `ox-check-<check>`. Recipes are usually a single
`cargo …` line; a handful (license-headers, ensure-no-cyclic-deps,
ensure-no-default-features, pr-title, the bench smoke loop) are short `[script]` blocks.
Every check recipe is prefixed with a quick version-gate dependency:

```just
ox-check-clippy: (_ox-check-require "cargo-clippy")
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

`_ox-check-require` is a private recipe in `tools.just` that calls `cargo install --list` to
verify the tool meets the catalog's declared minimum version; missing or below-minimum
tools fail with a one-line `cargo install` hint. The cost is one cheap `cargo install
--list` invocation per check, well under a second on a warm cache.

### groups.just

One recipe per CI-visible group, named `ox-check-<tier>-<group>`. The check-recipe and group-recipe
namespaces are kept disjoint by naming choice: no check is named `<tier>-<group>` for
any tier × group combination (e.g. the coverage-instrumented test check is named
`llvm-cov`, not `test`, so that group names like `ox-check-pr-slow1` unambiguously refer to a group recipe).

The `pr-slow` work is split into three independent CI-visible sub-groups
(`pr-slow1`, `pr-slow2`, `pr-slow3`) so they run as parallel CI jobs/stages.
A convenience umbrella `ox-check-pr-slow` recipe is also provided for local
use; it invokes the three sub-recipes sequentially. `pr-slow3` (mutants) is
diff-scoped against the PR base; `scheduled-exhaustive` runs the
full-workspace mutants recipe:

```just
ox-check-pr-fast: ox-check-fmt ox-check-clippy ox-check-cargo-sort ox-check-license-headers \
               ox-check-ensure-no-cyclic-deps ox-check-ensure-no-default-features \
               ox-check-doc-build ox-check-readme-check ox-check-spellcheck ox-check-pr-title \
               ox-check-deny ox-check-audit ox-check-udeps ox-check-semver-check \
               ox-check-external-types ox-check-aprz

ox-check-pr-slow: ox-check-pr-slow1 ox-check-pr-slow2 ox-check-pr-slow3
ox-check-pr-slow1: ox-check-llvm-cov ox-check-doc-test ox-check-examples
ox-check-pr-slow2: ox-check-miri ox-check-careful
ox-check-pr-slow3: ox-check-mutants

ox-check-scheduled-test: ox-check-llvm-cov ox-check-doc-test ox-check-examples
ox-check-scheduled-advisories: ox-check-deny ox-check-audit ox-check-aprz ox-check-clippy
ox-check-scheduled-exhaustive: ox-check-mutants-full ox-check-cargo-hack ox-check-bench
```

### tiers.just

Three tier aggregators. Each tier is a recipe that depends on the appropriate set of groups
in a deterministic order:

```just
ox-check-pr: ox-check-tools-check ox-check-pr-fast ox-check-pr-slow
ox-check-scheduled: ox-check-tools-check ox-check-scheduled-test ox-check-scheduled-advisories \
               ox-check-scheduled-exhaustive
ox-check-full: ox-check-pr ox-check-scheduled
```

### tools.just

- `ox-check-system-deps-check` — probe for system-level libs that catalog tools need to
  build from source (currently: `libclang` for `cargo-spellcheck`). Best-effort presence
  check; on missing deps emits per-OS install hints and exits non-zero. No auto-install.
  See §3.3.1 for the scope policy.
- `ox-check-tools-check` -- print a status table of every tool's installed version vs. the pin.
- `ox-check-setup` -- install everything ox-check needs in one shot: system library
  dependencies, rustup toolchains and components, cargo-binstall (if requested),
  and every cargo subcommand pinned in `tool-minimums.txt`. Idempotent: each
  sub-step is a no-op when the artifact is already present at or above the pin,
  so calling this recipe on every CI run costs nothing on a cache hit. There is
  intentionally no separate `tools-install-missing` variant -- this recipe IS
  the install-missing recipe. Runs `ox-check-system-deps-check` first when the
  source-build (`install`) backend is selected, so missing system libs surface
  as a clear "install libclang" hint instead of a cryptic build error 10 minutes
  in. Version policy: installs `=<pin>` exact, but accepts already-installed
  versions `>= <pin>` (see §3 for the rationale).
- `_ox-check-require <tool>` — internal helper called by each check.

The full tool-version policy these recipes implement is detailed in §3 below.

## 3. Tool versions, toolchains, and installation

### 3.1 Policy

The catalog records, for each cargo subcommand, a **pinned version** (e.g.
`cargo-nextest=0.9.122`). The pin is used two different ways:

- **On install** (`ox-check-setup` writing into `~/.cargo/bin`): the recipe installs
  *exactly* that version (`=<pin>`), never `>=`. Pulling latest-matching at install
  time is a CI reproducibility risk -- an upstream release between yesterday's green
  build and today's PR can break things, even though the catalog hasn't moved.
  `cargo-spellcheck 0.15.7`'s em-dash word-boundary regression is the canonical
  example: with `>=0.15.1` the catalog would have silently picked it up, breaking
  every PR until the catalog was edited. With `=0.15.1` the catalog locks in the
  version it was validated against.
- **On runtime check** (`_ox-check-require <tool>`): the recipe enforces
  `installed >= pin`. A local developer who has manually upgraded a tool for their
  own reasons (e.g. needing a bugfix the catalog hasn't pinned yet) is not
  downgraded by setup. Their newer version still satisfies the gate; recipes run
  against it.

This asymmetry -- "install exact, accept newer if already present" -- gives CI
reproducibility *and* leaves the user in control. Bumping a pin is a deliberate
catalog edit (`tool-minimums.txt`), not an upstream-release-triggered surprise.

The catalog file is still named `tool-minimums.txt` (historical name; no churn). The
semantics described above are documented at the top of the file.

### 3.2 Detecting installed versions

`_ox-check-require <tool>` (a private `just` recipe in `tools.just`) uses
`cargo install --list` to enumerate currently-installed cargo subcommands and their
versions, then checks `installed >= pin`. This avoids the problem of tools without a
stable `--version` flag, is fast, and works uniformly for everything the catalog
cares about. For non-cargo dependencies (`just` itself and `pwsh`), the recipe falls
back to `tool --version` and a known parser.

### 3.3 Installing tools (and toolchains, and components)

`ox-check-setup [installer]` is a single `just` recipe that brings an empty
environment up to where every ox-check recipe runs. It is fully idempotent; safe to
re-run. The four steps it performs in order:

1. **System library dependencies** (libclang for cargo-spellcheck's build script).
   Source-install path only -- the binstall path downloads prebuilt binaries that
   don't need libclang. See §3.3.1.
2. **Rustup toolchains and components**. Pinned nightlies from `versions.just` plus
   the components listed in `rustup-components.txt` -- `clippy` and `rustfmt` on the
   default toolchain; `rustfmt`, `miri`, and `rust-src` on the pinned nightly; etc.
   Without this step, `ox-check-miri` and `ox-check-careful` fail locally with
   "`cargo-miri` is not installed for the toolchain ...".
3. **`cargo-binstall` bootstrap** (binstall path only). Compiles binstall once via
   `cargo install --locked`; subsequent runs are no-ops.
4. **Cargo subcommands** from `tool-minimums.txt`. Per the policy above, each
   missing tool is installed at `=<pin>`; tools already at or above the pin are
   skipped (no downgrade).

The `installer` argument selects the backend for step 4 (and dictates whether
steps 1 and 3 run):

- `install` (default) -- `cargo install --locked <tool> --version '=<pin>'`. Pure
  source builds; works in any cargo environment with no extra runtime dependency.
  Slow on a cold runner (~30 min for the full catalog) because every tool
  re-compiles common deps (`clap`, `syn`, `quote`, ...) from scratch independently.
- `binstall` -- `cargo binstall --no-confirm --locked <tool> --version '=<pin>'`.
  Downloads a prebuilt binary from each tool's GitHub Releases when available, falls
  back to `cargo install` per-tool otherwise. Cuts the cold-runner install phase
  from ~30 min to ~1 min. Bootstraps `cargo-binstall` itself in step 3.

The GitHub composite setup action calls `just ox-check-setup binstall`. The ADO
setup template calls `just ox-check-setup` (default `install`): cargo-binstall has
unresolved compliance issues for internal ADO pipelines (the binary registry it
pulls from isn't on the standard allow-list), so the slower pure-cargo path is the
conservative choice there. Locally, users pick whichever matches their environment.

#### Two data files

- `tool-minimums.txt` -- cargo subcommand pins (step 4).
- `rustup-components.txt` -- rustup components per toolchain key (step 2). Format
  `<toolchain-key>:<component>` per line; keys are `default` / `nightly` /
  `nightly_external_types`. `nightly` and `nightly_external_types` resolve to the
  pins in `versions.just`.

Both are owned files; edits survive `cargo ox-check update`, and the dirty-file
flow re-asks before overwriting if the catalog ships a different default.

Two prerequisites are not cargo-installable and must be present before
`ox-check-setup` can run:

- **`just`** itself -- bootstrap with `cargo install just --locked` once, or use a
  system package. Every backend's setup composite/template installs it via cargo as
  a one-shot before calling `ox-check-setup`.
- **`pwsh`** (PowerShell Core) -- used by every `[script("pwsh")]` recipe in
  `checks.just`. Preinstalled on every relevant CI runner (GH-hosted
  Linux/Windows/macOS, Microsoft-hosted ADO agents). On a developer machine
  without pwsh, `_ox-check-require pwsh` fails with a per-OS install hint pointing
  at <https://github.com/PowerShell/PowerShell>.

Trade-off acknowledged: `cargo install --locked` is slow on a cold cache (several
minutes for the full catalog). It is also the most reliable mechanism in restricted
networks. Caching (via the GH cache action and the ADO pipeline workspace cache) is
configured by the setup action/template to key on `Cargo.lock`, the toolchain
channel, `tool-minimums.txt`, and `rustup-components.txt`. See
[github.md](./github.md#caching) and [ado.md](./ado.md#caching).

#### 3.3.1 System-level prerequisites

A small set of catalog tools have non-Rust build dependencies that `cargo install`
can't satisfy on its own. Today the only entry is `libclang`, needed by
`cargo-spellcheck` (via `clang-sys` / `hunspell-rs`) at build time. The `binstall`
install path sidesteps these entirely by downloading prebuilt binaries.

Scope policy: only check for system libs that an ox-check catalog tool **directly**
requires. ox-check is not a general-purpose dev-env doctor. Repository-specific
system deps (e.g. `openssl-devel`, `symcrypt` for the adopter's own crates) belong
in the adopter's `setup.yml` customization, not in the ox-check catalog.

Detection (`ox-check-system-deps-check`) uses presence-only probes -- file existence
in standard install dirs plus the `LIBCLANG_PATH` env var override. No version
checks: system libs upgrade independently of the catalog and any reasonably modern
libclang satisfies clang-sys.

On a missing dep the recipe prints per-OS install hints (apt-get / tdnf / brew /
scoop / winget) and exits non-zero. **No auto-install** -- admin/sudo decisions and
package-manager choice stay with the user. `ox-check-setup` runs the recipe first
(only on the source-build `install` backend), so missing system libs surface as a
clear hint instead of a cryptic clang-sys build error 10 minutes into the install.

Adding a new system dep is a one-block catalog change in `tools.just`; it
propagates to adopters via `cargo ox-check update` like any other catalog edit.

### 3.4 Per-check warnings

Every check recipe depends on `_ox-check-require <its-tool>` so even ad-hoc invocations like
`just ox-check-miri` warn loudly if the installed tool predates the catalog minimum. The full
tier invocations additionally print a one-line tools summary at the top.

### 3.5 The Rust toolchain

`rust-toolchain.toml` is read but never written, and ox-check never installs the *project's*
Rust toolchain itself. Per-backend rationale lives in [github.md](./github.md#rust-toolchain)
and [ado.md](./ado.md#rust-toolchain); short version: msrustup owns it on ADO/1ESPT, the
runner image owns it on GH, the user owns it locally.

`_ox-check-require` validates the installed `rustc` against the catalog's minimum at recipe
time; missing or below-minimum `rustc` produces a clean failure message naming the version
mismatch. Per-check toolchain requirements (e.g. miri, careful, udeps need nightly) are
also enforced by `_ox-check-require`, which suggests the user-environment-appropriate install
command in the failure message (`rustup install nightly` or "ask your team's pipeline
owner to add `nightly` to msrustup").

### 3.6 Nightly pinning

A handful of catalog checks need nightly Rust: `fmt`, `udeps`, `miri`, `careful`, and
`check-external-types`. We **pin** the nightly snapshots used by these checks rather than
floating bare `+nightly`. Pinning eliminates "rustup update on Tuesday broke main on
Wednesday" — every CI run uses the same nightly until we deliberately bump the pin.

`fmt` is on nightly because the catalog's `rustfmt.toml` opts into `unstable_features`
to get import grouping (`imports_granularity = "Module"`, `group_imports =
"StdExternalCrate"`) and `format_code_in_doc_comments`. Those are the high-value
opinions every surveyed Microsoft Rust repo reaches for; the stable rustfmt option set
doesn't include them. Pinning is what makes nightly fmt sustainable — formatting
churn happens on a pin bump, not on every `rustup update`.

The pins live in `justfiles/ox-check/versions.just` as plain just variables:

```just
rust_nightly := "nightly-YYYY-MM-DD"
rust_nightly_external_types := "nightly-YYYY-MM-DD"
```

**One source of truth, two consumers.** Recipes read the pins by `{{ }}` interpolation
(`cargo +{{ rust_nightly }} udeps ...`). The setup composites (`setup-action.yml`,
`steps/setup.yml`) read the pins via `just --evaluate <var>` and pre-install both
toolchains with `rustup toolchain install`. There is no env-file duplicate.

**Two pins, not one.** `rust_nightly` is the general-purpose nightly used by udeps, miri,
careful. `rust_nightly_external_types` is intentionally narrower: it's tied to the rustdoc
JSON schema version that the currently-selected `cargo-check-external-types` release
accepts. Bump it alongside `cargo-check-external-types` upgrades, not on the general
cadence. When the two pins resolve to the same date the setup composite installs only one
toolchain.

**Bump policy.** The general `rust_nightly` is intended to move on a regular cadence
(monthly is a reasonable default) so adopters absorb nightly drift in predictable chunks.
`rust_nightly_external_types` moves only when `cargo-check-external-types` releases a new
version that targets a newer rustdoc JSON schema. Both bumps are normal `cargo ox-check
update` operations: edit `versions.just`, regenerate, validate, commit. Adopters are free
to override either pin in their `versions.just` (it's an owned file) — the next run sees
the dirt and emits a `.ox-check-proposed` sibling instead of overwriting.

**Why pin, not float?** We tried floating nightly once and immediately needed
regex-based tolerance code in the `check-external-types` recipe to absorb rustdoc JSON
schema bumps. That was a tell: any tool that depends on nightly internals will routinely
break on schema/lint/intrinsic drift, and the alternative to pinning is per-tool
tolerance shims accumulating in the recipes. Pinning is one mechanism that handles all
present and future cases; tolerance shims are bespoke and silently degrade what the
check actually validates.

## 4. Impact-scoping pass-through env vars

Every check recipe whose work is per-crate accepts an optional pass-through env var
that the CI wiring populates from the `ox-check-impact` building block. There are three
such env vars, one per cargo-delta tier:

| Env var                      | Bucket    | What recipes do with it                                                                       |
|------------------------------|-----------|------------------------------------------------------------------------------------------------|
| `OX_CHECK_INCLUDE_MODIFIED`  | modified  | `--skip` → recipe exits 0. Otherwise: run unconditionally (modified-tier tools are workspace-wide). |
| `OX_CHECK_INCLUDE_AFFECTED`  | affected  | `--skip` → recipe exits 0. Otherwise: splice the value into the cargo invocation, defaulting to `--workspace` when unset. |
| `OX_CHECK_INCLUDE_REQUIRED`  | required  | Same semantics as `OX_CHECK_INCLUDE_AFFECTED`, but consumed by recipes that need transitive dep graph in scope (doc-build, cargo-hack, udeps). |

Each var holds either the literal sentinel `--skip` (the tier is empty for this PR), or
a pre-built argument string like `--package alpha --package beta`. The CI wiring sets
exactly one form; local invocations leave the vars unset, and recipes fall back to
`--workspace`.

A typical affected-tier recipe:

```just
ox-check-clippy:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-clippy: no affected packages; skipping"; exit 0; \
    fi; \
    cargo clippy ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-targets --all-features --locked -- -D warnings
```

A typical modified-tier recipe (the tool is workspace-wide, so there's nothing to
splice — only the skip guard matters):

```just
ox-check-fmt:
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-fmt: no modified packages; skipping"; exit 0; \
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
and exit 0 cleanly, keeping the CI job green while signalling that nothing in that tier
needed to run.

This separation is what makes the wiring layer durably structural: "which checks can
no-op when nothing in the relevant tier is affected" is a per-check property living in
the catalog/recipe, not in the wiring layer. Moving a check between buckets is a pure
catalog change; the CI templates always thread all three vars and never gate jobs on
their values.

### 4.2 Local impact-scoped runs

Not the default. To preview what CI would skip, run cargo-delta manually and export the
env vars:

```sh
# Compute the affected-tier include list (--package … form) against origin/main.
export OX_CHECK_INCLUDE_AFFECTED="$(cargo delta impact --base origin/main --format cargo-args --affected)"
just ox-check-pr-slow1
```

A wrapper recipe to compute and export all three vars in one shot is left to v2: it has
subtle git-state interactions and the manual flow is good enough for the rare case a
developer actually wants to reproduce CI scoping locally.

## 5. Daily driver

```text
$ just ox-check
[just] running ox-check-tools-check
[just] running ox-check-pr-fast
[just] running ox-check-pr-slow
ox-check OK
```

`ox-check` is an alias for `ox-check-pr` (set in the managed `Justfile` region). All three tiers
(`ox-check-pr`, `ox-check-scheduled`, `ox-check-full`) are first-class -- locally reproducible with
exactly the same arguments CI uses, because CI invokes the same `just` recipes.

## 6. No-tooling fallback

A user with only `cargo` (no `just`, no `cargo-ox-check`) can still run the basics:

```sh
cargo test   --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The same commands appear as the body of the corresponding `just` recipes in
`justfiles/ox-check/checks.just`, so they are discoverable by reading that file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective
tools.

## 7. Customization at the recipe level

Per the four customization tiers in [design.md §7](./design.md#7-customization):

- **Add your own recipes** to the top-level `Justfile` outside the managed region. The
  Justfile's managed region only contains `import` lines and an alias — your recipes never
  collide with it.
- **Add your own `.just` files** and `import` them after the managed region's closing
  sentinel.
- **Override a single ox-check recipe**: the `just` import-and-override rules make this awkward
  (just doesn't have a "the most specific definition wins" rule). The recommended way is to
  copy the recipe you want to change into your top-level Justfile with a different name
  (e.g. `my-clippy`) and reference *that* from your own group/tier recipes. Don't fight the
  ox-check-* names; just compose around them.
- **Disable a recipe wholesale**: opt out of the managed `Justfile` region per
  [updates.md §opt-out](./updates.md#6-opting-out-in-file-stubs). This stops the imports from
  happening at all, so all `ox-check-*` recipes vanish. Use this only when ox-check is no longer
  the right tool for your repo.

Customizing the *contents* of `justfiles/ox-check/*.just` is supported — they're owned files,
so editing them flips them to "dirty" and the next `update` writes a `.ox-check-proposed`
sibling instead of overwriting. See [updates.md](./updates.md) for the lifecycle.
