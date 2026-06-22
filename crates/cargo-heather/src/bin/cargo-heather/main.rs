// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-heather`: Validate license headers in source files.

mod cli;
mod config;
mod run;
mod scanner;

use clap::Parser;
use ohno::AppError;

use crate::cli::CargoCli;

fn main() -> Result<(), AppError> {
    let CargoCli::Heather(args) = CargoCli::parse();
    run::run(&args)
}
