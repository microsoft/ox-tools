// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Comment trivia: scanning raw source text for comments that `syn` never sees.

use core::ops::Range;

use rustc_lexer::{LiteralKind, Token, TokenKind, tokenize};

/// What kind of comment was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// `// ...`, the only kind that can carry a directive.
    Line,

    /// `/// ...` or `/** ... */`, documentation for the item that follows.
    OuterDoc,

    /// `//! ...` or `/*! ... */`, documentation for the enclosing item.
    InnerDoc,

    /// `/* ... */`.
    Block,
}

/// One comment, located in the file.
#[derive(Debug, Clone)]
pub struct Comment {
    /// What kind of comment this is.
    pub kind: CommentKind,

    /// Byte range of the comment including its delimiters.
    pub span: Range<usize>,

    /// Byte range of the comment text with its delimiters and surrounding whitespace removed.
    ///
    /// Stored as a range into the already-owned source text rather than an owned, trimmed copy:
    /// discovery scans every comment in a file to classify and locate it, but only ordinary line
    /// comments ever have their text read again downstream, so allocating a trimmed `String` for
    /// every documentation and block comment as well would pay for text that is never looked at.
    /// Use [`SourceFile::slice`](super::SourceFile::slice) to materialize it on demand.
    pub body: Range<usize>,

    /// 1-based line number of the comment's first line.
    pub line: usize,

    /// Whether anything other than whitespace precedes the comment on its own line.
    pub trailing: bool,
}

/// Scans source text for comments, skipping over anything inside a string or character literal.
pub(super) fn scan_comments(text: &str, lines: &[usize]) -> Vec<Comment> {
    comment_spans(text)
        .into_iter()
        .map(|span| {
            let raw = text
                .get(span.clone())
                .expect("comment_spans returns UTF-8-boundary ranges within this same text");

            build_comment(
                if raw.starts_with("//") {
                    line_comment_kind(raw)
                } else {
                    block_comment_kind(raw)
                },
                raw,
                span,
                text,
                lines,
            )
        })
        .collect()
}

/// Locates comments while skipping comment-shaped text inside literals.
pub(crate) fn comment_spans(text: &str) -> Vec<Range<usize>> {
    let mut rest = text;
    comment_spans_from_tokens(std::iter::from_fn(|| {
        let mut token = tokenize(rest).next()?;
        // The pinned lexer predates C strings; lex their raw form as the equivalent r literal.
        if &rest[..token.len] == "cr"
            && let Some(raw) = tokenize(&rest[1..]).next()
            && matches!(
                raw.kind,
                TokenKind::Literal {
                    kind: LiteralKind::RawStr { .. },
                    ..
                }
            )
        {
            token = raw;
            token.len += 1;
        }
        // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "Without advancing rest, from_fn returns the same nonempty token forever, so comment collection never terminates.")]
        rest = &rest[token.len..];
        Some(token)
    }))
}

fn comment_spans_from_tokens(tokens: impl Iterator<Item = Token>) -> Vec<Range<usize>> {
    let mut comments = Vec::new();
    let mut offset = 0;

    for token in tokens {
        let start = offset;
        offset += token.len;
        if matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment { .. }) {
            comments.push(start..offset);
        }
    }

    comments
}

/// Returns the offset past a literal beginning at `start`, or `None` for ordinary syntax.
pub(crate) fn literal_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let end = match bytes.get(start)? {
        b'"' => string_end(bytes, start),
        // #[gamma::skip(match_guard.always_true, reason = "raw_string_end rejects a non-raw r prefix by returning start plus one, which the final width guard reduces to the same None")]
        b'r' if matches!(bytes.get(start + 1), Some(b'"' | b'#')) => raw_string_end(bytes, start),
        b'\'' => quote_end(bytes, start),
        // #[gamma::skip(option.none_to_some, reason = "claiming ordinary syntax starts a zero-width literal leaves the outer scanner non-progressing")]
        _ => {
            return None;
        }
    };

    (end > start + 1).then_some(end)
}

/// Classifies a `//` comment. Order matters: `//!` and `///` must be checked before `//`.
pub(super) fn line_comment_kind(raw: &str) -> CommentKind {
    if raw.starts_with("//!") {
        CommentKind::InnerDoc
    } else if raw.starts_with("///") && !raw.starts_with("////") {
        CommentKind::OuterDoc
    } else {
        CommentKind::Line
    }
}

