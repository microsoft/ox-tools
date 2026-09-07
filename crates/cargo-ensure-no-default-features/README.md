<div align="center">
 <img src="./logo.png" alt="Cargo-Ensure-No-Default-Features Logo" width="96">

# Cargo-Ensure-No-Default-Features

[![crates.io](https://img.shields.io/crates/v/cargo-ensure-no-default-features.svg)](https://crates.io/crates/cargo-ensure-no-default-features)
[![docs.rs](https://docs.rs/cargo-ensure-no-default-features/badge.svg)](https://docs.rs/cargo-ensure-no-default-features)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ensure-no-default-features)](https://crates.io/crates/cargo-ensure-no-default-features)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A cargo sub-command that ensures every dependency in a `Cargo.toml` file is declared
with `default-features = false`.

Enabling default features by accident pulls in code you never asked for, which inflates
build times, binary size, and the dependency surface that must be audited. This tool
makes that mistake a build break instead of a silent regression.

If both `[workspace.dependencies]` and `[dependencies]` are present in the same
manifest, both sections are checked. Dependencies that use `workspace = true` are
skipped, since they inherit their settings from the workspace.

## Usage

Run this command in a cargo workspace or crate directory:

```bash
cargo ensure-no-default-features
```

The `--manifest-path` option lets you specify an explicit `Cargo.toml` file to check.
Without this option, it defaults to the `Cargo.toml` in the current directory.

The `--exceptions` (`-e`) option lets you specify a comma-separated list of dependencies
to exclude from the `default-features` check. This is useful for dependencies that you
explicitly want to have default features enabled.

```bash
cargo ensure-no-default-features --manifest-path path/to/Cargo.toml --exceptions serde,tokio
```

## Installation

```bash
cargo install cargo-ensure-no-default-features
```

## Example Output

When offending dependencies are found:

```text
Found 1 dependencies without default-features = false:

  - 'serde': missing default-features = false
```

When everything checks out:

```text
All required dependencies have default-features = false
```

The tool exits with code 0 if all dependencies are well-formed, or code 1 otherwise.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ensure-no-default-features">source code</a>.
</sub>

