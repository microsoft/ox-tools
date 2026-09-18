<div align="center">
 <img src="./logo.png" alt="Cargo-Each Logo" width="96">

# Cargo-Each

[![crates.io](https://img.shields.io/crates/v/cargo-each.svg)](https://crates.io/crates/cargo-each)
[![docs.rs](https://docs.rs/cargo-each/badge.svg)](https://docs.rs/cargo-each)
[![MSRV](https://img.shields.io/crates/msrv/cargo-each)](https://crates.io/crates/cargo-each)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

`cargo-each`: run a command over a cargo-style selection of workspace
members.

`cargo-each` resolves a package selection expressed with the same
selectors as `cargo build`, optionally narrows it with package predicates,
and runs a command over the result — once per member, once per matching
Cargo target, or exactly once for the whole set. It replaces hand-rolled
shell loops with one cargo-native, cross-platform command.

`cargo-each` ships as an executable only; it is a cargo subcommand, not a
library dependency.

## Installation

```text
cargo install cargo-each
```

## Usage

```text
cargo each [SELECTION] [FILTERS] [EXECUTION] -- <COMMAND> [ARG...]
```

Everything after `--` is the command template; `cargo-each` spawns it
directly (argv, not a shell string) after substituting placeholders.

### Selection (mirrors `cargo build`)

* `-p` / `--package <SPEC>` — select a member. Repeatable. `SPEC` is a
  package name, a `name@version` spec, or a Unix glob (`tokio-*`).
* `--package-file <PATH>` — read package specs from a UTF-8 file, one per
  nonempty line. Repeatable; specs are unioned with `--package`. An empty
  file explicitly selects no members. One leading UTF-8 byte-order mark is
  ignored.
* `--workspace` / `--all` — select every workspace member.
* `--exclude <SPEC>` — drop a member (with `--workspace`). Repeatable.
* `--none` — explicitly select zero members (a no-op that exits 0).

When nothing is named the default is cargo `default-members`, exactly
like `cargo build`; pass `--workspace` for every member. A selector that
matches no member is an error, so typos fail loudly. Package files contain
package specs only: comments, command-line tokens, malformed input, and
missing, unreadable, or non-UTF-8 files are errors.

### Filters

`--filter` accepts Boolean expressions using `not`, `and`, `or`, and
parentheses, with conventional precedence. Repeated `--filter` expressions
are AND-combined. Repeated `--exclude-filter` expressions are OR-combined,
and exclusion wins. Metadata values containing whitespace or Boolean syntax
can be double-quoted. Expression atoms:

* `lib` / `bin` / `target-kind:<kind>` — target-kind membership.
* `publishable` — Cargo permits publishing the package.
* `feature:<name>` — the package declares the feature.
* `dep:<name>` — the member declares `<name>` as a dependency.
* `metadata:<dotted.key>` — `package.metadata.<dotted.key>` is present.
* `metadata:<dotted.key>=<value>` — that key equals `<value>` (numeric
  compare when both sides parse as a number, else string compare).

### Execution modes

* *per-package* (default): run the command once per selected member, in
  name order, substituting the per-package placeholders below.
* `--once`: run the command exactly once when the set is non-empty (skip
  when empty), using the `{packages}` placeholder to inject the selection.
* `--each-target <KIND>`: run once per matching Cargo target, using
  `{target}` plus the package placeholders. Repeated kinds are OR-combined;
  `--target-required-feature` further narrows targets.

`--keep-going` runs every invocation and exits non-zero if any failed
(default is fail-fast). `--jobs <N|auto>` bounds concurrent per-package or
per-target work. Omitting it runs exactly one invocation at a time; `auto`
resolves once to the machine’s available parallelism. Detection failure is
reported explicitly without falling back. `--timeout <DURATION>` terminates
each invocation and its process tree independently (`250ms`, `30s`, or
`2m`).
Timeouts require sealed process-tree containment; on a host that only
offers best-effort containment, cargo-each reports an unsupported
infrastructure failure before starting the child.
`--chdir` runs each per-package or per-target command from that member crate
root; `--dry-run` prints commands without running them.

### Placeholders

Substituted inside each command argument:

* `{name}` — bare package name (per-package and per-target).
* `{spec}` — `name@version` (per-package and per-target).
* `{version}` — package version (per-package and per-target).
* `{manifest}` — absolute member `Cargo.toml` path (per-package and
  per-target).
* `{target}` — Cargo target name (per-target).
* `{packages}` — the cargo selection flags for the resolved set
  (`--workspace` for the whole workspace, else `--package name@version …`);
  valid only in `--once` mode and only as a standalone argument.
* `{workspace-rust-version}` — the root `[workspace.package].rust-version`,
  or root `[package].rust-version` in a single-package repository; valid in
  every mode.

Using a placeholder in the wrong mode is a usage error. Only the tokens
above are interpreted; any other `{…}` sequence (a typo, or a literal brace
an argument needs) passes through verbatim to the spawned command — there is
no brace-escape, so this passthrough is part of the contract.

## Behavior

An empty resolved selection (via `--none`, or a filter that removes every
member) is a **successful no-op**: `cargo-each` prints a one-line note and
exits 0. This is what lets callers drop bespoke nothing-to-do guards.
Workspace Rust-version validation is lazy: it runs only when the command
uses `{workspace-rust-version}` and the resolved plan has work, then requires
every member’s resolved minimum to be present and no newer than the root
floor. Placeholder mode validation still runs before an empty-plan no-op.

The effective worker count is the requested `--jobs` value capped by plan
size and scheduler capacity. An effective count of one uses sequential
execution with inherited standard input, output, and error even when the
requested value was larger. A genuinely parallel count disconnects child input and
buffers stdout and stderr; complete blocks are emitted in deterministic
plan order. Fail-fast stops launching after the first
observed failure, waits for running work, and chooses the final failure by
plan order. `--keep-going` runs the complete plan. Worker panics and
unexpected worker-channel disconnections become infrastructure-failure
outcomes instead of blocking the scheduler. Worker launch failures retain
output already collected at earlier plan indices. Without `--timeout`,
parallel commands retain ordinary direct-child semantics and do not kill
background descendants. Each output stream retains at most 1 MiB in memory
before spilling to a unique system-temporary file owned by the invocation
outcome; spill failures are infrastructure failures and spill files are
removed by RAII after deterministic plan-order emission.

Reader failures are observed while the child is running and trigger bounded
termination. Output drain is bounded after every completion: readers get
one second to observe EOF, then readiness-polling capture is cancelled and
joined while partial bytes become an explicit infrastructure failure. If a
cancelled reader remains stalled while holding its capture mutex, output
recovery is nonblocking and any unavailable partial bytes are reported
rather than extending the drain bound.
Timed-out tree termination likewise gets a bounded 250 ms leader-reap grace,
after which the leader handle moves to a shared detached reaper so no wait
or Drop path can defeat the timeout without abandoning reap ownership.
Child commands inherit `PATH` explicitly. On Windows this makes relative
program lookup honor the inherited `PATH` order instead of preferring an
unrelated executable beside `cargo-each`.
Exit `0` means all work succeeded or there was no work. In fail-fast mode a
command failure returns its code, a timeout returns `1`, and usage,
configuration, spawn, or post-spawn infrastructure failures return `2`.
Under `--keep-going`, any failure maps the aggregate result to `1`.

## Examples

Run a per-manifest tool over every library crate:

```text
cargo each --workspace --filter lib -- \
    cargo check-external-types --manifest-path {manifest}
```

Run one clippy invocation over a computed subset, skipping when it is empty:

```text
cargo each -p crate-a -p crate-b --once -- \
    cargo clippy {packages} --all-targets -- -D warnings
```

Run every Cargo test target that requires the `loom` feature:

```text
cargo each --workspace --each-target test --target-required-feature loom -- \
    cargo test -p {name} --test {target} --features loom
```


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-each">source code</a>.
</sub>

