// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use core::fmt::Write as _;
use core::iter::once;
use std::borrow::Cow;

use cargo_gamma_engine::text::{encode_controls, encode_preserving_color};

mod text_corpus;

use text_corpus::{CONTROLS, SEQUENCES};

const STYLE_RESET: &str = "\x1b[0m";

/// Parses the permitted styling independently so production cannot define its own test oracle.
fn allowed_sgr_prefix(text: &str) -> Option<usize> {
    let tail = text.strip_prefix("\x1b[")?;
    let end = tail.find('m')?;
    let parameters = &tail[..end];
    if !parameters.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':')) {
        return None;
    }
    if parameters.is_empty() {
        return Some(end + 3);
    }
    let fields = parameters.split(';').collect::<Vec<_>>();
    let mut at = 0;
    while let Some(field) = fields.get(at) {
        if field.contains(':') {
            let parts = field.split(':').collect::<Vec<_>>();
            let ["38" | "48", mode, rest @ ..] = parts.as_slice() else {
                return None;
            };
            let valid = match (*mode, rest) {
                ("5", [value]) => value.parse::<u8>().is_ok(),
                ("2", [red, green, blue] | ["", red, green, blue]) => [red, green, blue].iter().all(|value| value.parse::<u8>().is_ok()),
                _ => false,
            };
            if !valid {
                return None;
            }
            at += 1;
            continue;
        }
        let code = field.parse::<u16>().ok()?;
        // Only color, bold, and their corresponding reset forms are permitted.
        match code {
            0 | 1 | 22 | 30..=37 | 39 | 40..=47 | 49 | 90..=97 | 100..=107 => at += 1,
            38 | 48 => {
                let count = match fields.get(at + 1)?.parse::<u16>().ok()? {
                    5 => 1,
                    2 => 3,
                    _ => return None,
                };
                let values = fields.get(at + 2..at + 2 + count)?;
                if !values.iter().all(|value| value.parse::<u8>().is_ok()) {
                    return None;
                }
                at += 2 + count;
            }
            _ => return None,
        }
    }
    Some(end + 3)
}

