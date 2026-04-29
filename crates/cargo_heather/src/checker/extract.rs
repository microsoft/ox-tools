// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Header-comment extraction from regular source files and cargo-script
//! frontmatter.

use crate::comment::CommentStyle;

/// Collect a contiguous block of header-comment lines from `lines`, stripping
/// each line's comment prefix. Stops at the first non-comment line. Trailing
/// blank-comment lines are trimmed. Returns `None` if no comment line was
/// collected.
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

    if out.is_empty() { None } else { Some(out.join("\n")) }
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn extract_header_simple() {
        let content = "// Hello\n// World\n\nfn main() {}\n";
        let header = header_comment(content, CommentStyle::DoubleSlash).unwrap();
        assert_eq!(header, "Hello\nWorld");
    }

    #[test]
    fn extract_header_with_blank_comment_line() {
        let content = "// First\n//\n// Third\n\nfn main() {}\n";
        let header = header_comment(content, CommentStyle::DoubleSlash).unwrap();
        assert_eq!(header, "First\n\nThird");
    }

    #[test]
    fn extract_no_header() {
        let content = "fn main() {}\n";
        assert!(header_comment(content, CommentStyle::DoubleSlash).is_none());
    }

    #[test]
    fn extract_empty_content() {
        assert!(header_comment("", CommentStyle::DoubleSlash).is_none());
    }

    #[test]
    fn extract_header_trailing_empty_comment_lines_trimmed() {
        let content = "// Licensed under MIT\n//\n\nfn main() {}\n";
        let extracted = header_comment(content, CommentStyle::DoubleSlash);
        assert_eq!(extracted.as_deref(), Some("Licensed under MIT"));
    }

    #[test]
    fn extract_script_header_trailing_empty_comment_lines_trimmed() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under MIT\n#\n---\n";
        let extracted = script_header(content, CommentStyle::Hash);
        assert_eq!(extracted.as_deref(), Some("Licensed under MIT"));
    }

    #[test]
    fn extract_script_header_no_dash_line() {
        let content = "#!/usr/bin/env cargo\nnot-a-dash\n# License\n";
        let extracted = script_header(content, CommentStyle::Hash);
        assert!(extracted.is_none());
    }
}
