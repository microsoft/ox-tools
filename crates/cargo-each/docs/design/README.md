# cargo-each — Design

> Status: **Draft**.
> Crate name: `cargo-each`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

Repo tooling frequently needs to run a command **once per selected workspace
member** — or once for a *set* of members — with the selection expressed the way
`cargo build` expresses it (`-p`, `--workspace`, `--exclude`). Today that logic
is hand-rolled in shell, and in `ox-tools` specifically it is duplicated across
around 26 `cargo-anvil` check recipes plus two CI impact steps. Every scoped recipe
re-implements the same chores in PowerShell:

1. **Skip / default preamble.** The `anvil-impact` recipe (cargo-delta) writes a
   per-tier selection to `target/anvil/impact/include_<tier>.txt`. Each check
   reads its tier's file and must special-case an empty tier (`--skip` sentinel
   → exit 0) and a missing/blank file (local run → fall back to `--workspace`),
   then splat the value into the cargo call:
   `@(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' })`.
2. **`@version` stripping.** The impact list carries version-qualified specs
   (`name@version`) to disambiguate like-named transitive deps. Tools that key on
   the bare package name (`cargo semver-checks --package name`,
   `cargo coverage-gate --package name`) need the `@version` stripped back off.
3. **`cargo metadata` filtering.** Several checks must restrict the set by a
   metadata property the impact list does not carry: library-bearing crates
   (external-types, semver-check), coverage opt-outs (llvm-cov), crates that
   depend on `loom` (loom).
4. **Per-package iteration.** Per-manifest tools (external-types, semver-check,
   readme-check) can't take `--package`; the recipe walks a
   name-to-manifest-path map built from `cargo metadata` and invokes the tool once
   per crate.

The same four chores, re-spelled per recipe, are the bulk of the PowerShell in
`checks/`. Two recipes already carry a `TODO(anvil-runner)` noting that a helper
"absorbs the skip/splat preamble" is wanted.

`cargo-each` is that helper: one cargo-native tool that resolves a
cargo-style package selection, optionally filters it by a metadata predicate,
and runs a command over the result — either once per package (with placeholder
substitution) or exactly once for the whole set.

## 2. Goals

1. **Cargo-native selection.** Accept the same selectors as `cargo build`
   (`-p/--package` with glob support, `--workspace`/`--all`, `--exclude`) so the
   flag surface is already familiar and the impact step's `--package name@version`
   output can be consumed verbatim.
2. **Absorb the CI skip/default dance.** A resolved-empty selection is a no-op
   that exits 0 — no `--skip` sentinel in callers. cargo-each is entirely
   flag-driven: a computed selection (an impact tier) is fed in as ordinary
   `-p` / `--workspace` / `--none` flags via shell expansion, so cargo-each
   stays agnostic about where the selectors came from and callers never write
   a skip/default conditional.
3. **Two execution modes.** *per-package* (run the command once per member,
   substituting `{name}`/`{spec}`/`{version}`/`{manifest}`) covers per-manifest
   tools; *once* (run the command a single time when the set is non-empty)
   covers workspace-wide tools and single-invocation cargo commands, with a
   `{packages}` placeholder that expands to the cargo selection flags.
4. **A small, general filter language** (`--filter` / `--exclude-filter`) over
   cargo metadata — `lib`, `bin`, `dep:<name>`, `metadata:<dotted.key>[=<value>]`
   — so the bespoke `cargo metadata` filtering in recipes collapses to one flag.
5. **Bare names for free.** `{name}` yields the un-qualified package name, so
   `@version` stripping disappears from callers even though the input carries it.
6. **Works identically locally and in CI**, on any platform, with no shell
   dialect assumptions. **Open source**: ships from `ox-tools` to crates.io.

## 3. Non-Goals

- **Computing an impact/affected set.** That is cargo-delta's job. `cargo-each`
  consumes a selection; it does not diff git or walk the reverse-dep graph.
- **Replacing domain glue.** semver-check's error-tolerance + advisory-comment
  aggregation, llvm-cov's dual-config instrumentation, and the per-crate readme
  `doc2readme` reconciliation stay in their recipes. `cargo-each` owns only the
  selection → filter → iterate spine those recipes wrap.
- **Parallel scheduling / job pools.** Commands run sequentially. Parallelism, if
  ever wanted, is a later, additive concern.
- **A general templating engine.** Placeholder substitution is a fixed, small set
  of `{token}` replacements, not an expression language.

## 4. CLI surface

```
cargo each [SELECTION] [FILTERS] [EXECUTION] -- <COMMAND> [ARG...]
```

Everything after `--` is the command template. `cargo-each` never interprets it
beyond placeholder substitution.

### 4.1 Selection (mirrors `cargo build`)

