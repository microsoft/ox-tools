// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Pins the limit constants the proc-macro hand-copies from this library, and checks the two
//! nesting guards accept and reject the same syntax.
//!
//! `cargo-gamma-attrs-impl` is a proc-macro-support crate that the tool cannot depend on in the
//! direction that would let the two share a constant, so it re-declares the library's
//! limits and the crate docs on each side promise, in prose, that they agree. Nothing but this test
//! binds the copies: change `MOST_FACTOR`, `NESTING_LIMIT`, or `CHAIN_FACTOR` on one side alone and
//! the compile-time attribute check and the run-time directive scanner start accepting different
//! values, with the user told about the difference by whichever tool happens to run second and no
//! failing test to catch it. The values are compared symbol-to-symbol so no literal is duplicated
//! here for a later edit to leave stale.
//!
//! Matching constants is not the same claim as matching behavior: the engine's scanner walks raw
//! source bytes and the proc-macro's walks an already-tokenized stream, so an equal `NESTING_LIMIT`
//! does not by itself prove the two agree on where a chain of calls or an `else if` ladder crosses
//! it. Both sweeps below drive the two scanners with the same generated corpus, incrementally, from
//! shallow to well past the shared limit, for every syntax family [`cargo_gamma_engine`]'s doc
//! comments name as a nesting hazard — so a scanner that omitted a family, or counted it against a
//! different threshold, disagrees with its sibling at whatever depth that first matters.
//!
//! Both scanners charge delimiter entry and propagate completed-child peaks to subsequent links.
//! [`nesting_guards`] checks exact agreement for every family, including calls and method chains
//! whose final token is a group.
//!
//! [`the_two_channels_agree_on_timeout_multiplier_arguments`] pins a third kind of agreement: the
//! attribute and the comment directive are the same text with `//` in front of it, so an argument
//! list one channel accepts must not be a compile error to the other.

use cargo_gamma_attrs_impl::inert_timeout;
use cargo_gamma_attrs_impl::test_support::{
    CHAIN_FACTOR, MOST_FACTOR, NESTING_LIMIT, exceeds_nesting_limit as attrs_exceeds_nesting_limit,
};
use cargo_gamma_engine::parse::exceeds_nesting_limit as engine_exceeds_nesting_limit;
use cargo_gamma_lib::internals::parse::{SourceFile, nesting};
use cargo_gamma_lib::internals::{bounds, suppress};
use proc_macro2::TokenStream;

#[test]
fn the_proc_macro_limits_match_the_library() {
    // `MOST_FACTOR` is a float; comparing the bit patterns is an exact equality that also keeps the
    // pedantic `float_cmp` lint from firing on two constants that are, by construction, identical.
    assert_eq!(
        MOST_FACTOR.to_bits(),
        bounds::MOST_FACTOR.to_bits(),
        "the proc-macro's multiplier ceiling drifted from `bounds::MOST_FACTOR`"
    );
    assert_eq!(
        NESTING_LIMIT,
        nesting::NESTING_LIMIT,
        "the proc-macro's nesting limit drifted from `parse::nesting::NESTING_LIMIT`"
    );
    assert_eq!(
        CHAIN_FACTOR,
        nesting::CHAIN_FACTOR,
        "the proc-macro's chain factor drifted from `parse::nesting::CHAIN_FACTOR`"
    );
}

/// Sweeps one syntax family from shallow to `ceiling`, and asserts the engine's and the
/// proc-macro's scanners give the same accept-or-reject verdict at every depth.
///
/// A per-step comparison, rather than a single check at a hand-computed "the" boundary, is what
/// lets this catch a threshold that drifted on one side without this test needing to reproduce
/// either scanner's internal arithmetic: whatever depth the two disagree at, that depth is where
/// the sweep reports the mismatch. `ceiling` only has to be far enough past whichever scanner's
/// real threshold is highest for `saw_rejection` to fire; it is not a claim about where that
/// threshold falls.
#[track_caller]
fn nesting_guards(family: &str, ceiling: usize, generate: impl Fn(usize) -> String) {
    let mut saw_rejection = false;

    for n in 1..=ceiling {
        let (engine_rejects, attrs_rejects) = verdicts(family, n, &generate);

        assert_eq!(
            engine_rejects,
            attrs_rejects,
            "{family} disagrees at n={n}: the engine {} it, the proc-macro {} it\n{}",
            if engine_rejects { "rejects" } else { "accepts" },
            if attrs_rejects { "rejects" } else { "accepts" },
            generate(n)
        );

        saw_rejection |= engine_rejects;
    }

    assert!(
        saw_rejection,
        "{family}'s corpus never triggered a rejection up to n={ceiling}; the sweep never actually reached either scanner's limit, so it proved nothing about the boundary"
    );
}

