// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cargo_coverage_gate::{EvaluatedReport, evaluate_many_for_target, evaluate_many_for_target_with_tools};
use ohno::{AppError, IntoAppError};

use crate::cli::CoverageGateArgs;

pub(crate) fn run(args: &CoverageGateArgs) -> Result<ExitCode, AppError> {
    let lcov_paths: Vec<PathBuf> = if args.lcov.is_empty() {
        vec![PathBuf::from("target/coverage/lcov.info")]
    } else {
        args.lcov.clone()
    };
    evaluate_paths(args, &lcov_paths, &args.packages)
}

pub(crate) fn evaluate_paths(args: &CoverageGateArgs, lcov_paths: &[PathBuf], gated_packages: &[String]) -> Result<ExitCode, AppError> {
    evaluate_paths_with_toolchain(args, lcov_paths, gated_packages, None, None, None)
}

pub(crate) fn evaluate_paths_with_toolchain(
    args: &CoverageGateArgs,
    lcov_paths: &[PathBuf],
    gated_packages: &[String],
    cargo: Option<&OsStr>,
    rustc: Option<&OsStr>,
    rustup_toolchain: Option<&OsStr>,
) -> Result<ExitCode, AppError> {
    let mut lcov_texts: Vec<String> = Vec::with_capacity(lcov_paths.len());
    for path in lcov_paths {
        let text = fs::read_to_string(path).into_app_err(format!("failed to read lcov tracefile `{}`", path.display()))?;
        lcov_texts.push(text);
    }
    let lcov_refs: Vec<&str> = lcov_texts.iter().map(String::as_str).collect();

    let explicit_tools = explicit_tools(cargo, rustc)?;
    let report = if let Some((cargo, rustc)) = explicit_tools {
        evaluate_many_for_target_with_tools(
            &lcov_refs,
            None,
            gated_packages,
            args.target.as_deref(),
            Some(cargo),
            Some(rustc),
            rustup_toolchain,
        )
    } else {
        evaluate_many_for_target(&lcov_refs, None, gated_packages, args.target.as_deref())
    }
    .into_app_err("failed to evaluate coverage")?;

    write_text_output(&report, args.quiet).into_app_err("failed to write verdict to stdout")?;

    if let Some(path) = summary_target(args) {
        write_summary_file(&report, &path).into_app_err(format!("failed to write summary file `{}`", path.display()))?;
    }

    let code = u8::try_from(report.verdict().as_exit_code()).expect("Verdict::as_exit_code only ever produces values in 0..=2");
    Ok(ExitCode::from(code))
}

fn explicit_tools<'a>(cargo: Option<&'a OsStr>, rustc: Option<&'a OsStr>) -> Result<Option<(&'a OsStr, &'a OsStr)>, AppError> {
    match (cargo, rustc) {
        (Some(cargo), Some(rustc)) => Ok(Some((cargo, rustc))),
        (None, None) => Ok(None),
        _ => Err(AppError::new("Cargo and rustc tool overrides must be supplied together")),
    }
}

pub(crate) fn write_no_gate_summary(args: &CoverageGateArgs, result: &str) -> Result<(), AppError> {
    if let Some(path) = summary_target(args) {
        fs::write(&path, format!("### coverage-gate\n\n**Result:** {result}.\n"))
            .into_app_err(format!("failed to write summary file `{}`", path.display()))?;
    }
    Ok(())
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
/// `GITHUB_STEP_SUMMARY` environment variable, then the
/// `COVERAGE_GATE_SUMMARY` environment variable.
fn summary_target(args: &CoverageGateArgs) -> Option<PathBuf> {
    if let Some(p) = &args.summary_file {
        return Some(p.clone());
    }

    for var in ["GITHUB_STEP_SUMMARY", "COVERAGE_GATE_SUMMARY"] {
        if let Some(v) = env::var_os(var)
            && !v.is_empty()
        {
            return Some(PathBuf::from(v));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::explicit_tools;

    #[test]
    fn explicit_tool_overrides_are_all_or_nothing() {
        let cargo = OsStr::new("cargo");
        let rustc = OsStr::new("rustc");
        assert_eq!(explicit_tools(Some(cargo), Some(rustc)).expect("both tools"), Some((cargo, rustc)));
        assert_eq!(explicit_tools(None, None).expect("ambient tools"), None);
        explicit_tools(Some(cargo), None).expect_err("cargo-only override must fail");
        explicit_tools(None, Some(rustc)).expect_err("rustc-only override must fail");
    }
}
