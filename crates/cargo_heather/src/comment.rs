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
