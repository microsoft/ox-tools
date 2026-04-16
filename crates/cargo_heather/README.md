<div align="center">
 <img src="./logo.png" alt="Cargo Heather Logo" width="96">

# Cargo Heather

[![crate.io](https://img.shields.io/crates/v/cargo_heather.svg)](https://crates.io/crates/cargo_heather)
[![docs.rs](https://docs.rs/cargo_heather/badge.svg)](https://docs.rs/cargo_heather)
[![MSRV](https://img.shields.io/crates/msrv/cargo_heather)](https://crates.io/crates/cargo_heather)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-heather

A cargo sub-command to validate license headers in Rust (`.rs`) and TOML (`.toml`) source files.

### Installation

```bash
cargo install --path .
```

### Setup

Create a `.cargo-heather.toml` file in your project root, **or** simply set the `license` field in your `Cargo.toml` — the tool will use it automatically when no `.cargo-heather.toml` is present.

#### Using an SPDX License Identifier

```toml
license = "MIT"
```

#### Using a Custom Header

```toml
header = """
Copyright (c) 2024 MyCompany
All rights reserved.
"""
```

### Usage

```bash
# Check all .rs and .toml files for correct license headers
cargo heather

# Automatically fix files by adding/replacing headers
cargo heather --fix
```

#### Options

* `--project-dir <PATH>` — Path to the project directory (defaults to current directory)
* `--config <PATH>` — Path to the configuration file (defaults to `.cargo-heather.toml` in project dir)
* `--fix` — Fix files by adding or replacing missing/incorrect headers
* `--help` — Print help
* `--version` — Print version

#### Example

```bash
$ cargo heather
Checking 5 file(s)...
MISSING header: src/utils.rs
MISMATCH header: src/lib.rs
2 file(s) have missing or incorrect license headers

$ cargo heather --fix
Checking 5 file(s)...
Fixed (added header): src/utils.rs
Fixed (replaced header): src/lib.rs
Fixed 2 file(s).
```

### Supported SPDX Identifiers

|Identifier|License|
|----------|-------|
|`MIT`|MIT License|
|`Apache-2.0`|Apache License 2.0|
|`GPL-2.0-only`|GNU General Public License v2.0 only|
|`GPL-2.0-or-later`|GNU General Public License v2.0 or later|
|`GPL-3.0-only`|GNU General Public License v3.0 only|
|`GPL-3.0-or-later`|GNU General Public License v3.0 or later|
|`LGPL-2.1-only`|GNU Lesser General Public License v2.1 only|
|`LGPL-2.1-or-later`|GNU Lesser General Public License v2.1 or later|
|`LGPL-3.0-only`|GNU Lesser General Public License v3.0 only|
|`LGPL-3.0-or-later`|GNU Lesser General Public License v3.0 or later|
|`BSD-2-Clause`|BSD 2-Clause “Simplified” License|
|`BSD-3-Clause`|BSD 3-Clause “New” or “Revised” License|
|`ISC`|ISC License|
|`MPL-2.0`|Mozilla Public License 2.0|
|`AGPL-3.0-only`|GNU Affero General Public License v3.0 only|
|`AGPL-3.0-or-later`|GNU Affero General Public License v3.0 or later|
|`Unlicense`|The Unlicense|
|`BSL-1.0`|Boost Software License 1.0|
|`0BSD`|BSD Zero Clause License|
|`Zlib`|zlib License|

### How it works

1. **Config loading** — Reads `.cargo-heather.toml` from the project root and resolves the expected header text (from SPDX identifier or custom text).
1. **File scanning** — Walks the project directory to find all `.rs` and `.toml` files, skipping `target/`, hidden directories, and the config file itself.
1. **Header validation** — Extracts the first comment block from each file (`//` for Rust, `#` for TOML) and compares it to the expected header. Reports missing or mismatched headers.
1. **Fix mode** — When `--fix` is passed, automatically prepends the correct header to files that are missing it, or replaces incorrect headers.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo_heather">source code</a>.
</sub>