/// Classifies a `/* */` comment.
pub(super) fn block_comment_kind(raw: &str) -> CommentKind {
    if raw.starts_with("/*!") {
        CommentKind::InnerDoc
    } else if raw.starts_with("/**") && !raw.starts_with("/**/") && !raw.starts_with("/***") {
        CommentKind::OuterDoc
    } else {
        CommentKind::Block
    }
}

/// Builds a comment record, stripping delimiters and working out whether it trails code.
fn build_comment(kind: CommentKind, raw: &str, span: Range<usize>, text: &str, lines: &[usize]) -> Comment {
    let stripped = raw
        .trim_start_matches('/')
        .trim_start_matches('*')
        .trim_start_matches('!')
        .trim_end_matches('/')
        .trim_end_matches('*');
    let trimmed = stripped.trim();

    // Every `trim*` call above returns a subslice of `raw` rather than a copy, so the pointer
    // offset between `trimmed` and `raw` is exactly how many leading bytes were stripped; the
    // absolute body range is that offset from `span.start`, spanning `trimmed`'s own length.
    let leading = trimmed.as_ptr() as usize - raw.as_ptr() as usize;
    let body = span.start + leading..span.start + leading + trimmed.len();

    let line_index = match lines.binary_search(&span.start) {
        Ok(exact) => exact,
        Err(insertion) => insertion.saturating_sub(1),
    };

    let line_start = lines
        .get(line_index)
        .copied()
        .expect("line_starts contains the line covering every comment");
    let before = text
        .get(line_start..span.start)
        .expect("line and comment starts are ordered boundaries in this text");

    Comment {
        kind,
        span,
        body,
        line: line_index + 1,
        trailing: !before.trim().is_empty(),
    }
}

/// Returns the offset just past the end of a `"..."` literal.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;

    while i < bytes.len() {
        match bytes[i] {
            // #[gamma::skip(all, tag = "timeout", reason = "This escape branch must leave the current backslash; removing or reversing its update repeatedly revisits that byte.")]
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => {
                // #[gamma::skip(all, tag = "timeout", reason = "This increment is the ordinary-string text branch's only progress toward a quote or EOF.")]
                i += 1;
            }
        }
    }

    bytes.len()
}

/// Returns the offset just past the end of an `r#".."#` literal.
fn raw_string_end(bytes: &[u8], start: usize) -> usize {
    let mut hashes = 0;
    let mut i = start + 1;

    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        // #[gamma::skip(all, tag = "timeout", reason = "This increment is the raw-string prefix loop's only progress through its hash delimiters.")]
        i += 1;
    }

    if bytes.get(i) != Some(&b'"') {
        // Not a raw string after all, just an identifier beginning with `r`.
        return start + 1;
    }

    i += 1;

    while i < bytes.len() {
        if bytes[i] == b'"' {
            let closing = i + 1;
            let found = bytes[closing..].iter().take_while(|b| **b == b'#').count();

            if found >= hashes {
                return closing + hashes;
            }
        }

        // #[gamma::skip(all, tag = "timeout", reason = "This increment is the raw-string content loop's only progress toward a closing delimiter or EOF.")]
        i += 1;
    }

    bytes.len()
}

/// Returns the offset just past a `'`, which may open a character literal or a lifetime.
///
/// This is the one genuinely ambiguous case in Rust's lexical grammar for a scanner this small.
/// Treating the `'a` in `fn f<'a>()` as an unterminated character literal would make the
/// syntax-shape precheck skip the rest of the file.
fn quote_end(bytes: &[u8], start: usize) -> usize {
    // `'\n'`, `'\''`, `'\u{1f600}'` and friends. The escaped character is skipped rather than
    // examined, or the quote in `'\''` would be mistaken for the closing one.
    if bytes.get(start + 1) == Some(&b'\\') {
        let mut i = start + 3;

        // #[gamma::skip(cond.negate, reason = "negating the bounded quote search can index past the slice or exhaust the test budget")]
        while i < bytes.len() && bytes[i] != b'\'' {
            // #[gamma::skip(all, tag = "timeout", reason = "This increment is the escaped-character quote search's only progress toward its quote or EOF bound.")]
            i += 1;
        }

        // An unterminated literal at the end of the file has no closing quote to step past.
        return (i + 1).min(bytes.len());
    }

    // A single character followed by a closing quote is a character literal. The character may be
    // several bytes, so step by whole characters rather than by bytes.
    let rest = &bytes[start + 1..];
    // #[gamma::skip(literal.int_increment, reason = "when rest is empty, fallback bytes zero and one both have UTF-8 width one")]
    let width = utf8_width(rest.first().copied().unwrap_or(0));

    if rest.get(width) == Some(&b'\'') {
        return start + 1 + width + 1;
    }

    // Otherwise it is a lifetime or a label; consume just the quote.
    start + 1
}

