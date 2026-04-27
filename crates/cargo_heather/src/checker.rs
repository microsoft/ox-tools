// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header validation logic for `cargo-heather`.
//!
//! Extracts the first comment block from source files and compares it
//! against the expected license header text. Supports multiple comment
//! styles via [`CommentStyle`].

use std::path::{Path, PathBuf};

use crate::comment::{CommentStyle, FileKind};
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
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let kind = FileKind::detect(path, Some(&content)).ok_or_else(|| HeatherError::UnsupportedFileType { path: path.to_path_buf() })?;

    // Skip cargo-script files when scripts processing is disabled.
    if kind == FileKind::CargoScript && !config.scripts {
        return Ok(FileCheckResult {
            path: path.to_path_buf(),
            result: CheckResult::Ok,
        });
    }

    let style = kind.comment_style();
    let result = match kind {
        FileKind::CargoScript => check_content_script(&content, &config.header_text, style),
        _ => check_content(&content, &config.header_text, style),
    };

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
    let extracted = extract_header_comment(content, style);

    match extracted {
        None => CheckResult::Missing,
        Some(actual) => {
            if header_matches(&actual, expected_header) {
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

/// Check file content for the expected header inside a cargo-script frontmatter.
///
/// Skips the shebang line and opening `---`, then checks the `#` comment block.
/// Like [`check_content`], allows descriptive trailing comment lines after the
/// expected header.
#[must_use]
fn check_content_script(content: &str, expected_header: &str, style: CommentStyle) -> CheckResult {
    let extracted = extract_script_header(content, style);

    match extracted {
        None => CheckResult::Missing,
        Some(actual) => {
            if header_matches(&actual, expected_header) {
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

/// Returns `true` if `extracted` begins with `expected_header`.
///
/// Compares line-by-line (after trimming trailing whitespace per line and
/// stripping leading/trailing blank lines from both inputs). The expected
/// header is required to appear as a contiguous prefix of the extracted
/// block; any extra trailing comment lines in `extracted` are allowed.
fn header_matches(extracted: &str, expected_header: &str) -> bool {
    let expected_lines = normalize_to_lines(expected_header);
    let actual_lines = normalize_to_lines(extracted);

    if actual_lines.len() < expected_lines.len() {
        return false;
    }
    actual_lines[..expected_lines.len()] == expected_lines[..]
}

/// Normalize text to a vector of per-line strings with trailing whitespace
/// removed, and outer blank lines stripped. Used to compare headers
/// line-by-line for prefix matching.
fn normalize_to_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_owned()).collect();
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Extract the header comment block from inside a cargo-script frontmatter.
///
/// Expects the file to start with a shebang and `---`. Extracts `#` comment
/// lines immediately after the opening `---`, stopping at the first blank
/// or non-comment line.
fn extract_script_header(content: &str, style: CommentStyle) -> Option<String> {
    let mut lines = content.lines();

    // Skip shebang (line 1)
    lines.next()?;
    // Skip opening --- (line 2)
    let dash_line = lines.next()?;
    if dash_line.trim() != "---" {
        return None;
    }

    let mut comment_lines: Vec<String> = Vec::new();

    for line in lines {
        let trimmed = line.trim();
        if style.is_header_comment_line(trimmed) {
            comment_lines.push(style.strip_prefix(trimmed));
        } else {
            break;
        }
    }

    if comment_lines.is_empty() {
        return None;
    }

    // Trim trailing empty lines
    while comment_lines.last().is_some_and(String::is_empty) {
        comment_lines.pop();
    }

    Some(comment_lines.join("\n"))
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
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let kind = FileKind::detect(path, Some(&content)).ok_or_else(|| HeatherError::UnsupportedFileType { path: path.to_path_buf() })?;

    // Skip cargo-script files when scripts processing is disabled.
    if kind == FileKind::CargoScript && !config.scripts {
        return Ok(FileCheckResult {
            path: path.to_path_buf(),
            result: CheckResult::Ok,
        });
    }

    let style = kind.comment_style();
    let result = match kind {
        FileKind::CargoScript => check_content_script(&content, &config.header_text, style),
        _ => check_content(&content, &config.header_text, style),
    };

    match &result {
        CheckResult::Ok => {}
        CheckResult::Missing | CheckResult::Mismatch { .. } => {
            let fixed = match kind {
                FileKind::CargoScript => fix_script_content(&content, &config.header_text, style),
                _ => {
                    if matches!(result, CheckResult::Missing) {
                        prepend_header(&content, &config.header_text, style)
                    } else {
                        let without_header = strip_existing_header(&content, style, &config.header_text);
                        prepend_header(&without_header, &config.header_text, style)
                    }
                }
            };
            write_file(path, &fixed)?;
        }
    }

    Ok(FileCheckResult {
        path: path.to_path_buf(),
        result,
    })
}

/// Fix a cargo-script file by placing the header inside the frontmatter.
///
/// Preserves the shebang line and opening `---`. Strips up to
/// `header_text.lines().count()` consecutive header-comment lines (counting
/// blank-comment lines like `#` as comment lines, so multi-paragraph
/// configured headers are removed cleanly), then optionally consumes one
/// trailing blank-comment-line separator and one blank line, before inserting
/// the new header. Descriptive trailing comment lines that follow a blank
/// separator are preserved.
fn fix_script_content(content: &str, header_text: &str, style: CommentStyle) -> String {
    let mut lines_iter = content.lines();

    let shebang = lines_iter.next().unwrap_or("");
    let dash_open = lines_iter.next().unwrap_or("---");

    let body_lines: Vec<&str> = lines_iter.collect();

    // Skip leading blank lines after `---`. Using take_while + count avoids
    // a mutable += that cargo-mutants can mutate into an infinite loop.
    let leading_blank_count = body_lines.iter().take_while(|l| l.trim().is_empty()).count();
    let mut idx = leading_blank_count;

    let start = idx;
    let header_line_count = header_text.lines().count();

    // Strip up to `header_line_count` consecutive header-comment lines.
    // Blank-comment lines (`#` with nothing after) count as comment lines so
    // multi-paragraph configured headers are stripped fully. Stops early at
    // the first non-comment line.
    let mut stripped = 0;
    while stripped < header_line_count && idx < body_lines.len() {
        let trimmed = body_lines[idx].trim();
        if !style.is_header_comment_line(trimmed) {
            break;
        }
        idx += 1;
        stripped += 1;
    }

    if idx > start {
        // Consume one trailing blank-comment-line paragraph separator if the
        // existing header had a `#` separator after the lines we just stripped
        // (e.g. existing header was longer than the configured one).
        if idx < body_lines.len() {
            let trimmed = body_lines[idx].trim();
            if style.is_header_comment_line(trimmed) && style.strip_prefix(trimmed).is_empty() {
                idx += 1;
            }
        }
        // Consume one blank line after the stripped header.
        if idx < body_lines.len() && body_lines[idx].trim().is_empty() {
            idx += 1;
        }
    } else {
        // No leading comment lines at all — keep the whole body as-is.
        idx = 0;
    }

    let header_comment = style.format_header(header_text);
    let rest = body_lines[idx..].join("\n");

    if rest.is_empty() {
        format!("{shebang}\n{dash_open}\n{header_comment}\n")
    } else {
        format!("{shebang}\n{dash_open}\n{header_comment}\n\n{rest}\n")
    }
}

/// Strip the leading header from file content.
///
/// Removes up to `expected_header.lines().count()` consecutive header-comment
/// lines from the top of the file (skipping any leading blank lines first).
/// Blank-comment lines (e.g. `//` or `#`) count as comment lines, so
/// multi-paragraph configured headers are stripped fully. After that, an
/// optional blank-comment-line paragraph separator and one blank line are
/// consumed. Descriptive trailing comment lines that come after the blank
/// separator are preserved.
fn strip_existing_header(content: &str, style: CommentStyle, expected_header: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Skip leading blank lines (real blanks, not blank-comment lines).
    // Using take_while + count avoids a mutable += that cargo-mutants can
    // mutate into an infinite loop (e.g. += 1 -> *= 1).
    let leading_blank_count = lines.iter().take_while(|l| l.trim().is_empty()).count();
    let mut idx = leading_blank_count;

    let start = idx;
    let header_line_count = expected_header.lines().count();

    // Strip up to `header_line_count` consecutive header-comment lines.
    let mut stripped = 0;
    while stripped < header_line_count && idx < lines.len() {
        let trimmed = lines[idx].trim();
        if !style.is_header_comment_line(trimmed) {
            break;
        }
        idx += 1;
        stripped += 1;
    }

    if idx == start {
        // No header paragraph found — return unchanged.
        return content.to_owned();
    }

    // Consume one trailing blank-comment-line paragraph separator if the
    // existing header had a `//` or `#` separator after the stripped lines.
    if idx < lines.len() {
        let trimmed = lines[idx].trim();
        if style.is_header_comment_line(trimmed) && style.strip_prefix(trimmed).is_empty() {
            idx += 1;
        }
    }

    // Consume one blank line after the stripped paragraph.
    if idx < lines.len() && lines[idx].trim().is_empty() {
        idx += 1;
    }

    let remaining = lines[idx..].join("\n");
    if remaining.is_empty() {
        remaining
    } else if content.ends_with('\n') {
        format!("{remaining}\n")
    } else {
        remaining
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
    fn normalize_to_lines_trims_trailing_whitespace() {
        let result = normalize_to_lines("Hello   \nWorld  ");
        assert_eq!(result, vec!["Hello".to_owned(), "World".to_owned()]);
    }

    #[test]
    fn normalize_to_lines_strips_outer_blank_lines() {
        let result = normalize_to_lines("\n\nHello\nWorld\n\n");
        assert_eq!(result, vec!["Hello".to_owned(), "World".to_owned()]);
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "// Licensed under the MIT License.\n\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn check_file_nonexistent() {
        let config = HeatherConfig::with_defaults("MIT".into());

        let result = check_file(Path::new("/nonexistent/file.rs"), &config);
        result.unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    fn strip_existing_header_removes_comment_block() {
        let content = "// Old header\n// Second line\n\nfn main() {}\n";
        let expected = "Old header\nSecond line";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, expected);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn strip_existing_header_no_comment() {
        let content = "fn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
        assert_eq!(result, content);
    }

    #[test]
    fn strip_existing_header_only_comment() {
        let content = "// Just a comment\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Just a comment");
        assert!(result.is_empty());
    }

    #[test]
    fn strip_existing_header_with_leading_blanks() {
        let content = "\n\n// Header\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn toml_check_file_reads_and_checks() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.toml");
        std::fs::write(&file, "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n").unwrap();

        let config = HeatherConfig::with_defaults("Licensed under the MIT License.".into());

        let result = check_file(&file, &config).unwrap();
        assert_eq!(result.result, CheckResult::Ok);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    fn toml_strip_existing_header() {
        let content = "# Old header\n# Second line\n\n[package]\n";
        let expected = "Old header\nSecond line";
        let result = strip_existing_header(content, CommentStyle::Hash, expected);
        assert_eq!(result, "[package]\n");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn check_file_unsupported_extension() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readme.md");
        std::fs::write(&file, "# Hello").unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());

        let result = check_file(&file, &config);
        result.unwrap_err();
    }

    // --- Cargo-script tests ---

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
    fn script_fix_adds_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.starts_with("#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n"));
        assert!(fixed.contains("[package]"));
        assert!(fixed.contains("fn main()"));
    }

    #[test]
    fn script_fix_replaces_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Old Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.contains("# Licensed under the MIT License."));
        assert!(!fixed.contains("Old Header"));
        assert!(fixed.contains("[package]"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn inner_attribute_not_treated_as_script() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "#![allow(unused)]\nfn main() {}\n").unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());
        let result = check_file(&file, &config).unwrap();
        // Should be treated as regular Rust, not a script — missing header
        assert_eq!(result.result, CheckResult::Missing);
    }

    #[test]
    fn extract_header_trailing_empty_comment_lines_trimmed() {
        // Test that trailing empty comment lines are stripped from extracted header
        let content = "// Licensed under MIT\n//\n\nfn main() {}\n";
        let extracted = extract_header_comment(content, CommentStyle::DoubleSlash);
        assert_eq!(extracted.as_deref(), Some("Licensed under MIT"));
    }

    #[test]
    fn extract_script_header_trailing_empty_comment_lines_trimmed() {
        // Script header with trailing empty `#` comment line
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under MIT\n#\n---\n";
        let extracted = extract_script_header(content, CommentStyle::Hash);
        assert_eq!(extracted.as_deref(), Some("Licensed under MIT"));
    }

    #[test]
    fn extract_script_header_no_dash_line() {
        // Line 2 is not `---`, so no script header
        let content = "#!/usr/bin/env cargo\nnot-a-dash\n# License\n";
        let extracted = extract_script_header(content, CommentStyle::Hash);
        assert!(extracted.is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fix_file_nonexistent_returns_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("nonexistent.rs");
        let config = HeatherConfig::with_defaults("MIT".into());
        let err = fix_file(&file, &config).unwrap_err();
        assert!(err.to_string().contains("failed to read file"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    fn fix_script_content_empty_rest() {
        // Script with only shebang + --- + no other content
        let content = "#!/usr/bin/env cargo\n---\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.starts_with("#!/usr/bin/env cargo\n---\n# MIT License\n"));
        // Should NOT have double newlines before EOF
        assert!(!fixed.contains("\n\n"));
    }

    #[test]
    fn fix_script_content_strips_blank_line_between_old_header_and_manifest() {
        // Old header followed by blank line then TOML manifest
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Old Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "New Header", CommentStyle::Hash);
        // The blank line between old header and [package] must be stripped,
        // and a new blank line inserted between new header and [package].
        assert_eq!(
            fixed,
            "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# New Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn write_file_read_only_returns_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("readonly.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        // Make file read-only
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).unwrap();

        let config = HeatherConfig::with_defaults("MIT".into());
        let err = fix_file(&file, &config).unwrap_err();
        assert!(err.to_string().contains("failed to read file") || err.to_string().contains("denied"));

        // Cleanup: restore permissions so TempDir can clean up
        let mut perms = std::fs::metadata(&file).unwrap().permissions();
        #[expect(clippy::permissions_set_readonly_false, reason = "required to clean up test temp dir")]
        perms.set_readonly(false);
        std::fs::set_permissions(&file, perms).unwrap();
    }

    // --- Prefix-match tests (descriptive trailing comments allowed) ---

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
        // The block has comments, but they are not the configured header.
        let content = "// Some unrelated comment.\n// Licensed under the MIT License.\n\nfn main() {}\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::DoubleSlash);
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn check_still_fails_when_header_only_partially_present() {
        // Header is 2 lines, file only has line 1.
        let content = "// Copyright (c) Microsoft Corporation.\n\nfn main() {}\n";
        let result = check_content(
            content,
            "Copyright (c) Microsoft Corporation.\nLicensed under the MIT License.",
            CommentStyle::DoubleSlash,
        );
        assert!(matches!(result, CheckResult::Mismatch { .. }));
    }

    #[test]
    fn toml_check_passes_with_descriptive_trailing_comment() {
        let content = "# Licensed under the MIT License.\n# Build script for the foo crate.\n\n[package]\nname = \"foo\"\n";
        let result = check_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    #[test]
    fn script_check_passes_with_descriptive_trailing_comment() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n# Helper script for X.\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let result = check_content_script(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert_eq!(result, CheckResult::Ok);
    }

    // --- Safer --fix tests (descriptive trailing comments preserved) ---

    #[test]
    fn strip_existing_header_preserves_descriptive_comment_paragraph() {
        // Configured 1-line header — the wrong header line, the `//` separator,
        // and the blank line are stripped; the descriptive comment is preserved.
        let content = "// Old wrong header\n//\n// Module description.\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Old wrong header");
        assert_eq!(result, "// Module description.\n\nfn main() {}\n");
    }

    #[test]
    fn strip_existing_header_preserves_multi_line_descriptive_paragraph() {
        // Configured 2-line header strips both header lines + `//` separator,
        // preserving the descriptive paragraph that follows.
        let content = "// Old wrong header line 1\n// Old wrong header line 2\n//\n// Module description line 1.\n// Module description line 2.\n\nfn main() {}\n";
        let result = strip_existing_header(
            content,
            CommentStyle::DoubleSlash,
            "Old wrong header line 1\nOld wrong header line 2",
        );
        assert_eq!(
            result,
            "// Module description line 1.\n// Module description line 2.\n\nfn main() {}\n"
        );
    }

    #[test]
    fn strip_existing_header_no_paragraph_separator_strips_whole_block() {
        // 2-line configured header strips both contiguous comment lines.
        let content = "// Wrong header\n// Wrong line 2\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Wrong header\nWrong line 2");
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn strip_existing_header_multi_paragraph_configured_strips_all_paragraphs() {
        // Reviewer-flagged case: multi-paragraph SPDX header (with blank-comment
        // line in the middle). All N lines (including the blank-comment) must be
        // stripped, otherwise leftover lines from the old license remain.
        let content = "// Apache License 2.0\n//\n// Licensed under the old text.\n\nfn main() {}\n";
        let expected = "Apache License 2.0\n\nLicensed under the new text.";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, expected);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
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
    fn script_fix_preserves_descriptive_comments() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Apache License 2.0\n#\n# Helper script description.\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.contains("# Licensed under the MIT License."));
        assert!(fixed.contains("# Helper script description."));
        assert!(!fixed.contains("Apache"));
    }

    // --- header_matches direct tests ---

    #[test]
    fn header_matches_exact_single_line() {
        assert!(header_matches("MIT License", "MIT License"));
    }

    #[test]
    fn header_matches_exact_multi_line() {
        assert!(header_matches(
            "Copyright Microsoft.\nMIT License.",
            "Copyright Microsoft.\nMIT License."
        ));
    }

    #[test]
    fn header_matches_with_extra_trailing_lines() {
        assert!(header_matches(
            "Copyright Microsoft.\nMIT License.\nDescriptive comment.",
            "Copyright Microsoft.\nMIT License."
        ));
    }

    #[test]
    fn header_matches_rejects_when_first_line_differs() {
        assert!(!header_matches(
            "Apache License.\nMIT License.",
            "Copyright Microsoft.\nMIT License."
        ));
    }

    #[test]
    fn header_matches_rejects_when_actual_too_short() {
        assert!(!header_matches("Copyright Microsoft.", "Copyright Microsoft.\nMIT License."));
    }

    #[test]
    fn header_matches_tolerates_trailing_whitespace_on_lines() {
        assert!(header_matches("MIT License   ", "MIT License"));
    }

    // --- Mutation-testing-driven edge-case tests ---
    //
    // These tests target specific arithmetic/comparison boundaries in
    // `fix_script_content` and `strip_existing_header` so that flipped
    // operators in the strip-N-lines logic produce observably wrong output
    // (or a panic).

    #[test]
    fn fix_script_content_header_only_no_trailing_lines() {
        // Body is ONLY a single header line — exercises the boundary where
        // idx == body_lines.len() after stripping, so the optional consumes
        // must not access out-of-bounds indices.
        let content = "#!/usr/bin/env script\n---\n# Old header\n---\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(!fixed.contains("Old header"));
    }

    #[test]
    fn fix_script_content_consumes_blank_comment_separator() {
        // Header paragraph + `#` separator + content (no blank line between).
        // The separator MUST be consumed, otherwise it shows up in output.
        let content = "#!/usr/bin/env script\n---\n# Old header\n#\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        // The new header is followed by a single blank line, then `[package]`
        // — the `#` separator from the original must NOT survive.
        assert!(fixed.contains("# MIT License\n\n[package]"));
        assert!(!fixed.contains("# MIT License\n\n#\n"));
    }

    #[test]
    fn fix_script_content_consumes_blank_line_after_header() {
        // Header paragraph followed directly by a blank line and then content.
        // The blank line MUST be consumed (otherwise we'd insert a duplicate
        // blank, producing a double-blank gap before `[package]`).
        let content = "#!/usr/bin/env script\n---\n# Old header\n\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License\n\n[package]"));
        assert!(!fixed.contains("\n\n\n[package]"));
    }

    #[test]
    fn strip_existing_header_only_blank_lines() {
        // Content that is only blank lines — leading-blank-skip loop runs to
        // idx == lines.len(). The boundary check (`idx < lines.len()`) must
        // hold or a `<=` mutation would index out of bounds.
        let content = "\n\n\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
        // No header paragraph found → content returned unchanged.
        assert_eq!(result, content);
    }

    #[test]
    fn strip_existing_header_only_a_single_header_line() {
        // Content is exactly a one-line header with no trailing content. The
        // optional separator+blank-line consumes must respect bounds.
        let content = "// Old header\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Old header");
        assert_eq!(result, "");
    }

    // --- Strip-N-lines mutation kills ---
    //
    // These tests target arithmetic/comparison boundaries in the new strip-N
    // logic so flipped operators (< vs <=, > vs >=, += vs -=, < vs >) produce
    // observably wrong output.

    #[test]
    fn fix_script_content_skips_leading_blank_lines() {
        // Body has leading blank lines BEFORE the wrong header. The
        // skip-blank-lines loop must run (`idx < len`, `idx += 1`) — flipping
        // `<` to `>` makes the loop never enter and the wrong header survives,
        // and flipping `+=` to `-=` / `*=` panics or hangs.
        let content = "#!/usr/bin/env script\n---\n\n\n# Old wrong header\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(!fixed.contains("Old wrong header"));
    }

    #[test]
    fn fix_script_content_strip_n_does_not_eat_descriptive_comment() {
        // Configured 1-line header, body has wrong header (1 line) immediately
        // followed by a descriptive comment line (no blank separator). N-line
        // strip must remove ONLY the first line; flipping `<` to `<=` in the
        // strip loop bound would eat the descriptive comment too.
        let content =
            "#!/usr/bin/env script\n---\n# Wrong header\n# Descriptive (preserve)\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(fixed.contains("# Descriptive (preserve)"));
        assert!(!fixed.contains("# Wrong header"));
    }

    #[test]
    fn fix_script_content_strip_n_consumes_all_body_no_panic() {
        // Body consists entirely of N header-comment lines so `idx` ends at
        // `body_lines.len()`. Both optional-consume bounds checks
        // (`idx < body_lines.len()`) must hold — flipping to `<=` would panic
        // accessing `body_lines[len]`.
        let content = "#!/usr/bin/env script\n---\n# A\n# B\n";
        let fixed = fix_script_content(content, "MIT\nLicense", CommentStyle::Hash);
        assert!(fixed.contains("# MIT"));
        assert!(fixed.contains("# License"));
        assert!(!fixed.contains("# A"));
        assert!(!fixed.contains("# B"));
    }

    #[test]
    fn fix_script_content_no_header_preserves_leading_blank() {
        // Body has a leading blank then code (no header). After skip-blanks,
        // idx==start with strip stripping nothing. The `idx > start` else
        // branch must reset idx to 0 so the leading blank survives —
        // flipping to `idx >= start` would skip the reset and drop the blank.
        let content = "#!/usr/bin/env script\n---\n\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT", CommentStyle::Hash);
        // Header + mandatory blank separator + preserved leading blank + code.
        assert!(fixed.contains("# MIT\n\n\nfn main() {}"));
    }

    #[test]
    fn strip_existing_header_strip_n_does_not_eat_descriptive_comment() {
        // Mirror of fix_script_content_strip_n_does_not_eat_descriptive_comment
        // for `strip_existing_header` — kills the L404 `<` vs `<=` mutation.
        let content = "// Wrong header\n// Descriptive (preserve)\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Wrong header");
        assert!(result.contains("// Descriptive (preserve)"));
        assert!(!result.contains("Wrong header"));
    }
}
