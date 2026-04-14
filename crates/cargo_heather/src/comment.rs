// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Comment style detection and formatting.
//!
//! Maps file extensions to their comment syntax, enabling
//! `cargo-heather` to handle headers across different file types.

use std::path::Path;

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
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn from_path_rs() {
        assert_eq!(
            CommentStyle::from_path(&PathBuf::from("src/main.rs")),
            Some(CommentStyle::DoubleSlash)
        );
    }

    #[test]
    fn from_path_toml() {
        assert_eq!(
            CommentStyle::from_path(&PathBuf::from("Cargo.toml")),
            Some(CommentStyle::Hash)
        );
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
}
