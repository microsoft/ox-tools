# cargo-ensure-no-cyclic-deps ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![cargo-ensure-no-cyclic-deps on crates.io](https://img.shields.io/crates/v/cargo-ensure-no-cyclic-deps)](https://crates.io/crates/cargo-ensure-no-cyclic-deps) [![cargo-ensure-no-cyclic-deps on docs.rs](https://docs.rs/cargo-ensure-no-cyclic-deps/badge.svg)](https://docs.rs/cargo-ensure-no-cyclic-deps) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ensure-no-cyclic-deps) [![Rust Version: 1.88.0](https://img.shields.io/badge/rustc-1.88.0-orange.svg)](https://github.com/rust-lang/rust/releases/tag/1.88.0)

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


 [__link0]: https://github.com/crate-ci/cargo-release