/// Returns the number of bytes in a UTF-8 sequence given its leading byte.
const fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(all(test, not(miri)))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod fuzz {
    use super::scan_comments;
    use crate::parse::source_file::line_starts;

    /// Arbitrary text yields lexer-identical, ordered comment spans on UTF-8 boundaries.
    #[test]
    fn arbitrary_text_is_scanned_into_spans_that_exist() {
        bolero::check!().with_type::<String>().for_each(|text| {
            super::tests::assert_comment_scan_invariants(text);
            super::tests::assert_lexer_comment_spans(text);
        });
    }

    /// Text with no comment delimiter in it yields no comments.
    ///
    /// The scanner's job is to find `//` and `/*`, and its risk is finding them inside a literal
    /// where they are not comments at all. This is the cheap half of that: with the delimiters
    /// removed entirely, no amount of quoting or escaping should conjure one up.
    #[test]
    fn text_without_a_delimiter_yields_no_comments() {
        bolero::check!().with_type::<String>().for_each(|text| {
            let text: String = text.replace('/', "");

            assert!(
                scan_comments(&text, &line_starts(&text)).is_empty(),
                "a comment was found in text with no delimiter: {text:?}"
            );
        });
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::super::source_file::{SourceFile, line_starts};
    use super::*;

    #[test]
    fn raw_c_strings_preserve_following_directives_and_hide_embedded_comments() {
        for literal in [
            r##"cr#""text"#"##,
            r###"cr##"// hidden /* hidden */ " quoted"##"###,
            r#"cr"// hidden""#,
            r##"cr#""text"#suffix"##,
        ] {
            for directive in ["#[gamma::skip(arith.add_to_sub)]", "#[gamma::expect(killed)]"] {
                let text = format!("fn f() {{ let _ = {literal}// {directive}\n; }}");
                let start = text.find(&format!("// {directive}")).unwrap();
                assert_eq!(comment_spans(&text), vec![start..start + 3 + directive.len()]);
                let file = SourceFile::parse("c-string.rs", text).unwrap();
                assert_eq!(file.comments.len(), 1);
                assert_eq!(file.slice(&file.comments[0].body), directive);
            }
        }
    }

    #[test]
    fn literal_suffixes_do_not_hide_comments_in_macro_tokens() {
        for literal in ["\"x\"", "b\"x\"", "r\"x\"", "br\"x\"", "'x'", "b'x'"] {
            let text = format!("macro_rules! m {{ ($($token:tt)*) => {{}} }}\nm!({literal}r#\" body\" // real\n\"# \");");
            let expected_start = text.find("// real").unwrap();
            assert_eq!(comment_spans(&text), vec![expected_start..expected_start + 7], "{text}");
            assert_lexer_comment_spans(&text);
            let file = SourceFile::parse("macro.rs", text).unwrap();
            assert_eq!(file.comments.len(), 1);
            assert_eq!(file.slice(&file.comments[0].body), "real");
        }
    }

    #[test]
    fn malformed_quotes_and_byte_literals_keep_lexer_comment_boundaries() {
        for text in [
            "'\\x// real\nnext",
            "b'\\x// real\nnext",
            "b\"\" // real",
            "b\"'x'// hidden\" // real",
            "br##\"// hidden\"## // real",
            "before bname // real",
        ] {
            let start = text.rfind("// real").unwrap();
            assert_eq!(comment_spans(text), vec![start..start + 7], "{text}");
            assert_lexer_comment_spans(text);
        }
    }

    #[test]
    fn literal_endpoints_at_nonzero_offsets_include_raw_string_delimiters() {
        for literal in [
            "\"\"",
            "r\"\"",
            "r#\"\"#",
            "r\"x\"",
            "r#\"x\"#",
            "r##\"x\"##",
            "r##\"a\"# // still inside \"##",
            "r###\"a\"# /* still inside */ \"## // still inside \"###",
            "'x'",
            "'\\n'",
        ] {
            let text = format!("prefix {literal} // real");
            assert_eq!(literal_end(&text, 7), Some(7 + literal.len()), "{text}");
        }
    }

    #[test]
    fn block_comment_endpoints_do_not_treat_body_slashes_as_delimiters() {
        for raw in ["/*/ body */", "/* slash / body */", "/* outer /* inner */ tail */"] {
            let text = format!("xx{raw} trailing");
            assert_eq!(comment_spans(&text), vec![2..2 + raw.len()], "{text}");
            let file = SourceFile::parse("comments.rs", format!("{raw}\nfn f() {{}}")).unwrap();
            assert_eq!(file.slice(&file.comments[0].span), raw);
        }
    }

    #[test]
    fn token_walk_reaches_comments_after_noncomment_prefixes() {
        let text = "name + 27; // real";
        let mut tokens = tokenize(text);
        let mut observations = 0;
        // Bound runaway traversal without depending on a test timeout.
        let bounded = std::iter::from_fn(|| {
            observations += 1;
            assert!(observations <= 9, "the eight-token input must be exhausted on the ninth read");
            tokens.next()
        });
        assert_eq!(comment_spans_from_tokens(bounded), vec![11..18]);
        assert_eq!(comment_spans(text), vec![11..18]);
    }

    pub(super) fn assert_lexer_comment_spans(text: &str) {
        if text.contains("cr\"") || text.contains("cr#") {
            return; // The legacy lexer is not an oracle for raw C strings.
        }
        let mut offset = 0;
        let expected = tokenize(text)
            .filter_map(|token| {
                let span = offset..offset + token.len;
                offset = span.end;
                matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment { .. }).then_some(span)
            })
            .collect::<Vec<_>>();
        assert_eq!(comment_spans(text), expected, "{text:?}");
    }

    #[test]
    fn checked_in_comment_corpus_matches_the_rust_lexer() {
        for text in [
            include_str!("../../tests/corpus/comments/literals.txt"),
            include_str!("../../tests/corpus/comments/malformed.txt"),
        ] {
            assert_comment_scan_invariants(text);
            assert_lexer_comment_spans(text);
        }
        for text in [
            "name_r#\"// real",
            "br##\"// hidden\"## // real",
            "'ab// real",
            "'// hidden' // real",
            "1r#\"// real",
        ] {
            assert_lexer_comment_spans(text);
        }
    }

    #[test]
    fn empty_and_single_character_literals_stop_before_adjacent_real_comments() {
        for literal in ["\"\"", "'a'", "'é'", "'\\''", "'\\n'"] {
            let text = format!("xx{literal}// real");
            assert_eq!(literal_end(&text, 2), Some(2 + literal.len()), "{literal}");
            let comments = scan_comments(&text, &line_starts(&text));
            assert_eq!(comments.len(), 1, "{literal}");
            assert_eq!(&text[comments[0].span.clone()], "// real");
            assert_eq!(&text[comments[0].body.clone()], "real");
            assert_eq!(comments[0].line, 1);
            assert!(comments[0].trailing);
        }
        let text = "fn f<'a>() {} // real";
        assert_eq!(literal_end(text, 5), None);
        assert_eq!(scan_comments(text, &line_starts(text)).len(), 1);
    }

    #[test]
    fn scanner_endpoints_and_comment_records_match_exact_source_slices() {
        for (text, expected) in [
            ("x / y * z", vec![]),
            ("x // body\nz", std::iter::once(2..9).collect()),
            ("x /**/ z", std::iter::once(2..6).collect()),
            ("x /* a /* b */ c */ z", std::iter::once(2..19).collect()),
            ("x /* unfinished", std::iter::once(2..15).collect()),
            ("rvalue // real", std::iter::once(7..14).collect()),
            ("\"// hidden\" // real", std::iter::once(12..19).collect()),
            ("'é' // real", std::iter::once(5..12).collect()),
        ] {
            assert_eq!(comment_spans(text), expected, "{text}");
        }
        for (text, start, end) in [("xx//abc\nz", 2, 7), ("xx//", 2, 4), ("xx//é", 2, 6)] {
            assert_eq!(comment_spans(text), vec![start..end]);
        }
        for (text, start, expected) in [
            ("x 'a'", 2, Some(5)),
            ("x 'é'", 2, Some(6)),
            ("x '\\n'", 2, Some(6)),
            ("x '\\''", 2, Some(6)),
            ("x 'life", 2, None),
            ("x rname", 2, None),
            ("x \"z\"", 2, Some(5)),
            ("x \"\"", 2, Some(4)),
            ("x", 1, None),
        ] {
            assert_eq!(literal_end(text, start), expected, "{text}");
        }
        assert_eq!(quote_end(b"xxx'\\n'", 3), 7);
        assert_eq!(quote_end(b"'", 0), 1);
        assert_eq!(block_comment_kind("/** docs /**/"), CommentKind::OuterDoc);

        for (raw, kind, body) in [
            ("/**/", CommentKind::Block, ""),
            ("/** docs */", CommentKind::OuterDoc, "docs"),
            ("/*** plain */", CommentKind::Block, "plain"),
            ("/*! inner */", CommentKind::InnerDoc, "inner"),
            ("/* ** text ** */", CommentKind::Block, "** text **"),
            ("//! inner", CommentKind::InnerDoc, "inner"),
        ] {
            let text = format!("fn f() {{}}\n  {raw}\n");
            let comments = scan_comments(&text, &line_starts(&text));
            assert_eq!(comments.len(), 1, "{raw}");
            let comment = &comments[0];
            assert_eq!(comment.kind, kind, "{raw}");
            assert_eq!(&text[comment.body.clone()], body, "{raw}");
            assert_eq!(&text[comment.span.clone()], raw);
            assert_eq!(comment.line, 2);
            assert!(!comment.trailing);
        }
    }

    pub(crate) fn assert_comment_scan_invariants(text: &str) {
        let comments = scan_comments(text, &line_starts(text));
        let lines = text.lines().count();
        let mut previous = 0;

        for comment in &comments {
            assert!(comment.span.start <= comment.span.end, "a span runs backwards: {comment:?}");
            assert!(comment.span.end <= text.len(), "a span runs past the text: {comment:?}");
            assert!(
                text.get(comment.span.clone()).is_some(),
                "a span is not on a character boundary: {comment:?}"
            );

            assert!(comment.body.start <= comment.body.end, "a body range runs backwards: {comment:?}");
            assert!(
                comment.span.start <= comment.body.start && comment.body.end <= comment.span.end,
                "a body range escapes its own span: {comment:?}"
            );
            assert!(
                text.get(comment.body.clone()).is_some(),
                "a body range is not on a character boundary: {comment:?}"
            );

            // Comments are found by one forward pass, so they come out in order and cannot
            // overlap. A scanner that failed to advance past one would break this first.
            assert!(comment.span.start >= previous, "spans are out of order: {comment:?}");
            previous = comment.span.end;

            assert!(comment.line >= 1, "a comment is before the first line: {comment:?}");
            assert!(comment.line <= lines.max(1), "a comment is past the last line: {comment:?}");
        }
    }

    fn parse(text: &str) -> SourceFile {
        SourceFile::parse("test.rs", text.to_owned()).unwrap()
    }

    #[test]
    fn known_comments_satisfy_the_fuzz_scan_invariants() {
        assert_comment_scan_invariants("// one\nfn f() {} /* two */\n/// three\n");
    }

    #[test]
    fn line_comments_are_found_and_stripped() {
        let file = parse("// hello\nfn f() {}\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.comments[0].kind, CommentKind::Line);
        assert_eq!(file.slice(&file.comments[0].body), "hello");
        assert_eq!(file.comments[0].line, 1);
        assert!(!file.comments[0].trailing);
    }

    #[test]
    fn doc_comments_are_distinguished_from_directive_comments() {
        let file = parse("//! module\n/// item\n// plain\n//// also plain\nfn f() {}\n");
        let kinds: Vec<CommentKind> = file.comments.iter().map(|c| c.kind).collect();

        assert_eq!(
            kinds,
            vec![CommentKind::InnerDoc, CommentKind::OuterDoc, CommentKind::Line, CommentKind::Line]
        );
    }

    #[test]
    fn an_escaped_quote_literal_does_not_confuse_the_scanner() {
        // Stopping at the escaped quote in `'\''` would leave the scanner one quote out of step
        // for the rest of the line.
        assert_eq!(quote_end(br"'\''", 0), 4);
        assert_eq!(quote_end(br"'\n'", 0), 4);
        assert_eq!(quote_end(br"'\u{27}'", 0), 8);
        assert_eq!(quote_end(b"'a'", 0), 3);

        // A lifetime is not a literal, so only the quote is consumed.
        assert_eq!(quote_end(b"'static", 0), 1);
    }

    #[test]
    fn an_unterminated_literal_stays_inside_the_text() {
        // The result indexes the text, so running off the end would be a panic waiting to happen.
        for text in [r"'\'", r"'\", "'"] {
            let bytes = text.as_bytes();

            assert!(quote_end(bytes, 0) <= bytes.len(), "{text} ran past the end");
        }

        assert_eq!(quote_end(br"'\'xyz", 0), br"'\'xyz".len());
    }

    #[test]
    fn a_comment_after_an_escaped_quote_is_still_found() {
        let file = parse("fn f() { let _q = '\\''; } // after\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.slice(&file.comments[0].body), "after");
    }

    #[test]
    fn trailing_comments_are_marked() {
        let file = parse("fn f() { let _x = 1; } // after\n");

        assert!(file.comments[0].trailing);
    }

    #[test]
    fn comment_markers_inside_strings_are_not_comments() {
        let file = parse("fn f() -> &'static str { \"// not a comment\" }\n");

        assert!(file.comments.is_empty(), "{:?}", file.comments);
    }

    #[test]
    fn comment_markers_inside_raw_strings_are_not_comments() {
        let file = parse("fn f() -> &'static str { r#\"// nope /* nope */\"# }\n");

        assert!(file.comments.is_empty(), "{:?}", file.comments);
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let file = parse("fn f() -> &'static str { \"a\\\"// no\" }\n// yes\n");

        assert_eq!(file.comments.len(), 1);
        assert_eq!(file.slice(&file.comments[0].body), "yes");
    }

    #[test]
    fn lifetimes_do_not_swallow_the_rest_of_the_file() {
        let file = parse("fn f<'a>(x: &'a str) -> &'a str { x }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
        assert_eq!(file.slice(&file.comments[0].body), "found me");
    }

    #[test]
    fn character_literals_are_skipped() {
        let file = parse("fn f() -> char { '\\'' }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
        assert_eq!(file.slice(&file.comments[0].body), "found me");
    }

    #[test]
    fn multibyte_character_literals_are_skipped() {
        let file = parse("fn f() -> char { 'é' }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn a_quote_in_a_labelled_loop_is_not_a_literal() {
        let file = parse("fn f() { 'outer: loop { break 'outer; } }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn block_comments_nest() {
        let file = parse("/* outer /* inner */ still outer */\nfn f() {}\n");

        assert_eq!(file.comments.len(), 1);
        assert!(file.slice(&file.comments[0].body).contains("still outer"));
    }

    #[test]
    fn block_doc_comments_are_classified() {
        let text = "/*! module */\n/** item */\n/**/ /* plain */\nfn f() {}\n";
        let file = parse(text);
        let kinds: Vec<CommentKind> = file.comments.iter().map(|c| c.kind).collect();

        assert_eq!(
            kinds,
            vec![CommentKind::InnerDoc, CommentKind::OuterDoc, CommentKind::Block, CommentKind::Block]
        );
    }

    #[test]
    fn an_unterminated_block_comment_stops_at_end_of_file() {
        // syn rejects this, so scan the trivia directly.
        let text = "fn f() {} /* never closed";
        let comments = scan_comments(text, &line_starts(text));

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].span.end, text.len());
    }

    #[test]
    fn spans_point_at_the_comment_itself() {
        let file = parse("fn f() {}\n// directive\n");
        let span = file.comments[0].span.clone();

        assert_eq!(file.slice(&span), "// directive");
    }

    #[test]
    fn r_prefixed_identifiers_are_not_raw_strings() {
        let file = parse("fn f() { let range = 1; let _ = range; }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn raw_strings_with_many_hashes_are_handled() {
        let file = parse("fn f() -> &'static str { r###\"a\"# b\"### }\n// found me\n");

        assert_eq!(file.comments.len(), 1, "{:?}", file.comments);
    }

    #[test]
    fn unterminated_strings_and_raw_strings_stop_at_end_of_file() {
        assert_eq!(string_end(b"\"unterminated", 0), b"\"unterminated".len());
        assert_eq!(raw_string_end(br#"r#"unterminated"#, 0), br#"r#"unterminated"#.len());
    }

    #[test]
    fn invalid_raw_string_prefix_consumes_only_the_r() {
        assert_eq!(raw_string_end(b"range", 0), 1);
    }

    #[test]
    fn multibyte_widths_cover_three_and_four_byte_characters() {
        let euro = "€".as_bytes()[0];
        let crab = "🦀".as_bytes()[0];

        assert_eq!(utf8_width(euro), 3);
        assert_eq!(utf8_width(crab), 4);
    }

    #[test]
    fn comments_come_back_in_source_order() {
        let file = parse("// one\nfn f() {}\n// two\nfn g() {}\n// three\n");
        let bodies: Vec<&str> = file.comments.iter().map(|c| file.slice(&c.body)).collect();

        assert_eq!(bodies, vec!["one", "two", "three"]);
    }

    #[test]
    fn a_comment_immediately_after_ordinary_text_is_not_skipped() {
        assert_eq!(comment_spans("x// comment"), vec![1..11]);
        assert_eq!(comment_spans("x/* comment */"), vec![1..14]);
    }

    #[test]
    fn literal_and_comment_endpoints_have_exact_oracles() {
        assert_eq!(literal_end("plain", 0), None);
        assert_eq!(literal_end("\"x\" tail", 0), Some(3));
        assert_eq!(literal_end("r##\"x\"## tail", 0), Some(8));
        assert_eq!(literal_end("'é' tail", 0), Some(4));
        assert_eq!(literal_end("'name", 0), None);

        assert_eq!(comment_spans("//abc\nnext"), vec![0..5]);
        assert_eq!(comment_spans("//abc"), vec![0..5]);
        assert_eq!(comment_spans("/*a/*b*/c*/next"), vec![0..11]);
        assert_eq!(string_end(br#""a\"b"tail"#, 0), 6);
        assert_eq!(raw_string_end(br##"r#"a"#tail"##, 0), 6);
        assert_eq!(quote_end("'é'".as_bytes(), 0), 4);
    }

    #[test]
    fn comment_kind_and_body_boundaries_cover_lookalike_delimiters() {
        assert_eq!(line_comment_kind("//! inner"), CommentKind::InnerDoc);
        assert_eq!(line_comment_kind("/// outer"), CommentKind::OuterDoc);
        assert_eq!(line_comment_kind("//// plain"), CommentKind::Line);
        assert_eq!(block_comment_kind("/*! inner */"), CommentKind::InnerDoc);
        assert_eq!(block_comment_kind("/** outer */"), CommentKind::OuterDoc);
        assert_eq!(block_comment_kind("/**/"), CommentKind::Block);
        assert_eq!(block_comment_kind("/*** plain */"), CommentKind::Block);

        let text = "code();  //  body  \n";
        let comments = scan_comments(text, &line_starts(text));

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].span, 9..19);
        assert_eq!(comments[0].body, 13..17);
        assert_eq!(comments[0].line, 1);
        assert!(comments[0].trailing);
    }

    #[test]
    fn utf8_width_covers_every_leading_byte_class() {
        assert_eq!(utf8_width(b'a'), 1);
        assert_eq!(utf8_width("é".as_bytes()[0]), 2);
        assert_eq!(utf8_width("€".as_bytes()[0]), 3);
        assert_eq!(utf8_width("🦀".as_bytes()[0]), 4);
    }
}
