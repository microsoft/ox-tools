// Licensed under the MIT License.

//! Header validation logic for `cargo-heather`.
//!
//! Extracts the first comment block from source files and compares it
//! against the expected license header text. Supports multiple comment
//! styles via [`CommentStyle`].

use std::path::{Path, PathBuf};

use crate::comment::CommentStyle;
use crate::config::HeatherConfig;
use crate::error::HeatherError;

/// Result of checking a single file's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The file has the correct header.
    Ok,
    /// The file is missing a header entirely.
    Missing,
    /// The file has a header, but it doesn't match the expected text.
    Mismatch { expected: String, actual: String },
}

/// Result of checking a single file, including its path.
#[derive(Debug, Clone)]
pub struct FileCheckResult {
    pub path: PathBuf,
    pub result: CheckResult,
}

/// Check all given files for the expected license header.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if any file cannot be read,
/// or [`HeatherError::UnsupportedFileType`] if a file has an unknown extension.
pub fn check_files(
    files: &[PathBuf],
    config: &HeatherConfig,
) -> Result<Vec<FileCheckResult>, HeatherError> {
    files.iter().map(|path| check_file(path, config)).collect()
}

/// Check a single file for the expected license header.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if the file cannot be read,
/// or [`HeatherError::UnsupportedFileType`] if the extension is unknown.
pub fn check_file(path: &Path, config: &HeatherConfig) -> Result<FileCheckResult, HeatherError> {
    let style = CommentStyle::from_path(path).ok_or_else(|| HeatherError::UnsupportedFileType {
        path: path.to_path_buf(),
    })?;

    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let result = check_content(&content, &config.header_text, style);

    Ok(FileCheckResult {
        path: path.to_path_buf(),
        result,
    })
}

/// Check file content against the expected header text.
///
/// Extracts the first comment block and compares it to the expected header.
#[must_use]
pub fn check_content(content: &str, expected_header: &str, style: CommentStyle) -> CheckResult {
    let extracted = extract_header_comment(content, style);

    match extracted {
        None => CheckResult::Missing,
        Some(actual) => {
            let normalized_expected = normalize_text(expected_header);
            let normalized_actual = normalize_text(&actual);

            if normalized_expected == normalized_actual {
                CheckResult::Ok
            } else {
                CheckResult::Mismatch {
                    expected: expected_header.to_owned(),
                    actual,
                }
            }
        }
    }
}

/// Extract the first contiguous block of comment lines from file content.
///
/// Skips leading blank lines. Stops at the first non-comment line.
/// For Rust files, does NOT include doc comments (`///` or `//!`).
/// Returns `None` if no comment block is found at the start.
fn extract_header_comment(content: &str, style: CommentStyle) -> Option<String> {
    let mut comment_lines: Vec<String> = Vec::new();
    let mut found_comment = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip leading blank lines before the comment block
        if !found_comment && trimmed.is_empty() {
            continue;
        }

        if style.is_header_comment_line(trimmed) {
            found_comment = true;
            let text = style.strip_prefix(trimmed);
            comment_lines.push(text);
        } else if found_comment {
            // End of comment block
            break;
        } else {
            // First non-blank, non-comment line — no header found
            return None;
        }
    }

    if comment_lines.is_empty() {
        return None;
    }

    // Trim trailing empty lines from the comment block
    while comment_lines.last().is_some_and(String::is_empty) {
        comment_lines.pop();
    }

    Some(comment_lines.join("\n"))
}

