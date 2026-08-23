// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared phrasing helpers: durations, counts, shares, wrapping, and heading slugs.

use core::time::Duration;

/// Pluralizes a count's noun, because "1 mutants" reads as a bug in the tool.
pub(super) fn plural(count: u32, noun: &str) -> String {
    if count == 1 { noun.to_owned() } else { format!("{noun}s") }
}

/// Renders a duration the way a person would say it.
#[must_use]
pub fn human(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();

    if seconds < 1.0 {
        return format!("{}ms", duration.as_millis());
    }

    if seconds < 90.0 {
        return format!("{seconds:.1}s");
    }

    let minutes = seconds / 60.0;

    if minutes < 90.0 {
        return format!("{minutes:.0}m");
    }

    format!("{:.1}h", minutes / 60.0)
}

/// Capitalizes the first letter, for prose written to follow a lowercase console label.
///
/// The findings phrase themselves to sit after `remedy:` and `costs:`, which reads correctly there
/// and like a typo after a bold Markdown heading.
pub(super) fn sentence(text: &str) -> String {
    let mut chars = text.chars();

    chars
        .next()
        .map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + chars.as_str())
}

/// One duration as a percentage of another, for a table column.
pub(super) fn share(part: Duration, whole: Duration) -> String {
    if whole.is_zero() {
        return "—".to_owned();
    }

    format!("{:.0}%", part.as_secs_f64() * 100.0 / whole.as_secs_f64())
}

/// A GitHub-style anchor for a heading, so the table of contents actually resolves.
///
/// Matches the rule GitHub, GitLab and most static site generators share: lowercase, drop anything
/// that is not a letter, a digit, a space or a hyphen, then turn spaces into hyphens.
pub(super) fn slug(heading: &str) -> String {
    let mut anchor = String::with_capacity(heading.len());

    for character in heading.chars() {
        if character.is_alphanumeric() {
            anchor.extend(character.to_lowercase());
        } else if character == ' ' || character == '-' {
            anchor.push('-');
        }
    }

    anchor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_of_one_are_not_pluralized() {
        assert_eq!(plural(1, "mutant"), "mutant");
        assert_eq!(plural(0, "mutant"), "mutants");
        assert_eq!(plural(2, "mutant"), "mutants");
    }

    #[test]
    fn durations_read_the_way_a_person_says_them() {
        assert_eq!(human(Duration::from_millis(250)), "250ms");
        assert_eq!(human(Duration::from_secs(9)), "9.0s");
        assert_eq!(human(Duration::from_mins(10)), "10m");
        assert_eq!(human(Duration::from_hours(2)), "2.0h");
        assert_eq!(human(Duration::from_secs(90)), "2m");
        assert_eq!(human(Duration::from_mins(90)), "1.5h");
    }

    #[test]
    fn sentences_and_shares_cover_empty_and_nonzero_inputs() {
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("hello world"), "Hello world");
        assert_eq!(share(Duration::from_secs(1), Duration::from_secs(4)), "25%");
        assert_eq!(share(Duration::from_secs(1), Duration::ZERO), "—");
    }

    #[test]
    fn a_slug_matches_the_anchor_a_markdown_renderer_would_generate() {
        assert_eq!(slug("Yield by mutator family"), "yield-by-mutator-family");
        assert_eq!(slug("1. 97% of the run was the build"), "1-97-of-the-run-was-the-build");
    }
}
