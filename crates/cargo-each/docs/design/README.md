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

`cargo-each` is that helper: one portable tool that either resolves a
cargo-style package selection (optionally filtered by metadata) or reads JSON
Lines records, then runs a command over the result with placeholder
substitution. Cargo-backed execution runs once per package, once per matching
target, or exactly once for the whole set; record-backed execution runs once
per object.

## 2. Goals

1. **Cargo-native selection.** Accept the same selectors as `cargo build`
   (`-p/--package` with glob support, `--workspace`/`--all`, `--exclude`) so the
   flag surface is already familiar and the impact step's `--package name@version`
   output can be consumed verbatim.
2. **Absorb the CI skip/default dance.** A resolved-empty selection is a no-op
   that exits 0 — no `--skip` sentinel in callers. A computed selection can be
   supplied as ordinary `-p` flags or as one Cargo package spec per line in a
   `--package-file`, so callers do not need shell array expansion. cargo-each
   stays agnostic about who produced the file and what the selection means.
3. **Four execution modes.** *per-package* (run the command once per member,
   substituting `{name}`/`{spec}`/`{version}`/`{manifest}`) covers per-manifest
   tools; *once* (run the command a single time when the set is non-empty)
   covers workspace-wide tools and single-invocation cargo commands, with a
   `{packages}` placeholder that expands to the cargo selection flags; and
   *per-target* runs once for each Cargo target of requested kinds, preserving
   the package placeholders and adding `{target}`. The workspace-scoped
   `{workspace-rust-version}` placeholder exposes the root compatibility floor
   to commands that provision or validate a shared toolchain. *JSON-record*
   runs once per input object without loading Cargo metadata and expands
   top-level string fields through `{json:key}`.
4. **A small, general filter language** (`--filter` and `--exclude-filter`)
   with `not`, `and`, `or`, and parentheses over cargo metadata — target kinds,
   publication state, declared features and dependencies, and
   `metadata:<dotted.key>[=<value>]` — so bespoke `cargo metadata` filtering in
   recipes collapses to flags.
5. **Bare names for free.** `{name}` yields the un-qualified package name, so
   `@version` stripping disappears from callers even though the input carries it.
6. **Bounded execution.** Per-package, per-target, and JSON-record commands may
   run with a caller-selected concurrency limit and timeout. Defaults remain
   sequential and unbounded for backward compatibility.
7. **Works identically locally and in CI**, on any platform, with no shell
   dialect assumptions. **Open source**: ships from `ox-tools` to crates.io.

## 3. Non-Goals

- **Computing an impact/affected set.** That is cargo-delta's job. `cargo-each`
  consumes a selection; it does not diff git or walk the reverse-dep graph.
- **Replacing domain glue.** semver-check's error-tolerance + advisory-comment
  aggregation, llvm-cov's dual-config instrumentation, and the per-crate readme
  `doc2readme` reconciliation stay in their recipes. `cargo-each` owns only the
  selection → filter → iterate spine those recipes wrap.
- **A general templating engine.** Placeholder substitution is a fixed, small set
  of `{token}` replacements, not an expression language.
- **Tool installation or workflow orchestration.** cargo-each can execute
  `rustup` or another installer when the caller asks it to, but it does not
  decide which tools a repository needs, select binary versus source
  installation, inspect Git, collect coverage, or understand Anvil tiers.
- **A public library API.** `cargo-each` ships as an executable only. Its
  modules are crate-internal (`pub(crate)`), so there is no semver-committed
  library surface, no `check-external-types` obligation, and nothing to consume
  as a dependency — the executable is published to crates.io, the internals are
  not. The logic lives in ordinary crate modules (unit-tested in place) behind a
  thin `main`, not in a reusable library.

## 3a. Prior art

Two existing tools overlap the "iterate workspace members" surface. Neither
covers the selection + metadata-filter + arbitrary-command spine `cargo-each`
needs, so this section records why.

### `cargo-workspaces exec`

[`cargo-workspaces`](https://crates.io/crates/cargo-workspaces) offers
`cargo workspaces exec <cmd>`, which runs a command in each crate directory
(with `--ignore-private` / `--no-bail`). It overlaps the *per-package
iteration* chore but not the parts that motivate `cargo-each`:

- **No cargo-native selection.** `exec` runs over *every* member; it does not
  accept `cargo build`'s selectors (`-p`/`--package` with globs and
  `@version`, `--workspace`, `--exclude`, `default-members`). The whole point
  here is to consume the impact step's `--package name@version …` output
  verbatim — so callers never re-parse or re-filter it.
