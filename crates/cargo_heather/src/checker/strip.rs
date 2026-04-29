// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header-block stripping for `--fix` mode.
//!
//! Both regular files and cargo-script frontmatter blocks use the same algorithm:
//! skip leading blanks, then strip up to N header-comment lines, then
//! optionally consume one blank-comment paragraph separator and one blank
//! line. The two callers differ only in how they reassemble the output.

use crate::comment::CommentStyle;

/// Find where a header block ends in `lines`.
///
/// Algorithm:
/// 1. Skip leading blank lines.
/// 2. Strip up to `max_header_lines` consecutive header-comment lines.
///    Blank-comment lines (e.g. `//` or `#`) count as comment lines so
///    multi-paragraph configured headers are removed cleanly.
/// 3. If at least one header line was stripped, optionally consume one
///    blank-comment paragraph separator, then optionally one blank line.
///
/// Returns `Some(body_start_idx)` if a header block was found, else `None`.
fn find_header_end(lines: &[&str], style: CommentStyle, max_header_lines: usize) -> Option<usize> {
    let blanks = lines.iter().take_while(|l| l.trim().is_empty()).count();

    let stripped = lines[blanks..]
        .iter()
        .take(max_header_lines)
        .take_while(|l| style.is_header_comment_line(l.trim()))
        .count();

    if stripped == 0 {
        return None;
    }

    let mut idx = blanks + stripped;

    // Optional blank-comment paragraph separator (e.g. `//` or `#`).
    if let Some(line) = lines.get(idx) {
        let trimmed = line.trim();
        if style.is_header_comment_line(trimmed) && style.strip_prefix(trimmed).is_empty() {
            idx += 1;
        }
    }

    // Optional blank line after the header paragraph.
    if lines.get(idx).is_some_and(|l| l.trim().is_empty()) {
        idx += 1;
    }

    Some(idx)
}

/// Strip the leading header from regular file content.
///
/// If no header is found, returns content unchanged. Trailing newline is
/// preserved if and only if the original content had one.
pub(super) fn strip_existing_header(content: &str, style: CommentStyle, expected_header: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let max_header_lines = expected_header.lines().count();

    let Some(body_start) = find_header_end(&lines, style, max_header_lines) else {
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

    let max_header_lines = header_text.lines().count();
    let body_start = find_header_end(&body_lines, style, max_header_lines).unwrap_or(0);

    let header_comment = style.format_header(header_text);
    let rest = body_lines[body_start..].join("\n");

    if rest.is_empty() {
        format!("{shebang}\n{dash_open}\n{header_comment}\n")
    } else {
        format!("{shebang}\n{dash_open}\n{header_comment}\n\n{rest}\n")
    }
}
