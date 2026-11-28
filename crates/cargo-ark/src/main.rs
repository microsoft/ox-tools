// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![doc = include_str!("../docs/cli.md")]

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use cargo_ark::release_dependencies::{Candidates, CargoInvoker, check};
use clap::{Parser, Subcommand};

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
    let result = check_release_dependencies(io::stdin(), |candidates| {
        let cargo = CargoInvoker::discover(std::env::current_dir()?, BTreeMap::new(), temp_dir.as_deref())?;
        check(candidates, cargo)
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo ark release-dependencies: {error}");
            ExitCode::FAILURE
        }
    }
}

fn check_release_dependencies(
    mut input: impl Read,
    validate: impl FnOnce(Candidates) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut report = String::new();
    input.read_to_string(&mut report)?;
    let candidates = Candidates::from_report(&report)?;
    if candidates.is_empty() {
        println!("cargo ark release-dependencies: no release candidates");
        return Ok(());
    }

    validate(candidates)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn accepts_an_empty_report_without_discovering_a_workspace() {
        check_release_dependencies(Cursor::new("[]"), |_| panic!("an empty report attempted workspace discovery")).unwrap();
    }

    #[test]
    fn rejects_non_utf8_report_input() {
        let error =
            check_release_dependencies(Cursor::new([0xff]), |_| panic!("an invalid report attempted workspace discovery")).unwrap_err();
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_an_invalid_report_without_discovering_a_workspace() {
        let error = check_release_dependencies(Cursor::new(r#"[{"name":"peer"}]"#), |_| {
            panic!("an invalid report attempted workspace discovery")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "candidate is missing its version");
    }

    #[test]
    fn passes_a_nonempty_report_to_validation_and_propagates_its_error() {
        let report = Cursor::new(r#"[{"name":"peer","new_version":"1.0.0"}]"#);
        let error = check_release_dependencies(report, |candidates| {
            assert!(!candidates.is_empty());
            Err("scripted validation failure".into())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "scripted validation failure");
    }
}