- **No metadata filter language.** There is no `--filter lib` / `dep:<name>` /
  `metadata:<key>[=<value>]`, which is exactly the bespoke `cargo metadata`
  filtering (§1 chore 3) `cargo-each` collapses into one flag.
- **No selection-aware injection.** There is no `{packages}` once-mode token
  that expands to a single `--workspace` (or an explicit `--package` list) for
  workspace-wide tools, and no `{name}`/`{spec}`/`{manifest}` substitution — so
  the `@version`-stripping chore (§1 chore 2) would remain.
- **No empty-set-as-success contract.** `cargo-each` treats a resolved-empty
  selection as an exit-0 no-op, which is what lets CI recipes drop their
  `--skip`/default preamble (§1 chore 1).

In short, `exec` covers "run this in each crate dir"; `cargo-each` covers
"resolve a cargo-style, metadata-filtered selection (possibly empty) and run a
command over it, once-per-member or once for the set." The extra
`cargo-workspaces` dependency would not remove chores 1–3.

### `cargo-hack`

[`cargo-hack`](https://crates.io/crates/cargo-hack) runs a **cargo
subcommand** across a workspace, and *does* share package selectors
(`-p`/`--package`, `--workspace`, `--exclude`, `--ignore-private`). Its reason
for being, though, is **feature-flag and version-range combinatorics** —
`--each-feature`, `--feature-powerset`, `--version-range`/`--rust-version` — a
matrix explosion `cargo-each` explicitly leaves out (§4 has no feature axis).
The gaps that matter for the ox-tools recipes:

- **Runs cargo subcommands only.** `cargo hack <sub>` forwards to `cargo`;
  it cannot spawn an arbitrary program. The per-manifest tools these recipes
  drive — `check-external-types --manifest-path {manifest}`,
  `cargo-doc2readme`, per-crate scripts — are not `-p`-aware cargo subcommands,
  so `cargo-hack` cannot run them one-per-crate. `cargo-each` spawns any argv.
- **No `{manifest}`/`{name}`/`{spec}` substitution.** With no way to inject a
  member's manifest path or bare name into the command line, the tools above
  (which take `--manifest-path`, not `--package`) can't be targeted per crate.
- **No metadata filter language.** Selection is package + feature based; there
  is no `--filter lib`/`dep:<name>`/`metadata:<key>[=<value>]` over
  `package.metadata` (§1 chore 3).
- **No `{packages}` once-mode / empty-set no-op.** No single-invocation mode
  that injects the resolved selection into one workspace-wide command, and no
  resolved-empty → exit-0 contract for the CI skip/default dance (§1 chores 1–2).

So `cargo-hack` is the tool for "run a cargo subcommand across every
feature/version combination"; `cargo-each` is the tool for "run an *arbitrary*
command over a metadata-filtered cargo selection, with per-member
substitution." The overlap is package iteration; the feature axis and the
arbitrary-command + metadata-filter spine do not overlap.

## 4. CLI surface

```
cargo each [SELECTION | JSON INPUT] [FILTERS] [EXECUTION] -- <COMMAND> [ARG...]
```

Everything after `--` is the command template. `cargo-each` never interprets it
beyond placeholder substitution.

### 4.1 Selection (mirrors `cargo build`)

| Flag | Meaning |
|------|---------|
| `-p`, `--package <SPEC>` | Select a member. Repeatable. `SPEC` is a package name, a `name@version` spec, or a Unix glob (`tokio-*`), matching `cargo-coverage-gate`'s existing `-p` idiom. |
| `--package-file <PATH>` | Read package specs from a UTF-8 file, one spec per nonempty line. A single leading UTF-8 byte-order mark is ignored. Repeatable; specs are unioned with `--package`. A present empty file is an explicit empty selection. |
| `--workspace`, `--all` | Select every workspace member. |
| `--exclude <SPEC>` | Remove a member from the selection (requires `--workspace`). Repeatable. |
| `--none` | Explicitly select zero members. Resolves to an empty set (a no-op, exit 0). |

A package file contains only package specs, not command-line tokens, comments,
an impact-tier name, or policy. `foo@1.2.3` has the same meaning whether it came
from `--package foo@1.2.3` or a file. A missing, unreadable, non-UTF-8, or
malformed file is an error. This keeps cargo-each independent of cargo-delta
while allowing cargo-delta output to be consumed without command substitution.

**Resolution order.** The literal flags resolve to:

1. If `--none` appears anywhere → empty set.
2. Else if `--workspace`/`--all` appears → all members, minus `--exclude`.
3. Else if any direct or file-supplied spec exists → the matching members.
4. Else if at least one `--package-file` was supplied → empty set.
5. Else → `default-members` (exactly like `cargo build`; pass `--workspace`
   for the whole workspace).

A selector that matches no member is an error (same policy as
`cargo-coverage-gate`), so typos fail loudly rather than silently skipping.
An empty package file is different: it is the producer's explicit statement
that the computed set is empty and therefore exits successfully without
running the command.

### 4.2 Filters

`--filter <EXPR>` keeps only members matching the Boolean expression. Repeated
`--filter` expressions are AND-combined. `--exclude-filter <EXPR>` drops
members matching the Boolean expression; repeated exclusions are OR-combined,
and exclusion wins. Formally:

```
result = selection
       ∩ all(--filter)
       − any(--exclude-filter)
```

Expressions use `not`, `and`, `or`, and parentheses, with conventional
precedence (`not` before `and` before `or`). Operators must be lowercase and
separated from predicates by whitespace or parentheses. A metadata value that
contains whitespace, Boolean operators, or parentheses can be surrounded with double quotes;
inside it, `\"` escapes a quote and `\\` escapes a backslash. The expression
atoms are:

| Predicate | True when the member… |
|-----------|-----------------------|
| `lib` | has a plain `lib` target. Proc-macro, `cdylib`, and `staticlib` crates are **not** matched — the predicate means the plain `lib` target kind only. |
| `bin` | has a `bin` target. |
| `target-kind:<kind>` | has a target whose Cargo metadata kind is `<kind>`; accepted spellings are `lib`, `rlib`, `dylib`, `cdylib`, `staticlib`, `proc-macro`, `bin`, `example`, `test`, `bench`, and `custom-build`. |
| `publishable` | may be published: `package.publish` is absent or names at least one registry. `publish = false` is not publishable. |
| `feature:<name>` | declares the named package feature. |
| `dep:<name>` | lists `<name>` among its dependencies (any kind). |
| `metadata:<dotted.key>` | has `package.metadata.<dotted.key>` present. |
| `metadata:<dotted.key>=<value>` | has `package.metadata.<dotted.key>` equal to `<value>` (numeric compare when both parse as a number, else string compare). |

For example:

```
--filter 'publishable and (target-kind:lib or target-kind:proc-macro)'
--exclude-filter 'metadata:ox-gen-readme.disable=true or feature:internal-only'
```

Filtering runs after package selection and before any target selection. If the
filtered set is empty, `cargo-each` exits 0, exactly like an empty selection.

### 4.3 Execution

| Flag | Meaning |
|------|---------|
| *(default)* | **per-package**: run `<COMMAND>` once per selected member, in name order, with placeholders substituted. |
| `--once` | **once**: run `<COMMAND>` exactly once when the set is non-empty (skip when empty). Use `{packages}` to inject the selection. |
| `--json-lines <JSONL>` | **JSON-record mode**: parse one JSON object per nonempty line in the provided value and run `<COMMAND>` once per record. Repeatable. Mutually exclusive with Cargo package selection, filters, target/once modes, `--chdir`, `--manifest-path`, and workspace Rust-version behavior. |
| `--json-lines-file <PATH>` | Read JSON records from UTF-8 files instead of command-line values. Repeatable and may be combined with `--json-lines`; inline values are processed first, followed by files in argument order within each source. |
| `--each-target <KIND>` | **per-target**: run once for each selected member target of `KIND`. Repeatable; kinds are OR-combined and each target runs at most once. Mutually exclusive with `--once`. |
| `--target-required-feature <FEATURE>` | In per-target mode, retain targets whose `required-features` contains `FEATURE`. Repeatable; values are AND-combined. Requires `--each-target`. |
| `--keep-going` | Don't stop at the first failing command; run them all and exit non-zero if any failed. Default is fail-fast (exit with the first failure's code). |
| `--jobs <N\|auto>` | Run at most the positive integer `N` per-package, per-target, or JSON-record commands concurrently. When omitted, the default is exactly `1`. `auto` resolves once during CLI parsing via `std::thread::available_parallelism()`; detection failure is an explicit usage error with no fallback. The effective worker count remains capped by the plan size. With `--once`, resolved values other than `1` are a usage error. |
| `--timeout <DURATION>` | Terminate an invocation's Windows job object or Unix process group when it exceeds the positive duration, such as `30s` or `2m`. Applies independently to every invocation, including `--once`. Unix descendants can escape by starting a new session, so termination is best-effort for those escaped descendants. No timeout by default. |
| `--chdir` | Run each per-package or per-target command from that member's crate root (the directory containing its `Cargo.toml`) instead of the caller's CWD. Combined with `--once` it is a usage error (exit 2). Placeholders stay absolute, so only *relative* args in the command shift to the member dir. |
| `--manifest-path <PATH>` | Workspace root `Cargo.toml`. Defaults to auto-detection from CWD. |
| `--dry-run` | Print the fully-substituted commands that *would* run, one physical line per invocation, without executing. Empty arguments, `--chdir` paths, quotes, backslashes, and control or non-space whitespace characters are escaped for an unambiguous display. |

