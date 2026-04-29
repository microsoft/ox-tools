// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Line-by-line prefix matching for header comparison.

/// Returns `true` if `extracted` begins with `expected_header`.
///
/// Compares line-by-line (after trimming trailing whitespace per line and
/// stripping leading/trailing blank lines from both inputs). The expected
/// header is required to appear as a contiguous prefix of the extracted
/// block; any extra trailing comment lines in `extracted` are allowed.
pub(super) fn header_matches(extracted: &str, expected_header: &str) -> bool {
    let expected_lines = normalize_to_lines(expected_header);
    let actual_lines = normalize_to_lines(extracted);

    if actual_lines.len() < expected_lines.len() {
        return false;
    }
    actual_lines[..expected_lines.len()] == expected_lines[..]
}

/// Normalize text to a vector of per-line strings with trailing whitespace
/// removed, and outer blank lines stripped.
pub(super) fn normalize_to_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_owned()).collect();
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}
