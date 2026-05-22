// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header-block stripping for `--fix` mode.
//!
//! Both regular files and cargo-script frontmatter blocks use the same algorithm:
//! skip leading blanks, then strip the entire contiguous block of header-comment
//! lines (including blank-comment paragraph separators), then optionally consume
//! one blank line. The two callers differ only in how they reassemble the output.

use crate::comment::CommentStyle;

/// Find where a header block ends in `lines`.
///
/// Algorithm:
/// 1. Skip leading blank lines.
/// 2. Strip all contiguous header-comment lines. Blank-comment lines
///    (e.g. `//` or `#`) count as comment lines, so multi-paragraph
///    headers and the trailing blank-comment separator that often
///    follows a license block are consumed in a single pass.
/// 3. If at least one header line was stripped, optionally consume one
///    trailing blank line.
///
/// The caller is expected to invoke this only after the extractor has
/// already classified the leading comment block as a license header
/// (i.e. on `CheckResult::Mismatch`), so consuming the whole block
/// matches the extractor's view of "what the header is" and avoids
/// leaving leftover lines from a longer wrong header behind when the
/// configured/expected header is shorter.
///
/// Returns `Some(body_start_idx)` if a header block was found, else `None`.
fn find_header_end(lines: &[&str], style: CommentStyle) -> Option<usize> {
    let blanks = lines.iter().take_while(|l| l.trim().is_empty()).count();

    let stripped = lines[blanks..]
        .iter()
        .take_while(|l| style.is_header_comment_line(l.trim()))
        .count();

    if stripped == 0 {
        return None;
    }

    let mut idx = blanks + stripped;

    // Optional blank line after the header block.
    if lines.get(idx).is_some_and(|l| l.trim().is_empty()) {
        idx += 1;
    }

    Some(idx)
}

/// Strip the leading header from regular file content.
///
/// If no header is found, returns content unchanged. Trailing newline is
/// preserved if and only if the original content had one.
pub(super) fn strip_existing_header(content: &str, style: CommentStyle) -> String {
    let lines: Vec<&str> = content.lines().collect();

    let Some(body_start) = find_header_end(&lines, style) else {
        return content.to_owned();
    };

    let remaining = lines[body_start..].join("\n");
    if remaining.is_empty() {
        remaining
    } else if content.ends_with('\n') {
        format!("{remaining}\n")
    } else {
        remaining
    }
}

/// Reassemble a file with a shebang, a new header, and remaining body lines.
///
/// Shared by both the `Missing` path (which passes `body_start = 0` to
/// preserve all content) and the `Mismatch` path (which skips stripped
/// header lines).
fn reassemble_after_shebang(shebang: &str, header_text: &str, body_lines: &[&str], body_start: usize, style: CommentStyle) -> String {
    let header_comment = style.format_header(header_text);
    let rest = body_lines[body_start..].join("\n");

    if rest.is_empty() {
        format!("{shebang}\n{header_comment}\n")
    } else {
        format!("{shebang}\n{header_comment}\n\n{rest}\n")
    }
}

/// Replace or insert a header after an optional shebang line.
pub(super) fn fix_shebang_content(content: &str, header_text: &str, style: CommentStyle) -> String {
    let mut iter = content.lines();
    let Some(first) = iter.next() else {
        return format!("{}\n", style.format_header(header_text));
    };

    if !first.trim().starts_with("#!") {
        let stripped = strip_existing_header(content, style);
        return super::prepend_header(&stripped, header_text, style);
    }

    let body_lines: Vec<&str> = iter.collect();
    let body_start = find_header_end(&body_lines, style).unwrap_or(0);
    reassemble_after_shebang(first, header_text, &body_lines, body_start, style)
}

/// Prepend a header after an optional shebang line, preserving all
/// existing content (including descriptive comment blocks).
///
/// Used for `CheckResult::Missing` where no header needs to be stripped.
pub(super) fn prepend_after_optional_shebang(content: &str, header_text: &str, style: CommentStyle) -> String {
    let mut iter = content.lines();
    let Some(first) = iter.next() else {
        return format!("{}\n", style.format_header(header_text));
    };

    if !first.trim().starts_with("#!") {
        return super::prepend_header(content, header_text, style);
    }

    let body_lines: Vec<&str> = iter.collect();
    reassemble_after_shebang(first, header_text, &body_lines, 0, style)
}

