// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded, consistently labeled output from failed child processes.

use core::fmt::Write as _;

use super::verdict::tail;
use crate::report::unstyled;

/// Retain the final diagnostic lines without flooding failure reports with build progress.
const MAX_TAIL_LINES: usize = 30;
/// Bound each displayed stream, including output containing a single unusually long line.
const MAX_SECTION_BYTES: usize = 16 * 1024;

pub(super) fn failure_output(label: &str, stdout: &str, stderr: &str) -> String {
    let section = |text: &str| {
        let plain = unstyled(text);
        let plain = plain.trim_end();
        if plain.is_empty() {
            return "(no output)".to_owned();
        }
        let recent = tail(plain, MAX_TAIL_LINES);
        let start = recent.len().saturating_sub(MAX_SECTION_BYTES);
        let start =
        // #[gamma::skip(range.inclusive_to_exclusive, tag = "equivalent", reason = "the nonempty tail has a UTF-8 boundary at zero or within three bytes of len minus the positive 16384-byte cap, always before len")]
        (start..=recent.len())
            .find(|&offset| recent.is_char_boundary(offset))
            .expect("the string length is always a UTF-8 boundary");
        if start == 0 {
            recent.into_owned()
        } else {
            format!("(earlier output omitted)\n{}", &recent[start..])
        }
    };
    let mut output = format!("{label} Stderr\n{}", section(stderr));
    if !stdout.trim().is_empty() {
        let _ = write!(output, "\n\n{label} Stdout\n{}", section(stdout));
    }
    output
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn failure_sections_preserve_leading_whitespace_and_trim_only_the_end() {
        assert_eq!(
            failure_output("Test", "\n  stdout detail  \n", "\n  stderr detail  \n"),
            "Test Stderr\n\n  stderr detail\n\nTest Stdout\n\n  stdout detail"
        );
    }

    #[test]
    fn oversized_utf8_lines_retain_exactly_the_byte_limit_without_splitting_a_character() {
        assert_eq!(
            failure_output("Test", "", &"\u{e9}".repeat(20_000)),
            format!("Test Stderr\n(earlier output omitted)\n{}", "\u{e9}".repeat(8192))
        );
    }

    #[test]
    fn ascii_sections_enforce_the_exact_byte_limit_on_both_sides_of_the_boundary() {
        for length in [16383, 16384, 16385, 16386] {
            let prefix = if length > 16384 { "(earlier output omitted)\n" } else { "" };
            assert_eq!(
                failure_output("Test", "", &"x".repeat(length)),
                format!("Test Stderr\n{prefix}{}", "x".repeat(length.min(16384))),
                "{length}"
            );
        }
    }

    #[test]
    fn failures_share_labels_strip_color_and_bound_lines_and_long_utf8_records() {
        assert_eq!(
            failure_output("Test", "startup detail\n", "\u{1b}[31mloader failed\u{1b}[0m\n"),
            "Test Stderr\nloader failed\n\nTest Stdout\nstartup detail"
        );
        assert_eq!(failure_output("Cargo", "", ""), "Cargo Stderr\n(no output)");
        let mut lines = String::new();
        for index in 0..40 {
            writeln!(lines, "line {index}").unwrap();
        }
        let output = failure_output("Test", "", &lines);
        assert!(output.starts_with("Test Stderr\nline 10\n"));
        assert!(output.ends_with("line 39"));
        let output = failure_output("Test", "", &"\u{e9}".repeat(20_000));
        assert!(output.contains("(earlier output omitted)"));
        assert!(output.len() < 17_000);
    }
}
