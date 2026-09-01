// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Making repository-controlled text safe to print to a terminal or a CI log.
//!
//! Everything this tool reports on is written by whoever wrote the code under test: file paths,
//! item paths, test names, source fragments, and the diagnostics a build tool produces about them.
//! Those values end up on a console beside status lines the tool draws with real escape sequences,
//! and a terminal cannot tell the two apart. A path containing `\r\x1b[2K` erases the line above
//! it; one containing an OSC 8 sequence attaches a hyperlink of its author's choosing to text a
//! reader will assume the tool wrote; one containing a newline forges a whole line of output.
//!
//! The rule here is that a control character is *shown* rather than *obeyed*. Encoding is visible
//! by design — a reader who sees `\e` or `\u{9b}` in a filename is being told something true about
//! that filename — and it is reversible by eye, which a lossy strip would not be.
//!
//! Two policies exist because two kinds of text arrive here. Values the tool interpolates into its
//! own sentences never legitimately contain an escape, so [`encode_controls`] encodes every one.
//! Output relayed verbatim from another tool does legitimately arrive colored, and color cannot
//! move a cursor, erase a row, or address the terminal, so [`encode_preserving_color`] lets a
//! complete SGR sequence through and encodes everything else — including every other CSI sequence,
//! every operating-system command, and the C1 controls that spell those in a single byte.

use core::fmt::Write as _;
use std::borrow::Cow;

/// The escape that introduces every sequence a terminal acts on.
const ESCAPE: u8 = 0x1b;

/// The lead byte of the UTF-8 encoding of every C1 control.
///
/// The C1 controls are `U+0080..=U+009F`, which UTF-8 encodes as `0xC2` followed by the code point's
/// own low byte. They matter because a terminal in 8-bit mode reads them as one-byte spellings of
/// the sequences the escape introduces — `U+009B` is CSI and `U+009D` is OSC — so encoding `ESC`
/// alone would leave the same capabilities reachable by another spelling.
const C1_LEAD: u8 = 0xC2;

/// Encodes every control character, so nothing in `text` can address the terminal.
///
/// For values the tool interpolates into its own output: paths, identifiers, test names, notes, and
/// source fragments. None of them has a legitimate reason to carry a control character, so the
/// newline that would forge a line and the escape that would erase one are treated alike.
#[must_use]
pub fn encode_controls(text: &str) -> Cow<'_, str> {
    encode(text, false)
}

/// Encodes every control character except a complete color sequence.
///
/// For output relayed verbatim from another tool, where color is the reason the output is being
/// shown at all. A select-graphic-rendition sequence — `ESC [`, digits, `;` or `:`, then `m` —
/// changes how following text is painted and can do nothing else: it cannot move the cursor, erase
/// anything, resize the window, or speak to the operating system. Every other sequence, including
/// an `ESC [` run that never reaches its `m`, is encoded like any other control.
#[must_use]
pub fn encode_preserving_color(text: &str) -> Cow<'_, str> {
    encode(text, true)
}

/// Encodes `text`, borrowing it unchanged when there was nothing to encode.
///
/// Borrowing rather than always allocating is what makes this affordable on the path that renders a
/// progress line ten times a second: the overwhelming majority of values are ordinary text, and
/// they cost one scan and no allocation.
fn encode(text: &str, keep_color: bool) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut encoded: Option<String> = None;
    let mut copied = 0;
    let mut cursor = 0;

    while let Some(&byte) = bytes.get(cursor) {
        // A UTF-8 continuation or lead byte of anything above the C1 block is ordinary text, and
        // stepping over it one byte at a time is safe because no byte of a multi-byte character can
        // be confused with an ASCII control.
        if byte >= 0x80 {
            if byte == C1_LEAD && bytes.get(cursor + 1).is_some_and(|&low| (0x80..=0x9F).contains(&low)) {
                let control = char::from(bytes[cursor + 1]);
                let out = encoded.get_or_insert_with(|| String::with_capacity(text.len() + ESCAPE_HEADROOM));

                out.push_str(&text[copied..cursor]);
                push_escape(out, control);

                cursor += 2;
                copied = cursor;

                continue;
            }

            cursor += 1;

            continue;
        }

        if !byte.is_ascii_control() {
            cursor += 1;

            continue;
        }

        if keep_color
            && byte == ESCAPE
            && let Some(end) = color_sequence_end(bytes, cursor)
        {
            cursor = end;

            continue;
        }

        let out = encoded.get_or_insert_with(|| String::with_capacity(text.len() + ESCAPE_HEADROOM));

        out.push_str(&text[copied..cursor]);
        push_escape(out, char::from(byte));

        cursor += 1;
        copied = cursor;
    }

    match encoded {
        Some(mut out) => {
            out.push_str(&text[copied..]);

            Cow::Owned(out)
        }
        None => Cow::Borrowed(text),
    }
}

