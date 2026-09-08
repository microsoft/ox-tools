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

A Cargo subcommand that reports uninherited workspace dependencies.

A workspace root declares a dependency catalog that members draw from with
`dep = { workspace = true }`. Nothing requires an entry to be drawn from, so
an entry nobody inherits stays in the manifest forever: it never enters the
dependency graph, and no build fails because of it. It still carries a
version requirement, so it keeps attracting dependency-bump traffic and keeps
misleading readers about what the workspace depends on.

Unused-dependency tools resolve the crate graph and ask which *declared*
dependencies go unused, so an entry that no member declares is invisible to
them. This tool answers the prior question – is the entry inherited at all?
– from the manifests alone, which makes it free of false positives and cheap
enough to run on every pull request.

## Usage

Run in a Cargo workspace:

```bash
cargo unused-deps
```

Remove what it finds:

```bash
cargo unused-deps --fix
```

`--manifest-path` points at an explicit workspace root manifest, defaulting
to the `Cargo.toml` in the current directory. A manifest with no
`[workspace]` table declares no catalog and passes with a note;
`--require-workspace` turns that into an error for callers that know they
are pointing at a root manifest.

## Configuration

An entry kept on purpose is exempted in the workspace manifest:

```toml
[workspace.metadata.unused-deps]
allowed = ["kept-on-purpose"]
```

An `allowed` name that suppresses no unused catalog entry is reported as a
stale allow-list entry, on stderr, without failing the run.

## Fixing

`--fix` edits only the workspace root manifest, and does so carefully.

The replacement is written to a temporary file in the manifest’s own
directory and renamed over the original, so the manifest is never truncated
in place. The rename carries the permissions of the manifest it replaces.
A symlinked manifest is resolved first, so the rename lands on the file the
link points at rather than replacing the link.

Before the rename the manifest is re-read and compared against the bytes
that were parsed. An edit that arrives while `cargo metadata` runs is
therefore detected and the fix abandoned. The check narrows that window
rather than closing it: an edit landing between the comparison and the
rename is still overwritten.

Comments on a removed entry are carried to the next surviving entry, which
keeps a group header attached to the group it introduces. A note about one
specific dependency is indistinguishable from such a header, so every move
is reported on stderr: check that carried text still describes the entry it
landed on. Comments that cannot be placed – the removal emptied the table,
or left a trailing survivor with nothing to append to – are reported as
dropped.

## Installation

```bash
cargo install cargo-unused-deps
```

## Example output

```text
❌ Found 2 unused workspace dependencies in Cargo.toml:

  - once_cell
  - smallvec

Re-run with --fix to remove what is listed above.
```


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-unused-deps">source code</a>.
</sub>

