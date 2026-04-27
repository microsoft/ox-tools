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

#[cfg(test)]
mod tests {
    use super::*;

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
}
