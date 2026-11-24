// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Entry point for the `cargo ensure-no-test-features` subcommand.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::process::ExitCode;

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    cargo_ensure_no_test_features::run()
}
