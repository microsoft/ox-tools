// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! Anvil Release Kit (ARK) provides tools for Rust release workflows through `cargo ark`.
//!
//! # Release dependency validation
//!
//! `cargo ark release-dependencies` checks whether the selected packages build and
//! pass their shipped tests from their `.crate` archives. Run it from the workspace
//! and pass the candidates as a JSON array on standard input:
//!
//! ```sh
//! echo '[{"name":"my-crate","new_version":"1.2.0"}]' | cargo ark release-dependencies
//! ```
//!
//! The command runs each package's tests with all features and with no default features.
//! It does not discover candidates or publish packages.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod release_dependencies;

#[derive(Parser)]
#[command(name = "cargo ark", version, about)]
struct Cli {
    #[command(subcommand)]
    command: ArkCommand,
}

#[derive(Subcommand)]
enum ArkCommand {
    /// Validate release candidates by testing their packaged archives.
    ReleaseDependencies {
        /// Existing parent directory for release scratch files (defaults to the system temp directory).
        #[arg(long, value_name = "DIR")]
        temp_dir: Option<PathBuf>,
    },
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "ark") {
        // #[gamma::skip(literal.int_decrement, reason = "Clap discards argv[0] and has an explicit command name; leaving ark rather than the binary path in argv[0] is equivalent.")]
        args.remove(1);
    }
    let Cli {
        command: ArkCommand::ReleaseDependencies { temp_dir },
    } = Cli::parse_from(args);
    match release_dependencies::check(temp_dir.as_deref()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo ark release-dependencies: {error}");
            ExitCode::FAILURE
        }
    }
}