### 4.4 Placeholders

Substituted inside each `ARG` of the command template:

| Token | Expands to | Mode |
|-------|-----------|------|
| `{name}` | bare package name (`cargo-anvil`) | per-package |
| `{spec}` | `name@version` | per-package |
| `{version}` | package version | per-package |
| `{manifest}` | absolute path to the member's `Cargo.toml` | per-package |
| `{target}` | Cargo target name | per-target |
| `{packages}` | the cargo selection flags for the resolved set: `--workspace` when the whole workspace was selected via `--workspace`/`--all` with no excludes **and no package filters applied**, else `--package name@version …` (one pair per member). Only valid as a standalone `ARG`; it expands to multiple tokens. | once |
| `{workspace-rust-version}` | Root `[workspace.package].rust-version`, or root `[package].rust-version` in a single-package repository; empty when the root declaration is absent. | Cargo-backed per-package, per-target, once |
| `{json:key}` | The top-level string field named `key` from the current JSON object. Missing or non-string referenced fields are errors. Inserted values are not rescanned for placeholder-shaped text. | JSON-record |

Per-target mode accepts all per-package placeholders plus `{target}`. Using a
per-package or per-target token in `--once` mode, `{target}` in per-package
mode, or `{packages}` outside `--once` is a usage error.

Substitution scans each template argument once. Text inserted for one
placeholder is never scanned as another placeholder, so literal token-shaped
path components in manifest paths and other replacement values are preserved.

