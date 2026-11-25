<div align="center">
 <img src="./logo.png" alt="Cargo-Ensure-No-Test-Features Logo" width="96">

# Cargo-Ensure-No-Test-Features

[![crates.io](https://img.shields.io/crates/v/cargo-ensure-no-test-features.svg)](https://crates.io/crates/cargo-ensure-no-test-features)
[![docs.rs](https://docs.rs/cargo-ensure-no-test-features/badge.svg)](https://docs.rs/cargo-ensure-no-test-features)
[![MSRV](https://img.shields.io/crates/msrv/cargo-ensure-no-test-features)](https://crates.io/crates/cargo-ensure-no-test-features)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml/badge.svg)](https://github.com/microsoft/ox-tools/actions/workflows/anvil-scheduled.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

Keep test-only features and insecure settings out of production code.

This Cargo subcommand rejects requests for denied features in publishable
workspace packages so production builds do not accidentally opt into them.

## Installation

```bash
cargo install cargo-ensure-no-test-features --locked
```

## Usage

```bash
cargo ensure-no-test-features
cargo ensure-no-test-features --manifest-path path/to/Cargo.toml --policy policy.json
```

The checker scans the selected Cargo workspace, even when `--manifest-path`
names a member. It skips `publish = false` but checks members restricted to
named registries. It checks explicit requests on direct
normal and build dependencies and requests reachable from a member’s default features.
It ignores dev dependencies, transitive activation and dependency defaults.

By default it denies the exact, case-sensitive names `fakes`, `test-util`,
and `insecure`. A JSON policy passed with `--policy` can replace the list
with a nonempty list of unique feature names in `features`, or omit that
field to keep the defaults. Exceptions must name a feature on the active
deny list. Unknown fields are errors except for the optional top-level
`$comment` field. Duplicate exceptions are also errors. For example:

```json
{
  "exceptions": [
    {"package": "example", "feature": "fakes", "dependency": "transport"},
    {"package": "example", "feature": "test-util"}
  ]
}
```

The first exception matches a dependency manifest key (the alias if renamed).
The second matches only the package’s own `test-util` feature when reached
from `default`, not a request for `test-util` on a dependency.
The checker exits silently with 0 when there are no findings. It reports
findings and runtime errors to stderr and exits 1. Invalid CLI usage exits 2.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-ensure-no-test-features">source code</a>.
</sub>