/// Replace the header inside a cargo-script frontmatter.
///
/// Preserves the shebang and opening `---`, strips any existing header block
/// (per [`find_header_end`]), then inserts the new header. If no header is
/// found, the body is preserved verbatim (leading blanks included).
pub(super) fn fix_script_content(content: &str, header_text: &str, style: CommentStyle) -> String {
    let mut iter = content.lines();
    let shebang = iter.next().unwrap_or("");
    let dash_open = iter.next().unwrap_or("---");
    let body_lines: Vec<&str> = iter.collect();

    let body_start = find_header_end(&body_lines, style).unwrap_or(0);

    let header_comment = style.format_header(header_text);
    let rest = body_lines[body_start..].join("\n");

    if rest.is_empty() {
        format!("{shebang}\n{dash_open}\n{header_comment}\n")
    } else {
        format!("{shebang}\n{dash_open}\n{header_comment}\n\n{rest}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multi-paragraph leading comment block — header text + a trailing
    /// blank-comment separator + a second non-empty comment paragraph —
    /// must be stripped entirely. This matches the extractor's view of
    /// the header (it treats the whole contiguous comment run as one
    /// header) and ensures `--fix` cannot leave leftover lines from a
    /// long wrong header behind when the configured header is shorter.
    #[test]
    fn find_header_end_strips_full_multi_paragraph_header_returns_exact_index() {
        let lines = ["// Old.", "//", "// More.", "fn main() {}"];
        assert_eq!(
            find_header_end(&lines, CommentStyle::DoubleSlash),
            Some(3),
            "must strip all contiguous header-comment lines, not just the first"
        );
    }

    #[test]
    fn find_header_end_no_blank_when_next_line_is_real_code() {
        let lines = ["// Old.", "fn main() {}"];
        // No trailing blank line after a 1-line header.
        assert_eq!(find_header_end(&lines, CommentStyle::DoubleSlash), Some(1));
    }

    /// The `idx += 1` that consumes the optional trailing blank line
    /// must observably advance the index past it. Mutations like
    /// `idx -= 1` (returns `Some(0)`) or `idx *= 1` (returns `Some(1)`)
    /// are caught by asserting the exact `Some(2)`.
    #[test]
    fn find_header_end_consumes_trailing_blank_line() {
        let lines = ["// Old.", "", "fn main() {}"];
        assert_eq!(find_header_end(&lines, CommentStyle::DoubleSlash), Some(2));
    }

    #[test]
    fn find_header_end_returns_none_when_no_header_lines() {
        let lines = ["fn main() {}"];
        assert_eq!(find_header_end(&lines, CommentStyle::DoubleSlash), None);
    }

    #[test]
    fn find_header_end_skips_leading_blanks() {
        let lines = ["", "", "// Header.", "fn main() {}"];
        // 2 blanks + 1 stripped + no trailing blank ⇒ 3.
        assert_eq!(find_header_end(&lines, CommentStyle::DoubleSlash), Some(3));
    }

    /// Regression for the "expected shorter than existing header" case
    /// reported in code review on PR #16: a 5-line Apache-style header
    /// in the file vs. a 1-line configured header must strip the whole
    /// existing block, not just the first line. Otherwise `--fix`
    /// produces a Frankenstein file with the new header followed by
    /// leftover Apache lines.
    #[test]
    fn strip_existing_header_strips_full_block_when_expected_is_shorter() {
        let content = "\
// Apache-style line 1.
// Apache-style line 2.
// Apache-style line 3.
// Apache-style line 4.
// Apache-style line 5.

fn main() {}
";
        let stripped = strip_existing_header(content, CommentStyle::DoubleSlash);
        assert_eq!(stripped, "fn main() {}\n", "all 5 wrong header lines must be removed");
    }
}
