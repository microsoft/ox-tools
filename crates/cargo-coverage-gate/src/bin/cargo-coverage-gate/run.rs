// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cargo_coverage_gate::EvaluatedReport;
use ohno::{AppError, IntoAppError};

use crate::cli::CoverageGateArgs;

pub(crate) fn run(args: &CoverageGateArgs) -> Result<ExitCode, AppError> {
    let json_path = args.json.clone().unwrap_or_else(|| PathBuf::from("target/coverage/coverage.json"));
    let json_text = fs::read_to_string(&json_path).into_app_err(format!("failed to read coverage JSON `{}`", json_path.display()))?;

    let report = cargo_coverage_gate::evaluate(&json_text, None, &args.crates).map_err(|e| AppError::new(e.to_string()))?;

    write_text_output(&report, args.quiet).into_app_err("failed to write verdict to stdout")?;

    if let Some(path) = summary_target(args) {
        write_summary_file(&report, &path).into_app_err(format!("failed to write summary file `{}`", path.display()))?;
    }

    let code = u8::try_from(report.verdict().exit_code()).expect("Verdict::exit_code only ever produces values in 0..=2");
    Ok(ExitCode::from(code))
}

fn write_text_output(report: &EvaluatedReport, quiet: bool) -> io::Result<()> {
    if quiet {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    report.render_text(&mut handle)
}

fn write_summary_file(report: &EvaluatedReport, path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    report.render_markdown(&mut writer)?;
    Ok(())
}

/// Resolve where the Markdown summary should be written, if anywhere.
///
/// Priority order (first match wins): `--summary-file`, then the
/// `GITHUB_STEP_SUMMARY` env var, then the `COVERAGE_GATE_SUMMARY`
/// env var.
fn summary_target(args: &CoverageGateArgs) -> Option<PathBuf> {
    if let Some(p) = &args.summary_file {
        return Some(p.clone());
    }
    for var in ["GITHUB_STEP_SUMMARY", "COVERAGE_GATE_SUMMARY"] {
        if let Ok(v) = env::var(var)
            && !v.is_empty()
        {
            return Some(PathBuf::from(v));
        }
    }
    None
}
