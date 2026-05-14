// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of the `cargo heather` command.
//!
//! Walks the project, opens each candidate file, and delegates the
//! actual header validation/rewriting to the [`cargo_heather`] library
//! via its public stream API.

use std::path::{Path, PathBuf};

use cargo_heather::{CheckResult, CommentStyle, FileKind, HeatherError};
use ohno::AppError;
use tracing::info;

use crate::cli::HeatherArgs;
use crate::config::{self, HeatherConfig};
use crate::scanner;

pub(crate) fn run(args: &HeatherArgs) -> Result<(), AppError> {
    let project_dir = args.project_dir();
    let config_path = resolve_config_path(args, &project_dir);
    let config = load_config(args, &project_dir)?;
    let files = scanner::find_source_files(&project_dir, Some(&config_path), &config);

    if files.is_empty() {
        info!("No source files found in '{}'.", project_dir.display());
        return Ok(());
    }

    info!("Checking {} file(s)...", files.len());

    if args.fix {
        run_fix(&files, &config, &project_dir)?;
        Ok(())
    } else {
        run_check(&files, &config, &project_dir)
    }
}

fn resolve_config_path(args: &HeatherArgs, project_dir: &Path) -> PathBuf {
    match &args.config {
        Some(path) => path.clone(),
        None => config::config_path_for(project_dir),
    }
}

fn load_config(args: &HeatherArgs, project_dir: &Path) -> Result<HeatherConfig, HeatherError> {
    match &args.config {
        Some(path) => config::load_config_from_path(path),
        None => config::load_config(project_dir),
    }
}

fn run_check(files: &[PathBuf], config: &HeatherConfig, project_dir: &Path) -> Result<(), AppError> {
    let mut failures: usize = 0;
    let mut checked: usize = 0;

    for path in files {
        let Some((kind, content)) = read_and_classify(path, config)? else {
            continue;
        };
        checked += 1;
        let result = cargo_heather::check(content.as_bytes(), &config.header_text, kind).map_err(|e| HeatherError::FileRead {
            path: path.clone(),
            source: e,
        })?;
        let relative = make_relative(path, project_dir);
        match &result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                info!("  MISSING header: {}", relative.display());
                failures += 1;
            }
            CheckResult::Mismatch { expected, actual } => {
                info!("  MISMATCH header: {}", relative.display());
                for line in format_mismatch_details(expected, actual).lines() {
                    info!("{line}");
                }
                failures += 1;
            }
        }
    }

    if failures > 0 {
        ohno::bail!(HeatherError::ValidationFailed(failures));
    }

    info!("All {checked} file(s) have correct license headers.");
    Ok(())
}

fn run_fix(files: &[PathBuf], config: &HeatherConfig, project_dir: &Path) -> Result<usize, AppError> {
    let mut fixed_count: usize = 0;

    for path in files {
        let Some((kind, content)) = read_and_classify(path, config)? else {
            continue;
        };
        let mut output: Vec<u8> = Vec::with_capacity(content.len() + 128);
        let result =
            cargo_heather::fix(content.as_bytes(), &mut output, &config.header_text, kind).map_err(|e| HeatherError::FileRead {
                path: path.clone(),
                source: e,
            })?;
        let relative = make_relative(path, project_dir);
        match &result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                std::fs::write(path, &output).map_err(|e| HeatherError::FileRead {
                    path: path.clone(),
                    source: e,
                })?;
                info!("  Fixed (added header): {}", relative.display());
                fixed_count += 1;
            }
            CheckResult::Mismatch { .. } => {
                std::fs::write(path, &output).map_err(|e| HeatherError::FileRead {
                    path: path.clone(),
                    source: e,
                })?;
                info!("  Fixed (replaced header): {}", relative.display());
                fixed_count += 1;
            }
        }
    }

    match fixed_count {
        0 => info!("All files already have correct headers."),
        n => info!("Fixed {n} file(s)."),
    }

    Ok(fixed_count)
}

/// Read the file and classify its [`FileKind`]. Returns `Ok(None)` when the
/// file should be skipped (cargo-script with `config.scripts = false`).
fn read_and_classify(path: &Path, config: &HeatherConfig) -> Result<Option<(FileKind, String)>, HeatherError> {
    if CommentStyle::from_path(path).is_none() {
        return Err(HeatherError::UnsupportedFileType { path: path.to_path_buf() });
    }
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;
    let kind = FileKind::detect(path, Some(&content)).ok_or_else(|| HeatherError::UnsupportedFileType { path: path.to_path_buf() })?;
    if kind == FileKind::CargoScript && !config.scripts {
        return Ok(None);
    }
    Ok(Some((kind, content)))
}

/// Format a human-readable rendering of a header mismatch, showing both the
/// expected header and what was actually found. Uses `+`/`-` markers so it
/// reads like a unified diff.
fn format_mismatch_details(expected: &str, actual: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("    expected header:\n");
    if expected.is_empty() {
        out.push_str("      + <empty>\n");
    } else {
        for line in expected.lines() {
            writeln!(out, "      + {line}").expect("writing to String never fails");
        }
    }
    out.push_str("    actual header:\n");
    if actual.is_empty() {
        out.push_str("      - <empty>\n");
    } else {
        for line in actual.lines() {
            writeln!(out, "      - {line}").expect("writing to String never fails");
        }
    }
    out
}

fn make_relative(path: &Path, base: &Path) -> PathBuf {
    path.strip_prefix(base).unwrap_or(path).to_path_buf()
}
