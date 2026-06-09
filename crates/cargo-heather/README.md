<div align="center">
 <img src="./logo.png" alt="Cargo-Heather Logo" width="96">

# Cargo-Heather

[![crates.io](https://img.shields.io/crates/v/cargo-heather.svg)](https://crates.io/crates/cargo-heather)
[![docs.rs](https://docs.rs/cargo-heather/badge.svg)](https://docs.rs/cargo-heather)
[![MSRV](https://img.shields.io/crates/msrv/cargo-heather)](https://crates.io/crates/cargo-heather)
[![CI](https://github.com/microsoft/ox-tools/actions/workflows/main.yml/badge.svg?event=push)](https://github.com/microsoft/ox-tools/actions/workflows/main.yml)
[![Coverage](https://codecov.io/gh/microsoft/ox-tools/graph/badge.svg?token=FCUG0EL5TI)](https://codecov.io/gh/microsoft/ox-tools)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)
<a href="../.."><img src="../../logo.svg" alt="This crate was developed as part of the Oxidizer project" width="20"></a>

</div>

## cargo-heather

A `cargo` subcommand to validate license headers in Rust, TOML,
`PowerShell`, Just, and `constants.env` source files. The
`cargo-heather` binary uses the library in this crate to discover
files on disk and apply rewrites; the same library is reusable from
any Rust program.

### Setup

Create a `.cargo-heather.toml` file in your project root, **or**
simply set the `license` field in your `Cargo.toml` — the tool will
use it automatically when no `.cargo-heather.toml` is present.

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

#### Excluding Files and Directories

Use the `exclude` key to skip specific files or directories from
scanning. Entries are **literal paths**. Relative paths are resolved
against the project root (the directory passed to `--project-dir`,
or the current directory by default). Glob patterns and wildcards
are **not** supported.

```toml
exclude = ["vendor", "generated/bindings.rs"]
```

A directory entry excludes its entire subtree recursively. Entries
that do not exist on disk produce a warning and are ignored.

`target/`, `.git/`, `.github/`, `.vscode/`, `.idea/`,
`node_modules/`, and other dot-prefixed directories are already
skipped automatically.

### Usage

```bash
# Check all source files for correct license headers
cargo heather

# Automatically fix files by adding/replacing headers
cargo heather --fix
```

#### Options

* `--project-dir <PATH>` — Path to the project directory (defaults
  to the current directory).
* `--config <PATH>` — Path to the configuration file (defaults to
  `.cargo-heather.toml` in the project directory).
* `--fix` — Fix files by adding or replacing missing/incorrect
  headers.
* `--help` — Print help.
* `--version` — Print version.

#### Example

```text
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

1. **Config loading** — Reads `.cargo-heather.toml` from the
   project root and resolves the expected header text (from SPDX
   identifier or custom text).
1. **File scanning** — Walks the project directory to find all
   supported source files, skipping `target/`, hidden directories,
   and the config file itself.
1. **Header validation** — Extracts the first comment block from
   each file (`//` for Rust, `#` for TOML / `PowerShell` / Just /
   env) and compares it to the expected header. Reports missing or
   mismatched headers.
1. **Fix mode** — When `--fix` is passed, automatically prepends
   the correct header to files that are missing it, or replaces
   incorrect headers.

### Library

The library is intentionally minimal: a pair of stream-based
functions that operate on any [`std::io::Read`][__link0] / [`std::io::Write`][__link1].

* [`check`][__link2] reads content and reports whether the expected header is
  present, missing, or mismatched.
* [`fix`][__link3] reads content and writes the fixed-up content.

Callers are responsible for opening files, deciding which paths to
process, and writing results back to disk.

```rust
use cargo_heather::{CheckResult, FileKind, check, fix};

let input = b"fn main() {}\n";
let header = "Licensed under the MIT License.";

// Check whether the header is present.
let result = check(&input[..], header, FileKind::Rust).unwrap();
assert_eq!(result, CheckResult::Missing);

// Produce a fixed copy.
let mut output: Vec<u8> = Vec::new();
fix(&input[..], &mut output, header, FileKind::Rust).unwrap();
assert!(output.starts_with(b"// Licensed under the MIT License.\n"));
```

#### Supported file kinds

* [`FileKind::Rust`][__link4] — regular Rust source (`//` comments).
* [`FileKind::Toml`][__link5] — TOML files (`#` comments).
* [`FileKind::PowerShell`][__link6] — `PowerShell` scripts (`#` comments).
* [`FileKind::Just`][__link7] — Just recipes (`#` comments).
* [`FileKind::Env`][__link8] — `constants.env` files (`#` comments).
* [`FileKind::CargoScript`][__link9] — Rust script with shebang + `---`
  frontmatter; the header lives inside the frontmatter using `#`.

Use [`FileKind::detect`][__link10] (or [`is_cargo_script`][__link11]) to classify a file
from its path and content before calling [`check`][__link12] / [`fix`][__link13].

#### License header lookup

The [`license`][__link14] module maps SPDX identifiers to canonical short
header strings; this is what the binary uses when no custom header
is supplied.


<hr/>
<sub>
This crate was developed as part of <a href="../..">The Oxidizer Project</a>. Browse this crate's <a href="https://github.com/microsoft/ox-tools/tree/main/crates/cargo-heather">source code</a>.
</sub>

 [__cargo_doc2readme_dependencies_info]: ggGmYW0CYXZlMC43LjJhdIQbFhzZ8rzWNNYbuRaDSGWynFgbH4PMdoT7GNcbVwNPtPjAhvFhYvRhcoQbbnoq_88MiXUbxzT9hRe2HqEbqOK1RXPKs1gblg15FIYbTkZhZIGDbWNhcmdvLWhlYXRoZXJlMC4yLjFtY2FyZ29faGVhdGhlcg
 [__link0]: https://doc.rust-lang.org/stable/std/?search=io::Read
 [__link1]: https://doc.rust-lang.org/stable/std/?search=io::Write
 [__link10]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::detect
 [__link11]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=is_cargo_script
 [__link12]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=check
 [__link13]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=fix
 [__link14]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/license/index.html
 [__link2]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=check
 [__link3]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=fix
 [__link4]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::Rust
 [__link5]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::Toml
 [__link6]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::PowerShell
 [__link7]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::Just
 [__link8]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::Env
 [__link9]: https://docs.rs/cargo-heather/0.2.1/cargo_heather/?search=FileKind::CargoScript
