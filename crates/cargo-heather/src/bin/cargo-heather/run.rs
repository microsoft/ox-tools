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

use crate::cli::HeatherArgs;
use crate::config::{self, HeatherConfig};
use crate::scanner;

pub(crate) fn run(args: &HeatherArgs) -> Result<(), AppError> {
    let project_dir = args.project_dir();
    let config_path = resolve_config_path(args, &project_dir);
    let config = load_config(args, &project_dir)?;
    let files = scanner::find_source_files(&project_dir, Some(&config_path), &config);

    if files.is_empty() {
        println!("No source files found in '{}'.", project_dir.display());
        return Ok(());
    }

    println!("Checking {} file(s)...", files.len());

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
        let result = cargo_heather::check(content.as_bytes(), &config.header_text, kind)
            .expect("`content` is a String (valid UTF-8) read into memory by read_and_classify, so check's only fallible paths -- reader IO and UTF-8 decoding -- cannot fail on this in-memory slice");
        let relative = make_relative(path, project_dir);
        match &result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                println!("  MISSING header: {}", relative.display());
                failures += 1;
            }
            CheckResult::Mismatch { expected, actual } => {
                println!("  MISMATCH header: {}", relative.display());
                for line in format_mismatch_details(expected, actual).lines() {
                    println!("{line}");
                }
                failures += 1;
            }
        }
    }

    if failures > 0 {
        ohno::bail!(HeatherError::ValidationFailed(failures));
    }

    println!("All {checked} file(s) have correct license headers.");
    Ok(())
}

fn run_fix(files: &[PathBuf], config: &HeatherConfig, project_dir: &Path) -> Result<usize, AppError> {
    let mut fixed_count: usize = 0;

    for path in files {
        let Some((kind, content)) = read_and_classify(path, config)? else {
            continue;
        };
        let mut output: Vec<u8> = Vec::with_capacity(content.len() + 128);
        let result = cargo_heather::fix(content.as_bytes(), &mut output, &config.header_text, kind)
            .expect("`content` is a String (valid UTF-8) read into memory and `output` is a Vec, so fix's fallible paths -- reader IO/UTF-8 decoding and writer IO -- cannot fail here");
        let relative = make_relative(path, project_dir);
        match &result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                write_fixed(path, &output)?;
                println!("  Fixed (added header): {}", relative.display());
                fixed_count += 1;
            }
            CheckResult::Mismatch { .. } => {
                write_fixed(path, &output)?;
                println!("  Fixed (replaced header): {}", relative.display());
                fixed_count += 1;
            }
        }
    }

    match fixed_count {
        0 => println!("All files already have correct headers."),
        n => println!("Fixed {n} file(s)."),
    }

    Ok(fixed_count)
}

/// Persist the rewritten file contents to disk.
///
/// This is the single filesystem-write edge of the `--fix` path,
/// extracted from [`run_fix`] so the two fix arms share one write site.
fn write_fixed(path: &Path, output: &[u8]) -> Result<(), AppError> {
    std::fs::write(path, output).map_err(|e| HeatherError::FileWrite {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn config() -> HeatherConfig {
        HeatherConfig {
            header_text: "// Copyright".into(),
            scripts: true,
            dot_toml: false,
            exclude: Vec::new(),
        }
    }

    #[test]
    fn format_mismatch_details_handles_empty_expected_and_actual() {
        let s = format_mismatch_details("", "");
        assert!(s.contains("+ <empty>"), "{s}");
        assert!(s.contains("- <empty>"), "{s}");
    }

    #[test]
    fn format_mismatch_details_lists_nonempty_lines_with_markers() {
        let s = format_mismatch_details("E1\nE2", "A1");
        assert!(s.contains("+ E1") && s.contains("+ E2"), "{s}");
        assert!(s.contains("- A1"), "{s}");
    }

    #[test]
    fn read_and_classify_rejects_unsupported_file_type() {
        let err = read_and_classify(Path::new("weird.unknownext"), &config()).unwrap_err();
        assert!(matches!(err, HeatherError::UnsupportedFileType { .. }), "{err}");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn read_and_classify_returns_kind_and_content_for_supported_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a.rs");
        std::fs::write(&p, "// x\nfn main() {}\n").unwrap();
        let got = read_and_classify(&p, &config()).unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn make_relative_strips_base_or_falls_back() {
        assert_eq!(make_relative(Path::new("/a/b/c.rs"), Path::new("/a")), PathBuf::from("b/c.rs"));
        // Unrelated base: path returned unchanged.
        assert_eq!(make_relative(Path::new("/x/c.rs"), Path::new("/a")), PathBuf::from("/x/c.rs"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn read_and_classify_propagates_read_error() {
        // A directory named like a Rust file: CommentStyle recognizes the
        // extension, so the function proceeds past the type check, but
        // reading the "file" fails -> FileRead error.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("looks_like.rs");
        std::fs::create_dir(&dir).unwrap();
        assert!(matches!(read_and_classify(&dir, &config()), Err(HeatherError::FileRead { .. })));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn run_fix_skips_cargo_scripts_when_scripts_disabled() {
        // A cargo-script file with `scripts = false` is skipped by
        // read_and_classify (Ok(None)), exercising the `continue` in run_fix.
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("s.rs");
        std::fs::write(&script, "#!/usr/bin/env cargo\n---\n//! doc\nfn main() {}\n").unwrap();
        let mut cfg = config();
        cfg.scripts = false;
        let fixed = run_fix(&[script], &cfg, tmp.path()).unwrap();
        assert_eq!(fixed, 0);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn write_fixed_writes_bytes_to_disk() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("out.rs");
        write_fixed(&p, b"hello\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello\n");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn write_fixed_propagates_write_error() {
        // A directory occupies the target path, so `fs::write` fails and
        // the error is surfaced as `FileWrite`.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("blocked");
        std::fs::create_dir(&dir).unwrap();
        let err = write_fixed(&dir, b"x").expect_err("writing to a directory path must fail");
        assert!(err.to_string().contains("failed to write file"), "{err}");
    }
}
