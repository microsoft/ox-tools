// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Cargo subcommand that reports uninherited workspace dependencies.
//!
//! Every `[workspace.dependencies]` entry is expected to be inherited by at least
//! one workspace member.
//!
//! # Usage
//!
//! After installation, run in any Cargo workspace:
//!
//! ```bash
//! cargo unused-deps
//! ```
//!
//! Or point at an explicit workspace root manifest:
//!
//! ```bash
//! cargo unused-deps --manifest-path path/to/Cargo.toml
//! ```
//!
//! The tool exits with code 0 when every catalog entry is inherited by a member,
//! and code 1 otherwise. `--fix` removes the entries that are not.

use std::process::ExitCode;

use anyhow::Result;

fn main() -> Result<ExitCode> {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    cargo_unused_deps::run()
}
