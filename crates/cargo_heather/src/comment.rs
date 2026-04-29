// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Comment style detection and formatting.
//!
//! Maps file extensions to their comment syntax, enabling
//! `cargo-heather` to handle headers across different file types.

use std::path::Path;

/// The kind of source file, determining header placement and comment style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A regular Rust source file (`.rs`). Header uses `//` at the top.
    Rust,
    /// A TOML file (`.toml`). Header uses `#` at the top.
    Toml,
    /// A Rust cargo-script file (`.rs` with shebang + `---` frontmatter).
    /// Header uses `#` after the opening `---`.
    CargoScript,
}

impl FileKind {
    /// Detect the file kind from a path and (optionally) content.
    ///
    /// For `.rs` files, pass the content to distinguish cargo-scripts from
    /// regular Rust files. If `content` is `None`, assumes regular Rust.
    #[must_use]
    pub fn detect(path: &Path, content: Option<&str>) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => {
                if content.is_some_and(is_cargo_script) {
                    Some(Self::CargoScript)
                } else {
                    Some(Self::Rust)
                }
            }
            "toml" => Some(Self::Toml),
            _ => None,
        }
    }

    /// The comment style used for headers in this file kind.
    #[must_use]
    pub const fn comment_style(self) -> CommentStyle {
        match self {
            Self::Rust => CommentStyle::DoubleSlash,
            Self::Toml | Self::CargoScript => CommentStyle::Hash,
        }
    }
}

/// Returns `true` if the content looks like a cargo-script file.
///
/// A cargo-script starts with a shebang (`#!`, but not `#![` which is a
/// Rust inner attribute) on the first line, followed by `---` on the second.
#[must_use]
pub fn is_cargo_script(content: &str) -> bool {
    let mut lines = content.lines();
    let Some(first) = lines.next() else {
        return false;
    };
    if !first.starts_with("#!") || first.starts_with("#![") {
        return false;
    }
    lines.next().is_some_and(|l| l.trim() == "---")
}

/// Comment syntax for a supported file type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentStyle {
    /// `//` line comments used in Rust (`.rs`) files.
    DoubleSlash,
    /// `#` line comments used in TOML (`.toml`) files.
    Hash,
}