/// Both scanners' verdicts on one family at one depth.
///
/// The corpus text is parsed here rather than in either caller, so a generator that emits something
/// that is not a token stream is reported as the corpus bug it is rather than as a disagreement.
#[track_caller]
fn verdicts(family: &str, n: usize, generate: impl Fn(usize) -> String) -> (bool, bool) {
    let text = generate(n);
    let stream: TokenStream = text
        .parse()
        .unwrap_or_else(|error| panic!("{family}'s corpus at n={n} is not a valid token stream: {error}\n{text}"));

    (
        engine_exceeds_nesting_limit(&text),
        attrs_exceeds_nesting_limit(&stream, NESTING_LIMIT),
    )
}

/// Each generated chain family must cross both scanners' rejection boundary inside this ceiling.
const CHAIN_CEILING: usize = NESTING_LIMIT * CHAIN_FACTOR * 2;

#[test]
fn the_two_nesting_guards_agree_on_delimiters() {
    nesting_guards("delimiters", NESTING_LIMIT + 16, corpus::delimiters);
}

#[test]
fn the_engine_guards_postfix_calls_no_later_than_the_proc_macro() {
    nesting_guards("postfix calls", CHAIN_CEILING, corpus::postfix_calls);
}

#[test]
fn the_engine_guards_postfix_indexes_no_later_than_the_proc_macro() {
    nesting_guards("postfix indexes", CHAIN_CEILING, corpus::postfix_indexes);
}

#[test]
fn the_engine_guards_postfix_methods_no_later_than_the_proc_macro() {
    nesting_guards("postfix methods", CHAIN_CEILING, corpus::postfix_methods);
}

#[test]
fn the_two_nesting_guards_agree_on_unary_chains() {
    nesting_guards("unary chains", CHAIN_CEILING, corpus::unary_chain);
}

#[test]
fn the_two_nesting_guards_agree_on_binary_chains() {
    nesting_guards("binary chains", CHAIN_CEILING, corpus::binary_chain);
}

#[test]
fn the_two_nesting_guards_agree_on_greater_than_chains() {
    nesting_guards("greater-than chains", CHAIN_CEILING, corpus::greater_than_chain);
}

#[test]
fn the_two_nesting_guards_agree_on_shift_chains() {
    nesting_guards("shift chains", CHAIN_CEILING, corpus::shift_chain);
}

#[test]
fn the_two_nesting_guards_agree_on_cast_chains() {
    nesting_guards("cast chains", CHAIN_CEILING, corpus::cast_chain);
}

#[test]
fn the_two_nesting_guards_agree_on_mixed_operator_and_cast_chains() {
    nesting_guards(
        "mixed operator and cast chains",
        CHAIN_CEILING,
        corpus::mixed_operator_and_cast_chain,
    );
}

#[test]
fn the_engine_guards_mixed_postfix_and_cast_chains_no_later_than_the_proc_macro() {
    nesting_guards("mixed postfix and cast chains", CHAIN_CEILING, corpus::mixed_postfix_and_cast_chain);
}

#[test]
fn completed_nested_groups_propagate_their_peak_before_outer_postfix_links() {
    let fields = NESTING_LIMIT * CHAIN_FACTOR - 2 * CHAIN_FACTOR;
    let boundary = format!("((value{}))", ".field".repeat(fields));
    let overflow = format!("{boundary}.field");
    for (source, exceeds) in [(boundary, false), (overflow, true)] {
        let tokens = source.parse().expect("the fixed-depth regression tokenizes");
        assert_eq!(attrs_exceeds_nesting_limit(&tokens, NESTING_LIMIT), exceeds, "{source}");
        assert_eq!(engine_exceeds_nesting_limit(&source), exceeds, "{source}");
    }
}

