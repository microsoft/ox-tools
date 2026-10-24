// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How deeply source text nests, measured before anything recursive is pointed at it.

use super::Comment;
use super::comment::literal_end;

/// The deepest delimiter nesting a file may have and still be analyzed.
///
/// Source under audit is input, not code this project wrote: a user can point the tool at any file
/// that parses, and machine-generated files nest far deeper than hand-written ones. Everything
/// downstream descends by recursion — `syn`'s parser, the collector's visitor, the scope walk, and
/// the render that splices guards back in — so a file deep enough exhausts the stack, and a stack
/// overflow on Linux is a `SIGSEGV` that names neither the file nor the stage. Rejecting the file
/// by name costs one file's worth of coverage; the alternative costs the whole run and says
/// nothing about why.
///
/// The number is set from measurement at both ends. Every file in this workspace nests at most 20
/// levels, which is what hand-written Rust looks like, so the limit is three times what real code
/// needs. Against that, a debug build on a 2 MiB worker thread — the smallest stack the discovery
/// pass runs on — overflows between 140 and 200 levels on the parse-and-collect path. Sitting an
/// order of magnitude above real code and well under half the measured failure point leaves room
/// for both a deeper file than anyone here writes and a stage that grows its stack frames.
///
/// `pub` within this private module so the crate's proc-macro agreement test can pin the
/// proc-macro's hand-copied `NESTING_LIMIT` against it through the `internals` facade.
pub const NESTING_LIMIT: usize = 64;

/// How many links a chain may have per level of delimiter nesting allowed.
///
/// One operator in a chain costs a shallower stack than one delimiter does — a delimiter opens a
/// block or a call, several frames apiece, where an operator adds one `Expr` node — so the two
/// bounds are not the same number. The multiple is what separates a chain long enough to matter
/// from ordinary code: a `|` pattern with dozens of alternatives, or a builder called forty times
/// over, is written by hand and parses flat or nearly so, and refusing it would cost a file's
/// coverage for nothing.
///
/// `pub` within this private module for the same reason [`NESTING_LIMIT`] is: the proc-macro
/// agreement test pins the proc-macro's hand-copied `CHAIN_FACTOR` against this one.
pub const CHAIN_FACTOR: usize = 4;

/// Whether the tokens just before the current byte can end an expression.
///
/// The nesting precheck does not need to parse Rust to answer this narrowly: calls and indexes
/// are postfix only after an expression-shaped token, while an opening delimiter after an operator
/// or a separator starts an ordinary grouped, array, or block expression.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Previous {
    Other,
    Expression,
    Operator,
    Prefix,
}

impl Previous {
    const fn can_end_expression(self) -> bool {
        matches!(self, Self::Expression | Self::Operator)
    }
}

