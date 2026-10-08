// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Matching for Cargo-style package selectors.

/// Parse the same package-selector glob syntax Cargo accepts for `--package`.
pub(crate) fn parse(pattern: &str) -> Result<glob::Pattern, glob::PatternError> {
    glob::Pattern::new(pattern)
}

/// Encode terminal control characters before including a selector in a diagnostic.
pub(crate) fn diagnostic(pattern: &str) -> String {
    let mut encoded = String::with_capacity(pattern.len());
    for character in pattern.chars() {
        if character.is_control() {
            encoded.extend(character.escape_default());
        } else {
            encoded.push(character);
        }
    }
    encoded
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{diagnostic, parse};

    #[test]
    fn matches_complete_selector_semantics() {
        for (pattern, name, expected) in [
            ("alpha*", "alpha", true),
            ("alpha*", "alpha_macros", true),
            ("*macros", "alpha_macros", true),
            ("*alpha*", "my_alpha_lib", true),
            ("a?pha", "alpha", true),
            ("lib[12]", "lib1", true),
            ("lib[12]", "lib3", false),
            ("lib[!2]", "lib1", true),
            ("lib[!2]", "lib2", false),
            ("alpha", "alphax", false),
            ("alpha*", "beta", false),
            ("a?pha", "axxpha", false),
            ("a*b", "ac", false),
            ("a*b", "axyz", false),
            ("a?", "a", false),
            ("*a", "a", true),
            ("*a", "ba", true),
            ("*a", "", false),
            ("?a", "a", false),
            ("ab*cd", "abxxcd", true),
            ("", "", true),
            ("*", "", true),
            ("**", "", true),
            ("?", "", false),
            ("?*", "a", true),
            ("*?", "a", true),
            ("?*?", "a", false),
            ("*ab", "aaab", true),
            ("*ab", "aaaa", false),
            ("a*ab", "aaab", true),
            ("a*ab", "aaba", false),
            ("*a*b", "xxaayb", true),
            ("*a*b", "xxaaya", false),
            ("?*?", "ab", true),
        ] {
            assert_eq!(
                parse(pattern).expect("valid pattern").matches(name),
                expected,
                "{pattern:?} against {name:?}"
            );
        }
    }

    #[test]
    fn long_selector_is_bounded() {
        assert!(
            !parse(&format!("{}z", "a".repeat(10_000)))
                .expect("valid pattern")
                .matches(&"a".repeat(10_000))
        );
    }

    #[test]
    fn malformed_selector_is_rejected() {
        parse("lib[").expect_err("an unclosed character class is invalid");
    }

    #[test]
    fn diagnostic_encodes_controls_and_preserves_printable_text() {
        assert_eq!(diagnostic("lib[\n\r\u{1b}[2J"), r"lib[\n\r\u{1b}[2J");
        assert_eq!(diagnostic("alpha-*"), "alpha-*");
    }
}
