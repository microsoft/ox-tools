// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-heather
//!
//! A `cargo` subcommand to validate license headers in Rust, TOML,
//! `PowerShell`, Just, and `constants.env` source files. The
//! `cargo-heather` binary uses the library in this crate to discover
//! files on disk and apply rewrites; the same library is reusable from
//! any Rust program.
//!
//! ## Setup
//!
//! Create a `.cargo-heather.toml` file in your project root, **or**
//! simply set the `license` field in your `Cargo.toml` — the tool will
//! use it automatically when no `.cargo-heather.toml` is present.
//!
//! ### Using an SPDX License Identifier
//!
//! ```toml
//! license = "MIT"
//! ```
//!
//! ### Using a Custom Header
//!
//! ```toml
//! header = """
//! Copyright (c) 2024 MyCompany
//! All rights reserved.
//! """
//! ```
//!
//! ### Excluding Files and Directories
//!
//! Use the `exclude` key to skip specific files or directories from
//! scanning. Entries are **literal paths**. Relative paths are resolved
//! against the project root (the directory passed to `--project-dir`,
//! or the current directory by default). Glob patterns and wildcards
//! are **not** supported.
//!
//! ```toml
//! exclude = ["vendor", "generated/bindings.rs"]
//! ```
//!
//! A directory entry excludes its entire subtree recursively. Entries
//! that do not exist on disk produce a warning and are ignored.
//!
//! `target/`, `.git/`, `.github/`, `.vscode/`, `.idea/`,
//! `node_modules/`, and other dot-prefixed directories are already
//! skipped automatically.
//!
//! ## Usage
//!
//! ```bash
//! # Check all source files for correct license headers
//! cargo heather
//!
//! # Automatically fix files by adding/replacing headers
//! cargo heather --fix
//! ```
//!
//! ### Options
//!
//! - `--project-dir <PATH>` — Path to the project directory (defaults
//!   to the current directory).
//! - `--config <PATH>` — Path to the configuration file (defaults to
//!   `.cargo-heather.toml` in the project directory).
//! - `--fix` — Fix files by adding or replacing missing/incorrect
//!   headers.
//! - `--help` — Print help.
//! - `--version` — Print version.
//!
//! ### Example
//!
//! ```text
//! $ cargo heather
//! Checking 5 file(s)...
//! MISSING header: src/utils.rs
//! MISMATCH header: src/lib.rs
//! 2 file(s) have missing or incorrect license headers
//!
//! $ cargo heather --fix
//! Checking 5 file(s)...
//! Fixed (added header): src/utils.rs
//! Fixed (replaced header): src/lib.rs
//! Fixed 2 file(s).
//! ```
//!
//! ## Supported SPDX Identifiers
//!
//! | Identifier         | License                                              |
//! | ------------------ | ---------------------------------------------------- |
//! | `MIT`              | MIT License                                          |
//! | `Apache-2.0`       | Apache License 2.0                                   |
//! | `GPL-2.0-only`     | GNU General Public License v2.0 only                 |
//! | `GPL-2.0-or-later` | GNU General Public License v2.0 or later             |
//! | `GPL-3.0-only`     | GNU General Public License v3.0 only                 |
//! | `GPL-3.0-or-later` | GNU General Public License v3.0 or later             |
//! | `LGPL-2.1-only`    | GNU Lesser General Public License v2.1 only          |
//! | `LGPL-2.1-or-later`| GNU Lesser General Public License v2.1 or later      |
//! | `LGPL-3.0-only`    | GNU Lesser General Public License v3.0 only          |
//! | `LGPL-3.0-or-later`| GNU Lesser General Public License v3.0 or later      |
//! | `BSD-2-Clause`     | BSD 2-Clause "Simplified" License                    |
//! | `BSD-3-Clause`     | BSD 3-Clause "New" or "Revised" License              |
//! | `ISC`              | ISC License                                          |
//! | `MPL-2.0`          | Mozilla Public License 2.0                           |
//! | `AGPL-3.0-only`    | GNU Affero General Public License v3.0 only          |
//! | `AGPL-3.0-or-later`| GNU Affero General Public License v3.0 or later      |
//! | `Unlicense`        | The Unlicense                                        |
//! | `BSL-1.0`          | Boost Software License 1.0                           |
//! | `0BSD`             | BSD Zero Clause License                              |
//! | `Zlib`             | zlib License                                         |
//!
//! ## How it works
//!
//! 1. **Config loading** — Reads `.cargo-heather.toml` from the
//!    project root and resolves the expected header text (from SPDX
//!    identifier or custom text).
//! 2. **File scanning** — Walks the project directory to find all
//!    supported source files, skipping `target/`, hidden directories,
//!    and the config file itself.
//! 3. **Header validation** — Extracts the first comment block from
//!    each file (`//` for Rust, `#` for TOML / `PowerShell` / Just /
//!    env) and compares it to the expected header. Reports missing or
//!    mismatched headers.
//! 4. **Fix mode** — When `--fix` is passed, automatically prepends
//!    the correct header to files that are missing it, or replaces
//!    incorrect headers.
//!
//! ## Library
//!
//! The library is intentionally minimal: a pair of stream-based
//! functions that operate on any [`std::io::Read`] / [`std::io::Write`].
//!
//! - [`check`] reads content and reports whether the expected header is
//!   present, missing, or mismatched.
//! - [`fix`] reads content and writes the fixed-up content.
//!
//! Callers are responsible for opening files, deciding which paths to
//! process, and writing results back to disk.
//!
//! ```no_run
//! use cargo_heather::{CheckResult, FileKind, check, fix};
//!
//! let input = b"fn main() {}\n";
//! let header = "Licensed under the MIT License.";
//!
//! // Check whether the header is present.
//! let result = check(&input[..], header, FileKind::Rust).unwrap();
//! assert_eq!(result, CheckResult::Missing);
//!
//! // Produce a fixed copy.
//! let mut output: Vec<u8> = Vec::new();
//! fix(&input[..], &mut output, header, FileKind::Rust).unwrap();
//! assert!(output.starts_with(b"// Licensed under the MIT License.\n"));
//! ```
//!
//! ### Supported file kinds
//!
//! - [`FileKind::Rust`] — regular Rust source (`//` comments).
//! - [`FileKind::Toml`] — TOML files (`#` comments).
//! - [`FileKind::PowerShell`] — `PowerShell` scripts (`.ps1`), data
//!   files (`.psd1`), and module files (`.psm1`) (all use `#` comments).
//! - [`FileKind::Just`] — Just recipes (`#` comments).
//! - [`FileKind::Env`] — `constants.env` files (`#` comments).
//! - [`FileKind::CargoScript`] — Rust script with shebang + `---`
//!   frontmatter; the header lives inside the frontmatter using `#`.
//!
//! Use [`FileKind::detect`] (or [`is_cargo_script`]) to classify a file
//! from its path and content before calling [`check`] / [`fix`].
//!
//! ### License header lookup
//!
//! The [`license`] module maps SPDX identifiers to canonical short
//! header strings; this is what the binary uses when no custom header
//! is supplied.

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-heather/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-heather/favicon.ico")]
#![deny(unsafe_code)]

mod checker;
mod comment;
mod error;
mod process;

pub mod license;

pub use checker::CheckResult;
pub use comment::{CommentStyle, FileKind, is_cargo_script};
pub use error::HeatherError;
pub use process::{check, fix};
