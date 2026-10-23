// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

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
            // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "fitting must advance past a complete control sequence")]
            at = sequence_end;
            continue;
        }

        let cluster_start = at;
        // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "fitting must advance past the visible character to make progress")]
        at += character.len_utf8();
        while at < text.len() {
            let next = text[at..]
                .chars()
                .next()
                .expect("the byte cursor is advanced only along UTF-8 character boundaries");
            if next == '\u{1b}' {
                // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "the cluster scan cannot consume an escape and must hand it back to the outer scan")]
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
            // #[gamma::skip(loop.break_to_continue, loop.delete_break, tag = "timeout", reason = "a new visible cluster ends this scan; continuing revisits the same character")]
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
