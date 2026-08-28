// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use proc_macro2::{Delimiter, Literal, TokenStream, TokenTree};
use syn::parse::Parser as _;
use syn::punctuated::Punctuated;
use syn::{Expr, ImplItemFn, ItemFn, Token, TraitItemFn};

/// Validates the argument list of `#[gamma::<name>]` and returns the item untouched.
///
/// A well-formed attribute expands to exactly the item it was written on: these macros exist so
/// that `cargo-gamma` can see a suppression in the source, not to rewrite anything. A malformed
/// one expands to the item preceded by a `compile_error!`, so the item itself still parses and the
/// user gets one clear diagnostic rather than a cascade of follow-on errors.
#[must_use]
pub fn inert(name: &str, attr: TokenStream, item: TokenStream) -> TokenStream {
    match validate(attr) {
        Ok(()) => item,
        Err(message) => {
            let mut out = error(&format!("#[gamma::{name}]: {message}"));

            out.extend(item);
            out
        }
    }
}

/// Validates the argument list of `#[gamma::test_timeout_multiplier(...)]` and returns the item untouched.
#[must_use]
pub fn inert_timeout(name: &str, attr: &TokenStream, item: TokenStream) -> TokenStream {
    match validate_timeout_multiplier(attr) {
        Ok(()) => item,
        Err(message) => {
            let mut out = error(&format!("#[gamma::{name}]: {message}"));

            out.extend(item);
            out
        }
    }
}

/// Validates the argument of `#[gamma::value(<expr>)]` and returns the item untouched.
///
/// The attribute states the expression `cargo-gamma` substitutes for the annotated function's body
/// when it mutates its return value, in place of the one it would otherwise guess from the return
/// type. Like the suppression macros this expands to the item alone; unlike them, what it validates
/// is not a shape but a language — the argument has to be one Rust expression, because that text
/// ends up spliced into the user's crate.
///
/// Four things are rejected, each because the alternative is worse than a compile error:
///
/// - Nothing at all. `#[gamma::value()]` states no value, and reading it as "use the guess" would
///   make an attribute that looks like it did something do nothing.
/// - More than one expression. `#[gamma::value(0, 1)]` looks like a list of candidates, and this
///   is not one: a site states one value, so accepting the first and dropping the rest would be a
///   silent loss.
/// - Anything that is not an expression. The tool cannot repair `1 +`, and splicing it would move
///   the error to a mutant nobody wrote.
/// - A second `#[gamma::value(...)]` on the same item. Two stated values would leave which one
///   wins to the order the compiler happened to expand them in, and last-wins is a rule nobody can
///   see in the source.
///
/// It also insists the item is a function or method. An attribute on an `impl` block or a module
/// could only mean "every function beneath this states this value", and one expression essentially
/// never type-checks as the body of more than one signature, so the inheriting reading would be a
/// promise the compiler breaks at every use.
#[must_use]
pub fn value(attr: TokenStream, item: TokenStream) -> TokenStream {
    match validate_value(attr, &item) {
        Ok(()) => item,
        Err(message) => {
            let mut out = error(&format!("#[gamma::value]: {message}"));

            out.extend(item);
            out
        }
    }
}

/// The deepest delimiter nesting a token stream may have and still be parsed.
///
/// Matches the limit the library side applies in `cargo_gamma_lib::parse::nesting`. Handing a
/// token stream nested deeper than this to `syn`'s recursive descent parser would exhaust the
/// stack and abort the compiler without diagnostics.
///
/// Exposed (hidden from docs) so `cargo-gamma-lib`'s agreement test can pin this copy against the
/// library's own `NESTING_LIMIT`, which is the only thing that keeps the two in step.
#[doc(hidden)]
pub const NESTING_LIMIT: usize = 64;

/// How many postfix links are allowed per delimiter nesting level.
///
/// This stays in step with `cargo_gamma_lib::parse::nesting`: a run of calls or indexes is a
/// recursive expression tree even though each delimiter closes before the next one opens; field,
/// method, and try links add the same recursive shape.
///
/// Exposed (hidden from docs) so `cargo-gamma-lib`'s agreement test can pin this copy against the
/// library's own `CHAIN_FACTOR`.
#[doc(hidden)]
pub const CHAIN_FACTOR: usize = 4;

/// Whether the preceding token can end an expression.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Previous {
    Other,
    Expression,
}

/// One token-stream level waiting to be walked.
struct Frame {
    iter: proc_macro2::token_stream::IntoIter,
    depth: usize,
    postfix: usize,
    casts: usize,
    operators: usize,
    previous: Previous,
}

/// Returns whether a token stream exceeds its delimiter or postfix-expression limits.
///
/// Nested groups are walked with an explicit stack rather than recursion, because this code runs
/// inside `rustc` while compiling user code: a proc macro that exhausts the stack takes the
/// compiler with it.
fn exceeds_nesting_limit(stream: &TokenStream, limit: usize) -> bool {
    let postfix_limit = limit.saturating_mul(CHAIN_FACTOR);
    let mut frames = vec![Frame {
        iter: stream.clone().into_iter(),
        depth: 0,
        postfix: 0,
        casts: 0,
        operators: 0,
        previous: Previous::Other,
    }];

    while let Some(mut frame) = frames.pop() {
        while let Some(tree) = frame.iter.next() {
            match tree {
                TokenTree::Group(group) => {
                    let postfix =
                        matches!(group.delimiter(), Delimiter::Parenthesis | Delimiter::Bracket) && frame.previous == Previous::Expression;

                    if postfix {
                        frame.postfix += 1;

                        if frame.postfix > postfix_limit {
                            return true;
                        }
                    } else {
                        frame.postfix = 0;
                    }

                    let next_depth = frame.depth + 1;

                    if next_depth > limit {
                        return true;
                    }

                    // A complete group can be the receiver of the next call or index. The child
                    // gets a fresh postfix chain because only adjacent links share one expression.
                    frame.previous = Previous::Expression;
                    frames.push(frame);
                    frames.push(Frame {
                        iter: group.stream().into_iter(),
                        depth: next_depth,
                        postfix: 0,
                        casts: 0,
                        operators: 0,
                        previous: Previous::Other,
                    });
                    break;
                }

                TokenTree::Ident(ident) if ident == "as" => {
                    frame.postfix = 0;
                    frame.casts += 1;

                    if frame.casts > postfix_limit {
                        return true;
                    }

                    frame.previous = Previous::Other;
                }

                TokenTree::Ident(_) | TokenTree::Literal(_) => frame.previous = Previous::Expression,

                TokenTree::Punct(punct) => {
                    if matches!(
                        punct.as_char(),
                        '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '!' | '<' | '>' | '='
                    ) {
                        frame.operators += 1;

                        if frame.operators > postfix_limit {
                            return true;
                        }
                    }

                    let postfix = matches!(punct.as_char(), '.' | '?') && frame.previous == Previous::Expression;

                    if postfix {
                        frame.postfix += 1;

                        if frame.postfix > postfix_limit {
                            return true;
                        }
                    } else {
                        frame.postfix = 0;
                    }

                    if matches!(punct.as_char(), ',' | ';') {
                        frame.casts = 0;
                        frame.operators = 0;
                    }

                    frame.previous = if matches!(punct.as_char(), '?' | '>') {
                        Previous::Expression
                    } else {
                        Previous::Other
                    };
                }
            }
        }
    }

    false
}

