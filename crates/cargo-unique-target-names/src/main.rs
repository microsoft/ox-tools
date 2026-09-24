// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

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
//! Exits 0 when every uplifted file has one owner, 1 when any is contended, and
//! 2 when the workspace could not be read.

use std::env;
use std::process::ExitCode;

#[mutants::skip] // Entry point: one-line dispatch to the integration-tested run; nothing to unit-test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    ExitCode::from(cargo_unique_target_names::run(env::args_os()).exit_code())
}
