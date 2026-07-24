// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-each`: run a command over a cargo-style selection of workspace
//! members.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod cli;
mod run;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::CargoCli;

fn main() -> ExitCode {
    let CargoCli::Each(args) = CargoCli::parse();
    match run::run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            // Library-side failures are usage / configuration errors from
            // cargo-each's point of view; map them all to exit 2. A command
            // that ran but failed returns its own code via `Ok`.
            ExitCode::from(2)
        }
    }
}