#[test]
fn the_two_nesting_guards_agree_on_else_if_ladders() {
    nesting_guards("else if ladders", CHAIN_CEILING, corpus::else_if_ladder);
}

#[test]
fn generated_mixed_syntax_agrees_at_the_public_cumulative_boundary() {
    bolero::check!().with_type::<[u8; 16]>().for_each(|choices| {
        let source = corpus::mixed_nesting(choices);
        let rejects = |prefixes: usize| {
            let text = format!("{}{source}", "!".repeat(prefixes));
            assert!(text.len() <= 4096, "generated source must stay bounded");
            let tokens = text.parse().expect("the bounded grammar produces balanced Rust tokens");
            let engine = engine_exceeds_nesting_limit(&text);
            let attrs = attrs_exceeds_nesting_limit(&tokens, NESTING_LIMIT);
            assert_eq!(engine, attrs, "nesting guards disagree:\n{text}");
            engine
        };

        let mut accepted = 0;
        let mut rejected = NESTING_LIMIT * CHAIN_FACTOR + 1;
        assert!(!rejects(accepted), "the shallow grammar must begin below the production limit");
        assert!(rejects(rejected), "the leading unary run must exceed the production limit");
        while rejected - accepted > 1 {
            let width = rejected - accepted;
            let middle = accepted + width / 2;
            if rejects(middle) {
                rejected = middle;
            } else {
                accepted = middle;
            }
            assert!(rejected - accepted < width, "every search step must make progress");
        }
        for prefixes in accepted.saturating_sub(1)..=rejected + 1 {
            assert_eq!(rejects(prefixes), prefixes >= rejected, "the cumulative boundary must be adjacent");
        }
    });
}

/// The multiplier the comment-directive channel reads out of one argument list, or `None` when it
/// refuses the list or finds no multiplier in it.
fn directive_multiplier(arguments: &str) -> Option<f64> {
    let source = format!("// #[gamma::test_timeout_multiplier({arguments})]\nfn f() -> u32 {{ 1 }}\n");
    let file = SourceFile::parse("fixture.rs", source).expect("the fixture is valid Rust");

    suppress::directives(&file)
        .ok()?
        .first()
        .and_then(|directive| directive.test_timeout_multiplier)
}

/// Whether the attribute channel compiles one argument list rather than replacing it with a
/// `compile_error!`.
fn attribute_accepts(arguments: &str) -> bool {
    let attr: TokenStream = arguments.parse().expect("the arguments are valid Rust tokens");
    let item: TokenStream = "fn f() -> u32 { 1 }".parse().expect("the item is valid Rust tokens");

    !inert_timeout("test_timeout_multiplier", &attr, item)
        .to_string()
        .contains("compile_error")
}