`{workspace-rust-version}` is workspace-scoped rather than tied to one selected
member. An absent root declaration expands to an empty string; without a root
floor there is no member floor to compare or require. When a declaration
exists, cargo-each requires every workspace member to expose a resolved
`rust_version` no newer than the root floor. Missing values, a member requiring
a newer compiler, or a non-Rust semantic version is a configuration error.
Lower member minima are valid. This matches the meaning of one compiler
selected for a complete workspace; it is not a per-package toolchain matrix.
Resolution is lazy: commands that do not contain the placeholder do not read or
validate the root value, and a resolved package plan with no invocations does
not resolve it. Substitution is textual: the empty value does not remove its
argv element or surrounding text, so callers gate commands that require a
declared version.

JSON-record mode does not load Cargo metadata or require a `Cargo.toml`. Each
nonempty input line must be a JSON object. Objects may contain arbitrary JSON
values, but every field referenced by `{json:key}` must exist and be a string.
Records preserve source and line order, including duplicates. All records and
placeholder references are validated before any child process is spawned.
An empty record set is a successful no-op.

Targets run in package-name order and then target-name order. A target matching
more than one requested kind runs once. No matching targets is a successful
no-op.

## 5. Semantics

- **Exit codes.** `0` when every executed command succeeded *or* the set was
  empty. In fail-fast mode, a command failure returns that command's code, a
  timeout returns `1`, and a post-spawn infrastructure failure (including
  output capture, worker, wait, or termination failure) returns `2`.
  Pre-execution usage/configuration and spawn failures also return `2`. Under
  `--keep-going`, any command, timeout, spawn, or infrastructure failure maps
  the aggregate result to `1`.