/// Checks that an argument list is one expression, on a function that states no other value.
fn validate_value(attr: TokenStream, item: &TokenStream) -> Result<(), String> {
    if attr.is_empty() {
        return Err("expected one expression, as in `#[gamma::value(0)]`".to_owned());
    }

    if exceeds_nesting_limit(&attr, NESTING_LIMIT) {
        return Err("expression nests too deeply to be safely parsed".to_owned());
    }

    if exceeds_nesting_limit(item, NESTING_LIMIT) {
        return Err("item nests too deeply to be safely parsed".to_owned());
    }

    let written = attr.to_string();

    if let Err(reported) = syn::parse2::<Expr>(attr.clone()) {
        let parser = Punctuated::<Expr, Token![,]>::parse_terminated;

        return match parser.parse2(attr) {
            Ok(list) if list.len() > 1 => Err(format!(
                "expected one expression, but `{written}` is {}; a site states one value",
                list.len()
            )),
            _ => Err(format!("`{written}` is not a Rust expression: {reported}")),
        };
    }

    if !is_function(item) {
        return Err("expected a function or method; a value stated on an `impl` block or a module would have to type-check as the body of every function beneath it".to_owned());
    }

    if !has_a_body(item) {
        return Err(
            "expected a function with a body; a declaration has none to replace, and a value is not inherited by the implementations of a trait method"
                .to_owned(),
        );
    }

    if states_a_value(item) {
        return Err("an item may state one value; two would leave which of them applies to the order they were expanded in".to_owned());
    }

    Ok(())
}

/// Returns whether an item is a function, a method, or a trait method.
///
/// All three are asked, because the same attribute is written in all three positions and only the
/// grammar differs: a free function is an `ItemFn`, a method an `ImplItemFn`, and a trait method a
/// `TraitItemFn` whose body may be absent entirely.
fn is_function(item: &TokenStream) -> bool {
    // #[gamma::skip(logical.or_to_and, reason = "syn accepts every function-with-body form through the adjacent item and method parsers alike; the only distinct form is a bodyless trait declaration, which reaches the final parser unchanged")]
    syn::parse2::<ItemFn>(item.clone()).is_ok()
        || syn::parse2::<ImplItemFn>(item.clone()).is_ok()
        || syn::parse2::<TraitItemFn>(item.clone()).is_ok()
}

/// Returns whether a function has a body to replace.
///
/// A trait method may be a declaration ending in `;`, and a value stated there would substitute
/// nothing anywhere: the value is not inherited by the implementations, for the same reason it is
/// not inherited from an `impl` block. Saying so is better than expanding to an attribute that
/// reads as a working hint and generates no mutant.
fn has_a_body(item: &TokenStream) -> bool {
    // #[gamma::skip(literal.bool_flip, reason = "validate_value calls this only after is_function; every function form with a body parses as a TraitItemFn, so the parse-error default is unreachable for supported input")]
    syn::parse2::<TraitItemFn>(item.clone()).map_or(true, |method| method.default.is_some())
}

/// Returns whether an item still carries a `#[gamma::value(...)]` attribute of its own.
///
/// Attribute macros expand outermost first, and the item handed to one still carries every
/// attribute below it. So the first of two `#[gamma::value(...)]` attributes sees the second here,
/// which is what makes the duplicate a diagnostic rather than a coin toss.
///
/// Only the item's own attributes are examined — the ones before its body — so a nested function
/// inside the body stating its own value is left alone. It is a different site, and stating a value
/// there is exactly as legitimate.
fn states_a_value(item: &TokenStream) -> bool {
    let mut trees = item.clone().into_iter().peekable();

    while let Some(tree) = trees.next() {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == '#' => {
                let Some(TokenTree::Group(group)) = trees.peek() else {
                    continue;
                };

                if group.delimiter() == Delimiter::Bracket && names_value(&group.stream()) {
                    return true;
                }
            }

            // The first token that is not part of an attribute is the start of the signature, and
            // everything past it belongs to the function rather than to what is written on it.
            TokenTree::Punct(_) | TokenTree::Group(_) => {}
            TokenTree::Ident(_) | TokenTree::Literal(_) => break,
        }
    }

    false
}

/// Returns whether the body of an attribute names `gamma::value`.
fn names_value(inner: &TokenStream) -> bool {
    let mut trees = inner.clone().into_iter();

    let Some(TokenTree::Ident(namespace)) = trees.next() else {
        return false;
    };

    if namespace != "gamma" {
        return false;
    }

    // Two `:` tokens rather than one `::`, because a path separator reaches a proc macro as a pair
    // of joint puncts and never as a single tree.
    for _separator in 0..2 {
        match trees.next() {
            Some(TokenTree::Punct(punct)) if punct.as_char() == ':' => {}
            _other => return false,
        }
    }

    matches!(trees.next(), Some(TokenTree::Ident(name)) if name == "value")
}

