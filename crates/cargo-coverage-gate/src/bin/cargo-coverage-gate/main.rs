// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-coverage-gate`: gate pull requests on per-crate line coverage.

mod cli;
mod run;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::cli::CargoCli;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let CargoCli::CoverageGate(args) = CargoCli::parse();
    match run::run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            // Every library-side failure is a configuration error from
            // the gate's point of view; map them all to exit 2.
            ExitCode::from(2)
        }
    }
}
