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
//! Exits 0 when every uplifted file has one owner, 1 when any is contended, 2
//! when the workspace could not be read, and 3 when the command line was
//! rejected.

#[cfg(not(test))]
use std::env::args_os as arguments;
#[cfg(test)]
use std::ffi::OsString;
use std::process::ExitCode;

#[cfg(not(test))]
use cargo_unique_target_names::run as run_command;

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    // TODO: This could be a main.rs only crate, but CI complains when processing bin-only crates:
    //  https://github.com/rust-lang/cargo/issues/15231.
    ExitCode::from(run_command(arguments()).exit_code())
}

#[cfg(test)]
fn arguments() -> impl Iterator<Item = OsString> {
    ["cargo", "unique-target-names", "--manifest-path"].into_iter().map(OsString::from)
}

#[cfg(test)]
fn run_command(args: impl Iterator<Item = OsString>) -> cargo_unique_target_names::Outcome {
    assert_eq!(
        args.collect::<Vec<_>>(),
        ["cargo", "unique-target-names", "--manifest-path"].map(OsString::from)
    );
    cargo_unique_target_names::Outcome::Usage("scripted missing manifest path".to_owned())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn campaign_regression_main_preserves_a_rejected_invocations_nonzero_exit_code() {
        assert_eq!(main(), ExitCode::from(3));
    }
}
