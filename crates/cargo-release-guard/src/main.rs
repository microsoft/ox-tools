// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A publication-oriented dependency rehearsal for Rust workspaces.

use std::process::ExitCode;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    cargo_release_guard::run()
}