| Flag | Meaning |
|------|---------|
| `-p`, `--package <SPEC>` | Select a member. Repeatable. `SPEC` is a package name, a `name@version` spec, or a Unix glob (`tokio-*`), matching `cargo-coverage-gate`'s existing `-p` idiom. |
| `--workspace`, `--all` | Select every workspace member. |
| `--exclude <SPEC>` | Remove a member from the selection (requires `--workspace`). Repeatable. |
| `--none` | Explicitly select zero members. Resolves to an empty set (a no-op, exit 0). Emitted by the impact hand-off when a tier is empty; replaces the `--skip` sentinel. |

A computed selection (e.g. an impact tier) is fed in as ordinary flags via
shell expansion — cargo-each has no `--from-file` / `--from-env` source, so it
stays agnostic about origin. See section 6 for the anvil hand-off.

**Resolution order.** The literal flags resolve to:

1. If `--none` appears anywhere → empty set.
2. Else if `--workspace`/`--all` appears → all members, minus `--exclude`.
3. Else if any `-p` matched → the matched members.
4. Else → `default-members` (exactly like `cargo build`; pass `--workspace`
   for the whole workspace).

A `-p` selector that matches no member is an error (same policy as
`cargo-coverage-gate`), so typos fail loudly rather than silently skipping.

### 4.2 Filters

`--filter <PRED>` keeps only members matching `PRED`; `--exclude-filter <PRED>`
drops members matching `PRED`. Both are repeatable and AND-combined
(`--exclude-filter` wins over `--filter` on conflict). Predicates:

| Predicate | True when the member… |
|-----------|-----------------------|
| `lib` | has a `lib` target. |
| `bin` | has a `bin` target. |
| `dep:<name>` | lists `<name>` among its dependencies (any kind). |
| `metadata:<dotted.key>` | has `package.metadata.<dotted.key>` present. |
| `metadata:<dotted.key>=<value>` | has `package.metadata.<dotted.key>` equal to `<value>` (numeric compare when both parse as a number, else string compare). |

Filtering runs after selection. If the filtered set is empty, `cargo-each`
exits 0 (nothing to do), exactly like an empty selection.

### 4.3 Execution

| Flag | Meaning |
|------|---------|
| *(default)* | **per-package**: run `<COMMAND>` once per selected member, in name order, with placeholders substituted. |
| `--once` | **once**: run `<COMMAND>` exactly once when the set is non-empty (skip when empty). Use `{packages}` to inject the selection. |
| `--keep-going` | Don't stop at the first failing command; run them all and exit non-zero if any failed. Default is fail-fast (exit with the first failure's code). |
| `--chdir` | Run each per-package command from that member's crate root (its `{manifest_dir}`) instead of the caller's CWD. Per-package mode only — combined with `--once` it is a usage error (exit 2). Placeholders stay absolute, so only *relative* args in the command shift to the member dir. |
| `--manifest-path <PATH>` | Workspace root `Cargo.toml`. Defaults to auto-detection from CWD. |
| `--dry-run` | Print the fully-substituted commands that *would* run, one per line, without executing. |

### 4.4 Placeholders

Substituted inside each `ARG` of the command template:

| Token | Expands to | Mode |
|-------|-----------|------|
| `{name}` | bare package name (`cargo-anvil`) | per-package |
| `{spec}` | `name@version` | per-package |
| `{version}` | package version | per-package |
| `{manifest}` | absolute path to the member's `Cargo.toml` | per-package |
| `{packages}` | the cargo selection flags for the resolved set: `--workspace` when the whole workspace was selected via `--workspace`/`--all` with no excludes, else `--package name@version …` (one pair per member). Only valid as a standalone `ARG`; it expands to multiple tokens. | once |

Using a per-package token in `--once` mode, or `{packages}` outside `--once`, is
a usage error.

## 5. Semantics

- **Exit codes.** `0` when every executed command succeeded *or* the set was
  empty; the failing command's code (fail-fast) or `1` (`--keep-going` with any
  failure) otherwise; `2` for a `cargo-each` usage/configuration error
  (unknown selector, bad predicate, misused placeholder, or `--chdir` with
  `--once`).
- **Empty set is success.** Both an empty selection (`--none`, or an impact
  variable that resolved to nothing) and an empty *filtered* set exit 0 after a
  one-line note to stderr. This is what lets callers drop their `--skip` guards.
- **No shell.** The command is spawned directly (argv, not a shell string), so
  there is no quoting/dialect surface. Placeholder expansion is textual and
  happens before spawn.

## 6. How it simplifies cargo-anvil

