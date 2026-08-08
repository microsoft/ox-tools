// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A cargo tool to appraise the quality of Rust dependencies.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::io::{Write, stderr, stdout};

use cargo_aprz_lib::{Host, run};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Default host that runs real OS commands.
#[derive(Debug, Clone, Default)]
pub struct RealHost;

#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Process-boundary glue (stdout/stderr handles, `std::process::exit`); nothing to assert on.
impl Host for RealHost {
    fn output(&mut self) -> impl Write {
        stdout()
    }

    fn error(&mut self) -> impl Write {
        stderr()
    }

    fn exit(&mut self, code: i32) {
        std::process::exit(code);
    }
}

#[tokio::main]
#[cfg_attr(coverage_nightly, coverage(off))]
#[mutants::skip] // Entry point: thin wrapper, tested via integration tests on `run()`.
async fn main() {
    run(&mut RealHost, std::env::args()).await;
}