/// Normalize text for comparison: trim lines and collapse whitespace variations.
fn normalize_text(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Prepend the license header comment to file content.
///
/// Used by `--fix` mode to add missing headers.
#[must_use]
pub fn prepend_header(content: &str, header_text: &str, style: CommentStyle) -> String {
    let comment = style.format_header(header_text);

    if content.is_empty() {
        return format!("{comment}\n");
    }

    format!("{comment}\n\n{content}")
}

/// Fix a single file by prepending the header if it's missing or mismatched.
///
/// # Errors
///
/// Returns [`HeatherError::FileRead`] if the file cannot be read or written,
/// or [`HeatherError::UnsupportedFileType`] if the extension is unknown.
pub fn fix_file(path: &Path, config: &HeatherConfig) -> Result<FileCheckResult, HeatherError> {
    let style = CommentStyle::from_path(path).ok_or_else(|| HeatherError::UnsupportedFileType {
        path: path.to_path_buf(),
    })?;

    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let result = check_content(&content, &config.header_text, style);

    match &result {
        CheckResult::Ok => {}
        CheckResult::Missing => {
            let fixed = prepend_header(&content, &config.header_text, style);
            write_file(path, &fixed)?;
        }
        CheckResult::Mismatch { .. } => {
            let without_header = strip_existing_header(&content, style);
            let fixed = prepend_header(&without_header, &config.header_text, style);
            write_file(path, &fixed)?;
        }
    }

    Ok(FileCheckResult {
        path: path.to_path_buf(),
        result,
    })
}

/// Strip the existing header comment block from file content.
fn strip_existing_header(content: &str, style: CommentStyle) -> String {
    let mut lines = content.lines().peekable();
    let mut skipped_comment = false;

    // Skip leading blank lines
    while lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }

    // Skip the comment block
    while lines
        .peek()
        .is_some_and(|l| style.is_header_comment_line(l.trim()))
    {
        lines.next();
        skipped_comment = true;
    }

    if !skipped_comment {
        return content.to_owned();
    }

    // Skip one blank line after the comment block
    if lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }

    let remaining: String = lines.collect::<Vec<_>>().join("\n");
    if remaining.is_empty() {
        remaining
    } else {
        format!("{remaining}\n")
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
    use super::*;
    use tempfile::TempDir;

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
    fn extract_header_simple() {
        let content = "// Hello\n// World\n\nfn main() {}\n";
        let header = extract_header_comment(content, CommentStyle::DoubleSlash).unwrap();
        assert_eq!(header, "Hello\nWorld");
    }

    #[test]
    fn extract_header_with_blank_comment_line() {
        let content = "// First\n//\n// Third\n\nfn main() {}\n";
        let header = extract_header_comment(content, CommentStyle::DoubleSlash).unwrap();
        assert_eq!(header, "First\n\nThird");
    }

    #[test]
    fn extract_no_header() {
        let content = "fn main() {}\n";
        assert!(extract_header_comment(content, CommentStyle::DoubleSlash).is_none());
    }

    #[test]
    fn extract_empty_content() {
        assert!(extract_header_comment("", CommentStyle::DoubleSlash).is_none());
    }

    #[test]
    fn normalize_trims_trailing_whitespace() {
        let result = normalize_text("Hello   \nWorld  ");
        assert_eq!(result, "Hello\nWorld");
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
    fn check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(
            &file,
            "// Licensed under the MIT License.\n\nfn main() {}\n",
        )
        .unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    fn check_file_nonexistent() {
        let config = HeatherConfig {
            header_text: "MIT".into(),
        };

        let result = check_file(Path::new("/nonexistent/file.rs"), &config);
        assert!(result.is_err());
    }

    #[test]
    fn check_files_multiple() {
        let dir = TempDir::new().unwrap();
        let good = dir.path().join("good.rs");
        let bad = dir.path().join("bad.rs");
        std::fs::write(&good, "// MIT\n\nfn a() {}\n").unwrap();
        std::fs::write(&bad, "fn b() {}\n").unwrap();

        let config = HeatherConfig {
            header_text: "MIT".into(),
        };

        let results = check_files(&[good, bad], &config).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].result, CheckResult::Ok);
        assert_eq!(results[1].result, CheckResult::Missing);
    }

    #[test]
    fn fix_file_adds_missing_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
        assert!(content.contains("fn main()"));
    }

    #[test]
    fn fix_file_replaces_wrong_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "// Wrong header\n\nfn main() {}\n").unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = fix_file(&file, &config).unwrap();
        assert!(matches!(result.result, CheckResult::Mismatch { .. }));

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
        assert!(content.contains("fn main()"));
        assert!(!content.contains("Wrong header"));
    }

    #[test]
    fn fix_file_leaves_correct_header_unchanged() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        let original = "// Licensed under the MIT License.\n\nfn main() {}\n";
        std::fs::write(&file, original).unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);

        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn strip_existing_header_removes_comment_block() {
        let content = "// Old header\n// Second line\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn strip_existing_header_no_comment() {
        let content = "fn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash);
        assert_eq!(result, content);
    }

    #[test]
    fn strip_existing_header_only_comment() {
        let content = "// Just a comment\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash);
        assert!(result.is_empty());
    }

    #[test]
    fn strip_existing_header_with_leading_blanks() {
        let content = "\n\n// Header\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn check_trailing_whitespace_tolerance() {
        let content = "// Licensed under the MIT License.  \n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert_eq!(result, CheckResult::Ok);
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
    fn toml_check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(
            &file,
            "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n",
        )
        .unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    fn toml_fix_file_adds_missing_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = fix_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Missing);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("# Licensed under the MIT License."));
        assert!(content.contains("[package]"));
    }

    #[test]
    fn toml_fix_file_replaces_wrong_header() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "# Wrong header\n\n[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig {
            header_text: "Licensed under the MIT License.".into(),
        };

        let result = fix_file(&file, &config).unwrap();
        assert!(matches!(result.result, CheckResult::Mismatch { .. }));

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.starts_with("# Licensed under the MIT License."));
        assert!(content.contains("[package]"));
        assert!(!content.contains("Wrong header"));
    }

    #[test]
    fn toml_strip_existing_header() {
        let content = "# Old header\n# Second line\n\n[package]\n";
        let result = strip_existing_header(content, CommentStyle::Hash);
        assert_eq!(result, "[package]\n");
    }

    #[test]
    fn check_file_unsupported_extension() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readme.md");
        std::fs::write(&file, "# Hello").unwrap();

        let config = HeatherConfig {
            header_text: "MIT".into(),
        };

        let result = check_file(&file, &config);
        assert!(result.is_err());
    }
}
