// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo coverage-gate` command.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write as _};
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
    finish_evaluation(args, &report, write_text_output, write_summary_file)
}

fn finish_evaluation(
    args: &CoverageGateArgs,
    report: &EvaluatedReport,
    write_text: impl FnOnce(&EvaluatedReport, bool) -> Result<(), AppError>,
    write_summary_file: impl FnOnce(&EvaluatedReport, &Path) -> io::Result<()>,
) -> Result<ExitCode, AppError> {
    // Text output is authoritative and stops summary emission on failure. Summary rendering
    // likewise preserves its original error, while successful buffered output is explicitly
    // flushed so delayed writer failures are never reported as success.
    write_text(report, args.quiet)?;

    if let Some(path) = summary_target(args) {
        write_summary_file(report, &path).into_app_err(format!("failed to write summary file `{}`", path.display()))?;
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
    write_summary(report, file)
}

fn write_summary(report: &EvaluatedReport, out: impl io::Write) -> io::Result<()> {
    write_summary_with(out, |writer| report.render_markdown(writer))
}

fn write_summary_with(out: impl io::Write, render: impl FnOnce(&mut dyn io::Write) -> io::Result<()>) -> io::Result<()> {
    let mut writer = BufWriter::new(out);
    render(&mut writer)?;
    writer.flush()
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
    use std::cell::Cell;
    use std::rc::Rc;

    use cargo_coverage_gate::evaluate_many;
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

    fn zero_threshold_report() -> (tempfile::TempDir, cargo_coverage_gate::EvaluatedReport) {
        let tmp = tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("alpha/src")).expect("create member");
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\"]\n",
        )
        .expect("write workspace manifest");
        fs::write(
            tmp.path().join("alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [package.metadata.coverage-gate]\nmin-lines-percent = 0\n",
        )
        .expect("write member manifest");
        fs::write(tmp.path().join("alpha/src/lib.rs"), "").expect("write member source");
        let report = evaluate_many(&[], Some(&tmp.path().join("Cargo.toml")), &[]).expect("zero-threshold report");

        (tmp, report)
    }

    /// Injects rendering failures while allowing flushes to succeed independently.
    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected summary failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FlushObserver(Rc<Cell<bool>>);

    impl io::Write for FlushObserver {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.set(true);
            Ok(())
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

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata")]
    fn summary_creation_errors_are_returned() {
        let (tmp, report) = zero_threshold_report();
        let missing_parent = tmp.path().join("missing").join("summary.md");
        let error = write_summary_file(&report, &missing_parent).expect_err("missing parent must reject summary creation");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[cfg_attr(miri, ignore = "uses a temporary directory; miri isolation forbids filesystem access")]
    #[test]
    fn no_gate_summary_write_errors_are_returned() {
        let tmp = tempdir().expect("tempdir");
        let mut args = args();
        args.summary_file = Some(tmp.path().join("missing").join("summary.md"));

        let error = write_no_gate_summary(&args, "skipped").expect_err("missing parent must reject summary creation");
        assert!(error.to_string().contains("failed to write summary file"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata")]
    fn summary_rendering_errors_are_returned() {
        let (_tmp, report) = zero_threshold_report();

        let error = write_summary(&report, FailingWriter).expect_err("summary write must fail");
        assert_eq!(error.to_string(), "injected summary failure");
    }

    #[test]
    fn summary_returns_render_errors_before_flushing() {
        let flushed = Rc::new(Cell::new(false));
        let error = write_summary_with(FlushObserver(Rc::clone(&flushed)), |writer| {
            writer.write_all(b"partial summary")?;
            Err(io::Error::other("injected render failure"))
        })
        .expect_err("render failure must propagate");
        assert_eq!(error.to_string(), "injected render failure");
        assert!(!flushed.get(), "render failure must skip the explicit flush");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata")]
    fn evaluation_output_errors_are_returned_from_the_call_site() {
        let (_tmp, report) = zero_threshold_report();

        let text_error = finish_evaluation(
            &args(),
            &report,
            |_, _| Err(AppError::new("injected text failure")),
            |_, _| panic!("summary must not be attempted after text failure"),
        )
        .expect_err("text failure must be returned");
        assert_eq!(text_error.to_string(), "injected text failure");

        let mut summary_args = args();
        summary_args.summary_file = Some(PathBuf::from("summary.md"));
        let summary_error = finish_evaluation(
            &summary_args,
            &report,
            |_, _| Ok(()),
            |_, _| Err(io::Error::other("injected summary failure")),
        )
        .expect_err("summary failure must be returned");
        assert!(summary_error.to_string().contains("injected summary failure"));
    }
}
