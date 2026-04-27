// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header validation logic for `cargo-heather`.
//!
//! Extracts the first comment block from source files and compares it
//! against the expected license header text. Supports multiple comment
//! styles via [`CommentStyle`].
//!
//! Submodules split the responsibilities:
//! - `extract`  — pull the first comment block out of file content
//! - `matcher`  — line-by-line prefix matching of expected vs actual
//! - `strip`    — remove an existing header for `--fix` mode

mod extract;
mod matcher;
mod strip;

use std::path::{Path, PathBuf};

use crate::comment::{CommentStyle, FileKind};
use crate::config::HeatherConfig;
use crate::error::HeatherError;

use matcher::header_matches;

/// Result of checking a single file's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The file has the correct header.
    Ok,
    /// The file is missing a header entirely.
    Missing,
    /// The file has a header, but it doesn't match the expected text.
    Mismatch {
        /// The expected header text.
        expected: String,
        /// The actual header text found in the file.
        actual: String,
    },
}

/// Result of checking a single file, including its path.
#[derive(Debug, Clone)]
pub struct FileCheckResult {
    /// Path to the checked file.
    pub path: PathBuf,
    /// The result of the header check.
    pub result: CheckResult,
}

/// Check all given files for the expected license header.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if any file cannot be read,
/// or [`HeatherError::UnsupportedFileType`] if a file has an unknown extension.
pub fn check_files(files: &[PathBuf], config: &HeatherConfig) -> Result<Vec<FileCheckResult>, HeatherError> {
    files.iter().map(|path| check_file(path, config)).collect()
}

/// Check a single file for the expected license header.
///
/// For cargo-script files (shebang + `---`), the header is expected inside
/// the frontmatter using `#` comment style. These files are skipped when
/// `config.scripts` is `false`.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if the file cannot be read,
/// or [`HeatherError::UnsupportedFileType`] if the extension is unknown.
pub fn check_file(path: &Path, config: &HeatherConfig) -> Result<FileCheckResult, HeatherError> {
    let Some((content, kind, style)) = read_and_classify(path, config)? else {
        return Ok(skipped_ok(path));
    };

    Ok(FileCheckResult {
        path: path.to_path_buf(),
        result: check_with_kind(&content, &config.header_text, kind, style),
    })
}

/// Fix a single file by adding or replacing the header.
///
/// For cargo-script files, the header is placed inside the frontmatter
/// (after the shebang and `---`). These files are skipped when
/// `config.scripts` is `false`.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if the file cannot be read or written,
/// or [`HeatherError::UnsupportedFileType`] if the extension is unknown.
pub fn fix_file(path: &Path, config: &HeatherConfig) -> Result<FileCheckResult, HeatherError> {
    let Some((content, kind, style)) = read_and_classify(path, config)? else {
        return Ok(skipped_ok(path));
    };

    let result = check_with_kind(&content, &config.header_text, kind, style);

    if !matches!(result, CheckResult::Ok) {
        let fixed = match (kind, &result) {
            (FileKind::CargoScript, _) => strip::fix_script_content(&content, &config.header_text, style),
            (_, CheckResult::Missing) => prepend_header(&content, &config.header_text, style),
            (_, CheckResult::Mismatch { .. }) => {
                let stripped = strip::strip_existing_header(&content, style, &config.header_text);
                prepend_header(&stripped, &config.header_text, style)
            }
            (_, CheckResult::Ok) => unreachable!("filtered out above"),
        };
        write_file(path, &fixed)?;
    }

    Ok(FileCheckResult {
        path: path.to_path_buf(),
        result,
    })
}

/// Check file content against the expected header text.
///
/// Extracts the first comment block and validates that it begins with the
/// expected header. Additional descriptive comment lines after the header
/// are allowed (treated as documentation, not a mismatch).
#[must_use]
pub fn check_content(content: &str, expected_header: &str, style: CommentStyle) -> CheckResult {
    classify_header(extract::header_comment(content, style), expected_header)
}

/// Check cargo-script content against the expected header text.
#[must_use]
fn check_content_script(content: &str, expected_header: &str, style: CommentStyle) -> CheckResult {
    classify_header(extract::script_header(content, style), expected_header)
}

/// Prepend the license header comment to file content.
///
/// Used by `--fix` mode to add missing headers.
#[must_use]
pub fn prepend_header(content: &str, header_text: &str, style: CommentStyle) -> String {
    let comment = style.format_header(header_text);
    if content.is_empty() {
        format!("{comment}\n")
    } else {
        format!("{comment}\n\n{content}")
    }
}

// --- private helpers ---

