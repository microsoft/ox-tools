// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-anvil`: opinionated, unified Rust build and cloud-workflow scaffolding.

use std::process::ExitCode;

use cargo_anvil::Catalog;

fn main() -> ExitCode {
    cargo_anvil::run_app(Catalog::anvil())
}
