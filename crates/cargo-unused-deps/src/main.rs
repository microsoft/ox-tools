// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Cargo subcommand that finds unused and misplaced dependencies.
//!
//! It checks the workspace dependency catalog globally and uses compiler evidence
//! to judge declarations in explicitly selected packages.
//!
//! # Usage
//!
//! After installation, run in any Cargo workspace:
//!
//! ```bash
//! cargo +nightly unused-deps --workspace
//! ```
//!
//! Or point at an explicit workspace root manifest:
//!
//! ```bash
//! cargo unused-deps --manifest-path path/to/Cargo.toml
//! ```
//!
//! With no package selector, only the workspace-global catalog check runs.
//! `--fix` removes catalog entries that no member inherits.

use std::process::ExitCode;

use anyhow::Result;

fn main() -> Result<ExitCode> {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    cargo_unused_deps::dispatch(&std::env::args_os().collect::<Vec<_>>())
}