/// Read a file and classify it. Returns `Ok(None)` if the file should be
/// skipped (cargo-script with `config.scripts = false`).
fn read_and_classify(path: &Path, config: &HeatherConfig) -> Result<Option<(String, FileKind, CommentStyle)>, HeatherError> {
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let kind = FileKind::detect(path, Some(&content)).ok_or_else(|| HeatherError::UnsupportedFileType { path: path.to_path_buf() })?;

    if kind == FileKind::CargoScript && !config.scripts {
        return Ok(None);
    }

    let style = kind.comment_style();
    Ok(Some((content, kind, style)))
}

fn skipped_ok(path: &Path) -> FileCheckResult {
    FileCheckResult {
        path: path.to_path_buf(),
        result: CheckResult::Ok,
    }
}

fn check_with_kind(content: &str, expected_header: &str, kind: FileKind, style: CommentStyle) -> CheckResult {
    match kind {
        FileKind::CargoScript => check_content_script(content, expected_header, style),
        _ => check_content(content, expected_header, style),
    }
}

fn classify_header(extracted: Option<String>, expected_header: &str) -> CheckResult {
    match extracted {
        None => CheckResult::Missing,
        Some(actual) if header_matches(&actual, expected_header) => CheckResult::Ok,
        Some(actual) => CheckResult::Mismatch {
            expected: expected_header.to_owned(),
            actual,
        },
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), HeatherError> {
    std::fs::write(path, content).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    // Most content-level tests use DoubleSlash since the logic is shared;
    // TOML-specific behaviour is tested via Hash variants below.

    #[test]
    fn check_correct_header() {
        let content = "// Licensed under the MIT License.\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_missing_header() {
        let content = "fn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn check_mismatched_header() {
        let content = "// Some other header\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn check_multiline_header() {
        let content = "// Line one\n//\n// Line three\n\nfn main() {}\n";
        let result = check_content(content, "Line one\n\nLine three", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_empty_file() {
        let result = check_content("", "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn check_only_blank_lines() {
        let result = check_content("\n\n\n", "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn check_skips_leading_blanks() {
        let content = "\n\n// Licensed under the MIT License.\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_ignores_doc_comments() {
        let content = "//! Module doc\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn check_ignores_triple_slash_doc_comments() {
        let content = "/// Doc comment\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn prepend_header_to_content() {
        let result = prepend_header("fn main() {}\n", "MIT License", CommentStyle::DoubleSlash);
        assert_eq!(result, "// MIT License\n\nfn main() {}\n");
    }

    #[test]
    fn prepend_header_to_empty() {
        let result = prepend_header("", "MIT License", CommentStyle::DoubleSlash);
        assert_eq!(result, "// MIT License\n");
    }

    #[test]
    fn check_trailing_whitespace_tolerance() {
        let content = "// Licensed under the MIT License.  \n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    // --- Prefix-match (descriptive trailing comments allowed) ---

    #[test]
    fn check_passes_with_descriptive_trailing_comment() {
        let content = "// Licensed under the MIT License.\n// Hand-written types matching foo.\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_passes_with_multi_line_header_and_trailing_comment() {
        let content =
            "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\n// Module description here.\n\nfn main() {}\n";
        let result = check_content(
            content,
            "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.",
            CommentStyle::DoubleSlash,
        );
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_passes_with_blank_separator_then_descriptive_paragraph() {
        let content = "// Copyright (c) Microsoft Corporation.\n//\n// Module description here.\n\nfn main() {}\n";
        let result = check_content(content, "Copyright (c) Microsoft Corporation.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn check_still_fails_when_header_missing_from_block() {
        let content = "// Some unrelated comment.\n// Licensed under the MIT License.\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn check_still_fails_when_header_only_partially_present() {
        let content = "// Copyright (c) Microsoft Corporation.\n\nfn main() {}\n";
        let result = check_content(
            content,
            "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.",
            CommentStyle::DoubleSlash,
        );
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    // --- TOML-specific tests ---

    #[test]
    fn toml_check_correct_header() {
        let content = "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn toml_check_missing_header() {
        let content = "[package]\nname = \"foo\"\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn toml_check_mismatched_header() {
        let content = "# Some other header\n\n[package]\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn toml_check_multiline_header() {
        let content = "# Line one\n#\n# Line three\n\n[package]\n";
        let result = check_content(content, "Line one\n\nLine three", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn toml_prepend_header_to_content() {
        let result = prepend_header("[package]\nname = \"foo\"\n", "MIT License", CommentStyle::Hash);
        assert_eq!(result, "# MIT License\n\n[package]\nname = \"foo\"\n");
    }

    #[test]
    fn toml_prepend_header_to_empty() {
        let result = prepend_header("", "MIT License", CommentStyle::Hash);
        assert_eq!(result, "# MIT License\n");
    }

    #[test]
    fn toml_check_passes_with_descriptive_trailing_comment() {
        let content = "# Licensed under the MIT License.\n# Build script for the foo crate.\n\n[package]\nname = \"foo\"\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    // --- Cargo-script content-level tests ---

    #[test]
    fn script_check_correct_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let result = check_content_script(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn script_check_missing_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let result = check_content_script(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Missing);
    }

    #[test]
    fn script_check_mismatched_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Apache License\n\n[package]\n---\n";
        let result = check_content_script(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn script_check_multiline_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT License.\n\n[package]\nedition = \"2024\"\n---\n";
        let result = check_content_script(
            content,
            "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.",
            CommentStyle::Hash,
        );
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn script_check_passes_with_descriptive_trailing_comment() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n# Helper script for X.\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let result = check_content_script(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    // --- File-level tests (require filesystem; skip under Miri) ---

    #[test]
    #[cfg_attr(miri, ignore)]
    fn check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "// Licensed under the MIT License.\n\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn check_file_nonexistent() {
        let config = HeatherConfig::with_defaults("MIT".into());
        let result = check_file(Path::new("/nonexistent/file.rs"), &config);
        result.unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn check_files_multiple() {
        let dir = TempDir::new().unwrap();
        let good = dir.path().join("good.rs");
        let bad = dir.path().join("bad.rs");
        std::fs::write(&good, "// MIT\n\nfn a() {}\n").unwrap();
        std::fs::write(&bad, "fn b() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());

        let results = check_files(&[good, bad], &config).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].result, CheckResult::Ok);
        assert_eq!(results[1].result, CheckResult::Missing);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_adds_missing_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
        assert!(content.contains("fn main()"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_replaces_wrong_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "// Wrong header\n\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert!(matches!(result.result, CheckResult::Mismatch { .. }));

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
        assert!(content.contains("fn main()"));
        assert!(!content.contains("Wrong header"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_leaves_correct_header_unchanged() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        let original = "// Licensed under the MIT License.\n\nfn main() {}\n";
        std::fs::write(&file, original).unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_replaces_wrong_header_and_preserves_descriptive_comments() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "// Apache License 2.0\n//\n// Module description here.\n\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert!(matches!(result.result, CheckResult::Mismatch { .. }));

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
        assert!(content.contains("// Module description here."));
        assert!(!content.contains("Apache"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn toml_check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn toml_fix_file_adds_missing_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("# Licensed under the MIT License."));
        assert!(content.contains("[package]"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn toml_fix_file_replaces_wrong_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "# Wrong header\n\n[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = fix_file(&file, &config).unwrap();
        assert!(matches!(result.result, CheckResult::Mismatch { .. }));

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("# Licensed under the MIT License."));
        assert!(content.contains("[package]"));
        assert!(!content.contains("Wrong header"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn check_file_unsupported_extension() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readme.md");
        std::fs::write(&file, "# Hello").unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());

        let result = check_file(&file, &config);
        result.unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn script_check_file_correct() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("script.rs");
        std::fs::write(
            &file,
            "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n\n[package]\n---\n",
        )
        .unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());
        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn script_check_file_skipped_when_disabled() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("script.rs");
        std::fs::write(&file, "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[package]\n---\n").unwrap();

        let mut config = HeatherConfig::with_defaults("MIT".into());
        config.scripts = false;
        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn regular_file_still_checked_when_scripts_disabled() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let mut config = HeatherConfig::with_defaults("MIT".into());
        config.scripts = false;
        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn script_fix_file_adds_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("script.rs");
        std::fs::write(
            &file,
            "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[package]\nedition = \"2024\"\n---\nfn main() {}\n",
        )
        .unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());
        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);

        let fixed_content = std::fs::read_to_string(&file).unwrap();
        assert!(fixed_content.starts_with("#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n"));
        assert!(fixed_content.contains("[package]"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn inner_attribute_not_treated_as_script() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "#![allow(unused)]\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());
        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_nonexistent_returns_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.rs");
        let config = HeatherConfig::with_defaults("MIT".into());
        let err = fix_file(&file, &config).unwrap_err();
        assert!(err.to_string().contains("failed to read file"));
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn fix_file_skips_script_when_scripts_disabled() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("script.rs");
        std::fs::write(&file, "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n---\nfn main() {}\n").unwrap();

        let mut config = HeatherConfig::with_defaults("MIT".into());
        config.scripts = false;

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn write_file_read_only_returns_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readonly.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());
        let err = fix_file(&file, &config).unwrap_err();
        assert!(err.to_string().contains("failed to read file") || err.to_string().contains("denied"));

        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        #[expect(clippy::permissions_set_readonly_false, reason = "required to clean up test temp dir")]
        perms.set_readonly(false);
        std::fs::set_permissions(&file, perms).unwrap();
    }
}
