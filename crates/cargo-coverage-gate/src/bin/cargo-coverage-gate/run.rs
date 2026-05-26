// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use std::fs;
use std::path::PathBuf;
use std::process;

use cargo_coverage_gate::Verdict;
use ohno::{AppError, IntoAppError};
use tracing::info;

use crate::cli::CoverageGateArgs;

pub(crate) fn run(args: &CoverageGateArgs) -> Result<(), AppError> {
    let json_path = args
        .json
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/coverage/coverage.json"));
    let json_text = fs::read_to_string(&json_path)
        .into_app_err(format!("failed to read coverage JSON `{}`", json_path.display()))?;

    let verdict = cargo_coverage_gate::run(&json_text, None, &args.crates)
        .map_err(|e| AppError::new(e.to_string()))?;
    report_verdict(verdict, args.quiet);
    process::exit(verdict.exit_code());
}

fn report_verdict(verdict: Verdict, quiet: bool) {
    if quiet {
        return;
    }
    let label = match verdict {
        Verdict::Pass => "PASS",
        Verdict::Fail => "FAIL",
        Verdict::ConfigError => "CONFIG ERROR",
    };
    info!("coverage-gate verdict: {label}");
}