The recipes stop parsing the impact selection and metadata by hand. anvil's
`_anvil-impact-include <tier>` helper reads
`target/anvil/impact/include_<tier>.txt`, applies the `ANVIL_IMPACT=off`
override, and emits a **concrete selector for every tier** — `--workspace`
(unscoped / local / off), `--package name@version …` (scoped), or `--none`
(empty tier). A `cargo-each` check just splats that output straight in as
flags. Because the helper always emits a concrete selector, cargo-each never
falls back to `default-members`, so no per-call default flag is needed.
Illustrative before/after (the recipe keeps its own comments, setup deps,
`: anvil-impact` dependency, and any domain glue; only the selection spine
changes):

**clippy** (affected tier, single invocation):

```powershell
# before
if (-not $env:ANVIL_INCLUDE_AFFECTED) { $env:ANVIL_INCLUDE_AFFECTED = (& just _anvil-impact-include affected) }
if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
& cargo clippy @(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' }) --all-targets --all-features --locked -- -D warnings
```
```powershell
# after
cargo each @(& {{ just_executable() }} _anvil-impact-include affected) --once -- \
    cargo clippy {packages} --all-targets --all-features --locked -- -D warnings
```

**external-types** (affected tier, per-manifest, lib-only) — the whole
name-to-manifest map, `--workspace` branch, `@version` strip, and iteration loop
collapse to:

```powershell
cargo each @(& {{ just_executable() }} _anvil-impact-include affected) --filter lib -- \
    cargo +{{ rust_nightly_external_types }} check-external-types --manifest-path {manifest}
```

**loom** (affected packages that depend on loom):

```powershell
cargo each @(& {{ just_executable() }} _anvil-impact-include affected) --filter dep:loom -- \
    cargo +{{ rust_nightly }} test --package {name} ...
```

**llvm-cov opt-out drop** (exclude coverage-opted-out members):

```powershell
cargo each @(& {{ just_executable() }} _anvil-impact-include affected) \
    --exclude-filter metadata:coverage-gate.min-lines-percent=0 --once -- <measure...>
```

Recipes whose only per-tier logic is the skip/splat preamble (bench, clippy,
doc-build, examples, miri*, doc-test, cargo-hack, udeps, careful) become a
single `cargo each … --once` line. Modified-tier workspace-wide tools (fmt,
cargo-sort, license-headers, spellcheck, ensure-no-*) become
`cargo each @(& just _anvil-impact-include modified) --once -- <tool>` — the
`--once` skip-when-empty behavior replaces the `--skip` guard while the tool
still runs workspace-wide.

Three small `anvil-impact` adjustments complete the picture (all part of the
adoption change, not this crate):

- **Emit `--none`, not `--skip`, for an empty tier**, and drop the modified
  tier's empty default: `_anvil-impact-include` emits `--workspace` /
  `--package …` / `--none` **uniformly across all three tiers**. cargo-delta
  makes no fundamental distinction between the tiers — they are just three
  package sets — so neither should the helper. `--none` is `cargo-each`'s
  native "select zero members" token, so the include file needs no
  anvil-specific sentinel and `cargo each` skips the tier with no caller guard.
- **Print one token per line** from `_anvil-impact-include`, so the recipe's
  `@(& …)` capture is a ready-to-splat array — no `-split`, no `if/else`.
- **Stop version-qualifying.** `_anvil-impact-format` can emit bare package
  names; `cargo-each` derives `{spec}`/`{packages}` (the `name@version` form a
  child cargo command needs) from live metadata itself.

With the helper's output splatted straight into `cargo each`, the per-check
`_anvil-impact-include` *self-populate* line and the `ANVIL_INCLUDE_<TIER>`
environment variable are no longer needed by scoped checks.

## 7. Rejected alternatives

- **Extend cargo-delta to emit ready-to-run commands.** Couples impact analysis
  to command execution and to anvil's recipe shapes; `cargo-each` stays a
  general, reusable tool with no knowledge of diffs or tiers.
- **A pure `--print` resolver (emit the `--package` list, let the recipe run
  cargo).** Keeps the per-recipe splat/skip shell that is the thing we set out
  to delete. Owning execution (per-package and once) is what removes it.
- **A generic expression language for filters.** Over-built for the handful of
  predicates the recipes actually need; the fixed predicate set covers every
  current `cargo metadata` filter and stays trivially auditable.
- **A `--from-file` / `--from-env` selection source.** Rejected: it would pull
  the impact artifact layout (and the `ANVIL_IMPACT=off` widening + tier-default
  policy) into cargo-each, duplicating logic that already lives in anvil's
  `_anvil-impact-include` helper. Keeping cargo-each flag-only and letting the
  caller splat that helper's output in is smaller and keeps the impact policy in
  one place.
- **Reuse `cargo xtask`/a justfile function.** Neither is cargo-native selection;
  both re-introduce a shell dialect. A small binary is portable and testable.