/// The offset of the token that first takes `text` past what `limit` allows.
///
/// The scan is a byte walk with counters, which is the point: a recursive measurement of how deeply
/// something recurses would be the same defect wearing the fix's clothes.
///
/// The cumulative path count covers more than delimiters. A run of prefix operators (`----1`), a
/// chain of same-precedence binary operators (`a || b || c || …`), an `else if` ladder, and a
/// postfix chain (`f()()…` or `x[0][0]…`) each build a tree one level deeper per token while opening
/// no bracket that remains open — and `syn`'s parser, the collector's visitor and the `Box<Expr>`
/// drop all descend that tree once per level. Counting delimiters alone admits those files and
/// lets them overflow the stack, which on Linux is a `SIGSEGV` naming neither the file nor the
/// stage.
///
/// The path inherits the cost accumulated by every enclosing expression and is reset to that
/// inherited base by `;` and `,`, since sibling expressions do not lie on one AST path.
///
/// Comments and literals are stepped over rather than counted. A bracket inside a string or a
/// comment is text, and counting it would refuse a file for the shape of its documentation — a
/// module doc drawing a diagram out of brackets is not a nesting hazard. `comments` must be the
/// spans this module's own scanner found, in source order; it skips literals exactly as this walk
/// does, so the two agree on where a comment begins.
#[expect(
    clippy::too_many_lines,
    reason = "delimiter and expression-path state must advance together through one byte walk"
)]
pub(super) fn beyond(text: &str, comments: &[Comment], limit: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let path_limit = limit.saturating_mul(CHAIN_FACTOR);
    let mut depth: usize = 0;
    let mut path: Vec<usize> = vec![0];
    let mut base: Vec<usize> = vec![0];
    let mut peak: Vec<usize> = vec![0];
    let mut ladders: Vec<usize> = vec![0];
    let mut awaiting_else: Vec<bool> = vec![false];
    let mut previous = Previous::Other;
    let mut at = 0;
    let mut next = 0;

    while at < bytes.len() {
        if let Some(comment) = comments.get(next)
            && comment.span.start <= at
        {
            at = at.max(comment.span.end);
            // #[gamma::skip(all, reason = "not advancing the comment index repeatedly revisits it until timeout")]
            next += 1;
            continue;
        }

        if let Some(end) = literal_end(text, at) {
            if previous == Previous::Expression {
                let inherited = base.get(depth).copied().unwrap_or_default();
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut path, depth, inherited);
            }
            // #[gamma::skip(all, reason = "not advancing past a literal retries it forever")]
            at = end;
            previous = Previous::Expression;
            continue;
        }

        if let Some(end) = identifier_end(text, at) {
            let identifier = text
                .get(at..end)
                .expect("identifier_end returns an ordered UTF-8-boundary range in this text");

            if identifier == "else"
                && awaiting_else.get(depth).copied().unwrap_or(
                    // #[gamma::skip(literal.bool_flip, reason = "awaiting_else starts with a root entry and both opening-delimiter branches set an entry at every new depth; closers only lower depth with saturation. Its get(depth) is always Some, so changing the unwrap_or fallback is unobservable.", tag = "equivalent")]
                    false,
                )
            {
                // #[gamma::skip(stmt.delete_call, reason = "ladders already contains the current root or a slot initialized by the opening delimiter before an else can be handled. grow at this point is a no-op; growing one extra future slot also has no effect because opening that depth overwrites its ladder value before reading it.", tag = "equivalent")]
                // #[gamma::skip(call.replace_with_default, reason = "ladders already contains the current root or a slot initialized by the opening delimiter before an else can be handled. grow at this point is a no-op; growing one extra future slot also has no effect because opening that depth overwrites its ladder value before reading it.", tag = "equivalent")]
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                grow(
                    &mut ladders,
                    // #[gamma::skip(expr.increment, reason = "ladders already contains the current root or a slot initialized by the opening delimiter before an else can be handled. grow at this point is a no-op; growing one extra future slot also has no effect because opening that depth overwrites its ladder value before reading it.", tag = "equivalent")]
                    depth,
                );
                ladders[depth] += 1;
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, false);

                if base.get(depth).copied().unwrap_or_default().saturating_add(ladders[depth]) > path_limit
                    || link(&mut path, &mut peak, depth, path_limit)
                {
                    return Some(at);
                }
                previous = Previous::Other;
            } else if identifier == "as" {
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;

                if link(&mut path, &mut peak, depth, path_limit) {
                    return Some(at);
                }
            } else {
                if awaiting_else.get(depth).copied().unwrap_or(
                    // #[gamma::skip(literal.bool_flip, reason = "awaiting_else starts with a root entry and both opening-delimiter branches set an entry at every new depth; closers only lower depth with saturation. Its get(depth) is always Some, so changing the unwrap_or fallback is unobservable.", tag = "equivalent")]
                    false,
                ) {
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    set(&mut ladders, depth, 0);
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    set_bool(&mut awaiting_else, depth, false);
                }
                if previous == Previous::Expression {
                    let inherited = base.get(depth).copied().unwrap_or_default();
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    set(&mut path, depth, inherited);
                }
                previous = Previous::Expression;
            }

            // #[gamma::skip(all, reason = "not advancing past an identifier retries it forever")]
            at = end;
            continue;
        }

        let byte = bytes[at];

        // Counting every operator byte is conservative for multi-byte binary tokens and exact for
        // prefix runs, where each punctuator contributes another unary expression node.
        if is_operator(byte) {
            if link(&mut path, &mut peak, depth, path_limit) {
                return Some(at);
            }

            previous = if matches!(byte, b'?' | b'>') {
                Previous::Operator
            } else {
                Previous::Prefix
            };
            at += 1;
            continue;
        }

        match byte {
            b'(' | b'[' => {
                let is_postfix = previous.can_end_expression();

                if is_postfix &&
                // #[gamma::skip(call_result.default, reason = "default_after preserves link's path and peak increments. If its true result is replaced by false, the immediately following inherited=path+CHAIN_FACTOR test still rejects at the same opener offset; nonoverflowing links follow the identical path.", tag = "equivalent")]
                link(&mut path, &mut peak, depth, path_limit)
                {
                    return Some(at);
                }

                let inherited = path.get(depth).copied().unwrap_or_default().saturating_add(CHAIN_FACTOR);
                depth += 1;

                if inherited > path_limit {
                    return Some(at);
                }

                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut path, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut base, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut peak, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut ladders, depth, 0);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;
            }

            b'{' => {
                let enclosing = if previous == Previous::Prefix { &path } else { &base };
                let inherited = enclosing.get(depth).copied().unwrap_or_default().saturating_add(CHAIN_FACTOR);
                depth += 1;

                if inherited > path_limit {
                    return Some(at);
                }

                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut path, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut base, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut peak, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut ladders, depth, 0);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;
            }

            // An unbalanced closer means the file is not Rust, which the parser reports far better
            // than a depth counter could. Saturating keeps the count from wrapping around into a
            // depth that would reject every delimiter after it.
            b')' | b']' => {
                let nested = peak.get(depth).copied().unwrap_or_default();
                depth = depth.saturating_sub(1);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                raise(&mut path, depth, nested);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                raise(&mut peak, depth, nested);
                previous = Previous::Expression;
            }

            b'}' => {
                let nested = peak.get(depth).copied().unwrap_or_default();
                depth = depth.saturating_sub(1);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                raise(&mut path, depth, nested);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                raise(&mut peak, depth, nested);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, true);
                previous = Previous::Expression;
            }

            // A chain cannot span a separator, so the count starts again after one.
            b';' | b',' => {
                let inherited = base.get(depth).copied().unwrap_or_default();
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut path, depth, inherited);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set(&mut ladders, depth, 0);
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;
            }

            b':' | b'#' | b'@' | b'$' | b'\'' => {
                previous = Previous::Other;
            }

            byte if byte.is_ascii_whitespace() => {}

            byte if byte.is_ascii() => {
                if previous == Previous::Expression {
                    let inherited = base.get(depth).copied().unwrap_or_default();
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
                    set(&mut path, depth, inherited);
                }
                previous = Previous::Expression;
            }

            _ => {
                // #[gamma::skip(all, reason = "not advancing over a non-ASCII character leaves the scan on it forever")]
                at += text.get(at..).and_then(|rest| rest.chars().next()).map_or(1, char::len_utf8);
                continue;
            }
        }

        // #[gamma::skip(all, reason = "moving backward or not advancing makes the byte scan non-progressing")]
        at += 1;
    }

    None
}

