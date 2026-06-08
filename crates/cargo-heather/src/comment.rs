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
    /// A `PowerShell` script (`.ps1`) or data/module file (`.psd1`, `.psm1`).
    /// Header uses `#` at the top.
    PowerShell,
    /// A Just recipe file (`*.just` or `justfile`). Header uses `#` at the top.
    Just,
    /// The repository's `constants.env` file. Header uses `#` at the top.
    Env,
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
        let file_name = path.file_name()?.to_str()?;
        if file_name.eq_ignore_ascii_case("justfile") {
            return Some(Self::Just);
        }
        if file_name == "constants.env" {
            return Some(Self::Env);
        }

        match path.extension()?.to_str()? {
            ext if ext.eq_ignore_ascii_case("rs") => {
                if content.is_some_and(is_cargo_script) {
                    Some(Self::CargoScript)
                } else {
                    Some(Self::Rust)
                }
            }
            ext if ext.eq_ignore_ascii_case("toml") => Some(Self::Toml),
            ext if ext.eq_ignore_ascii_case("ps1") || ext.eq_ignore_ascii_case("psd1") || ext.eq_ignore_ascii_case("psm1") => {
                Some(Self::PowerShell)
            }
            ext if ext.eq_ignore_ascii_case("just") => Some(Self::Just),
            _ => None,
        }
    }

    /// The comment style used for headers in this file kind.
    #[must_use]
    pub const fn comment_style(self) -> CommentStyle {
        match self {
            Self::Rust => CommentStyle::DoubleSlash,
            Self::Toml | Self::PowerShell | Self::Just | Self::Env | Self::CargoScript => CommentStyle::Hash,
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
        FileKind::detect(path, None).map(FileKind::comment_style)
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
    pub fn format_header(self, header_text: &str, line_ending: &str) -> String {
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
            .join(line_ending)
    }

    /// Returns `true` if `trimmed` is a header comment line for this style.
    ///
    /// For Rust files, excludes doc comments (`///` and `//!`).
    /// For hash-commented files, any `#` line is a valid comment.
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

    #[test]
    fn inner_attribute_is_not_cargo_script() {
        // `#![...]` is a Rust inner attribute, not a shebang.
        assert!(!is_cargo_script("#![allow(unused)]\n---\n"));
    }

    #[test]
    fn shebang_without_frontmatter_is_not_cargo_script() {
        assert!(!is_cargo_script("#!/usr/bin/env cargo\nfn main() {}\n"));
    }

    #[test]
    fn valid_cargo_script() {
        assert!(is_cargo_script("#!/usr/bin/env cargo\n---\n"));
    }

    #[test]
    fn doc_comment_is_not_header_comment() {
        let style = CommentStyle::DoubleSlash;
        // `///` is a doc comment, not a header comment.
        assert!(!style.is_header_comment_line("///"));
        assert!(!style.is_header_comment_line("/// doc"));
    }

    #[test]
    fn inner_doc_comment_is_not_header_comment() {
        let style = CommentStyle::DoubleSlash;
        // `//!` is an inner doc comment, not a header comment.
        assert!(!style.is_header_comment_line("//!"));
        assert!(!style.is_header_comment_line("//! module doc"));
    }

    #[test]
    fn regular_comment_is_header_comment() {
        let style = CommentStyle::DoubleSlash;
        assert!(style.is_header_comment_line("// Copyright"));
        assert!(style.is_header_comment_line("//"));
    }

    #[test]
    fn detect_powershell_script_by_ps1_extension() {
        assert_eq!(FileKind::detect(Path::new("build.ps1"), None), Some(FileKind::PowerShell));
        // Case-insensitive — Windows paths often surface as `.PS1`.
        assert_eq!(FileKind::detect(Path::new("BUILD.PS1"), None), Some(FileKind::PowerShell));
    }

    #[test]
    fn detect_powershell_data_file_by_psd1_extension() {
        // `.psd1` is plain PowerShell data syntax with the same `#`
        // line-comment style as `.ps1`; should be classified as
        // PowerShell so `cargo-heather` can validate / fix headers.
        assert_eq!(FileKind::detect(Path::new("Module.psd1"), None), Some(FileKind::PowerShell));
        assert_eq!(FileKind::detect(Path::new("scenario.psd1"), None), Some(FileKind::PowerShell));
        assert_eq!(FileKind::detect(Path::new("MODULE.PSD1"), None), Some(FileKind::PowerShell));
    }

    #[test]
    fn detect_powershell_module_by_psm1_extension() {
        // `.psm1` is a PowerShell module file; same `#` comment style as `.ps1`.
        assert_eq!(FileKind::detect(Path::new("Module.psm1"), None), Some(FileKind::PowerShell));
        assert_eq!(FileKind::detect(Path::new("MODULE.PSM1"), None), Some(FileKind::PowerShell));
    }

    #[test]
    fn detect_returns_none_for_unsupported_extensions() {
        assert_eq!(FileKind::detect(Path::new("notes.txt"), None), None);
        assert_eq!(FileKind::detect(Path::new("README.md"), None), None);
        // Confirm we did not start matching on a substring (e.g. `ps`, `psd`).
        assert_eq!(FileKind::detect(Path::new("a.ps"), None), None);
        assert_eq!(FileKind::detect(Path::new("a.psd"), None), None);
        assert_eq!(FileKind::detect(Path::new("a.psm"), None), None);
    }
}
