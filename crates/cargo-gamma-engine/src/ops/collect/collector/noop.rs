// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Whether a proposed replacement would leave the program as it was.

use rustc_lexer::{LiteralKind, TokenKind};

use super::super::defaults::DefaultPaths;
use crate::ops::collect::Shape;

/// Returns whether a replacement reproduces the code it would replace.
///
/// Such a mutant is not a weak one, it is not a mutant at all: the compiled program is unchanged,
/// so no test can distinguish it and it survives every suite that will ever be written. Left in, it
/// is charged for like any other mutant and then reported as a survivor, which is an accusation
/// against tests that had nothing to answer for.
///
/// It arises whenever a function already returns one of the values the `fn_value` family offers —
/// `fn ready() -> bool { true }` is the everyday case, and `{ 0 }`, `{ None }` and `{ "" }` are the
/// others. The comparison is made on tokens rather than text so that layout, comments and the
/// braces around a body do not decide the answer; a body is stripped of its braces first, since
/// what replaces it is an expression rather than a block.
///
/// Both sides must parse for the answer to be yes. A replacement this tool cannot itself tokenise
/// is not one it can claim to have recognised as a no-op.
///
/// One shape is compared more closely than tokens allow. The reference family wraps its values in
/// `Box::leak(Box::new(...))`, and a body already written that way differs from the replacement
/// only in how the value inside spells its default: a type parameter explicitly bounded by the
/// standard `Default` trait can write `T::default()` instead of `Default::default()`. Those are
/// the same call, so the mutant is the original program under another name.
pub(super) fn is_noop(replacement: &str, original: &str, shape: Shape, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    let original = if shape == Shape::Block || shape == Shape::IterBlock {
        let trimmed = original.trim();

        trimmed
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or(original)
    } else {
        original
    };

    if same_tokens(replacement, original) {
        return true;
    }

    is_same_leak(replacement, original, defaults, defaulted_types)
}

/// Returns whether two expressions are the same `Box::leak(Box::new(...))`, up to how the value
/// inside names its `Default`.
///
/// Deliberately narrow: it answers for this one shape and nothing else, rather than pretending to
/// decide equivalence in general.
pub(super) fn is_same_leak(replacement: &str, original: &str, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    // Every match requires the terminal method identifier checked later by `path_ends_with` in
    // both expressions, so its absence from either raw text rules out a match without tokenizing
    // either one. Almost no replacement or original has this shape, so this skips the two token
    // passes below for the overwhelming majority of candidates.
    if !replacement.contains("leak") || !original.contains("leak") {
        return false;
    }

    let (Some(replacement), Some(original)) = (leaked_value(replacement), leaked_value(original)) else {
        return false;
    };

    if is_default_call(replacement, defaults, defaulted_types) && is_default_call(original, defaults, defaulted_types) {
        return true;
    }

    same_tokens(replacement, original)
}

/// The value a `Box::leak(Box::new(value))` leaks, or `None` for any other expression.
///
/// A leading `&*` is stripped first. A shared reference is offered as `&*Box::leak(..)`, and
/// without this the reborrow would hide the shape from the no-op check — so a body that already
/// leaks a default would be handed a mutant that is the same program, and it would survive every
/// suite that will ever be written.
pub(super) fn leaked_value(text: &str) -> Option<&str> {
    let tokens = lexemes(text)?;
    let mut expression = strip_parentheses(&tokens);

    if expression.first()?.kind == TokenKind::And && expression.get(1)?.kind == TokenKind::Star {
        expression = strip_parentheses(expression.get(2..)?);
    }

    let leaked = call_argument(text, expression, "Box", "leak")?;
    let tokens = lexemes(leaked)?;

    call_argument(leaked, strip_parentheses(&tokens), "Box", "new")
}

#[derive(Clone, Copy, Debug)]
struct Lexeme<'a> {
    kind: TokenKind,
    text: &'a str,
    start: usize,
    end: usize,
}

fn call_argument<'a>(text: &'a str, expression: &[Lexeme<'a>], qualifier: &str, name: &str) -> Option<&'a str> {
    let expression = strip_parentheses(expression);
    let open = expression.iter().position(|token| token.kind == TokenKind::OpenParen)?;
    let (callee, call) = expression.split_at(open);

    if !path_ends_with(callee, qualifier, name) || call.last()?.kind != TokenKind::CloseParen {
        return None;
    }

    let mut depth = 0_usize;
    for (index, token) in call.iter().enumerate() {
        match token.kind {
            TokenKind::OpenParen => depth = depth.checked_add(1)?,
            TokenKind::CloseParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + 1 != call.len() {
                    return None;
                }
            }
            TokenKind::Comma if depth == 1 => return None,
            _ => {}
        }
    }

    if depth != 0 || call.len() < 3 {
        return None;
    }

    let argument = call.get(1..call.len() - 1)?;
    text.get(argument.first()?.start..argument.last()?.end)
}

