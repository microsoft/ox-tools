// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-coverage-gate`: gate pull requests on per-crate line coverage.

mod cli;
mod run;

use clap::Parser;
use ohno::AppError;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::cli::CargoCli;

fn main() -> Result<(), AppError> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let CargoCli::CoverageGate(args) = CargoCli::parse();
    run::run(&args)
}
