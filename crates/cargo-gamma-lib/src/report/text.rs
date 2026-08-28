// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The small pieces of phrasing every rendering here shares.

/// Width of the status verb column, matching cargo.
pub(super) const VERB_WIDTH: usize = 12;

/// The empty status column, for a line that continues the one above it.
#[must_use]
pub fn continuation() -> String {
    " ".repeat(VERB_WIDTH)
}

/// Drops the ANSI escape sequences from text, leaving what a terminal would actually show.
///
/// Only needed for text gamma did not write: cargo's own output arrives already styled, and
/// anything that inspects it — matching a prefix, counting columns — is asking about what the
/// reader sees rather than about the bytes.
#[must_use]
pub(crate) fn unstyled(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }

        // A CSI sequence runs until a byte in `@`..=`~`; anything else after the escape is a
        // two-character sequence. Either way the terminator is consumed and nothing is kept.
        if characters.next() == Some('[') {
            for byte in characters.by_ref() {
                if matches!(byte, '\u{40}'..='\u{7e}') {
                    break;
                }
            }
        }
    }

    plain
}

/// Renders a count with its noun, pluralized.
///
/// Only regular nouns are ever counted here, so the rule is the naive one.
#[must_use]
pub fn quantity(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Renders a score without rounding an inexact boundary result to exact zero or one hundred.
pub(crate) fn score(value: f64, detected: usize, valid: usize) -> String {
    if detected == 0 || detected == valid {
        return format!("{value:.1}");
    }

    for precision in 1..=12 {
        let shown = format!("{value:.precision$}");

        if shown.parse::<f64>().is_ok_and(|rounded| rounded > 0.0 && rounded < 100.0) {
            return shown;
        }
    }

    format!("{value}")
}

/// Renders a byte count the way a person would say it.
pub(crate) fn bytes(count: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "three significant digits are printed")]
    let mut size = count as f64;

    for unit in ["bytes", "KB", "MB", "GB"] {
        if size < 1024.0 {
            return if unit == "bytes" {
                format!("{count} {unit}")
            } else {
                format!("{size:.1} {unit}")
            };
        }

        size /= 1024.0;
    }

    format!("{size:.1} TB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styling_is_removed_without_disturbing_the_text_around_it() {
        assert_eq!(
            unstyled("\u{1b}[1;36m    Building\u{1b}[0m [==>   ] 3/9"),
            "    Building [==>   ] 3/9"
        );
        assert_eq!(unstyled("nothing to strip"), "nothing to strip");
    }

    #[test]
    fn an_escape_that_never_terminates_consumes_the_rest_rather_than_leaking_it() {
        assert_eq!(unstyled("kept\u{1b}[38;5;"), "kept");
    }

    #[test]
    fn one_of_something_is_singular() {
        assert_eq!(quantity(1, "file"), "1 file");
        assert_eq!(quantity(1, "build round"), "1 build round");
    }

    #[test]
    fn any_other_count_is_plural() {
        assert_eq!(quantity(0, "file"), "0 files");
        assert_eq!(quantity(2, "mutant"), "2 mutants");
    }

    #[test]
    fn a_continuation_is_exactly_the_status_column() {
        assert_eq!(continuation().len(), VERB_WIDTH);
        assert!(continuation().chars().all(char::is_whitespace));
    }

    #[test]
    fn byte_counts_read_the_way_a_person_would_say_them() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(bytes(7 * 1024 * 1024 * 1024 * 1024), "7.0 TB");
    }

    #[test]
    fn only_exact_boundary_scores_are_rendered_at_the_boundary() {
        assert_eq!(score(0.0, 0, 10_000), "0.0");
        assert_eq!(score(100.0, 10_000, 10_000), "100.0");
        assert_eq!(score(0.01, 1, 10_000), "0.01");
        assert_eq!(score(99.99, 9_999, 10_000), "99.99");
    }
}