fn path_ends_with(path: &[Lexeme<'_>], qualifier: &str, name: &str) -> bool {
    let Some(segments) = path_segments(path) else {
        return false;
    };

    matches!(segments.as_slice(), [.., found_qualifier, found_name] if *found_qualifier == qualifier && *found_name == name)
}

fn path_segments<'a>(path: &[Lexeme<'a>]) -> Option<Vec<&'a str>> {
    let path = strip_parentheses(path);
    let mut segments = Vec::new();
    let mut index = usize::from(
        path.first().is_some_and(|token| token.kind == TokenKind::Colon) && path.get(1).is_some_and(|token| token.kind == TokenKind::Colon),
    ) * 2;

    loop {
        let token = path.get(index)?;
        if !matches!(token.kind, TokenKind::Ident | TokenKind::RawIdent) {
            return None;
        }
        segments.push(token.text);
        index += 1;

        if index == path.len() {
            return Some(segments);
        }
        if path.get(index)?.kind != TokenKind::Colon || path.get(index + 1)?.kind != TokenKind::Colon {
            return None;
        }
        index += 2;
    }
}

fn strip_parentheses<'slice, 'text>(mut expression: &'slice [Lexeme<'text>]) -> &'slice [Lexeme<'text>] {
    loop {
        if expression.first().is_none_or(|token| token.kind != TokenKind::OpenParen)
            || expression.last().is_none_or(|token| token.kind != TokenKind::CloseParen)
        {
            return expression;
        }

        let mut depth = 0_usize;
        let mut closes_at_end = false;
        for (index, token) in expression.iter().enumerate() {
            match token.kind {
                TokenKind::OpenParen => depth += 1,
                TokenKind::CloseParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        closes_at_end = index + 1 == expression.len();
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            return expression;
        }
        expression = &expression[1..expression.len() - 1];
    }
}

fn is_default_call(text: &str, defaults: &DefaultPaths, defaulted_types: &[String]) -> bool {
    let Some(tokens) = lexemes(text) else {
        return false;
    };
    let expression = strip_parentheses(&tokens);
    let Some(open) = expression.iter().position(|token| token.kind == TokenKind::OpenParen) else {
        return false;
    };
    let (callee, call) = expression.split_at(open);
    if !matches!(call, [open, close] if open.kind == TokenKind::OpenParen && close.kind == TokenKind::CloseParen) {
        return false;
    }
    let Some(segments) = path_segments(callee) else {
        return false;
    };
    let segments: Vec<String> = segments.into_iter().map(str::to_owned).collect();

    defaults.is_standard_default_segments(&segments)
        || matches!(segments.as_slice(), [qualifier, method] if method == "default" && defaulted_types.contains(qualifier))
}

fn same_tokens(left: &str, right: &str) -> bool {
    let (Some(left), Some(right)) = (lexemes(left), lexemes(right)) else {
        return false;
    };

    if left.len() != right.len()
        || !left
            .iter()
            .zip(&right)
            .all(|(left, right)| left.kind == right.kind && left.text == right.text)
    {
        return false;
    }

    left.windows(2).zip(right.windows(2)).all(|(left, right)| {
        let left_first = &left[0];
        let left_second = &left[1];
        let right_first = &right[0];
        let right_second = &right[1];
        let spacing_matters = spacing_matters(left_first.kind, left_second.kind);

        !spacing_matters || (left_first.end == left_second.start) == (right_first.end == right_second.start)
    })
}

fn lexemes(text: &str) -> Option<Vec<Lexeme<'_>>> {
    let mut at = 0;
    let mut lexemes = Vec::new();

    for token in rustc_lexer::tokenize(text) {
        let start = at;
        at += token.len;

        match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { terminated: true } => {}
            kind if valid(kind) => lexemes.push(Lexeme {
                kind,
                text: text.get(start..at)?,
                start,
                end: at,
            }),
            _invalid => return None,
        }
    }

    Some(lexemes)
}

const fn valid(kind: TokenKind) -> bool {
    match kind {
        TokenKind::Unknown | TokenKind::BlockComment { terminated: false } | TokenKind::Lifetime { starts_with_number: true } => false,
        TokenKind::Literal { kind, .. } => valid_literal(kind),
        _ => true,
    }
}

const fn valid_literal(kind: LiteralKind) -> bool {
    matches!(
        kind,
        LiteralKind::Int { empty_int: false, .. }
            | LiteralKind::Float { empty_exponent: false, .. }
            | LiteralKind::Char { terminated: true }
            | LiteralKind::Byte { terminated: true }
            | LiteralKind::Str { terminated: true }
            | LiteralKind::ByteStr { terminated: true }
            | LiteralKind::RawStr {
                started: true,
                terminated: true,
                ..
            }
            | LiteralKind::RawByteStr {
                started: true,
                terminated: true,
                ..
            }
    )
}

