// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compiled matcher for the user-configured bug label patterns.
//!
//! Patterns are regular expressions. They are compiled once and reused for every issue,
//! since a repository can carry thousands of issues and each issue several labels.

use ohno::IntoAppError;
use regex::{RegexBuilder, RegexSet, RegexSetBuilder};

use crate::Result;

/// Matches issue labels against the configured bug label patterns.
///
/// Patterns are case-insensitive regular expressions that are matched anywhere within the
/// label, so the plain pattern `bug` still matches conventional prefixed labels such as
/// `C-bug`, `type: bug`, and `kind/bug`. Anchor a pattern with `^` and `$` to require an
/// exact match.
#[derive(Debug, Clone)]
pub struct BugLabelMatcher {
    patterns: RegexSet,
}

impl BugLabelMatcher {
    /// Compile the configured bug label patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if any pattern is not a valid regular expression.
    pub fn new(patterns: &[String]) -> Result<Self> {
        // Compile each pattern individually first so that an invalid pattern can be
        // reported along with the pattern that caused the failure.
        for pattern in patterns {
            let _ = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .into_app_err_with(|| format!("compiling bug label pattern '{pattern}'"))?;
        }

        let mut builder = RegexSetBuilder::new(patterns);
        Ok(Self {
            patterns: Self::build_pattern_set(builder.case_insensitive(true))?,
        })
    }

    fn build_pattern_set(builder: &RegexSetBuilder) -> Result<RegexSet> {
        builder.build().into_app_err("compiling bug label patterns")
    }

    /// Returns true if the label matches any of the configured patterns.
    #[must_use]
    pub fn is_match(&self, label: &str) -> bool {
        self.patterns.is_match(label)
    }
}

impl Default for BugLabelMatcher {
    /// A matcher with no patterns, which never matches and so disables bug classification.
    fn default() -> Self {
        Self {
            patterns: RegexSet::empty(),
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plain_patterns_match_anywhere_in_the_label() {
        let matcher = BugLabelMatcher::new(&["bug".to_string()]).unwrap();

        assert!(matcher.is_match("bug"));
        assert!(matcher.is_match("C-bug"));
        assert!(matcher.is_match("type: bug"));
        assert!(!matcher.is_match("enhancement"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let matcher = BugLabelMatcher::new(&["bug".to_string()]).unwrap();

        assert!(matcher.is_match("Bug"));
        assert!(matcher.is_match("BUG"));
    }

    #[test]
    fn anchored_patterns_require_an_exact_match() {
        let matcher = BugLabelMatcher::new(&["^bug$".to_string()]).unwrap();

        assert!(matcher.is_match("bug"));
        assert!(!matcher.is_match("C-bug"));
        assert!(!matcher.is_match("debugging"));
    }

    #[test]
    fn alternation_and_character_classes_are_supported() {
        let matcher = BugLabelMatcher::new(&["^(c|kind)[-/](bug|defect)$".to_string()]).unwrap();

        assert!(matcher.is_match("C-bug"));
        assert!(matcher.is_match("kind/defect"));
        assert!(!matcher.is_match("C-enhancement"));
    }

    #[test]
    fn any_pattern_can_match() {
        let matcher = BugLabelMatcher::new(&["bug".to_string(), "regression".to_string()]).unwrap();

        assert!(matcher.is_match("regression"));
        assert!(matcher.is_match("bug"));
    }

    #[test]
    fn empty_pattern_list_matches_nothing() {
        let matcher = BugLabelMatcher::new(&[]).unwrap();

        assert!(!matcher.is_match("bug"));
    }

    #[test]
    fn default_matcher_matches_nothing() {
        assert!(!BugLabelMatcher::default().is_match("bug"));
    }

    #[test]
    fn invalid_pattern_is_rejected() {
        let err = BugLabelMatcher::new(&["bug(".to_string()]).unwrap_err();

        assert!(format!("{err}").contains("bug("), "unexpected error: {err}");
    }

    #[test]
    fn aggregate_regex_set_errors_are_propagated_with_context() {
        let mut builder = RegexSetBuilder::new(["bug"]);
        let error = BugLabelMatcher::build_pattern_set(builder.size_limit(0)).expect_err("a zero-size automaton cannot be compiled");

        assert!(error.to_string().contains("compiling bug label patterns"), "{error}");
    }
}
