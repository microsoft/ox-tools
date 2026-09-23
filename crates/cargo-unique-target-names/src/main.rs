// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that fails when two workspace packages own targets which
//! uplift to the same build artifact.
//!
//! # Usage
//!
//! After installation, run in any cargo workspace or crate directory:
//!
//! ```bash
//! cargo unique-target-names
//! ```
//!
//! Or specify a manifest path:
//!
//! ```bash
//! cargo unique-target-names --manifest-path path/to/Cargo.toml
//! ```
//!
//! The tool exits with code 0 when every uplifted file has one owner, or code 1
//! otherwise.

use std::process::ExitCode;

use anyhow::Result;

fn main() -> Result<ExitCode> {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    cargo_unique_target_names::run()
}
