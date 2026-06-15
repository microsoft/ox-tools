// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-anvil`: opinionated, unified Rust build and cloud-workflow scaffolding.

use std::process::ExitCode;

use cargo_anvil::cli::Cli;
use tracing_subscriber::fmt::format::FmtSpan;

#[mutants::skip] // Entry point: tracing/clap setup + dispatch to lib::run; behavior is integration-tested.
fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_level(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .init();

    let cli = match Cli::parse_from_cargo_args(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => {
            // clap formats and prints the help/error itself.
            err.exit();
        }
    };

    match cargo_anvil::run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