/// Returns whether a literal's source text is a string literal that yields a `&str`.
///
/// Rust writes a string literal in four shapes: `"x"`, `r"x"`, `r#"x"#`, and any number of hashes
/// beyond that. All of them are strings, and a `reason` or `tag` written as a raw string is a
/// perfectly reasonable thing to want — it is how one embeds a quote or a backslash without
/// escaping. Asking only whether the text opens with a quote rejected every raw form.
///
/// The prefix is what carries the type, so it is what this looks at. Stripping an optional `r` and
/// then any run of hashes must leave a quote. A byte string opens with `b` and a C string with `c`,
/// neither of which survives that strip, so both stay rejected: `b"x"` is a `&[u8]` and `c"x"` is a
/// `&CStr`, and neither is the `&str` the attribute promises. `br"x"` is rejected for the same
/// reason, since the `b` comes first and the strip never reaches its `r`.
fn is_string_literal(text: &str) -> bool {
    let body = text.strip_prefix('r').unwrap_or(text);

    body.trim_start_matches('#').starts_with('"')
}

/// The largest multiplier worth accepting, mirroring `bounds::factor` in `cargo-gamma-lib`.
///
/// The two cannot share a constant: this is a proc-macro crate and the tool does not depend on it
/// in the direction that would allow it. They must nevertheless agree, because disagreeing means
/// `cargo build` accepts a multiplier that `cargo gamma` then refuses, and the user is told about
/// their typo by whichever happens to run second.
///
/// Exposed (hidden from docs) so `cargo-gamma-lib`'s agreement test can pin this copy against the
/// library's own `MOST_FACTOR`.
#[doc(hidden)]
pub const MOST_FACTOR: f64 = 1e6;

/// Checks a multiplier is positive, finite, and small enough to scale a baseline without overflow.
fn is_bounded_multiplier(value: f64) -> bool {
    value > 0.0 && value.is_finite() && value <= MOST_FACTOR
}

/// Checks that a timeout multiplier attribute has at least one argument and validates its contents.
fn validate_timeout_multiplier(attr: &TokenStream) -> Result<(), String> {
    if attr.is_empty() {
        return Err("expected a timeout multiplier, as in `#[gamma::test_timeout_multiplier(2.0)]`".to_owned());
    }

    let positional = match attr.clone().into_iter().next() {
        Some(TokenTree::Literal(_)) => true,
        Some(TokenTree::Punct(punct)) => matches!(punct.as_char(), '+' | '-'),
        // `inf`, `nan`, and `infinity` tokenize as identifiers, not literals, so without this arm
        // they slip past the positional check and fall through to `validate`, which treats a bare
        // identifier as a selector and accepts it. They parse as non-finite floats and are never
        // valid selectors, so a lone one is a malformed multiplier that must be refused here —
        // otherwise `#[gamma::test_timeout_multiplier(inf)]` compiles clean and the tool's scanner
        // rejects it later, or an unbounded factor reaches `Duration::mul_f64`.
        Some(TokenTree::Ident(ident)) => ident.to_string().parse::<f64>().is_ok(),
        _ => false,
    };

    if !positional {
        return validate(attr.clone());
    }

    match attr.to_string().parse::<f64>() {
        Ok(value) if is_bounded_multiplier(value) => Ok(()),
        _ => Err(format!(
            "timeout multiplier must be a positive number no greater than {MOST_FACTOR}"
        )),
    }
}

/// Checks the structural shape of an argument list.
///
/// Selector *names* are not checked here: the registry lives in the tool, and duplicating it in a
/// proc macro would mean two lists that drift apart. `cargo gamma` reports unknown selectors as
/// hard errors with a spelling suggestion. What this does check is the shape that a human is
/// likely to get wrong without noticing — a `reason` or `tag` that is not a string, or a timeout
/// multiplier that is not a positive number.
///
/// Nested groups are walked with an explicit stack rather than by recursion. The nesting is
/// whatever the user wrote inside an attribute, and this code runs inside `rustc` while their
/// crate is being compiled: a proc macro that exhausts the stack takes the compiler with it, and
/// presents as a crash nobody would think to blame on a parenthesis in an attribute argument.
/// Depth on the heap has no such cliff, and the traversal order is unchanged, so a file with more
/// than one malformed argument still reports the first one.
fn validate(attr: TokenStream) -> Result<(), String> {
    let mut frames: Vec<(Vec<TokenTree>, usize)> = vec![(attr.into_iter().collect(), 0)];

    'frames: while let Some((trees, mut index)) = frames.pop() {
        while index < trees.len() {
            if let TokenTree::Ident(ident) = &trees[index] {
                let key = ident.to_string();

                if matches!(key.as_str(), "reason" | "tag") {
                    let is_equals = matches!(trees.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '=');

                    if !is_equals {
                        return Err(format!("`{key}` must be written as `{key} = \"...\"`"));
                    }

                    match trees.get(index + 2) {
                        Some(TokenTree::Literal(literal)) if is_string_literal(&literal.to_string()) => {}
                        _ => return Err(format!("`{key}` must be a string literal")),
                    }

                    if trees
                        .get(index + 3)
                        .is_some_and(|tree| !matches!(tree, TokenTree::Punct(punct) if punct.as_char() == ','))
                    {
                        return Err(format!("`{key}` must not have trailing tokens after its value"));
                    }

                    // #[gamma::skip(literal.int_decrement, literal.int_to_one, reason = "the two tokens this steps over are the `=` and the string literal, and neither branch of this loop acts on either, so a shorter step reaches the same next meaningful token")]
                    index += 3;
                    continue;
                }

                if matches!(
                    key.as_str(),
                    "test_timeout_multiplier" | "timeout_multiplier" | "factor" | "multiplier"
                ) {
                    let is_equals = matches!(trees.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '=');

                    if !is_equals {
                        return Err(format!("`{key}` must be written as `{key} = <number>`"));
                    }

                    match trees.get(index + 2) {
                        Some(TokenTree::Literal(literal)) if literal.to_string().parse::<f64>().is_ok_and(is_bounded_multiplier) => {}
                        _ => return Err(format!("`{key}` must be a positive number no greater than {MOST_FACTOR}")),
                    }

                    if trees
                        .get(index + 3)
                        .is_some_and(|tree| !matches!(tree, TokenTree::Punct(punct) if punct.as_char() == ','))
                    {
                        return Err(format!("`{key}` must not have trailing tokens after its value"));
                    }

                    index += 3;
                    continue;
                }
            }

            if let TokenTree::Group(group) = &trees[index]
                && group.delimiter() == Delimiter::Parenthesis
            {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();

                // The rest of this list is resumed once the group is done — the point a recursive
                // walk would return to.
                frames.push((trees, index + 1));
                frames.push((inner, 0));

                continue 'frames;
            }

            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_decrement, reason = "subtracting, removing, or zeroing this advancement intentionally makes the token walk loop forever")]
            index += 1;
        }
    }

    Ok(())
}

