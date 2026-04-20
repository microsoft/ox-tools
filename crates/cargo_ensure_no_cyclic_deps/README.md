<div align="center">
 <img src="./logo.png" alt="Cargo-Ensure-No-Cyclic-Deps Logo" width="96">

# Cargo-Ensure-No-Cyclic-Deps

[![crate.io](https://img.shields.io/crates/v/cargo-ensure-no-cyclic-deps.svg)](https://crates.io/crates/cargo-ensure-no-cyclic-deps)
[![docs.rs](https://docs.rs/cargo-ensure-no-cyclic-deps/badge.svg)](https://docs.rs/cargo-ensure-no-cyclic-deps)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ensure-no-cyclic-deps)](https://crates.io/crates/cargo-ensure-no-cyclic-deps)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

A cargo sub-command that detects cyclic dependencies between crates in a workspace. This is useful if you
want to prevent dev-dependencies from creating dependency cycles as that can cause issues,
e.g. for [`cargo-release`][__link0].

## Usage

Run this command in a cargo workspace:

```bash
cargo ensure-no-cyclic-deps
```

The command will:

* Analyze all workspace crates
* Check for cyclic dependencies (including dev-dependencies)
* Report any cycles found
* Exit with code 1 if cycles are detected, 0 otherwise

## Installation

```bash
cargo install --path .
```

Or from within the workspace:

```bash
cargo install cargo-ensure-no-cyclic-deps
```

## Example Output

When cycles are detected:

```text
Error: Cyclic dependencies detected!

Cycle 1:
  crate_a -> crate_b -> crate_c -> crate_a

Cycle 2:
  crate_x -> crate_y -> crate_x
```

When no cycles are found:

```text
No cyclic dependencies found.
```

The tool will exit with code 0 if no cycles are found, or code 1 if cycles are detected.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ensure-no-cyclic-deps">source code</a>.
</sub>

 [__link0]: https://github.com/crate-ci/cargo-release