/// `#[gamma::test_timeout_multiplier(...)]` and `// #[gamma::test_timeout_multiplier(...)]` are the
/// same text with two characters in front of it, and users move one to the other by adding or
/// deleting those characters. So an argument list one channel reads must not be a compile error to
/// the other.
///
/// The proc macro used to parse everything after a leading number as one `f64`, which made
/// `3.0, reason = "slow"` — ordinary to the directive scanner, which splits on commas first —
/// into a compile error blaming the numeric bound rather than the argument shape. Deleting the
/// `//` from a working directive broke the build, and the message pointed at the wrong thing.
///
/// Splitting on commas alone was not enough, because the proc macro then read only the *first*
/// argument as a possible multiplier while the directive scanner classifies every argument on its
/// own. `arith, 2.5` — a selector and a multiplier, in the order the tool's own suppression tests
/// have always written it — stayed a compile error. Position carries no meaning on either channel
/// now, so the accepted table below deliberately writes the same multiplier before, after, and
/// between its selectors.
///
/// The multiplier itself is compared, not merely the accept/reject decision: a channel that
/// accepted the text and then read a different number out of it would be the same defect with a
/// quieter symptom.
#[test]
fn the_two_channels_agree_on_timeout_multiplier_arguments() {
    let accepted: &[(&str, f64)] = &[
        ("2.5", 2.5),
        ("3.0, reason = \"slow\"", 3.0),
        ("2.5, tag = \"integration\"", 2.5),
        ("2.5, arith", 2.5),
        ("2.5, arith, reason = \"complex math\"", 2.5),
        // Position carries no meaning on either channel: the tool's own suppression tests have
        // read `#[gamma::test_timeout_multiplier(arith, 4.0)]` as a selector and a multiplier all
        // along, so a proc macro that only ever looked at the first argument made valid,
        // already-supported text a compile error.
        ("arith, 2.5", 2.5),
        ("arith, 2.5, reason = \"complex math\"", 2.5),
        ("reason = \"slow\", 2.5", 2.5),
        ("test_timeout_multiplier = 2.5", 2.5),
        ("arith, test_timeout_multiplier = 2.5, reason = \"complex math\"", 2.5),
    ];

    for (arguments, expected) in accepted {
        assert!(
            attribute_accepts(arguments),
            "the attribute channel refused `{arguments}`, which the directive channel reads as {expected}"
        );
        assert_eq!(
            directive_multiplier(arguments).map(f64::to_bits),
            Some(expected.to_bits()),
            "the directive channel did not read {expected} out of `{arguments}`"
        );
    }

    for arguments in [",arith", "arith,,reason = \"covered\"", "arith,", ","] {
        assert!(!attribute_accepts(arguments), "the attribute channel accepted `{arguments}`");
        assert_eq!(
            directive_multiplier(arguments),
            None,
            "the directive channel accepted `{arguments}`"
        );
    }

    // Neither channel may take a multiplier that cannot bound a timeout, whichever spelling it
    // arrives in and wherever in the list it sits. `inf` and `nan` tokenize as identifiers rather
    // than literals, which is exactly the shape a scanner keyed on literals lets through.
    for arguments in [
        "0",
        "-1",
        "1e300",
        "inf",
        "nan",
        "-1.0, reason = \"slow\"",
        "arith, -1.0",
        "reason = \"slow\", 0",
        "arith, inf",
    ] {
        assert!(!attribute_accepts(arguments), "the attribute channel accepted `{arguments}`");
        assert_eq!(
            directive_multiplier(arguments),
            None,
            "the directive channel accepted `{arguments}`"
        );
    }

    // An item has one timeout, so a second multiplier says something neither channel can honour.
    // Both refuse it rather than keeping whichever one they happened to read last: silently
    // dropping one leaves a directive that reads as though it says two things and does one, and a
    // channel that dropped a *different* one than its counterpart would give the same text two
    // meanings depending on whether the `//` was there.
    for arguments in [
        "2.0, 3.0",
        "2.0, arith, 3.0",
        "2.0, 3.0, 4.0",
        "factor = 2.0, multiplier = 3.0",
        "2.0, factor = 3.0",
        "factor = 2.0, 3.0",
        "test_timeout_multiplier = 2.0, arith, 3.0",
    ] {
        assert!(
            !attribute_accepts(arguments),
            "the attribute channel accepted the duplicate multiplier in `{arguments}`"
        );
        assert_eq!(
            directive_multiplier(arguments),
            None,
            "the directive channel accepted the duplicate multiplier in `{arguments}`"
        );
    }
}

