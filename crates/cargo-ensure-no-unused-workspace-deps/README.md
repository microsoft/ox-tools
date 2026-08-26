<div align="center">
 <img src="./logo.png" alt="Cargo-Ensure-No-Unused-Workspace-Deps Logo" width="96">

# Cargo-Ensure-No-Unused-Workspace-Deps

[![crates.io](https://img.shields.io/crates/v/cargo-ensure-no-unused-workspace-deps.svg)](https://crates.io/crates/cargo-ensure-no-unused-workspace-deps)
[![docs.rs](https://docs.rs/cargo-ensure-no-unused-workspace-deps/badge.svg)](https://docs.rs/cargo-ensure-no-unused-workspace-deps)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ensure-no-unused-workspace-deps)](https://crates.io/crates/cargo-ensure-no-unused-workspace-deps)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A cargo sub-command that ensures every `[workspace.dependencies]` entry is
inherited by at least one workspace member.

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

Run in a cargo workspace:

```bash
cargo ensure-no-unused-workspace-deps
```

Remove what it finds:

```bash
cargo ensure-no-unused-workspace-deps --fix
```

`--manifest-path` points at an explicit workspace root, defaulting to the
`Cargo.toml` in the current directory. A manifest with no `[workspace]` table
declares no catalog and passes with a note; `--require-workspace` turns that
into an error for callers that know they are pointing at a workspace root.

## Configuration

An entry kept on purpose is exempted in the workspace manifest:

```toml
[workspace.metadata.ensure-no-unused-workspace-deps]
allowed = ["kept-on-purpose"]
```

An `allowed` name that suppresses nothing is reported as stale, on stderr,
without failing the run.

## Fixing

`--fix` replaces the manifest atomically – a temporary file in the same
directory, renamed over the original – and refuses to write at all if the
file changed after it was read, so a concurrent edit is never clobbered.

Comments on a removed entry are carried to the next surviving entry, which
keeps a group header attached to the group it introduces. A note about one
specific dependency is indistinguishable from such a header, so every move
is reported on stderr: check that carried text still describes the entry it
landed on.

## Installation

```bash
cargo install cargo-ensure-no-unused-workspace-deps
```

## Example output

```text
Found 2 unused workspace dependencies in Cargo.toml:

  - once_cell
  - smallvec

Re-run with --fix to remove them.
```


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ensure-no-unused-workspace-deps">source code</a>.
</sub>