/// Counts one more link at `depth`, returning whether that took the chain past `limit`.
fn link(path: &mut Vec<usize>, peak: &mut Vec<usize>, depth: usize, limit: usize) -> bool {
    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
    grow(path, depth);
    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
    grow(peak, depth);

    let links = path.get_mut(depth).expect("grow(path, depth) made `depth` a valid index");

    *links += 1;
    peak[depth] = peak[depth].max(*links);

    *links > limit
}

/// Sets the path cost at `depth`.
fn set(path: &mut Vec<usize>, depth: usize, value: usize) {
    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
    grow(path, depth);
    path[depth] = value;
}

/// Retains the deeper of the current and completed nested paths.
fn raise(path: &mut Vec<usize>, depth: usize, value: usize) {
    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after evaluates it first, preserving every effect and the same unit result.", tag = "equivalent")]
    grow(path, depth);
    path[depth] = path[depth].max(value);
}

/// Makes `depth` a valid index, which an unbalanced closer can otherwise leave it short of.
fn grow(chain: &mut Vec<usize>, depth: usize) {
    // #[gamma::skip(all, reason = "reversing the growth condition appends without approaching termination and exhausts memory")]
    while chain.len() <= depth {
        // #[gamma::skip(stmt.delete_call, reason = "removing the only growth operation makes this loop infinite")]
        chain.push(0);
    }
}

fn set_bool(values: &mut Vec<bool>, depth: usize, value: bool) {
    // #[gamma::skip(relational.le_to_ge, reason = "reversing the growth condition appends without approaching termination and exhausts memory")]
    while values.len() <= depth {
        // #[gamma::skip(stmt.delete_call, reason = "removing the only growth operation makes this loop infinite")]
        values.push(false);
    }
    values[depth] = value;
}

/// Whether a byte can be part of an operator token.
///
/// `.` is here because a field access and a method call nest their receiver exactly as a binary
/// operator nests its left operand, so a chain of forty calls is forty levels deep. `:` is not,
/// because a path is flat and `a::b::c::d` says nothing about depth.
const fn is_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'!' | b'<' | b'>' | b'=' | b'?' | b'.'
    )
}

