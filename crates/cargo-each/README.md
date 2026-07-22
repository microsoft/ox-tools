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
selectors as `cargo build`, optionally narrows it with a small metadata
predicate language, and runs a command over the result — either once per
member (with placeholder substitution) or exactly once for the whole set.
It exists to replace hand-rolled for-each-package shell loops with a
single cargo-native, cross-platform command.

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

`--filter <PRED>` keeps only members matching `PRED`; `--exclude-filter <PRED>` drops them. Both are repeatable and AND-combined
(`--exclude-filter` wins on conflict). Predicates:

* `lib` / `bin` — the member has a target of that kind.
* `dep:<name>` — the member declares `<name>` as a dependency.
* `metadata:<dotted.key>` — `package.metadata.<dotted.key>` is present.
* `metadata:<dotted.key>=<value>` — that key equals `<value>` (numeric
  compare when both sides parse as a number, else string compare).

### Execution modes

* *per-package* (default): run the command once per selected member, in
  name order, substituting the per-package placeholders below.
* `--once`: run the command exactly once when the set is non-empty (skip
  when empty), using the `{packages}` placeholder to inject the selection.

`--keep-going` runs every invocation and exits non-zero if any failed
(default is fail-fast); `--chdir` runs each per-package command from that
member crate root (its `Cargo.toml` directory) instead of the current
directory — per-package mode only, so it cannot be combined with `--once`;
`--dry-run` prints the fully-substituted commands without running them.

### Placeholders

Substituted inside each command argument:

* `{name}` — bare package name (per-package mode).
* `{spec}` — `name@version` (per-package mode).
* `{version}` — package version (per-package mode).
* `{manifest}` — absolute path to the member `Cargo.toml` (per-package).
* `{packages}` — the cargo selection flags for the resolved set
  (`--workspace` for the whole workspace, else `--package name@version …`);
  valid only in `--once` mode and only as a standalone argument.

Using a placeholder in the wrong mode is a usage error.

## Behavior

An empty resolved selection (via `--none`, or a filter that removes every
member) is a **successful no-op**: `cargo-each` prints a one-line note and
exits 0. This is what lets callers drop bespoke nothing-to-do guards.
Otherwise the exit code is the first failing command code (fail-fast),
`1` under `--keep-going` if any command failed, or `2` for a `cargo-each`
usage error (unknown selector, bad predicate, misused placeholder).

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

## Library

The binary (`src/bin/cargo-each`) is a thin shell over this library, which
owns the reusable, testable spine:

* [`Workspace`][__link0] / [`Member`][__link1] — `cargo metadata` discovery.
* [`Selection`][__link2] — parse selectors and resolve them against a workspace.
* [`Predicate`][__link3] — the `--filter` / `--exclude-filter` metadata language.
* [`substitute`][__link4] — placeholder expansion for the command template.
* [`Plan`][__link5] — the resolved list of command invocations to run.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-each">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGkYW0CYXSEGxYc2fK81jTWG7kWg0hlspxYGx-DzHaE-xjXG1cDT7T4wIbxYXKEG9CUPcNa5oVYGxMUpErv_0qaG0i0nLqcHFV5G5CdBXlxkH8jYWSBg2pjYXJnby1lYWNoZTAuMS4wamNhcmdvX2VhY2g
 [__link0]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=Workspace
 [__link1]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=Member
 [__link2]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=Selection
 [__link3]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=Predicate
 [__link4]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=substitute
 [__link5]: https://docs.rs/cargo-each/0.1.0/cargo_each/?search=Plan