- **Empty set is success.** Both an empty selection (`--none`, or an impact
  variable that resolved to nothing) and an empty *filtered* set exit 0 after a
  one-line note to stderr. This is what lets callers drop their `--skip` guards.
- **An absent workspace Rust version is an empty value.** A nonempty package
  plan using `{workspace-rust-version}` substitutes `""` when the root manifest
  has no declaration. Malformed TOML and invalid or inconsistent metadata still
  fail before execution.
- **JSON input is strict and shell-free.** Invalid JSON, non-object records,
  malformed JSON placeholders, and missing or non-string referenced fields are
  usage/configuration errors before execution. Record values become argv text
  directly; they are never interpreted by a shell.
- **No shell.** The command is spawned directly (argv, not a shell string), so
  there is no quoting/dialect surface. Placeholder expansion is textual and
  happens before spawn.
- **Bounded concurrency.** Omitting `--jobs` requests exactly one concurrent
  invocation. A positive integer requests that fixed limit; `auto` resolves
  exactly once during CLI parsing to the machine's available parallelism and
  fails explicitly if detection is unavailable. The scheduler caps every
  request by the plan size. With an effective job
  count above one, output from each invocation is buffered and emitted as one
  block in deterministic plan order. Fail-fast stops launching new work after
  the first observed failure and waits for already-running children;
  `--keep-going` launches the complete plan. The final failure is chosen by
  plan order, not scheduler timing. Requested parallelism does not by itself
  select this captured mode: when plan-size capping leaves
  an effective worker count of one, cargo-each uses the sequential path and the
  child inherits standard input, output, and error. With a genuinely parallel
  effective worker count, child standard input is disconnected (`null`) so
  workers cannot race to consume the caller's input; output and error are
  captured for deterministic emission. A worker panic is converted into an
  infrastructure-failure outcome; each worker has a dedicated completion
  channel, so an unexpected exit is observable as disconnection rather than
  leaving the scheduler blocked forever. A worker-thread launch failure is
  represented as an infrastructure outcome at that invocation's plan index,
  so output already collected from earlier invocations is still emitted.
  Parallel work runs in plan-contiguous waves capped by the effective worker
  count. A wave is fully observed, emitted, and dropped before the next wave
  starts, so the number of retained invocation captures is also capped by the
  effective worker count.
  Untimed effective-one execution uses an ordinary child so inherited terminal
  streams, foreground-group behavior, and Ctrl-C delivery match direct command
  execution. It spawns and waits separately; a post-spawn observation failure
  makes one best-effort direct-child termination request before reporting the
  infrastructure failure. Timed and genuinely parallel commands use a Windows
  job or Unix process group. Without `--timeout`, cargo-each observes only the
  launched leader and does not kill ordinary background descendants.
- **Parallel capture uses finite temporary-file snapshots.** Every genuinely
  parallel invocation redirects stdout and stderr directly to separate unique
  temporary files before group spawn; no pipe-reader threads are created. The
  child writer and parent reader are separately reopened so parent seeks cannot
  move a descendant's write position. The parent records each file's current
  length when the leader completes, or immediately after the timeout
  termination request returns.
  Plan-order emission seeks to the beginning and streams exactly that many
  bytes, so memory does not scale with command output and output is not
  intentionally truncated.
  The plan-contiguous wave bound also caps the number of retained capture files.
  A background or escaped descendant can continue and can append through an
  inherited handle; bytes written after finalization are outside the finite
  snapshot. RAII removes cargo-each's directory entry after emission, but an
  untimed descendant that preserves the inherited writer can keep the backing
  storage allocated and continue growing it until that handle closes. The wave
  bound therefore limits cargo-each-owned files, not storage retained by
  preserved descendants. There is no portable way to revoke an inherited file
  handle without terminating that descendant. Capture create, handle-reopen,
  length, seek, or read failures are infrastructure failures.
- **Timeouts terminate jobs or process groups.** A timed-out command is a
  failure. `command-group` creates a job object on Windows and a process group
  on Unix. Both timed streamed and captured execution observe the launched
  leader directly, preserving its exit status even while an ordinary
  background group member remains. The group handle remains available solely
  for deadline termination. At the deadline cargo-each makes one termination
  request for that boundary and returns the timeout result without waiting for
  the operating system to finish process teardown. A failed termination request
  is an infrastructure failure. Unix process groups are not sealed containment:
  a descendant can escape by creating a new session, and process termination is
  asynchronous on every platform, so timeout cleanup is best-effort. On Unix,
  an uncollected timed-out leader may remain as a zombie until cargo-each exits,
  consuming one temporary process-table entry per timed-out invocation.
