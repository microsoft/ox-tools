// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-ox-release`: a deterministic release planner for Oxidizer-style Cargo
//! workspaces.

use std::process::ExitCode;

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    cargo_ox_release::run_main()
}
