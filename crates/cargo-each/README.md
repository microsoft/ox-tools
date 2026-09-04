<div align="center">
 <img src="./logo.png" alt="Cargo-Each Logo" width="96">

# Cargo-Each

[![crates.io](https://img.shields.io/crates/v/cargo-each.svg)](https://crates.io/crates/cargo-each)
[![docs.rs](https://docs.rs/cargo-each/badge.svg)](https://docs.rs/cargo-each)
[![MSRV](https://img.shields.io/crates/msrv/cargo-each)](https://crates.io/crates/cargo-each)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
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
* `--workspace` / `--all` — select every workspace member.
* `--exclude <SPEC>` — drop a member (with `--workspace`). Repeatable.
* `--none` — explicitly select zero members (a no-op that exits 0).

When nothing is named the default is cargo `default-members`, exactly
like `cargo build`; pass `--workspace` for every member. A selector that
matches no member is an error, so typos fail loudly. A computed selection
(for example a CI affected-packages set) is fed in as ordinary flags via
shell expansion — `cargo-each` has no file or environment-variable source
of its own.

### Filters

`--filter` accepts Boolean expressions using `not`, `and`, `or`, and
parentheses, with conventional precedence. Repeated `--filter` expressions
are AND-combined. Repeated `--exclude-filter` expressions are OR-combined,
and exclusion wins. Metadata values containing Boolean syntax can be
double-quoted. Expression atoms:

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
(default is fail-fast); `--chdir` runs each per-package or per-target
command from that member crate root; `--dry-run` prints commands without
running them.

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

Using a placeholder in the wrong mode is a usage error. Only the tokens
above are interpreted; any other `{…}` sequence (a typo, or a literal brace
an argument needs) passes through verbatim to the spawned command — there is
no brace-escape, so this passthrough is part of the contract.

## Behavior

An empty resolved selection (via `--none`, or a filter that removes every
member) is a **successful no-op**: `cargo-each` prints a one-line note and
exits 0. This is what lets callers drop bespoke nothing-to-do guards.
Otherwise the exit code is the first failing command code (fail-fast),
`1` under `--keep-going` if any command failed, or `2` for a `cargo-each`
usage error (unknown selector, bad filter expression, misused placeholder).

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

