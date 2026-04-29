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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    // --- strip_existing_header ---

    #[test]
    fn removes_comment_block() {
        let content = "// Old header\n// Second line\n\nfn main() {}\n";
        let expected = "Old header\nSecond line";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, expected);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn no_comment() {
        let content = "fn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
        assert_eq!(result, content);
    }

    #[test]
    fn preserves_lack_of_trailing_newline() {
        // Original content has no trailing newline → stripped result must also lack it.
        let content = "// Old header\n\nfn main() {}";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Old header");
        assert_eq!(result, "fn main() {}");
    }

    #[test]
    fn only_comment() {
        let content = "// Just a comment\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Just a comment");
        assert!(result.is_empty());
    }

    #[test]
    fn with_leading_blanks() {
        let content = "\n\n// Header\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn toml_strip() {
        let content = "# Old header\n# Second line\n\n[package]\n";
        let expected = "Old header\nSecond line";
        let result = strip_existing_header(content, CommentStyle::Hash, expected);
        assert_eq!(result, "[package]\n");
    }

    #[test]
    fn preserves_descriptive_comment_paragraph() {
        let content = "// Old wrong header\n//\n// Module description.\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Old wrong header");
        assert_eq!(result, "// Module description.\n\nfn main() {}\n");
    }

    #[test]
    fn preserves_multi_line_descriptive_paragraph() {
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
    fn no_paragraph_separator_strips_whole_block() {
        let content = "// Wrong header\n// Wrong line 2\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Wrong header\nWrong line 2");
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn multi_paragraph_configured_strips_all_paragraphs() {
        let content = "// Apache License 2.0\n//\n// Licensed under the old text.\n\nfn main() {}\n";
        let expected = "Apache License 2.0\n\nLicensed under the new text.";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, expected);
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn only_blank_lines() {
        let content = "\n\n\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Header");
        assert_eq!(result, content);
    }

    #[test]
    fn only_a_single_header_line() {
        let content = "// Old header\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Old header");
        assert_eq!(result, "");
    }

    #[test]
    fn strip_n_does_not_eat_descriptive_comment() {
        let content = "// Wrong header\n// Descriptive (preserve)\n\nfn main() {}\n";
        let result = strip_existing_header(content, CommentStyle::DoubleSlash, "Wrong header");
        assert!(result.contains("// Descriptive (preserve)"));
        assert!(!result.contains("Wrong header"));
    }

    // --- fix_script_content ---

    #[test]
    fn script_adds_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.starts_with("#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Licensed under the MIT License.\n"));
        assert!(fixed.contains("[package]"));
        assert!(fixed.contains("fn main()"));
    }

    #[test]
    fn script_replaces_header() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Old Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.contains("# Licensed under the MIT License."));
        assert!(!fixed.contains("Old Header"));
        assert!(fixed.contains("[package]"));
    }

    #[test]
    fn script_preserves_descriptive_comments() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Apache License 2.0\n#\n# Helper script description.\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "Licensed under the MIT License.", CommentStyle::Hash);
        assert!(fixed.contains("# Licensed under the MIT License."));
        assert!(fixed.contains("# Helper script description."));
        assert!(!fixed.contains("Apache"));
    }

    #[test]
    fn script_empty_rest() {
        let content = "#!/usr/bin/env cargo\n---\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.starts_with("#!/usr/bin/env cargo\n---\n# MIT License\n"));
        assert!(!fixed.contains("\n\n"));
    }

    #[test]
    fn script_strips_blank_line_between_old_header_and_manifest() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# Old Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "New Header", CommentStyle::Hash);
        assert_eq!(
            fixed,
            "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# New Header\n\n[package]\nedition = \"2024\"\n---\nfn main() {}\n"
        );
    }

    #[test]
    fn script_header_only_no_trailing_lines() {
        let content = "#!/usr/bin/env script\n---\n# Old header\n---\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(!fixed.contains("Old header"));
    }

    #[test]
    fn script_consumes_blank_comment_separator() {
        let content = "#!/usr/bin/env script\n---\n# Old header\n#\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License\n\n[package]"));
        assert!(!fixed.contains("# MIT License\n\n#\n"));
    }

    #[test]
    fn script_consumes_blank_line_after_header() {
        let content = "#!/usr/bin/env script\n---\n# Old header\n\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License\n\n[package]"));
        assert!(!fixed.contains("\n\n\n[package]"));
    }

    #[test]
    fn script_skips_leading_blank_lines() {
        let content = "#!/usr/bin/env script\n---\n\n\n# Old wrong header\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(!fixed.contains("Old wrong header"));
    }

    #[test]
    fn script_strip_n_does_not_eat_descriptive_comment() {
        let content =
            "#!/usr/bin/env script\n---\n# Wrong header\n# Descriptive (preserve)\n[package]\nname = \"foo\"\n---\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT License", CommentStyle::Hash);
        assert!(fixed.contains("# MIT License"));
        assert!(fixed.contains("# Descriptive (preserve)"));
        assert!(!fixed.contains("# Wrong header"));
    }

    #[test]
    fn script_strip_n_consumes_all_body_no_panic() {
        let content = "#!/usr/bin/env script\n---\n# A\n# B\n";
        let fixed = fix_script_content(content, "MIT\nLicense", CommentStyle::Hash);
        assert!(fixed.contains("# MIT"));
        assert!(fixed.contains("# License"));
        assert!(!fixed.contains("# A"));
        assert!(!fixed.contains("# B"));
    }

    #[test]
    fn script_no_header_preserves_leading_blank() {
        let content = "#!/usr/bin/env script\n---\n\nfn main() {}\n";
        let fixed = fix_script_content(content, "MIT", CommentStyle::Hash);
        assert!(fixed.contains("# MIT\n\n\nfn main() {}"));
    }
}
