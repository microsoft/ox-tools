// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header-comment extraction from regular source files and cargo-script
//! frontmatter.

use crate::comment::CommentStyle;

/// Collect a contiguous block of header-comment lines from `lines`, stripping
/// each line's comment prefix. Stops at the first non-comment line. Trailing
/// blank-comment lines are trimmed. Returns `None` if no comment line was
/// collected, or if the collected block does not look like a license header
/// (see [`looks_like_license_header`]).
fn collect_comment_block<'a, I>(lines: I, style: CommentStyle) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if !style.is_header_comment_line(trimmed) {
            break;
        }
        out.push(style.strip_prefix(trimmed));
    }

    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }

    if out.is_empty() {
        return None;
    }
    let block = out.join("\n");
    looks_like_license_header(&block).then_some(block)
}

/// Heuristic: does `block` look like a license / copyright header?
///
/// Returns `true` if any line contains a license-related keyword
/// (case-insensitive): `license`, `copyright`, or `spdx`. This prevents
/// `cargo-heather` from treating an unrelated leading `//` comment as a
/// header to be replaced.
fn looks_like_license_header(block: &str) -> bool {
    const KEYWORDS: &[&str] = &["license", "copyright", "spdx"];
    let lower = block.to_ascii_lowercase();
    KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Extract the first contiguous block of comment lines from file content.
///
/// Skips leading blank lines. Returns `None` if the first non-blank line
/// is not a header comment.
pub(super) fn header_comment(content: &str, style: CommentStyle) -> Option<String> {
    // Skip leading blank lines, then defer to the shared collector. If the
    // first non-blank line isn't a header comment, the collector breaks
    // immediately and returns None — matching the original behaviour.
    let lines = content.lines().skip_while(|l| l.trim().is_empty());
    collect_comment_block(lines, style)
}

/// Extract the header comment block from inside a cargo-script frontmatter.
///
/// Expects the file to start with a shebang and `---`. Extracts comment lines
/// immediately after the opening `---`, stopping at the first blank or
/// non-comment line.
pub(super) fn script_header(content: &str, style: CommentStyle) -> Option<String> {
    let mut lines = content.lines();
    lines.next()?; // shebang
    if lines.next()?.trim() != "---" {
        return None;
    }
    collect_comment_block(lines, style)
}