/// Extra capacity reserved once a value is known to need encoding, so the common case of one or two
/// control characters does not grow the buffer again.
const ESCAPE_HEADROOM: usize = 16;

/// Writes one control character as text a terminal will show rather than obey.
///
/// The five with a conventional spelling get it, because `\n` in a filename reads as what it is
/// while `\u{0a}` makes a reader look it up. Everything else, including every C1 control, is
/// written as its code point.
fn push_escape(out: &mut String, control: char) {
    match control {
        '\0' => out.push_str("\\0"),
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\u{1b}' => out.push_str("\\e"),
        other => {
            // Writing to a `String` cannot fail, and the result is discarded rather than propagated
            // because there is no failure to propagate.
            let _ = write!(out, "\\u{{{:02x}}}", u32::from(other));
        }
    }
}

/// The index just past a complete SGR sequence beginning at `start`, if there is one.
///
/// Returns `None` for anything else, including a sequence that runs off the end of the text: an
/// unterminated `ESC [` is exactly what a value would carry to make the *next* thing printed part
/// of its own sequence, so it must be encoded rather than passed on in the hope of a later `m`.
fn color_sequence_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start + 1) != Some(&b'[') {
        return None;
    }

    let mut cursor = start + 2;

    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'0'..=b'9' | b';' | b':' => cursor += 1,
            b'm' => return Some(cursor + 1),
            _ => return None,
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for code in (0..=0x1f_u8).chain(core::iter::once(0x7f)) {
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
            let subject = String::from(control);
            let encoded = encode_controls(&subject);

            assert_eq!(encoded, format!("\\u{{{code:02x}}}"), "C1 {code:#04x} was not encoded");
        }
    }

    #[test]
    fn multibyte_text_around_a_control_survives_intact() {
        assert_eq!(encode_controls("héllo→\u{1b}wörld"), "héllo→\\ewörld");
    }

    #[test]
    fn color_is_kept_only_by_the_relaying_policy() {
        let painted = "\u{1b}[1;32mCompiling\u{1b}[0m gamma";

        assert_eq!(encode_preserving_color(painted), painted);
        assert_eq!(encode_controls(painted), "\\e[1;32mCompiling\\e[0m gamma");
    }

    #[test]
    fn relaying_still_refuses_every_sequence_that_is_not_color() {
        // Cursor motion, erasure, the private-mode set that hides a cursor, and an OSC are all CSI
        // or OSC sequences that a color policy must not mistake for color.
        assert_eq!(encode_preserving_color("\u{1b}[2K"), "\\e[2K");
        assert_eq!(encode_preserving_color("\u{1b}[10A"), "\\e[10A");
        assert_eq!(encode_preserving_color("\u{1b}[?25l"), "\\e[?25l");
        assert_eq!(encode_preserving_color("\u{1b}]0;title\u{7}"), "\\e]0;title\\u{07}");
        assert_eq!(encode_preserving_color("line\rline"), "line\\rline");
    }

    #[test]
    fn an_unterminated_color_sequence_is_encoded_rather_than_trusted() {
        // Passing this through would let the value swallow whatever is printed after it into its
        // own sequence, which is the escape it was trying to write in the first place.
        assert_eq!(encode_preserving_color("\u{1b}[31"), "\\e[31");
        assert_eq!(encode_preserving_color("\u{1b}[31;"), "\\e[31;");
        assert_eq!(encode_preserving_color("\u{1b}"), "\\e");
    }

    #[test]
    fn relayed_color_around_encoded_controls_keeps_both_decisions() {
        assert_eq!(
            encode_preserving_color("\u{1b}[31merror\u{1b}[0m: src/\u{9b}2K.rs\n"),
            "\u{1b}[31merror\u{1b}[0m: src/\\u{9b}2K.rs\\n"
        );
    }
}