/// Builds a `compile_error!` invocation carrying `message`.
///
/// The message is rendered by [`Literal::string`] rather than by escaping the text by hand, which
/// is what makes the result correct for every input: a message containing a quote, a backslash or
/// a newline needs no special handling here, and there is consequently no way for this to produce
/// something that does not parse.
fn error(message: &str) -> TokenStream {
    let rendered = format!("::core::compile_error!({});", Literal::string(message));

    rendered.parse().expect("a `Literal::string` always parses as a string literal")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses test input the way `rustc` would hand it to a proc macro.
    fn stream(source: &str) -> TokenStream {
        source.parse().expect("test input is valid Rust tokens")
    }

    /// The overwhelmingly common case: a well-formed attribute must expand to the item and to
    /// nothing else. Anything extra would show up in every annotated crate in the world.
    #[test]
    fn a_valid_attribute_expands_to_the_item_alone() {
        let out = inert("skip", stream("arith, reason = \"checked by proptest\""), stream("fn f() {}"));

        assert_eq!(out.to_string(), "fn f () { }");
    }

    /// No arguments at all means every mutator, and is the shortest thing a user can write, so it
    /// must not be mistaken for a malformed list.
    #[test]
    fn an_empty_argument_list_is_accepted() {
        let out = inert("skip", stream(""), stream("fn f() {}"));

        assert_eq!(out.to_string(), "fn f () { }");
    }

    /// The whole selector vocabulary has to survive validation untouched, since none of it is
    /// checked here. This pins the shapes: bare names, dotted names, families, presets, academic
    /// aliases and negation.
    #[test]
    fn the_whole_selector_vocabulary_is_accepted() {
        for selectors in [
            "arith.add_to_sub",
            "arith",
            "@arithmetic",
            "ROR",
            "arith, !arith.add_to_sub",
            "literal, @boundary, !stmt",
        ] {
            let out = inert("skip", stream(selectors), stream("fn f() {}"));

            assert_eq!(out.to_string(), "fn f () { }", "`{selectors}` should have been accepted");
        }
    }

    /// A malformed attribute must still leave the item behind. Swallowing it would turn one
    /// diagnostic about the attribute into a pile of diagnostics about the missing function.
    #[test]
    fn a_malformed_attribute_keeps_the_item_and_adds_one_error() {
        let out = inert("skip", stream("reason = performance"), stream("fn f() {}")).to_string();

        assert!(out.contains("compile_error"), "expected a compile_error in `{out}`");
        assert!(out.ends_with("fn f () { }"), "expected the item to survive in `{out}`");
    }

    /// The message names the macro that was misused, because a file can carry all three and the
    /// user needs to know which line to look at.
    #[test]
    fn the_message_names_the_macro_that_was_misused() {
        for name in ["skip", "expect_survived", "expect_killed"] {
            let out = inert(name, stream("reason = performance"), stream("fn f() {}")).to_string();

            assert!(
                out.contains(&format!("#[gamma::{name}]")),
                "expected `{name}` to be named in `{out}`"
            );
        }
    }

    /// `reason = performance` is the mistake this validation exists for: it looks like it works,
    /// and a regex-based tool would accept it. The exact wording is asserted because the wording
    /// is the entire value of the diagnostic.
    #[test]
    fn a_bare_word_value_is_rejected_as_not_a_string() {
        assert_eq!(
            validate(stream("reason = performance")),
            Err("`reason` must be a string literal".to_owned())
        );
        assert_eq!(
            validate(stream("tag = telemetry")),
            Err("`tag` must be a string literal".to_owned())
        );
    }

    /// A number is a literal but not a string literal, so [`is_string_literal`] is what separates
    /// them. Without it `reason = 42` would be accepted.
    #[test]
    fn a_non_string_literal_value_is_rejected() {
        assert_eq!(validate(stream("reason = 42")), Err("`reason` must be a string literal".to_owned()));
        assert_eq!(validate(stream("tag = 1.5")), Err("`tag` must be a string literal".to_owned()));
        assert_eq!(
            validate(stream("reason = 'c'")),
            Err("`reason` must be a string literal".to_owned())
        );
    }

    /// `reason("...")` is the other natural way to get it wrong, borrowed from attribute grammars
    /// that do use that form. It has to be reported as a spelling problem rather than accepted.
    #[test]
    fn the_call_form_is_rejected_with_the_spelling_it_should_have() {
        assert_eq!(
            validate(stream("reason(\"x\")")),
            Err("`reason` must be written as `reason = \"...\"`".to_owned())
        );
        assert_eq!(
            validate(stream("tag(\"x\")")),
            Err("`tag` must be written as `tag = \"...\"`".to_owned())
        );
    }

    /// Nesting deeper than any stack can hold is walked without one.
    ///
    /// The argument list of an attribute is user input, and this code runs inside `rustc` while
    /// the user's crate is compiled. A recursive walk turns a machine-generated argument into a
    /// compiler crash — no line, no attribute named, and no reason anyone would connect it to the
    /// parentheses they wrote. The depth here is far past what a recursive walk survives, and the
    /// argument is well-formed, so the only thing being asked is whether the walk completes.
    #[test]
    fn nesting_deeper_than_the_stack_is_still_walked_to_the_end() {
        let depth = 50_000;
        let source = format!("{}arith{}", "(".repeat(depth), ")".repeat(depth));

        assert_eq!(validate(stream(&source)), Ok(()));
    }

    /// A malformed argument buried under nesting is still found, and still reported first.
    #[test]
    fn a_malformed_argument_inside_a_nested_group_is_still_reported() {
        assert_eq!(
            validate(stream("arith, (((reason = performance))), tag = \"x\"")),
            Err("`reason` must be a string literal".to_owned())
        );
    }

    /// A key with nothing after it at all runs off the end of the token list, which must be a
    /// diagnostic rather than an out-of-bounds index.
    #[test]
    fn a_key_with_nothing_after_it_is_rejected() {
        assert_eq!(
            validate(stream("reason")),
            Err("`reason` must be written as `reason = \"...\"`".to_owned())
        );
        assert_eq!(validate(stream("reason =")), Err("`reason` must be a string literal".to_owned()));
    }

    /// A punctuation mark that is not `=` must not be mistaken for one.
    #[test]
    fn a_key_followed_by_the_wrong_punctuation_is_rejected() {
        assert_eq!(
            validate(stream("reason : \"x\"")),
            Err("`reason` must be written as `reason = \"...\"`".to_owned())
        );
    }

    /// Both named arguments are optional and may appear together, in either order, after the
    /// selectors. Skipping three tokens per key is what lets the second one be found.
    #[test]
    fn both_named_arguments_are_accepted_together_in_either_order() {
        assert_eq!(validate(stream("stmt, reason = \"why\", tag = \"group\"")), Ok(()));
        assert_eq!(validate(stream("stmt, tag = \"group\", reason = \"why\"")), Ok(()));
    }

    /// Skipping past a validated key must not skip past the value's own tokens in a way that
    /// hides a later mistake. This puts a bad key immediately after a good one.
    #[test]
    fn a_mistake_after_a_valid_named_argument_is_still_found() {
        assert_eq!(
            validate(stream("reason = \"why\", tag = broken")),
            Err("`tag` must be a string literal".to_owned())
        );
    }

    /// A word that merely contains a key name is not a key, or `reasoning` would be treated as
    /// `reason` and rejected for having no `=` after it.
    #[test]
    fn an_identifier_that_only_resembles_a_key_is_left_alone() {
        assert_eq!(validate(stream("reasoning, tagged")), Ok(()));
    }

    /// Arguments nested in parentheses are validated too, so a mistake cannot be hidden one level
    /// down.
    #[test]
    fn a_mistake_nested_in_parentheses_is_found() {
        assert_eq!(
            validate(stream("outer(reason = performance)")),
            Err("`reason` must be a string literal".to_owned())
        );
        assert_eq!(validate(stream("a(b(tag = 7))")), Err("`tag` must be a string literal".to_owned()));
    }

    /// A valid nested list is still valid, which is what stops the recursion from being a way to
    /// reject things.
    #[test]
    fn a_valid_nested_list_is_accepted() {
        assert_eq!(validate(stream("outer(reason = \"why\")")), Ok(()));
    }

    /// Only parentheses are argument lists. Brackets and braces belong to another grammar, and
    /// descending into them would invent errors in token trees this macro does not own.
    #[test]
    fn other_delimiters_are_not_treated_as_argument_lists() {
        assert_eq!(validate(stream("outer[reason = performance]")), Ok(()));
        assert_eq!(validate(stream("outer{reason = performance}")), Ok(()));
    }

    /// A byte string is not a string literal — it is a `&[u8]` — so it must be rejected. This is
    /// also what pins the check to the *start* of the literal's text: `b"x"` ends with a quote just
    /// as a real string does, and only the opening character tells them apart. `br"x"` is the case
    /// that pins the strip to a *leading* `r` rather than any `r`: its `r` must not be reached.
    #[test]
    fn a_byte_string_value_is_rejected() {
        assert_eq!(
            validate(stream("reason = b\"x\"")),
            Err("`reason` must be a string literal".to_owned())
        );
        assert_eq!(validate(stream("tag = b\"x\"")), Err("`tag` must be a string literal".to_owned()));
        assert_eq!(
            validate(stream("reason = br\"x\"")),
            Err("`reason` must be a string literal".to_owned())
        );
        assert_eq!(
            validate(stream("tag = br#\"x\"#")),
            Err("`tag` must be a string literal".to_owned())
        );
    }

    /// A raw string is a string, and it is how one writes a reason containing a quote or a
    /// backslash without escaping it. Every hash count must be accepted, since `r##"x"##` differs
    /// from `r#"x"#` only in how much of the text can appear verbatim.
    #[test]
    fn a_raw_string_value_is_accepted_at_every_hash_count() {
        assert_eq!(validate(stream("reason = r\"x\"")), Ok(()));
        assert_eq!(validate(stream("reason = r#\"x\"#")), Ok(()));
        assert_eq!(validate(stream("tag = r##\"x\"##")), Ok(()));
        assert_eq!(validate(stream("arith.add_to_sub, reason = r#\"say \"no\"\"#")), Ok(()));
    }

    /// A C string is a `&CStr`, not a `&str`, so it is rejected for the same reason a byte string
    /// is. It is asserted separately because its prefix letter differs, and a check written to
    /// name only `b` would let it through.
    #[test]
    fn a_c_string_value_is_rejected() {
        assert_eq!(
            validate(stream("reason = c\"x\"")),
            Err("`reason` must be a string literal".to_owned())
        );
        assert_eq!(validate(stream("tag = cr\"x\"")), Err("`tag` must be a string literal".to_owned()));
    }

    /// The hash strip must not be all that stands between a non-string and acceptance. A lifetime
    /// or an identifier reaching this point has no quote after its hashes — of which it has none —
    /// so the closing test on the quote is what rejects it, and this pins that test.
    #[test]
    fn a_literal_whose_prefix_strips_to_no_quote_is_rejected() {
        assert!(!is_string_literal("42"));
        assert!(!is_string_literal("r42"));
        assert!(!is_string_literal("#"));
        assert!(!is_string_literal(""));
        assert!(is_string_literal("\"x\""));
    }

    /// A named value ends at its literal. Anything before the separating comma is malformed,
    /// rather than another selector or a nested list that could hide a typo.
    #[test]
    fn trailing_tokens_after_a_named_value_are_rejected() {
        assert_eq!(
            validate(stream("reason = \"x\" (tag = broken)")),
            Err("`reason` must not have trailing tokens after its value".to_owned())
        );
        assert_eq!(
            validate(stream("test_timeout_multiplier = 2.0 + unexpected")),
            Err("`test_timeout_multiplier` must not have trailing tokens after its value".to_owned())
        );
    }

    /// This has to be a compiler error, not merely a rejected token stream: otherwise a malformed
    /// selector-free directive reaches the source scanner and widens into a suppression of all
    /// mutations in scope.
    #[test]
    fn trailing_named_value_tokens_expand_to_a_compile_error() {
        let out = inert("skip", stream("reason = \"checked\" + unexpected"), stream("fn f() {}")).to_string();

        assert!(out.contains("compile_error"), "expected a compile_error in `{out}`");
        assert!(out.contains("trailing tokens"), "expected the reason in `{out}`");
        assert!(out.ends_with("fn f () { }"), "expected the item to survive in `{out}`");
    }

    /// The message ends up inside a string literal, so a message containing a quote or a backslash
    /// has to come back out intact rather than ending the literal early. This is the case that the
    /// previous hand-rolled escaping existed to handle.
    #[test]
    fn a_message_containing_quotes_and_backslashes_still_parses() {
        let out = error(r#"he said "hi" and \ then left"#).to_string();

        assert!(out.starts_with(":: core :: compile_error !"), "unexpected shape: `{out}`");
        assert!(out.contains(r#"\"hi\""#), "quotes should be escaped in `{out}`");
        assert!(out.contains(r"\\"), "backslashes should be escaped in `{out}`");
    }

    /// The invocation is fully qualified and terminated, so that it works in a crate that has
    /// shadowed `core` and in statement position alike.
    #[test]
    fn the_error_is_a_fully_qualified_terminated_invocation() {
        assert_eq!(error("boom").to_string(), ":: core :: compile_error ! (\"boom\") ;");
    }

    /// The case every annotated function is: one expression, on a function, expanding to that
    /// function and nothing else. Anything extra would appear in every crate that states a value.
    #[test]
    fn a_stated_value_expands_to_the_item_alone() {
        let out = value(stream("u32::MAX"), stream("fn f() -> u32 { 1 }"));

        assert_eq!(out.to_string(), "fn f () -> u32 { 1 }");
    }

    /// Every shape of expression a user might reach for has to survive, since the point of the
    /// attribute is to name a value the tool could not guess — which is rarely a bare literal.
    #[test]
    fn the_expressions_worth_stating_are_accepted() {
        for expression in [
            "0",
            "-1",
            "\"xyzzy\"",
            "Box::new(File)",
            "Some(Config { size: 8 })",
            "vec![1, 2, 3]",
            "(1, 2)",
            "if cfg!(unix) { 1 } else { 2 }",
            "Vec::<u8>::new()",
            "core::iter::once(1).collect()",
        ] {
            assert_eq!(
                validate_value(stream(expression), &stream("fn f() {}")),
                Ok(()),
                "`{expression}` should have been accepted"
            );
        }
    }

    /// An empty list states nothing. Reading it as "use the guess" would make an attribute that
    /// looks deliberate do nothing at all, which is the failure this crate exists to prevent.
    #[test]
    fn stating_nothing_is_rejected() {
        assert_eq!(
            validate_value(stream(""), &stream("fn f() {}")),
            Err("expected one expression, as in `#[gamma::value(0)]`".to_owned())
        );
    }

    /// A comma-separated list looks like a set of candidate values, and it is not one. Taking the
    /// first and dropping the rest would lose the others without saying so.
    #[test]
    fn stating_more_than_one_expression_is_rejected_by_count() {
        assert_eq!(
            validate_value(stream("0, 1"), &stream("fn f() {}")),
            Err("expected one expression, but `0 , 1` is 2; a site states one value".to_owned())
        );
    }

    /// A trailing comma still contains only one expression. It must take the parser-error path,
    /// not the diagnostic reserved for lists with two or more candidates.
    #[test]
    fn one_expression_with_a_trailing_comma_is_not_reported_as_multiple_values() {
        let rejected = validate_value(stream("0,"), &stream("fn f() {}")).expect_err("a trailing comma is not one expression");

        assert!(rejected.starts_with("`0 ,` is not a Rust expression: "), "{rejected}");
    }

    /// Text that is not an expression cannot be repaired, and splicing it would move the error to
    /// a mutant the user never wrote. The parser's own words are carried through, because they are
    /// better than anything this could invent.
    #[test]
    fn text_that_is_not_an_expression_is_rejected() {
        let rejected = validate_value(stream("1 +"), &stream("fn f() {}"));

        assert!(
            rejected
                .as_ref()
                .is_err_and(|message| message.starts_with("`1 +` is not a Rust expression: ")),
            "{rejected:?}"
        );
    }

    /// A statement is not an expression, and `let` is the one users are most likely to try.
    #[test]
    fn a_statement_is_rejected() {
        let rejected = validate_value(stream("let x = 1;"), &stream("fn f() {}"));

        assert!(rejected.is_err(), "{rejected:?}");
    }

    /// Two stated values would leave which one applies to the order the compiler expanded them in,
    /// which is not a rule anyone can see in the source. The first to expand sees the second,
    /// because an attribute macro is handed the item with the attributes below it still attached.
    #[test]
    fn a_second_stated_value_on_the_same_item_is_rejected() {
        let item = stream("#[gamma::value(1)] fn f() -> u32 { 2 }");

        assert_eq!(
            validate_value(stream("0"), &item),
            Err("an item may state one value; two would leave which of them applies to the order they were expanded in".to_owned())
        );
    }

    /// The duplicate check must not reach past the signature into the body, or a nested function
    /// stating its own value — a different site, and an entirely legitimate one — would be read as
    /// a second value on the outer function.
    #[test]
    fn a_value_stated_by_a_nested_function_is_not_a_duplicate() {
        let item = stream("fn f() -> u32 { #[gamma::value(1)] fn g() -> u32 { 2 } g() }");

        assert_eq!(validate_value(stream("0"), &item), Ok(()));
    }

    /// Other attributes on the same item are not stated values, however many of them there are.
    #[test]
    fn other_attributes_on_the_item_are_not_duplicates() {
        let item = stream("#[doc = \"why\"] #[inline] #[must_use] fn f() -> u32 { 2 }");

        assert_eq!(validate_value(stream("0"), &item), Ok(()));
    }

    /// A path that merely begins like the one being looked for is not it, or `#[gamma::skip]` on
    /// the same function would be reported as a second stated value.
    #[test]
    fn another_attribute_in_the_same_namespace_is_not_a_duplicate() {
        for other in [
            "#[gamma::skip(arith)]",
            "#[gamma::expect_killed]",
            "#[mutants::skip]",
            "#[value(1)]",
        ] {
            let item = stream(&format!("{other} fn f() -> u32 {{ 2 }}"));

            assert_eq!(validate_value(stream("0"), &item), Ok(()), "`{other}` is not a stated value");
        }
    }

    /// The three positions the attribute is written in have three different grammars, and all
    /// three have to be accepted — including a trait method, which may have no body at all.
    #[test]
    fn a_function_a_method_and_a_trait_method_are_all_functions() {
        for item in [
            "fn f() -> u32 { 1 }",
            "pub async fn f() -> u32 { 1 }",
            "unsafe fn f() -> u32 { 1 }",
            "fn f<T: Clone>(t: T) -> T where T: Send { t }",
            "fn f(&self) -> u32 { self.n }",
            "fn f(&self) -> u32;",
        ] {
            assert!(is_function(&stream(item)), "`{item}` is a function");
        }

        let declaration = stream("fn f(&self) -> u32;");
        let _ = syn::parse2::<ItemFn>(declaration.clone()).unwrap_err();
        let _ = syn::parse2::<ImplItemFn>(declaration.clone()).unwrap_err();
        let _ = syn::parse2::<TraitItemFn>(declaration).unwrap();
    }

    /// A value stated on an `impl` block or a module would have to mean "every function beneath
    /// this returns this", and one expression essentially never type-checks as more than one
    /// signature's body. Rejecting it is what keeps inheritance from being invented by accident.
    #[test]
    fn a_value_stated_on_something_that_is_not_a_function_is_rejected() {
        for item in [
            "impl Cursor { fn at(&self) -> usize { self.at } }",
            "mod m { pub fn at() -> usize { 0 } }",
            "struct S { n: u8 }",
            "trait T { fn at(&self) -> usize; }",
            "const N: u8 = 1;",
        ] {
            assert!(
                validate_value(stream("0"), &stream(item)).is_err(),
                "`{item}` is not a function and must be rejected"
            );
        }
    }

    /// The exact diagnostic matters here: it explains why the attribute is not inherited rather
    /// than merely saying that parsing failed.
    #[test]
    fn a_non_function_reports_the_inheritance_problem() {
        assert_eq!(
            validate_value(stream("0"), &stream("mod m {}")),
            Err("expected a function or method; a value stated on an `impl` block or a module would have to type-check as the body of every function beneath it".to_owned())
        );
    }

    /// Attribute recognition is deliberately exact. Near misses exercise each token boundary so
    /// punctuation elsewhere in the item cannot be mistaken for `#[gamma::value]`.
    #[test]
    fn malformed_value_attribute_paths_are_not_recognized() {
        assert!(!states_a_value(&stream("! [gamma::value(1)] fn f() {}")));
        assert!(!states_a_value(&stream("fn #[gamma::value(1)]")));

        // A `#` not immediately followed by a bracketed group is not the start of an attribute,
        // whether something else follows it or it is the stream's last token; both must be passed
        // over rather than mistaken for `#[gamma::value(...)]`.
        assert!(!states_a_value(&stream("# fn f() {}")));
        assert!(!states_a_value(&stream("#")));

        for inner in ["", "other::value", "gamma=value", "gamma==value", "gamma::skip"] {
            assert!(!names_value(&stream(inner)), "`{inner}` is not gamma::value");
        }
        assert!(names_value(&stream("gamma::value")));
    }

    /// A declared trait method is a function with nothing to replace, and its implementations do
    /// not inherit the value — so an attribute there would be a hint that generates no mutant
    /// anywhere. It is the one function-shaped position a value cannot be stated on.
    #[test]
    fn a_value_stated_on_a_function_with_no_body_is_rejected() {
        assert!(has_a_body(&stream("fn f() -> u32 { 1 }")));
        assert!(!has_a_body(&stream("fn f(&self) -> u32;")));

        let rejected = validate_value(stream("0"), &stream("fn f(&self) -> u32;")).expect_err("a declaration has no body to replace");

        assert!(rejected.contains("a declaration has none"), "{rejected}");
    }

    /// A rejected value still leaves the item behind, for the same reason a malformed suppression
    /// does: one diagnostic about the attribute beats a pile about the missing function.
    #[test]
    fn a_rejected_value_keeps_the_item_and_names_the_macro() {
        let out = value(stream(""), stream("fn f() -> u32 { 1 }")).to_string();

        assert!(out.contains("#[gamma::value]"), "expected the macro to be named in `{out}`");
        assert!(out.ends_with("fn f () -> u32 { 1 }"), "expected the item to survive in `{out}`");
    }

    /// An accepted timeout multiplier expands to the item alone, with no `compile_error!` added —
    /// the timeout family's counterpart to
    /// [`a_valid_attribute_expands_to_the_item_alone`]. A regression that returned the
    /// `compile_error!`-carrying item from the `Ok` arm would break every valid
    /// `#[gamma::test_timeout_multiplier(...)]` in the wild.
    #[test]
    fn an_accepted_timeout_multiplier_expands_to_the_item_alone() {
        let out = inert_timeout("test_timeout_multiplier", &stream("2.0"), stream("fn f() {}"));

        assert_eq!(out.to_string(), "fn f () { }");
    }

    /// A rejected timeout multiplier leaves the item behind and names the macro, exactly as a
    /// rejected suppression or value does — the timeout family's counterpart. A regression that
    /// returned the item unchanged from the `Err` arm (dropping the `compile_error!`) would let a
    /// malformed multiplier compile clean.
    #[test]
    fn a_rejected_timeout_multiplier_keeps_the_item_and_names_the_macro() {
        let out = inert_timeout("test_timeout_multiplier", &stream("inf"), stream("fn f() {}")).to_string();

        assert!(out.contains("compile_error"), "expected a compile_error in `{out}`");
        assert!(
            out.contains("#[gamma::test_timeout_multiplier]"),
            "expected the macro to be named in `{out}`"
        );
        assert!(out.ends_with("fn f () { }"), "expected the item to survive in `{out}`");
    }

    /// A timeout multiplier must be a positive, bounded number when written with `key = value`.
    #[test]
    fn timeout_multiplier_validation() {
        let refused = Err("`test_timeout_multiplier` must be a positive number no greater than 1000000".to_owned());

        assert_eq!(validate(stream("test_timeout_multiplier = 2.5")), Ok(()));
        assert_eq!(validate(stream("timeout_multiplier = 3.0")), Ok(()));
        assert_eq!(validate(stream("test_timeout_multiplier = 2")), Ok(()));
        assert_eq!(validate(stream("test_timeout_multiplier = \"fast\"")), refused);
        assert_eq!(validate(stream("test_timeout_multiplier = -1.0")), refused);

        // The bound must agree with `bounds::factor` in the tool, so that a value the tool refuses
        // is not silently accepted while the user's crate compiles.
        assert_eq!(validate(stream("test_timeout_multiplier = 1e300")), refused);
        assert_eq!(validate(stream("test_timeout_multiplier = 1000001")), refused);
        assert_eq!(validate(stream("test_timeout_multiplier = 1000000")), Ok(()));

        assert_eq!(
            validate(stream("test_timeout_multiplier(2.0)")),
            Err("`test_timeout_multiplier` must be written as `test_timeout_multiplier = <number>`".to_owned())
        );
        assert_eq!(validate_timeout_multiplier(&stream("2.5")), Ok(()));
        assert_eq!(
            validate_timeout_multiplier(&stream("arith, test_timeout_multiplier = 2.5, reason = \"complex math\"")),
            Ok(())
        );
        // A leading group (here a parenthesized list) is neither a literal, a sign, nor an
        // identifier, so it falls through the positional check's wildcard arm exactly as a
        // non-numeric leading identifier does, and the whole attribute is handed to the general
        // structural validator instead. That validator only inspects identifier keys and bare
        // groups it recurses into, so a lone parenthesized literal — matching none of the
        // recognized keys — is accepted.
        assert_eq!(validate_timeout_multiplier(&stream("(2.5)")), Ok(()));
        // `inf`, `nan`, and `infinity` arrive as identifiers rather than literals and parse as
        // non-finite floats, so they slip past the literal check; they must be refused like any
        // other out-of-range multiplier rather than mistaken for a bare selector and accepted.
        for value in ["0", "-1", "1e300", "\"fast\"", "inf", "nan", "infinity", "NaN"] {
            assert!(validate_timeout_multiplier(&stream(value)).is_err(), "`{value}` was accepted");
        }
        assert_eq!(
            validate_timeout_multiplier(&stream("")),
            Err("expected a timeout multiplier, as in `#[gamma::test_timeout_multiplier(2.0)]`".to_owned())
        );
    }

    /// Deeply nested expressions or items are rejected before syn's recursive descent parser can
    /// overflow the compiler's stack.
    #[test]
    fn deeply_nested_tokens_are_rejected_safely() {
        let depth = 100;
        let deep_expr = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        let err = validate_value(stream(&deep_expr), &stream("fn f() -> u32 { 1 }")).expect_err("nested expr rejected");
        assert_eq!(err, "expression nests too deeply to be safely parsed");

        let deep_item = format!("fn f() -> u32 {{ {}1{} }}", "(".repeat(depth), ")".repeat(depth));
        let err = validate_value(stream("1"), &stream(&deep_item)).expect_err("nested item rejected");
        assert_eq!(err, "item nests too deeply to be safely parsed");
    }

    #[test]
    fn deeply_chained_postfix_expressions_are_rejected_safely() {
        let chains = [
            format!("call{}", "()".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1)),
            format!("value{}", "[0]".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1)),
            format!("value{}", ".call()".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1)),
        ];

        for expression in chains {
            let err = validate_value(stream(&expression), &stream("fn f() -> u32 { 1 }")).expect_err("postfix chain rejected");

            assert_eq!(err, "expression nests too deeply to be safely parsed");
        }
    }

    #[test]
    fn deeply_chained_cast_expressions_are_rejected_safely() {
        let expression = format!("1{}", " as u64".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1));
        let err = validate_value(stream(&expression), &stream("fn f() -> u32 { 1 }")).expect_err("cast chain rejected");

        assert_eq!(err, "expression nests too deeply to be safely parsed");
    }

    #[test]
    fn a_long_unary_chain_expands_to_a_guard_diagnostic() {
        let expression = format!("{}1", "-".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1));
        let output = value(stream(&expression), stream("fn f() -> i32 { 1 }")).to_string();

        assert!(output.contains("compile_error"), "{output}");
        assert!(output.contains("expression nests too deeply"), "{output}");
    }

    #[test]
    fn a_long_binary_chain_is_rejected_by_the_guard() {
        let expression = format!("true{}", " || true".repeat(NESTING_LIMIT * CHAIN_FACTOR + 1));
        let error = validate_value(stream(&expression), &stream("fn f() -> bool { true }")).expect_err("binary chain must be rejected");

        assert_eq!(error, "expression nests too deeply to be safely parsed");
    }
}
