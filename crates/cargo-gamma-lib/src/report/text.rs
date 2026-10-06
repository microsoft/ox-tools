// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The small pieces of phrasing every rendering here shares.

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// Width of the status verb column, matching cargo.
pub(super) const VERB_WIDTH: usize = 12;
const MAX_SCORE_PRECISION: usize = 12;

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
#[cfg_attr(coverage_nightly, coverage(off))]
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

pub(crate) fn unstyled_width(text: &str) -> usize {
    unstyled(text).width()
}

/// Fits terminal text to `width`, preserving complete control sequences and visible clusters.
pub(crate) fn fit(text: &str, width: usize) -> String {
    if unstyled_width(text) <= width {
        return text.to_owned();
    }

    let ellipsis = ".".repeat(width.min(3));
    let retained = width.saturating_sub(ellipsis.len());
    let mut shown = 0usize;
    let mut end = 0;
    let mut at = 0;

    while at < text.len() {
        let character = text[at..]
            .chars()
            .next()
            .expect("the byte cursor is advanced only along UTF-8 character boundaries");
        if character == '\u{1b}' {
            let Some(sequence_end) = control_sequence_end(text, at) else {
                break;
            };
            end = sequence_end;
            at = sequence_end;
            continue;
        }

        let cluster_start = at;
        at += character.len_utf8();
        while at < text.len() {
            let next = text[at..]
                .chars()
                .next()
                .expect("the byte cursor is advanced only along UTF-8 character boundaries");
            if next == '\u{1b}' {
                break;
            }
            if next == '\u{200d}' {
                at += next.len_utf8();
                if at < text.len() {
                    let joined = text[at..]
                        .chars()
                        .next()
                        .expect("the byte cursor is advanced only along UTF-8 character boundaries");
                    at += joined.len_utf8();
                }
                continue;
            }
            if next.width().unwrap_or(0) == 0 {
                at += next.len_utf8();
                continue;
            }
            break;
        }

        let cluster_width = text[cluster_start..at].width();
        if shown.saturating_add(cluster_width) > retained {
            break;
        }
        shown += cluster_width;
        end = at;
    }

    let mut fitted = text[..end].to_owned();
    let styled = fitted.contains('\u{1b}');
    fitted.push_str(&ellipsis);
    if styled {
        fitted.push_str("\u{1b}[0m");
    }
    fitted
}

fn control_sequence_end(text: &str, start: usize) -> Option<usize> {
    let mut characters = text[start..].char_indices();
    let (_, escape) = characters.next()?;
    debug_assert_eq!(escape, '\u{1b}');
    let (next_at, next) = characters.next()?;
    let mut end = start + next_at + next.len_utf8();
    if next != '[' {
        return Some(end);
    }
    for (at, character) in characters {
        end = start + at + character.len_utf8();
        if matches!(character, '\u{40}'..='\u{7e}') {
            return Some(end);
        }
    }
    None
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

    for precision in 1..=MAX_SCORE_PRECISION {
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn styling_is_removed_without_disturbing_the_text_around_it() {
        assert_eq!(
            unstyled("\u{1b}[1;36m    Building\u{1b}[0m [==>   ] 3/9"),
            "    Building [==>   ] 3/9"
        );
        assert_eq!(unstyled("nothing to strip"), "nothing to strip");
        assert_eq!(unstyled_width("\u{1b}[1;36mBuilding\u{1b}[0m 3/9"), "Building 3/9".chars().count());
        assert_eq!(unstyled_width("nothing to strip"), "nothing to strip".chars().count());
        assert_eq!(unstyled_width("\u{1b}[1m界e\u{301}\u{1b}[0m"), 3);
        assert_eq!(unstyled_width("\u{1b}[1m👩‍💻\u{1b}[0m"), 2);
    }

    #[test]
    fn an_escape_that_never_terminates_consumes_the_rest_rather_than_leaking_it() {
        assert_eq!(unstyled("kept\u{1b}[38;5;"), "kept");
    }

    #[test]
    fn fit_preserves_clusters_controls_and_width() {
        assert_eq!(fit("abcdefghij", 6), "abc...");
        assert_eq!(fit("界界界", 5), "界...");
        assert_eq!(fit("👩‍💻abcd", 5), "👩‍💻...");
        assert_eq!(fit("\u{1b}[1mBuilding\u{1b}[0m", 8), "\u{1b}[1mBuilding\u{1b}[0m");
        assert_eq!(unstyled(&fit("\u{1b}[1mBuilding\u{1b}[0m", 5)), "Bu...");
        assert!(fit("\u{1b}[1mBuilding\u{1b}[0m", 5).ends_with("\u{1b}[0m"));
    }

    #[test]
    fn fit_respects_widths_narrower_than_an_ellipsis() {
        assert_eq!(fit("abc", 0), "");
        assert_eq!(fit("abc", 1), ".");
        assert_eq!(fit("abc", 2), "..");
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
        assert_eq!(score(0.000_000_000_001_234, 1, usize::MAX), "0.000000000001");
        assert_eq!(
            score(0.000_000_000_000_123_4, 1, usize::MAX),
            "0.0000000000001234",
            "precision beyond the display cap must use the unrounded fallback"
        );
    }
}