impl CommentStyle {
    /// Detect the comment style from a file path's extension.
    ///
    /// Returns `None` for unsupported file types.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::DoubleSlash),
            "toml" => Some(Self::Hash),
            _ => None,
        }
    }

    /// The bare comment prefix (e.g. `"//"` or `"#"`).
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::DoubleSlash => "//",
            Self::Hash => "#",
        }
    }

    /// The comment prefix followed by a space (e.g. `"// "` or `"# "`).
    #[must_use]
    pub const fn prefix_space(self) -> &'static str {
        match self {
            Self::DoubleSlash => "// ",
            Self::Hash => "# ",
        }
    }

    /// Build the expected commented header lines for this style.
    ///
    /// Converts plain-text header into commented lines.
    #[must_use]
    pub fn format_header(self, header_text: &str) -> String {
        header_text
            .lines()
            .map(|line| {
                if line.is_empty() {
                    self.prefix().to_owned()
                } else {
                    format!("{}{line}", self.prefix_space())
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns `true` if `trimmed` is a header comment line for this style.
    ///
    /// For Rust files, excludes doc comments (`///` and `//!`).
    /// For TOML files, any `#` line is a valid comment.
    #[must_use]
    pub fn is_header_comment_line(self, trimmed: &str) -> bool {
        match self {
            Self::DoubleSlash => {
                if !trimmed.starts_with("//") {
                    return false;
                }
                let after = &trimmed[2..];
                !after.starts_with('/') && !after.starts_with('!')
            }
            Self::Hash => trimmed.starts_with('#'),
        }
    }

    /// Strip the comment prefix from a line, removing the optional trailing space.
    #[must_use]
    pub fn strip_prefix(self, line: &str) -> String {
        let prefix_len = self.prefix().len();
        let after = &line[prefix_len..];
        if let Some(stripped) = after.strip_prefix(' ') {
            stripped.to_owned()
        } else {
            after.to_owned()
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn from_path_rs() {
        assert_eq!(
            CommentStyle::from_path(&PathBuf::from("src/main.rs")),
            Some(CommentStyle::DoubleSlash)
        );
    }

    #[test]
    fn from_path_toml() {
        assert_eq!(CommentStyle::from_path(&PathBuf::from("Cargo.toml")), Some(CommentStyle::Hash));
    }

    #[test]
    fn from_path_unsupported() {
        assert_eq!(CommentStyle::from_path(&PathBuf::from("README.md")), None);
    }

    #[test]
    fn from_path_no_extension() {
        assert_eq!(CommentStyle::from_path(&PathBuf::from("Makefile")), None);
    }

    #[test]
    fn prefix_double_slash() {
        assert_eq!(CommentStyle::DoubleSlash.prefix(), "//");
        assert_eq!(CommentStyle::DoubleSlash.prefix_space(), "// ");
    }

    #[test]
    fn prefix_hash() {
        assert_eq!(CommentStyle::Hash.prefix(), "#");
        assert_eq!(CommentStyle::Hash.prefix_space(), "# ");
    }

    #[test]
    fn format_header_double_slash_single_line() {
        let result = CommentStyle::DoubleSlash.format_header("MIT License");
        assert_eq!(result, "// MIT License");
    }

    #[test]
    fn format_header_double_slash_multiline() {
        let result = CommentStyle::DoubleSlash.format_header("Line one\n\nLine three");
        assert_eq!(result, "// Line one\n//\n// Line three");
    }

    #[test]
    fn format_header_hash_single_line() {
        let result = CommentStyle::Hash.format_header("MIT License");
        assert_eq!(result, "# MIT License");
    }

    #[test]
    fn format_header_hash_multiline() {
        let result = CommentStyle::Hash.format_header("Line one\n\nLine three");
        assert_eq!(result, "# Line one\n#\n# Line three");
    }

    #[test]
    fn is_header_comment_line_rs_regular() {
        let style = CommentStyle::DoubleSlash;
        assert!(style.is_header_comment_line("// hello"));
        assert!(style.is_header_comment_line("//"));
        assert!(style.is_header_comment_line("// "));
    }

    #[test]
    fn is_header_comment_line_rs_excludes_doc() {
        let style = CommentStyle::DoubleSlash;
        assert!(!style.is_header_comment_line("/// doc"));
        assert!(!style.is_header_comment_line("//! module doc"));
    }

    #[test]
    fn is_header_comment_line_rs_rejects_non_comment() {
        let style = CommentStyle::DoubleSlash;
        assert!(!style.is_header_comment_line("fn main() {}"));
        assert!(!style.is_header_comment_line(""));
    }

    #[test]
    fn is_header_comment_line_hash() {
        let style = CommentStyle::Hash;
        assert!(style.is_header_comment_line("# hello"));
        assert!(style.is_header_comment_line("#"));
        assert!(style.is_header_comment_line("# "));
    }

    #[test]
    fn is_header_comment_line_hash_rejects_non_comment() {
        let style = CommentStyle::Hash;
        assert!(!style.is_header_comment_line("[package]"));
        assert!(!style.is_header_comment_line(""));
        assert!(!style.is_header_comment_line("name = \"foo\""));
    }

    #[test]
    fn strip_prefix_double_slash_with_space() {
        assert_eq!(CommentStyle::DoubleSlash.strip_prefix("// Hello"), "Hello");
    }

    #[test]
    fn strip_prefix_double_slash_without_space() {
        assert_eq!(CommentStyle::DoubleSlash.strip_prefix("//Hello"), "Hello");
    }

    #[test]
    fn strip_prefix_double_slash_empty() {
        assert_eq!(CommentStyle::DoubleSlash.strip_prefix("//"), "");
    }

    #[test]
    fn strip_prefix_hash_with_space() {
        assert_eq!(CommentStyle::Hash.strip_prefix("# Hello"), "Hello");
    }

    #[test]
    fn strip_prefix_hash_without_space() {
        assert_eq!(CommentStyle::Hash.strip_prefix("#Hello"), "Hello");
    }

    #[test]
    fn strip_prefix_hash_empty() {
        assert_eq!(CommentStyle::Hash.strip_prefix("#"), "");
    }

    #[test]
    fn is_cargo_script_detects_shebang_and_frontmatter() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n# License\n";
        assert!(is_cargo_script(content));
    }

    #[test]
    fn is_cargo_script_rejects_inner_attribute() {
        let content = "#![allow(unused)]\nfn main() {}\n";
        assert!(!is_cargo_script(content));
    }

    #[test]
    fn is_cargo_script_rejects_no_frontmatter() {
        let content = "#!/usr/bin/env cargo\nfn main() {}\n";
        assert!(!is_cargo_script(content));
    }

    #[test]
    fn is_cargo_script_rejects_empty() {
        assert!(!is_cargo_script(""));
    }

    #[test]
    fn is_cargo_script_rejects_non_shebang_with_frontmatter() {
        // First line doesn't start with `#!` but second line is `---`
        let content = "fn main() {}\n---\nsome content\n";
        assert!(!is_cargo_script(content));
    }

    #[test]
    fn file_kind_detect_regular_rs() {
        let kind = FileKind::detect(&PathBuf::from("src/main.rs"), Some("fn main() {}"));
        assert_eq!(kind, Some(FileKind::Rust));
    }

    #[test]
    fn file_kind_detect_cargo_script() {
        let content = "#!/usr/bin/env -S cargo +nightly -Zscript\n---\n";
        let kind = FileKind::detect(&PathBuf::from("script.rs"), Some(content));
        assert_eq!(kind, Some(FileKind::CargoScript));
    }

    #[test]
    fn file_kind_detect_toml() {
        let kind = FileKind::detect(&PathBuf::from("Cargo.toml"), None);
        assert_eq!(kind, Some(FileKind::Toml));
    }

    #[test]
    fn file_kind_detect_unsupported() {
        let kind = FileKind::detect(&PathBuf::from("README.md"), None);
        assert_eq!(kind, None);
    }

    #[test]
    fn file_kind_comment_style() {
        assert_eq!(FileKind::Rust.comment_style(), CommentStyle::DoubleSlash);
        assert_eq!(FileKind::Toml.comment_style(), CommentStyle::Hash);
        assert_eq!(FileKind::CargoScript.comment_style(), CommentStyle::Hash);
    }
}