const fn is_punctuation(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Semi
            | TokenKind::Comma
            | TokenKind::Dot
            | TokenKind::At
            | TokenKind::Pound
            | TokenKind::Tilde
            | TokenKind::Question
            | TokenKind::Colon
            | TokenKind::Dollar
            | TokenKind::Eq
            | TokenKind::Not
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::Minus
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Plus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Caret
            | TokenKind::Percent
    )
}

const fn spacing_matters(left: TokenKind, right: TokenKind) -> bool {
    if is_punctuation(left) && is_punctuation(right) {
        return true;
    }

    is_word(left) && is_word(right)
}

const fn is_word(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident | TokenKind::RawIdent | TokenKind::Literal { .. } | TokenKind::Lifetime { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> DefaultPaths {
        DefaultPaths::of(&syn::parse_file("").expect("an empty file parses"))
    }

    #[test]
    fn layout_and_comments_do_not_change_token_equality() {
        assert!(same_tokens("Some(1)", "Some /* note */ ( 1 )"));
    }

    #[test]
    fn punctuation_jointness_remains_semantic() {
        assert!(!same_tokens("a >= b", "a > = b"));
        assert!(!same_tokens("c\"text\"", "c \"text\""));
    }

    #[test]
    fn repeated_noop_checks_do_not_advance_the_proc_macro_source_map() {
        let marker = || {
            "source_map_marker"
                .parse::<proc_macro2::TokenStream>()
                .expect("the marker tokenizes")
                .into_iter()
                .next()
                .expect("the marker has one token")
                .span()
                .byte_range()
                .start
        };
        let first = marker();
        let second = marker();
        let expected_growth = second.saturating_sub(first);
        let defaults = DefaultPaths::of(&syn::parse_file("").expect("an empty file parses"));
        let defaulted = vec!["T".to_owned()];

        for _candidate in 0..10_000 {
            assert!(is_noop(
                "&*Box::leak(Box::new(Default::default()))",
                "{ Box::leak(Box::new(T::default())) }",
                Shape::Block,
                &defaults,
                &defaulted,
            ));
        }

        assert_eq!(marker().saturating_sub(second), expected_growth);
    }

    /// The substring pre-check that skips tokenizing non-leak shapes must not change the answer:
    /// it has to still recognize a genuine match and still reject anything lacking the literal
    /// `leak` text, which is the only case it fast-paths.
    #[test]
    fn the_leak_prefilter_rejects_and_accepts_the_same_pairs_as_full_tokenization() {
        let defaults = defaults();

        assert!(is_same_leak(
            "Box::leak(Box::new(Default::default()))",
            "Box::leak(Box::new(T::default()))",
            &defaults,
            &[String::from("T")],
        ));
        assert!(!is_same_leak("None", "0", &defaults, &[]));
        assert!(!is_same_leak("Box::leak(Box::new(1))", "0", &defaults, &[]));
    }

    #[test]
    fn leaked_values_and_call_arguments_reject_non_plain_calls() {
        let leaked = "&*(Box::leak(Box::new((value))))";

        assert_eq!(leaked_value(leaked), Some("(value)"));

        let extra = "Box::new(value)()";
        let extra_tokens = lexemes(extra).expect("the extra call tokenizes");
        assert_eq!(call_argument(extra, &extra_tokens, "Box", "new"), None);

        let multiple = "Box::new(value, other)";
        let multiple_tokens = lexemes(multiple).expect("the multi-argument call tokenizes");
        assert_eq!(call_argument(multiple, &multiple_tokens, "Box", "new"), None);

        let empty = "Box::new()";
        let empty_tokens = lexemes(empty).expect("the empty call tokenizes");
        assert_eq!(call_argument(empty, &empty_tokens, "Box", "new"), None);
    }

    #[test]
    fn default_call_checks_reject_invalid_shapes() {
        let defaults = defaults();

        assert!(!is_default_call("/*", &defaults, &[]));
        assert!(!is_default_call("Default::default", &defaults, &[]));
        assert!(!is_default_call("Default::default(value)", &defaults, &[]));
        assert!(!is_default_call("1()", &defaults, &[]));
    }

    #[test]
    fn invalid_tokens_are_rejected_while_literals_remain_valid() {
        assert!(!same_tokens("/*", "/*"));
        assert!(lexemes("/*").is_none());

        let invalid = rustc_lexer::tokenize("/*")
            .next()
            .expect("the unterminated block comment yields one token")
            .kind;
        assert!(!valid(invalid));

        #[rustfmt::skip]
        let literal_kind = rustc_lexer::tokenize("1").find_map(|token| match token.kind { TokenKind::Literal { kind, .. } => Some(kind), _ => None }).expect("an integer tokenizes as a literal");
        let valid_literal_fn: fn(LiteralKind) -> bool = valid_literal;
        assert!(valid_literal_fn(literal_kind));
    }
}
