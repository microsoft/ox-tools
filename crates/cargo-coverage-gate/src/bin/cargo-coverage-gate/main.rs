// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-coverage-gate`: gate pull requests on per-crate line coverage.

mod cli;
mod collect;
mod run;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{CargoCli, CoverageGateCommand};

fn main() -> ExitCode {
    let CargoCli::CoverageGate(args) = CargoCli::parse();
    let result = match &args.command {
        Some(CoverageGateCommand::Run(collection)) => collect::run(&args, collection),
        None => run::run(&args),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            // Every library-side failure is a configuration error from
            // the gate's point of view; map them all to exit 2.
            ExitCode::from(2)
        }
    }
}
