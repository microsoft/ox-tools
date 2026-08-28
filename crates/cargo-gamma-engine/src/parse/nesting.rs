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
            next += 1;
            continue;
        }

        if let Some(end) = literal_end(text, at) {
            if previous == Previous::Expression {
                let inherited = base.get(depth).copied().unwrap_or_default();
                set(&mut path, depth, inherited);
            }
            at = end;
            previous = Previous::Expression;
            continue;
        }

        if let Some(end) = identifier_end(text, at) {
            let identifier = text.get(at..end).unwrap_or("");

            if identifier == "else" && awaiting_else.get(depth).copied().unwrap_or(false) {
                grow(&mut ladders, depth);
                ladders[depth] += 1;
                set_bool(&mut awaiting_else, depth, false);

                if ladders[depth] > path_limit || link(&mut path, &mut peak, depth, path_limit) {
                    return Some(at);
                }
                previous = Previous::Other;
            } else if identifier == "as" {
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;

                if link(&mut path, &mut peak, depth, path_limit) {
                    return Some(at);
                }
            } else {
                if awaiting_else.get(depth).copied().unwrap_or(false) {
                    set(&mut ladders, depth, 0);
                    set_bool(&mut awaiting_else, depth, false);
                }
                if previous == Previous::Expression {
                    let inherited = base.get(depth).copied().unwrap_or_default();
                    set(&mut path, depth, inherited);
                }
                previous = Previous::Expression;
            }

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
                Previous::Expression
            } else {
                Previous::Other
            };
            at += 1;
            continue;
        }

        match byte {
            b'(' | b'[' => {
                let is_postfix = previous == Previous::Expression;

                if is_postfix && link(&mut path, &mut peak, depth, path_limit) {
                    return Some(at);
                }

                let inherited = path.get(depth).copied().unwrap_or_default().saturating_add(CHAIN_FACTOR);
                depth += 1;

                if inherited > path_limit {
                    return Some(at);
                }

                set(&mut path, depth, inherited);
                set(&mut base, depth, inherited);
                set(&mut peak, depth, inherited);
                set(&mut ladders, depth, 0);
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;
            }

            b'{' => {
                let inherited = base.get(depth).copied().unwrap_or_default().saturating_add(CHAIN_FACTOR);
                depth += 1;

                if inherited > path_limit {
                    return Some(at);
                }

                set(&mut path, depth, inherited);
                set(&mut base, depth, inherited);
                set(&mut peak, depth, inherited);
                set(&mut ladders, depth, 0);
                set_bool(&mut awaiting_else, depth, false);
                previous = Previous::Other;
            }

            // An unbalanced closer means the file is not Rust, which the parser reports far better
            // than a depth counter could. Saturating keeps the count from wrapping around into a
            // depth that would reject every delimiter after it.
            b')' | b']' => {
                let nested = peak.get(depth).copied().unwrap_or_default();
                depth = depth.saturating_sub(1);
                raise(&mut path, depth, nested);
                raise(&mut peak, depth, nested);
                previous = Previous::Expression;
            }

            b'}' => {
                let nested = peak.get(depth).copied().unwrap_or_default();
                depth = depth.saturating_sub(1);
                raise(&mut path, depth, nested);
                raise(&mut peak, depth, nested);
                set_bool(&mut awaiting_else, depth, true);
                previous = Previous::Expression;
            }

            // A chain cannot span a separator, so the count starts again after one.
            b';' | b',' => {
                let inherited = base.get(depth).copied().unwrap_or_default();
                set(&mut path, depth, inherited);
                set(&mut ladders, depth, 0);
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
                    set(&mut path, depth, inherited);
                }
                previous = Previous::Expression;
            }

            _ => {
                at += text.get(at..).and_then(|rest| rest.chars().next()).map_or(1, char::len_utf8);
                continue;
            }
        }

        at += 1;
    }

    None
}

/// Counts one more link at `depth`, returning whether that took the chain past `limit`.
fn link(path: &mut Vec<usize>, peak: &mut Vec<usize>, depth: usize, limit: usize) -> bool {
    grow(path, depth);
    grow(peak, depth);

    let links = path.get_mut(depth).expect("grow(path, depth) made `depth` a valid index");

    *links += 1;
    peak[depth] = peak[depth].max(*links);

    *links > limit
}

/// Sets the path cost at `depth`.
fn set(path: &mut Vec<usize>, depth: usize, value: usize) {
    grow(path, depth);
    path[depth] = value;
}

/// Retains the deeper of the current and completed nested paths.
fn raise(path: &mut Vec<usize>, depth: usize, value: usize) {
    grow(path, depth);
    path[depth] = path[depth].max(value);
}

/// Makes `depth` a valid index, which an unbalanced closer can otherwise leave it short of.
fn grow(chain: &mut Vec<usize>, depth: usize) {
    while chain.len() <= depth {
        chain.push(0);
    }
}

fn set_bool(values: &mut Vec<bool>, depth: usize, value: bool) {
    while values.len() <= depth {
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
fn identifier_end(text: &str, at: usize) -> Option<usize> {
    let mut characters = text.get(at..)?.char_indices();
    let (_, first) = characters.next()?;

    if !rustc_lexer::is_id_start(first) {
        return None;
    }

    Some(
        characters
            .take_while(|(_, character)| rustc_lexer::is_id_continue(*character))
            .last()
            .map_or_else(|| at + first.len_utf8(), |(offset, character)| at + offset + character.len_utf8()),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8Path;
    use walkdir::WalkDir;

    use super::super::comment::scan_comments;
    use super::super::source_file::line_starts;
    use super::*;

    fn beyond_limit(text: &str, limit: usize) -> Option<usize> {
        let comments = scan_comments(text, &line_starts(text));

        beyond(text, &comments, limit)
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

    /// Every source file in this workspace passes its own guard, at the limit the run uses.
    ///
    /// The counts added here are proxies, and a proxy that refuses ordinary Rust is worse than the
    /// crash it prevents. This is the check that keeps them calibrated against real code.
    #[test]

    fn this_workspace_is_within_the_limit() {
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
