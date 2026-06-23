// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-anvil`: opinionated, unified Rust build and cloud-workflow scaffolding.

use std::process::ExitCode;

use cargo_anvil::Catalog;

#[mutants::skip] // Entry point: one-line dispatch to the integration-tested run_app; nothing to unit-test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    cargo_anvil::run_app(Catalog::anvil())
}