- **Child executable resolution follows `PATH`.** `cargo-each` explicitly
  copies an inherited `PATH` onto every child command. This is equivalent to
  ordinary inheritance on other platforms and makes Windows resolve a relative
  program from `PATH` before considering an unrelated executable beside
  `cargo-each`. If the parent has no `PATH`, cargo-each leaves it unset.

## 6. How it simplifies cargo-anvil

A planned cargo-anvil adoption can stop parsing impact selections and metadata
by hand, but the examples below are not usable with the current producer yet.
Today it writes `include_<tier>.txt` values containing `--package` tokens or the
`--workspace` / `--skip` sentinels, all of which package-file validation
intentionally rejects. The producer must first change to write one
`name@version` package spec per line under `target/anvil/impact/`, with an empty
file for an empty tier. After that producer change, a cargo-each check can
supply the appropriate file directly and get a successful no-op for an empty
tier. Illustrative planned before/after (the recipe keeps its own setup and
`anvil-impact` dependencies; only the selection spine changes):

**clippy** (affected tier, single invocation):

```powershell
# before
if (-not $env:ANVIL_INCLUDE_AFFECTED) { $env:ANVIL_INCLUDE_AFFECTED = (& just _anvil-impact-include affected) }
if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
& cargo clippy @(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' }) --all-targets --all-features --locked -- -D warnings
```
```just
# after
cargo each --package-file target/anvil/impact/affected.packages --once -- \
    cargo clippy {packages} --all-targets --all-features --locked -- -D warnings
```

**external-types** (affected tier, per-manifest, lib-only) — the whole
name-to-manifest map, `--workspace` branch, `@version` strip, and iteration loop
collapse to:

```just
cargo each --package-file target/anvil/impact/affected.packages --filter lib -- \
    cargo +{{ rust_nightly_external_types }} check-external-types --manifest-path {manifest}
```

**loom** (affected packages that depend on loom):

```just
cargo each --package-file target/anvil/impact/affected.packages --filter dep:loom -- \
    cargo +{{ rust_nightly }} test --package {name} ...
```

**per-target examples** (run each selected example with a timeout):

```just
cargo each --package-file target/anvil/impact/affected.packages \
    --each-target example --timeout 30s -- \
    cargo run --package {spec} --example {target}
```

Recipes whose only per-tier logic is the skip/splat preamble become one
`cargo each --package-file …` command. Unscoped runs pass `--workspace`
instead; choosing scoped versus unscoped input remains caller policy and is not
encoded into cargo-each.

The setup graph can resolve the optional root value without parsing Cargo TOML
in a shell, then use Just expressions to decide whether an MSRV-only command is
applicable:

```just
set lazy

workspace_rust_version_line := `cargo each --workspace --once --dry-run -- "workspace-rust-version={workspace-rust-version}"`
workspace_rust_version := replace(workspace_rust_version_line, "workspace-rust-version=", "")

install_msrv_command := if workspace_rust_version == "" {
    "# no root MSRV declared"
} else {
    "rustup toolchain install " + workspace_rust_version + " --profile minimal"
}

[private]
install-msrv-if-declared:
    {{ install_msrv_command }}
```

On a cold setup, a parent recipe first installs cargo-each and then invokes this
private recipe in a child Just process. The child boundary ensures the lazy
backtick is evaluated only after cargo-each is available.

A caller can discover records separately and execute one command per JSON line
without loading Cargo metadata:

```text
cargo each --json-lines '{"package":"alpha","test":"fuzz_one"}' -- \
    cargo bolero test --package {json:package} {json:test}
```

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
- **An Anvil-aware selection source.** Rejected: cargo-each does not accept a
  tier name, inspect `ANVIL_IMPACT`, or assume a `target/anvil` layout.
  `--package-file` is deliberately generic: one Cargo package spec per line,
  with an empty file meaning an explicitly empty set.
- **Reuse `cargo xtask`/a justfile function.** Neither is cargo-native selection;
  both re-introduce a shell dialect. A small binary is portable and testable.
