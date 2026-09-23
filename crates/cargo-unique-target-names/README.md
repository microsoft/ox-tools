<div align="center">
 <img src="./logo.png" alt="Cargo-Unique-Target-Names Logo" width="96">

# Cargo-Unique-Target-Names

[![crates.io](https://img.shields.io/crates/v/cargo-unique-target-names.svg)](https://crates.io/crates/cargo-unique-target-names)
[![docs.rs](https://docs.rs/cargo-unique-target-names/badge.svg)](https://docs.rs/cargo-unique-target-names)
[![MSRV](https://img.shields.io/crates/msrv/cargo-unique-target-names)](https://crates.io/crates/cargo-unique-target-names)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A cargo sub-command that fails when two workspace packages own targets which
uplift to the same build artifact.

Cargo compiles each unit into `target/<profile>/deps/` under a
metadata-hashed name, then uplifts the root unit of most target kinds to
`target/<profile>/` (examples to `target/<profile>/examples/`) under a
plain name carrying no hash. Two workspace packages whose targets uplift to
the same file therefore write the same file. Cargo reports this as `output  filename collision`, warns that it may become a hard error, and keeps
building – so the workspace stays green while the last writer wins and
concurrent jobs race for one path. This tool turns that warning into a
failure before anything is compiled.

The most likely way to hit it needs no configuration at all: Cargo
normalizes `-` to `_` in the default library target name, so packages
`foo-bar` and `foo_bar` in one workspace both uplift to `libfoo_bar.rlib`.

## Usage

Run this command in a cargo workspace or crate directory:

```bash
cargo unique-target-names
```

The `--manifest-path` option lets you point at an explicit `Cargo.toml`.
Without it, the manifest is discovered from the current directory.

## Installation

```bash
cargo install cargo-unique-target-names
```

## Example Output

When two packages contend for one file:

```text
cargo-unique-target-names: target 'basic' is declared by 2 targets: metabench (example 'basic'), observed (example 'basic')
  they uplift to the same files: target/<profile>/examples/basic.pdb, target/<profile>/examples/basic[.exe]

Rename the reported targets so each one uplifts to its own path.
```

When everything checks out:

```text
All workspace targets uplift to their own path
```

The tool exits with code 0 when every uplifted file has one owner, code 1
when at least one is contended, and code 2 when the workspace could not be
read at all – so a broken workspace is distinguishable from a finding.

## What is and is not reported

Targets are keyed by *every* file they uplift, because the relationship
between crate type and file runs both ways:

* `cdylib`, `dylib`, and `proc-macro` all emit one platform shared library,
  so a collision crosses those crate types;
* an executable and a shared library emit different primary files
  (`tool.exe`, `tool.dll`) but both write `tool.pdb`;
* an `rlib` and a `staticlib` emit no uplifted debug-info file, so they
  coexist with a binary of the same name.

Test and benchmark binaries and build scripts keep their metadata hash in
`deps/` and are exempt. Owners are keyed per target rather than per package,
so a package that contends with itself — Cargo permits a `[lib]` and a
`[[bin]]` of one name, and they share a debug-info file — is reported like
any other pair.

The Windows debug-info file is considered on every platform, so a
Windows-only collision still fails a Linux run and every leg of a build
matrix agrees.

Two hazards Cargo itself does not warn about are deliberately not reported:
dep-info files, which collapse by file stem, and target names differing only
in case on a case-insensitive filesystem. Both are recorded in the crate’s
design document.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-unique-target-names">source code</a>.
</sub>

