// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo sub-command that ensures dependencies are declared with `default-features = false`.
//!
//! # Usage
//!
//! After installation, run in any cargo workspace or crate directory:
//!
//! ```bash
//! cargo ensure-no-default-features
//! ```
//!
//! Or specify a manifest path:
//!
//! ```bash
//! cargo ensure-no-default-features --manifest-path path/to/Cargo.toml
//! ```
//!
//! The tool will exit with code 0 if all dependencies are declared with
//! `default-features = false`, or code 1 otherwise.

use std::process::ExitCode;

use anyhow::Result;

fn main() -> Result<ExitCode> {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    cargo_ensure_no_default_features::run()
}
