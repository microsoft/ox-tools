// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Entry point for the `cargo ensure-no-test-features` subcommand.

use std::process::ExitCode;

fn main() -> ExitCode {
    cargo_ensure_no_test_features::run()
}
