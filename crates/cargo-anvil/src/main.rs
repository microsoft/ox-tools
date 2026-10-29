// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-anvil`: opinionated, unified Rust build and cloud-workflow scaffolding.

use std::process::ExitCode;

use cargo_anvil::Catalog;

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    cargo_anvil::run_app(Catalog::anvil())
}
