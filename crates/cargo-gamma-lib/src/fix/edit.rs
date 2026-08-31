// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeSet;

use camino::Utf8PathBuf;

/// One directive to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The file to patch.
    pub file: Utf8PathBuf,

    /// The one-based line the directive is written above.
    pub line: usize,

    /// The mutators named in the directive, in registry order.
    pub mutators: BTreeSet<String>,

    /// The tag used to group everything this tool wrote.
    pub tag: &'static str,
}

impl Edit {
    /// Renders the directive, indented to match the line it precedes.
    ///
    /// The generated text is a comment shaped exactly like the attribute it stands in for, so that
    /// if in-source attributes ever reach stable Rust the two slashes can simply be deleted.
    ///
    /// `ending` is the terminator the file already uses. A generated line is the only line in the
    /// file this tool wrote, and giving it a different ending from its neighbours turns a
    /// suppression into a whitespace diff that every reviewer has to look at twice.
    #[must_use]
    pub fn render(&self, indent: &str, date: &str, ending: &str) -> String {
        let selectors = self.mutators.iter().cloned().collect::<Vec<_>>().join(", ");

        format!(
            "{indent}// #[gamma::skip({selectors}, tag = \"{tag}\", reason = \"written by cargo gamma suppress {date}\")]{ending}",
            tag = self.tag
        )
    }
}

#[cfg(test)]
mod tests {
    use core::iter::once;

    use super::*;

    #[test]
    fn a_directive_names_the_exact_mutators_never_a_family() {
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 3,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let text = edit.render("    ", "2026-08-05", "\n");

        assert!(text.contains("gamma::skip(stmt.delete,"), "{text}");
        assert!(!text.contains("all"), "{text}");
        assert!(text.contains("tag = \"timeout\""), "{text}");
        assert!(text.contains("reason ="), "a directive with no reason is unauditable");
        assert!(text.ends_with(")]\n"), "{text}");
    }

    #[test]
    fn a_directive_ends_the_way_the_file_it_joins_does() {
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 3,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        assert!(edit.render("", "2026-08-05", "\r\n").ends_with(")]\r\n"));
    }
}
