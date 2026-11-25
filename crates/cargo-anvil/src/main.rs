// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! `cargo-anvil`: opinionated, unified Rust build and cloud-workflow scaffolding.

use std::path::PathBuf;
use std::process::ExitCode;

use cargo_anvil::Catalog;
use clap::Parser;

mod release_dependencies;

#[derive(Parser)]
#[command(name = "cargo anvil release-dependencies")]
struct ReleaseDependenciesArgs {
    /// Existing parent directory for release scratch files (defaults to the system temp directory).
    #[arg(long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let args = match args.as_slice() {
        [anvil, rest @ ..] if anvil == "anvil" => rest,
        args => args,
    };
    if matches!(args, [command, ..] if command == "release-dependencies") {
        let args = ReleaseDependenciesArgs::parse_from(args);
        return match release_dependencies::check(args.temp_dir.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("anvil-release-dependencies: {error}");
                ExitCode::FAILURE
            }
        };
    }
    cargo_anvil::run_app(Catalog::anvil())
}
