// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header validation and rewriting primitives.
//!
//! Pure string-based helpers driving the [`crate::process`] streaming API.
//! Sub-modules split the responsibilities:
//!
//! - `extract` — pull the first comment block out of file content
//! - `matcher` — line-by-line prefix matching of expected vs actual
//! - `strip`   — remove an existing header for `--fix` mode

mod extract;
mod matcher;
mod strip;

use crate::comment::{CommentStyle, FileKind};

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

/// Validate `content` against `expected_header` for the given `kind`.
pub(crate) fn check(content: &str, expected_header: &str, kind: FileKind) -> CheckResult {
    let style = kind.comment_style();
    let extracted = match kind {
        FileKind::CargoScript => extract::script_header(content, style),
        _ => extract::header_comment(content, style),
    };
    classify(extracted, expected_header)
}

/// Compute the fixed-up content for `content` against `expected_header`.
///
/// Returns the [`CheckResult`] for the *input* alongside the rewritten
/// content. When the input is already [`CheckResult::Ok`], the returned
/// content is byte-equivalent to the input.
pub(crate) fn fix(content: &str, expected_header: &str, kind: FileKind) -> (CheckResult, String) {
    let style = kind.comment_style();
    let result = check(content, expected_header, kind);
    let new_content = match (&result, kind) {
        (CheckResult::Ok, _) => content.to_owned(),
        (_, FileKind::CargoScript) => strip::fix_script_content(content, expected_header, style),
        (CheckResult::Missing, _) => prepend_header(content, expected_header, style),
        (CheckResult::Mismatch { .. }, _) => {
            let stripped = strip::strip_existing_header(content, style, expected_header);
            prepend_header(&stripped, expected_header, style)
        }
    };
    (result, new_content)
}

/// Prepend the license header comment to file content.
fn prepend_header(content: &str, header_text: &str, style: CommentStyle) -> String {
    let comment = style.format_header(header_text);
    if content.is_empty() {
        format!("{comment}\n")
    } else {
        format!("{comment}\n\n{content}")
    }
}

fn classify(extracted: Option<String>, expected_header: &str) -> CheckResult {
    match extracted {
        None => CheckResult::Missing,
        Some(actual) if header_matches(&actual, expected_header) => CheckResult::Ok,
        Some(actual) => CheckResult::Mismatch {
            expected: expected_header.to_owned(),
            actual,
        },
    }
}