/// The end of the Rust identifier beginning at `at`.
// #[gamma::skip(parameter.default_shadow, tag = "timeout", reason = "resetting at returns endpoints from the beginning of the text and repeatedly revisits identifiers")]
fn identifier_end(text: &str, at: usize) -> Option<usize> {
    let mut characters = text.get(at..)?.char_indices();
    let (_, first) = characters.next()?;

    if !rustc_lexer::is_id_start(first) {
        // #[gamma::skip(option.none_to_some, reason = "claiming punctuation starts a zero-width identifier leaves the outer scanner non-progressing")]
        return
        // #[gamma::skip(return_value.default, reason = "None is already Option::default(); replacing just this value cannot change an outcome.", tag = "equivalent")]
        None;
    }

    Some(
        characters
            .take_while(|(_, character)| rustc_lexer::is_id_continue(*character))
            .last()
            // #[gamma::skip(all, reason = "corrupting the endpoint can move the outer scanner backward or leave it non-progressing")]
            .map_or_else(
                || at + first.len_utf8(),
                |(offset, character)| {
                    // #[gamma::skip(arith.add_to_sub, tag = "timeout", reason = "subtracting an identifier offset or width can move the outer scanner backward and repeat the same identifier")]
                    at + offset + character.len_utf8()
                },
            ),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::num::NonZeroUsize;
    use std::{env, fs};

    use camino::Utf8Path;
    use walkdir::WalkDir;

    use super::super::comment::scan_comments;
    use super::super::source_file::line_starts;
    use super::*;

    #[test]
    fn postfix_operator_markers_preserve_the_exact_last_link_before_an_open_delimiter() {
        for operator in ["?", ">"] {
            let at_limit = format!("{}()", operator.repeat(3));
            let beyond_limit = format!("{}()", operator.repeat(4));
            assert_eq!(beyond(&at_limit, &[], 2), None, "{at_limit}");
            assert_eq!(beyond(&beyond_limit, &[], 2), Some(4), "{beyond_limit}");
        }
        for literal in ["\"text\"", "'q'", "r#\"text\"#"] {
            let source = format!("++++{literal} \"second\" +");
            assert_eq!(beyond(&source, &[], 1), None, "{source}");
        }
    }

    #[test]
    fn repeated_casts_and_unrelated_tokens_clear_only_current_else_markers() {
        for separator in ["as T ", "value ", "\"text\" ", "; ", ", "] {
            let source = format!("if ready {{}} {separator}else {{}}");
            assert_eq!(beyond(&source, &[], 2), None, "{source}");
        }
        assert_eq!(beyond("((((1))))", &[], 4), None);
        assert_eq!(beyond("((((1))))+", &[], 4), Some(9));
    }

    #[test]
    fn a_cast_clears_the_current_else_marker_even_when_opaque_macro_tokens_omit_its_type() {
        let source = "m!{ if a {} ++ as else }";
        assert_eq!(beyond(source, &[], 3), None);
    }

    #[test]
    fn nested_sibling_boundaries_keep_peak_and_base_costs_at_the_current_depth() {
        for (source, limit, expected) in [
            ("(------1;())+++", 3, Some(14)),
            ("((1),(()))", 2, Some(6)),
            ("!!!call(())", 3, None),
            ("(())()", 3, Some(4)),
            ("m!{----1 2 ++++++++}", 3, Some(18)),
        ] {
            assert_eq!(beyond(source, &[], limit), expected, "{source}");
        }
    }

    #[test]
    fn public_shape_guard_accumulates_unary_cost_across_mixed_groups() {
        // Each level adds its unary run plus CHAIN_FACTOR for the delimiter. The chosen total
        // equals NESTING_LIMIT * CHAIN_FACTOR while group depth remains below NESTING_LIMIT;
        // one extra prefix exceeds the cumulative allowance, including with mixed delimiters.
        for (open, close) in [("(", ")"), ("[", "]"), ("{", "}")] {
            let boundary = format!("{}true{}", format!("{}{open}", "!".repeat(12)).repeat(16), close.repeat(16));
            assert!(!crate::parse::exceeds_nesting_limit(&boundary), "{open}");
            assert!(crate::parse::exceeds_nesting_limit(&format!("!{boundary}")), "{open}");
        }
        let mixed = format!("{}true{}", "!!!!!!!!!!!!(!!!!!!!!!!!!{".repeat(8), "})".repeat(8));
        assert!(!crate::parse::exceeds_nesting_limit(&mixed));
        assert!(crate::parse::exceeds_nesting_limit(&format!("!{mixed}")));
    }

    #[test]
    fn macro_token_else_markers_only_count_after_a_current_closing_block() {
        for (text, limit) in [
            ("m!((---else))", 3),
            ("m!({} else (--1))", 4),
            ("m!({} item-------else)", 3),
            ("m!(({}) (else--1))", 5),
            ("m!({{}} {else-------1})", 4),
            ("m!({},-------else)", 3),
        ] {
            assert_eq!(beyond_limit(text, limit), None, "{text}");
        }
        let text = "m!({} else (---1))";
        assert_eq!(beyond_limit(text, 4), text.rfind('-'));
    }

    #[test]
    fn unmatched_closers_do_not_erase_the_root_else_ladder_budget() {
        let prefix = format!("if ready {{}}{}", " else if ready {}".repeat(8));
        assert_eq!(beyond_limit(&prefix, 2), None);
        for closer in [')', ']'] {
            let text = format!("{prefix}{closer} else {{}}");
            assert_eq!(beyond_limit(&text, 2), Some(prefix.len() + 2), "{text}");
        }
    }

    #[test]
    fn exact_else_ladder_budget_resets_in_sibling_expressions_and_reused_delimiter_depths() {
        fn ladder(links: NonZeroUsize) -> String {
            format!("if a {{}} {}else {{}}", "else if a {} ".repeat(links.get() - 1))
        }
        let at_limit = ladder(NonZeroUsize::new(8).unwrap());
        assert_eq!(beyond(&at_limit, &[], 2), None);
        let excessive = ladder(NonZeroUsize::new(9).unwrap());
        assert_eq!(beyond(&excessive, &[], 2), excessive.match_indices("else").nth(8).map(|(at, _)| at));
        for separator in ["; ", ", ", " "] {
            let siblings = format!("{at_limit}{separator}{at_limit}");
            assert_eq!(beyond(&siblings, &[], 2), None, "{separator:?}");
        }
        let nested = ladder(NonZeroUsize::new(8).unwrap());
        for text in [
            format!("({nested}); ({nested})"),
            format!("{{{nested}}} {{{nested}}}"),
            format!("[{nested}], [{nested}]"),
        ] {
            assert_eq!(beyond(&text, &[], 3), None, "{text}");
        }
        let retained_outer_base = "{{----1}; 2---------1}";
        assert_eq!(beyond(retained_outer_base, &[], 3), retained_outer_base.rfind('-'));
    }

    fn beyond_limit(text: &str, limit: usize) -> Option<usize> {
        let comments = scan_comments(text, &line_starts(text));

        beyond(text, &comments, limit)
    }

    #[test]
    fn nested_chain_diagnostics_keep_inherited_cost_after_comments_literals_and_closers() {
        for (open, close) in [("(", ")"), ("[", "]"), ("{", "}")] {
            let at_limit = format!("{open}{open}----1{close}{close}");
            assert_eq!(beyond_limit(&at_limit, 3), None, "{at_limit}");
            let overflow = format!("{open}{open}-----1{close}{close}");
            assert_eq!(beyond_limit(&overflow, 3), Some(6), "{overflow}");
            let after_close = format!("{open}{open}----1{close}+{close}");
            assert_eq!(beyond_limit(&after_close, 3), Some(8), "{after_close}");
            let after_both = format!("{open}{open}----1{close}{close}+");
            assert_eq!(beyond_limit(&after_both, 3), Some(9), "{after_both}");
            for ignored in ["/* [ ((( +++++ */", "// ] } +++++\n"] {
                let text = format!("{open}{open}----{ignored}+1{close}{close}");
                assert_eq!(beyond_limit(&text, 3), Some(6 + ignored.len()), "{text}");
            }
            for literal in ["\"++++(]\"", "r##\"+++({\"##", "'+'"] {
                let text = format!("{open}{open}----{literal}+{close}{close}");
                assert_eq!(beyond_limit(&text, 3), Some(6 + literal.len()), "{text}");
            }
            let byte_literal = format!("{open}{open}b'('----1{close}{close}");
            assert_eq!(beyond_limit(&byte_literal, 3), None, "{byte_literal}");
            let byte_literal = format!("{open}{open}b'('-----1{close}{close}");
            assert_eq!(beyond_limit(&byte_literal, 3), Some(10), "{byte_literal}");
            for separator in [";", ","] {
                let text = format!("{open}{open}----1{separator}----2{close}{close}");
                assert_eq!(beyond_limit(&text, 3), None, "{text}");
                let text = format!("{open}{open}----1{separator}-----2{close}{close}");
                assert_eq!(beyond_limit(&text, 3), Some(12), "{text}");
            }
        }
    }

    #[test]
    fn adjacent_tokens_reset_paths_without_erasing_postfix_or_ladder_cost() {
        for boundary in [" value ", " \"text\" ", " 7 "] {
            let text = format!("++++1{boundary}+");
            assert_eq!(beyond_limit(&text, 1), None, "{text}");
        }
        for operator in ["?", ">"] {
            let text = format!("{operator}()");
            assert_eq!(beyond_limit(&text, 1), Some(1), "{text}");
        }
        for punctuation in [":", "#", "@", "$", "'"] {
            let text = format!("value{punctuation}()");
            assert_eq!(beyond_limit(&text, 1), None, "{text}");
        }
        for text in ["++++1🦀+", "++++1 /* comment */ +", "++++1 // comment\n+"] {
            assert_eq!(beyond_limit(text, 1), Some(text.len() - 1), "{text}");
        }
        assert_eq!(beyond_limit("else", 0), None);
        assert_eq!(beyond_limit("if a {} else", 1), Some(8));
        assert_eq!(beyond_limit("if a {} unrelated else", 1), None);
        assert_eq!(beyond_limit("if a {} as T; else", 1), Some(8));
        assert_eq!(beyond_limit("if a {}; else", 1), None);
        assert_eq!(beyond_limit("if a {}, else", 1), None);
    }

    #[test]
    fn nesting_helpers_retain_exact_depths_peaks_and_unicode_endpoints() {
        let mut path = Vec::new();
        let mut peak = Vec::new();
        assert!(!link(&mut path, &mut peak, 2, 2));
        assert_eq!(path, [0, 0, 1]);
        assert_eq!(peak, [0, 0, 1]);
        assert!(!link(&mut path, &mut peak, 2, 2));
        assert!(link(&mut path, &mut peak, 2, 2));
        assert_eq!(peak, [0, 0, 3]);
        set(&mut path, 2, 1);
        raise(&mut path, 2, 2);
        raise(&mut path, 2, 1);
        raise(&mut path, 4, 7);
        assert_eq!(path, [0, 0, 2, 0, 7]);
        let mut flags = Vec::new();
        set_bool(&mut flags, 2, true);
        set_bool(&mut flags, 2, false);
        assert_eq!(flags, [false, false, false]);

        for (text, at, expected) in [
            ("xx café!", 3, Some(8)),
            ("xx a!", 3, Some(4)),
            ("xx αβ!", 3, Some(7)),
            ("a", 1, None),
            ("é", 1, None),
            ("", 0, None),
            ("+", 0, None),
        ] {
            assert_eq!(identifier_end(text, at), expected, "{text} at {at}");
        }
    }

    #[test]
    fn nesting_path_resets_and_postfix_boundaries_have_exact_offsets() {
        for (text, expected) in [
            ("++++", None),
            ("+++++", Some(4)),
            ("a()", Some(1)),
            ("()", None),
            ("()()", Some(2)),
            ("[]", None),
            ("[][]", Some(2)),
            ("{{", Some(1)),
            ("++++ {}", Some(5)),
            ("++++ {}+", Some(5)),
            ("++++ 1 \"x\" +", None),
            ("++++ 1 value +", None),
            ("++++ 1 2 +", None),
            ("++++ 1;+", None),
            ("++++ 1,+", None),
            ("++++// ignored\n+", Some(15)),
        ] {
            assert_eq!(beyond_limit(text, 1), expected, "{text}");
        }
    }

    #[test]
    fn cast_and_else_ladder_limits_reset_only_at_real_path_boundaries() {
        for limit in 2..=5 {
            let links = limit * CHAIN_FACTOR;
            let casts = format!("value{}", " as T".repeat(links));
            assert_eq!(beyond_limit(&casts, limit), None, "{casts}");
            let over = format!("{casts} as T");
            assert_eq!(beyond_limit(&over, limit), Some(casts.len() + 1), "{over}");
            for separator in [";", ","] {
                let sibling = format!("{casts}{separator}{casts}");
                assert_eq!(beyond_limit(&sibling, limit), None, "{sibling}");
            }
            let ladder = format!("if ready {{}}{}", " else if ready {}".repeat(links));
            assert_eq!(beyond_limit(&ladder, limit), None, "{ladder}");
            let over = format!("{ladder} else if ready {{}}");
            assert_eq!(beyond_limit(&over, limit), Some(ladder.len() + 1), "{over}");
            if limit >= 3 {
                let inner = format!("if ready {{}}{}", " else if ready {}".repeat(links - CHAIN_FACTOR));
                let nested = format!("{{{inner}}}");
                assert_eq!(beyond_limit(&nested, limit), None, "{nested}");
                let nested_over = format!("{{{inner} else if ready {{}}}}");
                assert_eq!(beyond_limit(&nested_over, limit), Some(inner.len() + 2), "{nested_over}");
            }
            for separator in [";", ",", " unrelated ", " as T;"] {
                let siblings = format!("{ladder}{separator}{ladder}");
                assert_eq!(beyond_limit(&siblings, limit), None, "{siblings}");
            }
            for delimiters in [("(", ")"), ("[", "]"), ("{", "}")] {
                let balanced = format!("{}{}", delimiters.0.repeat(limit), delimiters.1.repeat(limit));
                assert_eq!(beyond_limit(&balanced, limit), None, "{balanced}");
                let over = delimiters.0.repeat(limit + 1);
                assert_eq!(beyond_limit(&over, limit), Some(limit), "{over}");
            }
        }
        for punctuation in [":", "#", "@", "$", "'"] {
            let text = format!("value{punctuation}()");
            assert_eq!(beyond_limit(&text, 1), None, "{text}");
        }
        for operator in ["?", ">"] {
            let text = format!("value{operator}()");
            assert_eq!(beyond_limit(&text, 1), Some("value?".len()), "{text}");
        }
    }

    #[test]
    fn a_file_within_the_limit_reports_nothing() {
        assert_eq!(beyond_limit("fn f() -> i32 { (((1))) }\n", 4), None);
    }

    #[test]
    fn the_reported_offset_is_the_delimiter_that_crossed_the_limit() {
        let text = "fn f() -> i32 { ((1)) }\n";

        // `{`, `(`, `(` is three levels, so a limit of two is crossed by the second parenthesis.
        assert_eq!(beyond_limit(text, 2), Some(text.find("((").expect("the fixture nests") + 1));
    }

    /// Brackets in prose and in strings are text, not structure.
    ///
    /// A doc comment drawing a tree out of brackets, or a parser test holding a string of them, is
    /// ordinary and must not be refused: the depth that matters is the one the tree has, and
    /// neither of those reaches the tree at all.
    #[test]
    fn brackets_inside_comments_and_literals_are_not_nesting() {
        let commented = format!("// {}\nfn f() {{}}\n", "(".repeat(500));
        let documented = format!("/* {} */\nfn f() {{}}\n", "[".repeat(500));
        let quoted = format!("fn f() -> &'static str {{ \"{}\" }}\n", "{".repeat(500));
        let raw = format!("fn f() -> &'static str {{ r#\"{}\"# }}\n", "(".repeat(500));

        for text in [commented, documented, quoted, raw] {
            assert_eq!(beyond_limit(&text, 8), None, "for {text:.40}");
        }
    }

    /// Nesting is depth, not count: a file of a thousand sibling constructs nests one level.
    #[test]
    fn siblings_do_not_accumulate_depth() {
        let text = "fn f() {}\n".repeat(1_000);

        assert_eq!(beyond_limit(&text, 2), None);
    }

    /// A closer with nothing open cannot lower the count below zero, and cannot make the
    /// delimiters after it look deeper than they are.
    #[test]
    fn an_unbalanced_closer_does_not_wrap_the_count() {
        assert_eq!(beyond_limit(")))))fn f() { (1) }\n", 4), None);
    }

    #[test]
    fn blocks_cross_the_limit_without_needing_parentheses() {
        assert_eq!(beyond_limit("{{", 1), Some(1));
    }

    /// A run of prefix operators nests one level per operator while opening no delimiter.
    ///
    /// `syn` recurses once per operator to parse it, the visitor recurses once per operator to walk
    /// it, and the `Box<Expr>` chain recurses once per operator to drop it. Counted by delimiters
    /// alone the file looks trivial, and a few hundred of them segfault a discovery worker with no
    /// diagnostic at all.
    #[test]
    fn a_run_of_prefix_operators_is_nesting() {
        let text = format!("fn f() -> i32 {{ {}1 }}\n", "-".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a stack of unary operators must be refused");
    }

    #[test]
    fn an_operator_chain_crosses_its_exact_path_limit() {
        assert_eq!(beyond_limit("+++++", 1), Some(4));
    }

    /// The same run written with spaces between the operators is the same tree.
    #[test]
    fn a_spaced_run_of_prefix_operators_is_still_nesting() {
        let text = format!("fn f() -> i32 {{ {}1 }}\n", "- ".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "whitespace does not flatten the tree");
    }

    /// A same-precedence binary chain is linearly nested, however flat it looks.
    #[test]
    fn a_long_binary_chain_is_nesting() {
        let text = format!("fn f(a: bool) -> bool {{ a{} }}\n", " || a".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a chain of operators must be refused");
    }

    #[test]
    fn a_long_postfix_call_chain_is_nesting() {
        let text = format!("fn f() {{ g{}; }}\n", "()".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a chain of calls must be refused");
    }

    #[test]
    fn a_long_postfix_index_chain_is_nesting() {
        let text = format!("fn f() {{ value{}; }}\n", "[0]".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a chain of indexes must be refused");
    }

    #[test]
    fn a_long_postfix_method_chain_is_nesting() {
        let text = format!("fn f() {{ value{}; }}\n", ".call()".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a chain of method calls must be refused");
    }

    #[test]
    fn mixed_method_and_index_links_accumulate_on_one_path() {
        let text = format!("fn f() {{ value{}; }}\n", ".call()[0]".repeat(20));

        assert!(beyond_limit(&text, 8).is_some(), "interleaved postfix links must be refused");
    }

    #[test]
    fn operator_chains_inside_nested_delimiters_accumulate_on_one_path() {
        let mut expression = "value".to_owned();

        for _ in 0..8 {
            expression = format!("({expression} + value + value + value)");
        }

        assert!(
            beyond_limit(&format!("fn f() {{ {expression}; }}\n"), 8).is_some(),
            "nested operator chains must be refused"
        );
    }

    #[test]
    fn a_long_cast_chain_is_nesting() {
        let text = format!("fn f() {{ 1{}; }}\n", " as u64".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "a chain of casts must be refused");
    }

    /// An `else if` ladder de-recurses in the parser but not in the visitor or the drop.
    #[test]
    fn a_long_else_if_ladder_is_nesting() {
        let text = format!("fn f(a: bool) {{ if a {{}} {} }}\n", "else if a {} ".repeat(64));

        assert!(beyond_limit(&text, 8).is_some(), "an else-if ladder must be refused");
    }

    /// Ordinary code is not a chain: a block of separate statements starts the count again at each.
    ///
    /// Without the reset, any function long enough would be refused for being long rather than for
    /// being deep, which costs a file's coverage and explains nothing.
    #[test]
    fn statements_in_sequence_are_not_a_chain() {
        let body = "let x = 1 + 2 * 3; ".repeat(200);
        let text = format!("fn f() {{ {body} }}\n");

        assert_eq!(beyond_limit(&text, 8), None);
    }

    /// Nor are the elements of a long literal, which are separated by commas.
    #[test]
    fn a_long_list_of_elements_is_not_a_chain() {
        let elements = "1 + 2, ".repeat(200);
        let text = format!("fn f() {{ let xs = [{elements}]; }}\n");

        assert_eq!(beyond_limit(&text, 8), None);
    }

    /// Operators inside comments and strings are text here exactly as brackets are.
    #[test]
    fn operators_inside_comments_and_literals_are_not_nesting() {
        let commented = format!("// {}\nfn f() {{}}\n", "-".repeat(500));
        let quoted = format!("fn f() -> &'static str {{ \"{}\" }}\n", "|".repeat(500));

        for text in [commented, quoted] {
            assert_eq!(beyond_limit(&text, 8), None, "for {text:.40}");
        }
    }

    #[test]
    fn chain_keywords_inside_unicode_identifiers_are_not_counted() {
        assert_eq!(beyond_limit("{ let caféelse; }\n", 1), None);
        assert_eq!(beyond_limit("{ let µas; }\n", 1), None);
    }

    #[test]
    fn non_ascii_non_identifier_bytes_are_skipped_without_affecting_depth() {
        assert_eq!(beyond_limit("{ 🦀 }\n", 1), None);
    }

    #[test]
    fn scanner_primitives_have_exact_boundary_oracles() {
        assert!(!Previous::Other.can_end_expression());
        assert!(Previous::Expression.can_end_expression());
        assert!(Previous::Operator.can_end_expression());

        let mut path = vec![2];
        let mut peak = vec![3];
        assert!(!link(&mut path, &mut peak, 0, 3));
        assert_eq!(path, [3]);
        assert_eq!(peak, [3]);
        assert!(link(&mut path, &mut peak, 0, 3));
        assert_eq!(path, [4]);
        assert_eq!(peak, [4]);

        set(&mut path, 2, 7);
        assert_eq!(path, [4, 0, 7]);
        raise(&mut path, 1, 5);
        assert_eq!(path, [4, 5, 7]);
        raise(&mut path, 1, 3);
        assert_eq!(path, [4, 5, 7]);

        let mut flags = vec![true];
        set_bool(&mut flags, 2, true);
        assert_eq!(flags, [true, false, true]);
        set_bool(&mut flags, 1, true);
        assert_eq!(flags, [true, true, true]);

        assert!(is_operator(b'+'));
        assert!(is_operator(b'.'));
        assert!(!is_operator(b':'));
        assert_eq!(identifier_end("café + 1", 0), Some("café".len()));
        assert_eq!(identifier_end("café + 1", "café ".len()), None);
        assert_eq!(identifier_end("🦀", 0), None);
    }

    #[test]
    fn small_limit_crossings_report_the_exact_token() {
        for (text, expected) in [
            ("a+b", text_offset("a+b", "+")),
            ("a as u8", text_offset("a as u8", "as")),
            ("f()", text_offset("f()", "(")),
            ("()", 0),
        ] {
            assert_eq!(beyond_limit(text, 0), Some(expected), "{text:?}");
        }

        let ladder = "if a {} else if b {}";
        assert_eq!(beyond_limit(ladder, 1), Some(text_offset(ladder, "else")));
        assert_eq!(beyond_limit("a+b;c+d", 1), None);
        assert_eq!(beyond_limit("a+b,c+d", 1), None);
    }

    fn text_offset(text: &str, needle: &str) -> usize {
        text.find(needle).expect("the oracle needle is present")
    }

    /// Every source file in this workspace passes its own guard, at the limit the run uses.
    ///
    /// The counts added here are proxies, and a proxy that refuses ordinary Rust is worse than the
    /// crash it prevents. This is the check that keeps them calibrated against real code.
    #[test]
    fn this_workspace_is_within_the_limit() {
        // Gamma's scratch tree contains instrumented source whose guard expressions are
        // intentionally deeper than the source this calibration test is meant to measure.
        let _ = env::var_os("CARGO_GAMMA").is_none().then(assert_workspace_is_within_limit);
    }

    fn assert_workspace_is_within_limit() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

        for entry in WalkDir::new(root.as_std_path()).into_iter().filter_map(Result::ok) {
            let path = entry.path();

            if path.extension() != Some("rs".as_ref()) {
                continue;
            }

            let actual = fs::read_to_string(path).ok().and_then(|text| beyond_limit(&text, NESTING_LIMIT));
            let message = format!("{} is refused by its own guard", path.display());

            assert_eq!(actual, None, "{message}");
        }
    }
}