/// Independently encodes terminal-safe output rather than deriving expectations from production.
fn reference_encoding(mut text: &str, color: bool) -> String {
    let mut output = String::new();
    let mut styled = false;
    while !text.is_empty() {
        if color && let Some(end) = allowed_sgr_prefix(text) {
            output.push_str(&text[..end]);
            text = &text[end..];
            styled = true;
            continue;
        }
        let character = text.chars().next().expect("the loop condition excludes an empty string");
        text = &text[character.len_utf8()..];
        match character {
            '\0' => output.push_str("\\0"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\x1b' => output.push_str("\\e"),
            control if control.is_control() => {
                let _ = write!(output, "\\u{{{:02x}}}", u32::from(control));
            }
            printable => output.push(printable),
        }
    }
    if styled {
        output.push_str(STYLE_RESET);
    }
    output
}

fn assert_encoding_policies(text: &str) {
    let encoded = encode_controls(text);
    assert_eq!(encoded, reference_encoding(text, false), "{text:?}");
    assert!(!encoded.chars().any(char::is_control), "{text:?}: {encoded:?}");
    let encoded = encode_preserving_color(text);
    assert_eq!(encoded, reference_encoding(text, true), "{text:?}");
    let mut remaining = encoded.as_ref();
    while let Some(character) = remaining.chars().next() {
        if character == '\x1b' {
            let end = allowed_sgr_prefix(remaining).unwrap_or_else(|| panic!("unsafe escape in {encoded:?}"));
            remaining = &remaining[end..];
        } else {
            assert!(!character.is_control(), "{text:?}: {encoded:?}");
            remaining = &remaining[character.len_utf8()..];
        }
    }
}

#[test]
fn checked_in_terminal_corpus_obeys_both_encoding_policies() {
    for seed in CONTROLS.iter().chain(SEQUENCES) {
        assert_encoding_policies(seed);
    }
}

#[test]
#[cfg(not(miri))]
fn arbitrary_multibyte_text_and_seeded_sequences_obey_both_encoding_policies() {
    bolero::check!()
        .with_type::<String>()
        .for_each(|text| assert_encoding_policies(text));
    bolero::check!()
        .with_type::<(String, u16, String)>()
        .for_each(|(before, seed, after)| {
            let index = usize::from(*seed) % (CONTROLS.len() + SEQUENCES.len());
            let seed = CONTROLS.iter().chain(SEQUENCES).nth(index).unwrap();
            assert_encoding_policies(&format!("{before}{seed}{after}"));
        });
}

#[test]
fn mixed_unicode_controls_do_not_hide_del_or_change_safe_text_ownership() {
    let text = "é\0\t\n\r\x1b\u{7f}\u{80}🦀";
    let expected = "é\\0\\t\\n\\r\\e\\u{7f}\\u{80}🦀";
    assert_eq!(encode_controls(text), expected);
    assert_eq!(encode_preserving_color(text), expected);
    assert!(matches!(encode_controls("é🦀"), Cow::Borrowed("é🦀")));
    assert_eq!(encode_preserving_color("\x1b[31mé\u{7f}"), "\x1b[31mé\\u{7f}\x1b[0m");
}

#[test]
fn ordinary_text_is_borrowed_unchanged() {
    let encoded = encode_controls("src/lib.rs::tests::boundary — replace a < b with a <= b");
    assert!(matches!(encoded, Cow::Borrowed(_)), "ordinary text must not allocate");
    assert_eq!(encoded, "src/lib.rs::tests::boundary — replace a < b with a <= b");
}

#[test]
fn a_newline_cannot_forge_a_second_line() {
    assert_eq!(encode_controls("a\nb"), "a\\nb");
    assert_eq!(encode_controls("a\r\nb"), "a\\r\\nb");
}

#[test]
fn the_erase_sequence_is_shown_rather_than_obeyed() {
    assert_eq!(encode_controls("evil\r\u{1b}[2Kforged"), "evil\\r\\e[2Kforged");
}

#[test]
fn an_osc_hyperlink_is_encoded_in_both_of_its_spellings() {
    assert_eq!(
        encode_controls("\u{1b}]8;;https://example.test\u{7}click\u{1b}]8;;\u{7}"),
        "\\e]8;;https://example.test\\u{07}click\\e]8;;\\u{07}"
    );
    assert_eq!(
        encode_controls("\u{9d}8;;https://example.test\u{9c}"),
        "\\u{9d}8;;https://example.test\\u{9c}"
    );
}

#[test]
fn every_c0_control_and_delete_is_encoded() {
    for code in (0..=0x1f_u8).chain(once(0x7f)) {
        let subject = String::from(char::from(code));
        let encoded = encode_controls(&subject);
        assert!(!encoded.chars().any(char::is_control), "`{code:#04x}` survived as a control");
        assert!(encoded.starts_with('\\'), "`{code:#04x}` was not encoded: {encoded}");
    }
}

#[test]
fn every_c1_control_is_encoded() {
    for code in 0x80..=0x9f_u32 {
        let control = char::from_u32(code).expect("the C1 block is valid");
        assert_eq!(
            encode_controls(&String::from(control)),
            format!("\\u{{{code:02x}}}"),
            "C1 {code:#04x} was not encoded"
        );
    }
}

#[test]
fn multibyte_text_around_a_control_survives_intact() {
    assert_eq!(encode_controls("héllo→\u{1b}wörld"), "héllo→\\ewörld");
}

#[test]
fn color_is_kept_only_by_the_relaying_policy() {
    let painted = "\u{1b}[1;32mCompiling\u{1b}[0m gamma";
    assert_eq!(encode_preserving_color(painted), format!("{painted}{STYLE_RESET}"));
    assert_eq!(encode_controls(painted), "\\e[1;32mCompiling\\e[0m gamma");
}

#[test]
fn relaying_still_refuses_every_sequence_that_is_not_color() {
    for (input, expected) in [
        ("\u{1b}[2K", "\\e[2K"),
        ("\u{1b}[10A", "\\e[10A"),
        ("\u{1b}[?25l", "\\e[?25l"),
        ("\u{1b}]0;title\u{7}", "\\e]0;title\\u{07}"),
        ("line\rline", "line\\rline"),
        ("\u{1b}[8mhidden", "\\e[8mhidden"),
    ] {
        assert_eq!(encode_preserving_color(input), expected);
    }
}

#[test]
fn an_unterminated_color_sequence_is_encoded_rather_than_trusted() {
    assert_eq!(encode_preserving_color("\u{1b}[31"), "\\e[31");
    assert_eq!(encode_preserving_color("\u{1b}[31;"), "\\e[31;");
    assert_eq!(encode_preserving_color("\u{1b}"), "\\e");
}

#[test]
fn an_escape_followed_by_a_non_csi_introducer_is_not_styling() {
    assert_eq!(encode_preserving_color("\u{1b}Xm"), "\\eXm");
}

#[test]
fn relayed_color_around_encoded_controls_keeps_both_decisions() {
    assert_eq!(
        encode_preserving_color("\u{1b}[31merror\u{1b}[0m: src/\u{9b}2K.rs\n"),
        format!("\u{1b}[31merror\u{1b}[0m: src/\\u{{9b}}2K.rs\\n{STYLE_RESET}")
    );
}

#[test]
fn relayed_styling_is_contained_by_a_trusted_reset() {
    assert_eq!(
        encode_preserving_color("\u{1b}[38;5;196merror"),
        format!("\u{1b}[38;5;196merror{STYLE_RESET}")
    );
    assert_eq!(
        encode_preserving_color("\u{1b}[48:2::1:2:3mbackground"),
        format!("\u{1b}[48:2::1:2:3mbackground{STYLE_RESET}")
    );
}