/// Generates one syntax family at a chosen depth, for [`nesting_guards`].
///
/// Every generator produces a flat token sequence — sibling calls, sibling operators, sibling
/// `else if` arms — rather than one token nested inside another, with the sole exception of
/// [`delimiters`]. That is deliberate: this file parses its own corpus with `proc_macro2` to drive
/// the proc-macro scanner, and a flat sequence costs that parser one stack frame regardless of how
/// many repetitions it holds, where genuine nesting costs one frame per level. Keeping every family
/// but one flat is what lets the corpus safely sweep hundreds of repetitions on an ordinary thread
/// stack.
mod corpus {
    /// Eight balanced wrappers keep tokenization shallow while mixing path resets and child peaks.
    pub(super) fn mixed_nesting(choices: &[u8; 16]) -> String {
        const LITERALS: [&str; 4] = ["true", "0", "\"[!;]\"", "r#\"/*[!?;]*/\"#"];
        const COMMENTS: [&str; 4] = [" ", " /* [(!;)] */ ", " // [(!;)]\n ", " /* [ /* ! */ ; ] */ "];
        let mut text = LITERALS[usize::from(choices[0] % 4)].to_owned();
        for pair in choices.chunks_exact(2) {
            let shape = pair[0];
            let detail = pair[1];
            let (open, close, separator) = match shape % 3 {
                0 => ("(", ")", ", "),
                1 => ("[", "]", ", "),
                _ => ("{", "}", "; "),
            };
            let sibling = LITERALS[usize::from(detail % 4)];
            text = match (detail >> 6) % 3 {
                0 => text,
                1 => format!("{sibling}{separator}{text}"),
                _ => format!("{text}{separator}{sibling}"),
            };
            let prefix = "!".repeat(usize::from((shape >> 2) % 4));
            let comment = COMMENTS[usize::from((detail >> 2) % 4)];
            let postfix = ["", ".field", "()", "[0]", "?"][usize::from((shape >> 4) % 5)];
            text = format!("{prefix}{open}{comment}{text}{close}{postfix}");
        }
        text
    }

    /// `n` levels of parenthesis nesting around a literal.
    ///
    /// The one family here that is genuinely nested rather than flat, so it alone is swept over a
    /// modest range in [`nesting_guards`] — comfortably past where either scanner's delimiter check
    /// can trigger, but far short of where nested-group parsing itself would risk this test's own
    /// stack.
    pub(super) fn delimiters(n: usize) -> String {
        format!("{}0{}", "(".repeat(n), ")".repeat(n))
    }

    /// `n` sibling calls: `a()()()...`.
    pub(super) fn postfix_calls(n: usize) -> String {
        format!("a{}", "()".repeat(n))
    }

    /// `n` sibling indexing operations: `a[0][0][0]...`.
    pub(super) fn postfix_indexes(n: usize) -> String {
        format!("a{}", "[0]".repeat(n))
    }

    /// `n` sibling method calls: `a.m().m().m()...`.
    pub(super) fn postfix_methods(n: usize) -> String {
        format!("a{}", ".m()".repeat(n))
    }

    /// `n` prefix negations: `----...1`.
    pub(super) fn unary_chain(n: usize) -> String {
        format!("{}1", "-".repeat(n))
    }

    /// `n` links of same-precedence addition: `a + a + a + ...`.
    pub(super) fn binary_chain(n: usize) -> String {
        let mut text = "a".to_owned();

        for _ in 0..n {
            text.push_str(" + a");
        }

        text
    }

    /// `n` greater-than comparisons: `a > a > a > ...`.
    pub(super) fn greater_than_chain(n: usize) -> String {
        let mut text = "a".to_owned();

        for _ in 0..n {
            text.push_str(" > a");
        }

        text
    }

    /// `n` right shifts: `1 >> 1 >> 1 >> ...`.
    pub(super) fn shift_chain(n: usize) -> String {
        let mut text = "1".to_owned();

        for _ in 0..n {
            text.push_str(" >> 1");
        }

        text
    }

    /// `n` links of `as`-casts: `0 as i64 as i64 as i64...`.
    pub(super) fn cast_chain(n: usize) -> String {
        let mut text = "0".to_owned();

        for _ in 0..n {
            text.push_str(" as i64");
        }

        text
    }

    /// `n` additions followed by `n` casts, so neither family owns the combined depth alone.
    pub(super) fn mixed_operator_and_cast_chain(n: usize) -> String {
        format!("{}{}", binary_chain(n), " as i64".repeat(n))
    }

    /// `n` method calls followed by `n` casts, crossing from postfix to cast nesting.
    pub(super) fn mixed_postfix_and_cast_chain(n: usize) -> String {
        format!("{}{}", postfix_methods(n), " as i64".repeat(n))
    }

    /// An `else if` ladder with `n` middle arms.
    pub(super) fn else_if_ladder(n: usize) -> String {
        let mut text = "if true {}".to_owned();

        for _ in 0..n {
            text.push_str(" else if true {}");
        }

        text.push_str(" else {}");

        text
    }
}
