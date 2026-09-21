<div align="center">
 <img src="./logo.png" alt="Cargo-Unused-Deps Logo" width="96">

# Cargo-Unused-Deps

[![crates.io](https://img.shields.io/crates/v/cargo-unused-deps.svg)](https://crates.io/crates/cargo-unused-deps)
[![docs.rs](https://docs.rs/cargo-unused-deps/badge.svg)](https://docs.rs/cargo-unused-deps)
[![MSRV](https://img.shields.io/crates/msrv/cargo-unused-deps)](https://crates.io/crates/cargo-unused-deps)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A Cargo subcommand that finds unused dependencies.

It answers three questions that usually take two other tools and still leave
a gap:

* **Catalog.** Which `[workspace.dependencies]` entries does no member
  inherit? Inheritance is written in the manifest, so this needs no compiler
  and cannot produce a false positive.
* **Unused.** Which declared dependencies did no compiled unit load?
* **Misplaced.** Which `[dependencies]` entries only development units load,
  and therefore belong in `[dev-dependencies]`?

The last two are answered by rustc itself, through the
`unused_crate_dependencies` lint, aggregated across every unit of a package:
a dependency is unused only when every unit that had it in scope said so.
Doctests are included, which no other tool manages – rustdoc discards the
compiler’s output for them, so this binary stands in for the compiler rustdoc uses
and keeps a copy.

## Requirements

Package-level checks require a nightly toolchain because compiling doctests
without running them is unstable. The catalog-only invocation with no package
selector is manifest-only and runs on stable.

## Usage

Run every check across a Cargo workspace:

```bash
cargo +nightly unused-deps --workspace
```

Restrict the compiled evidence the way cargo does, which is what lets an
impact-scoped pipeline pass its own package list straight through:

```bash
cargo +nightly unused-deps --package my-crate --package other-crate
```

Run only the workspace-global catalog check by omitting package selection:

```bash
cargo unused-deps
```

Remove the catalog entries nobody inherits:

```bash
cargo +nightly unused-deps --fix
```

`--manifest-path` points at an explicit workspace root, defaulting to the
`Cargo.toml` in the current directory. A manifest with no `[workspace]` table
declares no catalog, so that check passes with a note while the rest still
run; `--require-workspace` turns it into an error instead.

Package selection scopes the compiled evidence only. The catalog check always
reads every member, because “no member inherits this entry” is only true if
every member was consulted. With no `--package` or `--workspace`, no package
is compiled and only that catalog check runs.

## Configuration

A dependency kept on purpose is exempted in the workspace manifest:

```toml
[workspace.metadata.unused-deps]
allowed = ["kept-on-purpose"]

[package.metadata.unused-deps]
allowed = ["package-local-side-effect"]
```

A workspace-level `allowed` name declared by neither the catalog nor any
member is reported as stale without failing the run. The lists are also the
answer for a dependency linked for its side effects and never named – an
allocator or `-sys` shim – where “unused” is literally true and
operationally wrong.

## Fixing

`--fix` covers the catalog only. It replaces the manifest atomically – a
temporary file in the same directory, renamed over the original, carrying the
permissions of the manifest it replaces and following a symlinked manifest to
its target. Before replacement it rechecks workspace membership and every
manifest input; a change detected there aborts the write. This narrows but
cannot close the final comparison-to-rename race.

Comments on a removed entry are carried to the next surviving entry, which
keeps a group header attached to the group it introduces. A note about one
specific dependency is indistinguishable from such a header, so every move
is reported on stderr: check that carried text still describes the entry it
landed on. Comments that cannot be placed – the removal emptied the table,
or left a trailing survivor with nothing to append to – are reported as
dropped.

Removing a dependency a crate declares is not automated: the evidence is
strong enough to fail a build and ask a human, not strong enough to edit code
paths nobody compiled.

## Installation

```bash
cargo install cargo-unused-deps
```

## Example output

```text
✅ All 70 workspace dependencies in Cargo.toml are inherited by one of 10 members.
❌ Found 1 dependency problem:

  my-crate [dependencies] once_cell: no compiled unit loaded it.
      remove it, or gate the declaration to where it is used.
```


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-unused-deps">source code</a>.
</sub>

