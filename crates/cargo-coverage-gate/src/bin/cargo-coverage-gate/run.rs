// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cargo_coverage_gate::{EvaluatedReport, evaluate_many_for_target};
use ohno::{AppError, IntoAppError};

use crate::cli::CoverageGateArgs;

pub(crate) fn run(args: &CoverageGateArgs) -> Result<ExitCode, AppError> {
    let lcov_paths: Vec<PathBuf> = if args.lcov.is_empty() {
        vec![PathBuf::from("target/coverage/lcov.info")]
    } else {
        args.lcov.clone()
    };
    evaluate_paths(args, &lcov_paths, &args.packages, args.target.as_deref())
}

pub(crate) fn evaluate_paths(
    args: &CoverageGateArgs,
    lcov_paths: &[PathBuf],
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<ExitCode, AppError> {
    let mut lcov_texts: Vec<String> = Vec::with_capacity(lcov_paths.len());
    for path in lcov_paths {
        let text = fs::read_to_string(path).into_app_err(format!("failed to read lcov tracefile `{}`", path.display()))?;
        lcov_texts.push(text);
    }
    let lcov_refs: Vec<&str> = lcov_texts.iter().map(String::as_str).collect();

    let report = evaluate_many_for_target(&lcov_refs, None, gated_packages, target).into_app_err("failed to evaluate coverage")?;

    write_text_output(&report, args.quiet)?;

    if let Some(path) = summary_target(args) {
        write_summary_file(&report, &path).into_app_err(format!("failed to write summary file `{}`", path.display()))?;
    }

    let code = u8::try_from(report.verdict().as_exit_code()).expect("Verdict::as_exit_code only ever produces values in 0..=2");
    Ok(ExitCode::from(code))
}

pub(crate) fn write_no_gate_summary(args: &CoverageGateArgs, result: &str) -> Result<(), AppError> {
    if let Some(path) = summary_target(args) {
        fs::write(&path, format!("### coverage-gate\n\n**Result:** {result}.\n"))
            .into_app_err(format!("failed to write summary file `{}`", path.display()))?;
    }
    Ok(())
}

fn write_text_output(report: &EvaluatedReport, quiet: bool) -> Result<(), AppError> {
    if quiet {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    write_text_output_to(report, &mut handle)
}

fn write_text_output_to(report: &EvaluatedReport, out: &mut dyn io::Write) -> Result<(), AppError> {
    verdict_write_result(report.render_text(out))
}

fn verdict_write_result(result: io::Result<()>) -> Result<(), AppError> {
    result.into_app_err("failed to write verdict to stdout")
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
    summary_target_with(args, |name| env::var_os(name))
}

fn summary_target_with(args: &CoverageGateArgs, mut var_os: impl FnMut(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(p) = &args.summary_file {
        return Some(p.clone());
    }

    for var in ["GITHUB_STEP_SUMMARY", "COVERAGE_GATE_SUMMARY"] {
        if let Some(v) = var_os(var)
            && !v.is_empty()
        {
            return Some(PathBuf::from(v));
        }
    }
    None
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::cli::CoverageGateCommand;

    fn args() -> CoverageGateArgs {
        CoverageGateArgs {
            lcov: Vec::new(),
            packages: Vec::new(),
            target: None,
            summary_file: None,
            quiet: false,
            command: None::<CoverageGateCommand>,
        }
    }

    #[test]
    fn summary_target_queries_documented_environment_variables_in_order() {
        let mut queried = Vec::new();
        let target = summary_target_with(&args(), |name| {
            queried.push(name.to_owned());
            (name == "COVERAGE_GATE_SUMMARY").then(|| "coverage.md".into())
        });
        assert_eq!(queried, ["GITHUB_STEP_SUMMARY", "COVERAGE_GATE_SUMMARY"]);
        assert_eq!(target, Some(PathBuf::from("coverage.md")));
    }

    #[test]
    fn verdict_write_error_has_exact_context() {
        let error = verdict_write_result(Err(io::Error::other("injected"))).expect_err("write must fail");
        assert!(error.to_string().contains("failed to write verdict to stdout"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses a temporary LCOV file; miri isolation forbids filesystem access")]
    fn evaluation_error_has_exact_context() {
        let tmp = tempdir().expect("tempdir");
        let lcov = tmp.path().join("malformed.info");
        fs::write(&lcov, "not lcov").expect("write malformed lcov");
        let error = evaluate_paths(&args(), &[lcov], &[], None).expect_err("malformed lcov must fail");
        assert!(error.to_string().contains("failed to evaluate coverage"));
    }
}
