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
each invocation’s Windows job object or Unix process group independently
(`250ms`, `30s`, or `2m`). Unix descendants can escape a process group by
starting a new session, so timeout cleanup is best-effort for those escaped
descendants.
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
size. An effective count of one uses sequential
execution with inherited standard input, output, and error even when the
requested value was larger. A genuinely parallel count disconnects child input and
buffers stdout and stderr; complete blocks are emitted in deterministic
plan order. Fail-fast stops launching after the first
observed failure, waits for running work, and chooses the final failure by
plan order. `--keep-going` runs the complete plan. Worker panics and
unexpected worker-channel disconnections become infrastructure-failure
outcomes instead of blocking the scheduler. Worker launch failures retain
output already collected at earlier plan indices. Parallel work runs in
plan-contiguous waves capped by the effective worker count; each completed
wave is emitted and dropped before the next wave starts, bounding retained
temporary-file storage. Without `--timeout`, parallel commands are launched
in a job or process group, but cargo-each observes only the leader and does
not kill background descendants. Every genuinely parallel invocation
redirects stdout and stderr directly to separate unique temporary files.
Child writers and parent readers are separately reopened so parent seeks
cannot move descendant write positions. Cargo-each records each file’s
current length when the leader completes (or after timeout cleanup), then
reads exactly that finite snapshot in plan order without loading unbounded
output into memory. Later writes by background or escaped descendants are
outside the snapshot, and inherited file handles cannot hold capture open.
Capture create, reopen, length, seek, and read failures are infrastructure
failures; files are removed by RAII.

Timed-out group termination gets a bounded 250 ms reap grace. If the group
still has not completed, its handle moves to a cargo-each-local polling
reaper started before any command. The reaper checks every retained group
without blocking on one child, remains the wait owner after the caller
returns, and exits after all senders disconnect and retained groups are
collected. Reaper startup and handoff failures are explicit infrastructure
failures; a failed handoff retains the group handle in a persistent fallback.
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

