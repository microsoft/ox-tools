// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{Expr, Item, Member, Stmt};

use super::super::{Defaults, collect_in};
use super::{Collector, compact_path};
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

/// The mutators a source admits once its spans no longer index into its text.
///
/// Every splice in this module refuses a node whose byte range reaches past the file, because
/// such a range means the node came from somewhere other than the text being edited and
/// splicing it would corrupt the source. Truncating the text after parsing puts the collector
/// in exactly that state for every span past the cut, which is the only way to reach those
/// guards from a unit test.
fn mutators_past_the_end(source: &str, keep: usize, ops: &str) -> Vec<&'static str> {
    let mut file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse(ops).unwrap();

    file.text.truncate(keep);

    collect_in(&file, &selection, &CfgSet::unconditional())
        .into_iter()
        .map(|candidate| candidate.mutator)
        .collect()
}

/// The mutators a source admits under a selection, with the given predicates holding.
fn mutators(source: &str, ops: &str, cfg: &CfgSet) -> Vec<&'static str> {
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse(ops).unwrap();

    collect_in(&file, &selection, cfg)
        .into_iter()
        .map(|candidate| candidate.mutator)
        .collect()
}

#[test]
fn a_wildcard_arm_that_is_configured_out_is_not_a_wildcard() {
    // The arm is not in the program the compiler builds, so an earlier arm stopped from
    // matching has nothing to fall through to and the mutant fails to compile as a
    // non-exhaustive match, taking the whole run down with it.
    let source = "fn f(x: bool) -> i32 { match x { true => 1, false => 2, #[cfg(not(unix))] _ => 3 } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

/// A statement the build does not contain is not mutated, and neither is anything inside it.
///
/// The text is discarded after parsing, so a mutant there compiles away: the crate builds, every
/// test passes, and the mutant is scored as a survivor. The reader is told their tests miss a line
/// that is not in their program, and the denominator carries mutants nothing could ever kill.
#[test]
fn a_statement_that_is_configured_out_is_not_mutated() {
    let source = "fn f() { #[cfg(not(unix))] g(1 + 2); }";
    let found = mutators(source, "stmt.delete_call,arith.add_to_sub", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_match_arm_that_is_configured_out_takes_its_body_with_it() {
    let source = "fn f(x: bool) -> i32 { match x { #[cfg(not(unix))] true => 1 + 2, _ => 0 } }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_match_arm_that_is_configured_in_still_mutates_its_body() {
    let source = "fn f(x: bool) -> i32 { match x { #[cfg(unix)] true => 1 + 2, _ => 0 } }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix"));

    assert_eq!(found, vec!["arith.add_to_sub"]);
}

/// The same statement, in the build this time, is mutated as it always was.
#[test]
fn a_statement_that_is_configured_in_still_is_mutated() {
    let source = "fn f() { #[cfg(unix)] g(1 + 2); }";
    let found = mutators(source, "stmt.delete_call,arith.add_to_sub", &CfgSet::parse("unix"));

    assert_eq!(found, vec!["stmt.delete_call", "arith.add_to_sub"]);
}

#[test]
fn an_active_cfg_attr_that_removes_an_item_is_not_mutated() {
    let source = "#[cfg_attr(unix, cfg(windows))] fn f() -> i32 { 1 + 2 }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix").with_test());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn an_active_cfg_attr_that_adds_a_test_gate_is_not_mutated() {
    let source = "#[cfg_attr(unix, cfg(test))] fn helper() -> i32 { 1 + 2 }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix").with_test());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_test_cfg_attr_is_not_mutated_when_test_cfg_differs_between_targets() {
    let source = "#[cfg_attr(test, test)] fn helper() -> i32 { 1 + 2 }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix").with_test());

    assert!(found.is_empty(), "{found:?}");
}

/// A `let` the build does not contain is not mutated, and does not shadow the binding that is.
#[test]
fn a_local_that_is_configured_out_is_not_mutated() {
    let source = "fn f() { #[cfg(not(unix))] let x = 1 + 2; }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

/// The whole of a configured-out statement goes, including the blocks nested inside it.
#[test]
fn a_configured_out_statement_takes_its_nested_blocks_with_it() {
    let source = "fn f(p: bool) { #[cfg(not(unix))] if p { g(1 + 2); } }";
    let found = mutators(source, "arith.add_to_sub,cond.negate,stmt.delete_call", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_wildcard_arm_that_is_configured_in_still_is_one() {
    let source = "fn f(x: bool) -> i32 { match x { true => 1, false => 2, #[cfg(unix)] _ => 3 } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::parse("unix"));

    assert_eq!(found, vec!["match_arm.never_matches", "match_arm.never_matches"]);
}

#[test]
fn a_suppressed_wildcard_arm_still_counts_as_present() {
    // Suppression withholds a mutant, it does not delete code: the arm is compiled and the
    // match is exhaustive because of it, so the arms above it stay mutable.
    let source = "fn f(x: bool) -> i32 { match x { true => 1, #[gamma::skip(match_arm.never_matches)] _ => 3 } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::parse("unix"));

    assert_eq!(found, vec!["match_arm.never_matches"]);
}

#[test]
fn a_binding_catch_all_receives_what_falls_through() {
    let source = "fn f(x: i32) -> i32 { match x { 1 => 10, other => other } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::unconditional());

    assert_eq!(found, vec!["match_arm.never_matches"]);
}

#[test]
fn a_unit_variant_arm_is_not_a_catch_all() {
    // `None` parses as a binding but matches one value, so relying on it would leave the
    // mutated match non-exhaustive.
    let source = "fn f(x: Option<i32>) -> i32 { match x { Some(n) => n, None => 0 } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_leaked_default_is_not_offered_to_a_body_that_already_leaks_one() {
    // `Box::leak(Box::new(T::default()))` and the replacement's
    // `Box::leak(Box::new(Default::default()))` are the same call written two ways, so the
    // mutant is the original program and no test could ever kill it.
    let source = "fn f<T: Default>() -> &'static T { Box::leak(Box::new(T::default())) }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_leaked_value_is_still_offered_to_a_body_that_leaks_something_else() {
    let source = "fn f<T: Default>() -> &'static T { Box::leak(Box::new(make())) }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert!(!found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_condition_literal_is_still_the_literal_it_would_become() {
    // `if (true)` replaced by `true` is the original program, so offering it would plant a
    // survivor no test could ever kill. Seeing through the parentheses is what prevents that.
    let source = "fn f() -> i32 { if (true) { 1 } else { 2 } }";
    let found = mutators(source, "cond.always_true,cond.always_false", &CfgSet::unconditional());

    assert_eq!(found, vec!["cond.always_false"]);
}

#[test]
fn a_parenthesized_zero_is_still_zero_when_negated() {
    // Negating zero yields zero, so dropping the `-` changes nothing.
    let source = "fn f() -> i32 { -(0) }";
    let found = mutators(source, "unary.remove_neg", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_default_call_is_still_a_default_call() {
    let source = "fn f(x: &mut i32) { *x = (Default::default()); }";
    let found = mutators(source, "assign_value.default", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_callee_still_names_the_function_it_calls() {
    let source = "fn f(x: &mut i32) { *x = (Default::default)(); }";
    let found = mutators(source, "assign_value.default", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn only_standard_zero_argument_default_calls_are_noops() {
    let standard = [
        "fn f(mut n: i32) { n = core::default::Default::default(); }",
        "use std::default::Default as StdDefault; fn f(mut n: i32) { n = StdDefault::default(); }",
        "use std::default as defaults; fn f(mut n: i32) { n = defaults::Default::default(); }",
    ];
    let bare_custom = "trait Default { fn default() -> i32; } struct Thing; impl Default for Thing { fn default() -> i32 { 0 } } fn f(mut n: i32) { n = Default::default(); }";
    let nonstandard = [
        "struct Default; impl Default { fn default() -> i32 { 0 } } fn f(mut n: i32) { n = (Default::default)(); }",
        "struct Thing; impl Thing { fn default(_: i32) -> i32 { 0 } } fn f(mut n: i32) { n = Thing::default(1); }",
        "mod custom { pub trait Default { fn default() -> i32; } } struct Thing; impl custom::Default for Thing { fn default() -> i32 { 0 } } fn f(mut n: i32) { n = custom::Default::default(); }",
        "mod custom { pub trait Default { fn default() -> i32; } } use custom::Default as Alias; struct Thing; impl Alias for Thing { fn default() -> i32 { 0 } } fn f(mut n: i32) { n = Alias::default(); }",
    ];

    for source in standard {
        let found = mutators(source, "assign_value.default", &CfgSet::unconditional());

        assert!(found.is_empty(), "{source}: {found:?}");
    }

    assert!(
        mutators(bare_custom, "assign_value.default", &CfgSet::unconditional()).is_empty(),
        "the replacement text is the same local Default call"
    );

    for source in nonstandard {
        let found = mutators(source, "assign_value.default", &CfgSet::unconditional());

        assert_eq!(found, vec!["assign_value.default"], "{source}: {found:?}");
    }
}

#[test]
fn only_standard_default_implementations_suppress_recursive_replacements() {
    let standard = "use core::default::Default as StdDefault; struct S; impl StdDefault for S { fn default() -> Self { S } }";
    let bare_custom = "trait Default { fn default() -> Self; } struct S; impl Default for S { fn default() -> Self { S } }";
    let custom =
        "mod custom { pub trait Default { fn default() -> Self; } } struct S; impl custom::Default for S { fn default() -> Self { S } }";
    let aliased_custom = "mod custom { pub trait Default { fn default() -> Self; } } use custom::Default as Alias; struct S; impl Alias for S { fn default() -> Self { S } }";
    let aliased_standard_with_a_bare_custom_trait = "trait Default { fn default() -> Self; } use core::default::Default as StdDefault; struct S; impl Default for S { fn default() -> Self { S } } impl StdDefault for S { fn default() -> Self { S } }";

    assert!(mutators(standard, "fn_value.default", &CfgSet::unconditional()).is_empty());
    assert!(
        mutators(bare_custom, "fn_value.default", &CfgSet::unconditional()).is_empty(),
        "the fallback's bare spelling would recurse through the local trait"
    );

    for source in [custom, aliased_custom] {
        let found = mutators(source, "fn_value.default", &CfgSet::unconditional());

        assert_eq!(found, vec!["fn_value.default"], "{source}: {found:?}");
    }

    assert_eq!(
        mutators(
            aliased_standard_with_a_bare_custom_trait,
            "fn_value.default",
            &CfgSet::unconditional()
        ),
        vec!["fn_value.default"]
    );
}

#[test]
fn a_parenthesized_binding_pattern_still_catches_what_falls_through() {
    let source = "fn f(x: i32) -> i32 { match x { 1 => 10, (other) => other } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::unconditional());

    assert_eq!(found, vec!["match_arm.never_matches"]);
}

#[test]
fn a_parenthesized_return_type_still_resolves_to_the_type_inside() {
    let source = "fn f() -> (i32) { g() }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert_eq!(found, vec!["fn_value.minus_one", "fn_value.one", "fn_value.zero"]);
}

#[test]
fn a_parenthesized_tuple_return_type_is_still_a_tuple() {
    let source = "fn f() -> ((i32, u8)) { g() }";
    let found = mutators(source, "fn_value.tuple", &CfgSet::unconditional());

    assert!(!found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_iterator_return_type_is_still_an_iterator() {
    let source = "fn f() -> (impl Iterator<Item = u32>) { core::iter::empty() }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert!(!found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_reference_return_type_is_still_a_reference() {
    let source = "fn f<T: Default>() -> (&'static mut T) { g() }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert_eq!(found, vec!["fn_value.default"]);
}

#[test]
fn only_standard_default_bounds_make_generic_fallbacks_available() {
    let standard = "use core::default::Default as StdDefault; fn f<T: StdDefault>() -> &'static mut T { g() }";
    let custom = "mod custom { pub trait Default {} } fn f<T: custom::Default>() -> &'static mut T { g() }";
    let aliased_custom = "mod custom { pub trait Default {} } use custom::Default as Alias; fn f<T: Alias>() -> &'static mut T { g() }";

    assert_eq!(mutators(standard, "fn_value", &CfgSet::unconditional()), vec!["fn_value.default"]);
    assert!(mutators(custom, "fn_value", &CfgSet::unconditional()).is_empty());
    assert!(mutators(aliased_custom, "fn_value", &CfgSet::unconditional()).is_empty());
}

#[test]
fn a_parenthesized_abstract_payload_is_still_abstract() {
    // `impl Clone` has no value this tool can write down, so the `Some` case must stay
    // withheld however many parentheses stand between the option and the trait.
    let source = "fn f() -> Option<(impl Clone)> { g() }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert_eq!(found, vec!["fn_value.none"]);
}

#[test]
fn a_parenthesized_leak_is_still_the_leak_it_wraps() {
    // Same reasoning as the unparenthesized case: the replacement would be the original.
    let source = "fn f<T: Default>() -> &'static T { (Box::leak((Box::new(T::default())))) }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_name_is_still_the_name_that_proves_it_numeric() {
    // The pre-pass reads the whole file for evidence that a name holds a number, and
    // `(a) + 1` is the same evidence as `a + 1`.
    let source = "fn f() { let a = g(); let _ = (a) + 1; h(a); }";
    let found = mutators(source, "expr.increment", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.increment"]);
}

#[test]
fn a_parenthesized_integer_literal_still_fixes_the_other_side() {
    // `+` is ambiguous until one side is an integer literal, which `(1)` still is.
    let source = "fn f() { let a = g(); let _ = a + (1); h(a); }";
    let found = mutators(source, "expr.increment", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.increment"]);
}

#[test]
fn a_parenthesized_callee_still_names_the_type_it_returns() {
    // `(i32::from)(x)` yields a number, so the call is perturbed as well as its argument.
    let source = "fn f(x: u8) { h((i32::from)(x)); }";
    let found = mutators(source, "expr.increment", &CfgSet::unconditional());

    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn a_parenthesized_capacity_call_is_still_a_capacity_call() {
    // A capacity is a request, not an answer: growing or shrinking one changes no result a
    // test could read, so the perturbation is withheld however it is parenthesized. The
    // signature is taken at its word here, which is what puts the call in numeric position.
    let source = "fn f(n: usize) -> usize { (Vec::with_capacity(n)) }";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_parenthesized_let_condition_still_binds_a_pattern() {
    // A condition that binds cannot be replaced by a bare `true`, `false`, or negation
    // without leaving the body without its binding, so no condition mutant is offered.
    let source = "fn f(o: Option<i32>) { if (let Some(v) = o) { h(v); } }";
    let found = mutators(source, "cond", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

/// A rename mutant is named after the swap it performs, so the name and the edit have to agree.
///
/// Asserting the mutator name alone would let the table pair `any` with `count` and still pass
/// — shipping a mutant whose name says one thing and whose edit does another. That is worse
/// than a missing mutant, because the user reads the name in the report and concludes something
/// about a test gap that does not exist. Each case therefore pins the exact replacement text,
/// which is the standard the `fn_value` family already meets.
#[test]
fn every_method_rename_replaces_the_call_its_name_promises() {
    // The table pairs methods that agree on receiver, arity and result type. Each pair is
    // listed here so that dropping an entry is a failure rather than a silent loss of reach.
    let cases: &[(&str, &str, &str)] = &[
        ("fn f(v: V) -> bool { v.any(|x| x) }", "iter.any_to_all", "v.all(|x| x)"),
        ("fn f(v: V) -> bool { v.all(|x| x) }", "iter.all_to_any", "v.any(|x| x)"),
        ("fn f(v: V) -> O { v.min() }", "iter.min_to_max", "v.max()"),
        ("fn f(a: A, b: A) -> A { a.min(b) }", "iter.min_to_max", "a.max(b)"),
        ("fn f(v: V) -> O { v.max() }", "iter.max_to_min", "v.min()"),
        ("fn f(a: A, b: A) -> A { a.max(b) }", "iter.max_to_min", "a.min(b)"),
        ("fn f(v: V) -> O { v.first() }", "iter.first_to_last", "v.last()"),
        ("fn f(v: V) -> O { v.last() }", "iter.last_to_first", "v.first()"),
        (
            "fn f(s: S) -> bool { s.starts_with(\"a\") }",
            "string.starts_with_to_ends_with",
            "s.ends_with(\"a\")",
        ),
        (
            "fn f(s: S) -> bool { s.ends_with(\"a\") }",
            "string.ends_with_to_starts_with",
            "s.starts_with(\"a\")",
        ),
        ("fn f(s: S) -> S { s.to_lowercase() }", "string.lower_to_upper", "s.to_uppercase()"),
        ("fn f(s: S) -> S { s.to_uppercase() }", "string.upper_to_lower", "s.to_lowercase()"),
        (
            "fn f(s: S) -> S { s.to_ascii_lowercase() }",
            "string.lower_to_upper",
            "s.to_ascii_uppercase()",
        ),
        (
            "fn f(s: S) -> S { s.to_ascii_uppercase() }",
            "string.upper_to_lower",
            "s.to_ascii_lowercase()",
        ),
        (
            "fn f(s: S) -> S { s.trim_start() }",
            "string.trim_start_to_trim_end",
            "s.trim_end()",
        ),
        (
            "fn f(s: S) -> S { s.trim_end() }",
            "string.trim_end_to_trim_start",
            "s.trim_start()",
        ),
    ];

    for (source, expected, replacement) in cases {
        let file = SourceFile::parse("test.rs", (*source).to_owned()).expect("parses");
        let selection = Selection::parse(expected).expect("a selection");
        let found: Vec<(&str, String)> = collect_in(&file, &selection, &CfgSet::unconditional())
            .into_iter()
            .map(|candidate| (candidate.mutator, candidate.replacement.to_string()))
            .collect();

        assert_eq!(found, vec![(*expected, (*replacement).to_owned())], "{source}");
    }
}

#[test]
fn every_in_place_reorder_is_offered_for_the_call_it_names() {
    let cases: &[(&str, &str)] = &[
        ("fn f(v: &mut V) { v.sort(); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.sort_by(g); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.sort_by_key(g); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.sort_unstable(); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.sort_unstable_by(g); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.sort_unstable_by_key(g); }", "iter.remove_sort"),
        ("fn f(v: &mut V) { v.dedup(); }", "iter.remove_dedup"),
        ("fn f(v: &mut V) { v.dedup_by(g); }", "iter.remove_dedup"),
        ("fn f(v: &mut V) { v.dedup_by_key(g); }", "iter.remove_dedup"),
    ];

    for (source, expected) in cases {
        let found = mutators(source, expected, &CfgSet::unconditional());

        assert_eq!(found, vec![*expected], "{source}");
    }
}

/// The synthesized values for a return type, as `mutator=replacement`, in a stable order.
fn values_for_return(ty: &str) -> Vec<String> {
    let source = format!("fn f() -> {ty} {{ g() }}");
    let file = SourceFile::parse("test.rs", source).unwrap();
    let selection = Selection::parse("fn_value").unwrap();

    let mut values: Vec<String> = collect_in(&file, &selection, &CfgSet::unconditional())
        .into_iter()
        .map(|candidate| format!("{}={}", candidate.mutator, candidate.replacement))
        .collect();

    values.sort();
    values
}

#[test]
fn every_return_type_is_served_the_values_that_belong_to_it() {
    // One row per kind the resolver can name. Both halves matter: the mutator says what
    // question is being asked, and the replacement says whether the type was understood --
    // `&[u8]` and `&u8` agree on the former and disagree on the latter.
    let cases: &[(&str, &[&str])] = &[
        ("()", &["fn_value.unit=()"]),
        ("bool", &["fn_value.bool_false=false", "fn_value.bool_true=true"]),
        ("i32", &["fn_value.minus_one=-1", "fn_value.one=1", "fn_value.zero=0"]),
        ("u32", &["fn_value.one=1", "fn_value.zero=0"]),
        ("f32", &["fn_value.minus_one=-1.0", "fn_value.one=1.0", "fn_value.zero=0.0"]),
        ("f64", &["fn_value.minus_one=-1.0", "fn_value.one=1.0", "fn_value.zero=0.0"]),
        ("&'static str", &["fn_value.empty_string=\"\"", "fn_value.xyzzy_string=\"xyzzy\""]),
        // A string literal is `&'static str` and will not type-check where a mutable slice was
        // promised, so a leaked boxed `str` stands in for it.
        (
            "&'static mut str",
            &[
                "fn_value.empty_string=Box::leak(String::new().into_boxed_str())",
                "fn_value.xyzzy_string=Box::leak(String::from(\"xyzzy\").into_boxed_str())",
            ],
        ),
        (
            "&mut str",
            &[
                "fn_value.empty_string=Box::leak(String::new().into_boxed_str())",
                "fn_value.xyzzy_string=Box::leak(String::from(\"xyzzy\").into_boxed_str())",
            ],
        ),
        (
            "String",
            &["fn_value.empty_string=String::new()", "fn_value.xyzzy_string=\"xyzzy\".to_owned()"],
        ),
        (
            "NonZeroU32",
            &[
                "fn_value.one=NonZeroU32::new(1).unwrap()",
                "fn_value.two=NonZeroU32::new(2).unwrap()",
            ],
        ),
        // A slice reference has a `Default` of its own, so it is not leaked into being.
        ("&'static [u8]", &["fn_value.default=Default::default()"]),
        (
            "&'static u8",
            &["fn_value.one=&*Box::leak(Box::new(1))", "fn_value.zero=&*Box::leak(Box::new(0))"],
        ),
        (
            "&'static mut u8",
            &["fn_value.one=Box::leak(Box::new(1))", "fn_value.zero=Box::leak(Box::new(0))"],
        ),
        (
            "Vec<u8>",
            &[
                "fn_value.empty_collection=Vec::new()",
                "fn_value.one_element=core::iter::once(0).collect()",
                "fn_value.one_element=core::iter::once(1).collect()",
            ],
        ),
        ("Box<u8>", &["fn_value.one=Box::new(1)", "fn_value.zero=Box::new(0)"]),
        ("Rc<u8>", &["fn_value.one=Rc::new(1)", "fn_value.zero=Rc::new(0)"]),
        ("Arc<u8>", &["fn_value.one=Arc::new(1)", "fn_value.zero=Arc::new(0)"]),
        ("Cow<'static, str>", &["fn_value.default=Cow::Owned(Default::default())"]),
        (
            "Option<u8>",
            &["fn_value.none=None", "fn_value.some=Some(0)", "fn_value.some=Some(1)"],
        ),
        (
            "Result<u8, E>",
            &[
                "fn_value.err_default=Err(Default::default())",
                "fn_value.ok=Ok(0)",
                "fn_value.ok=Ok(1)",
            ],
        ),
        (
            "HashMap<u8, u8>",
            &[
                "fn_value.empty_collection=HashMap::new()",
                "fn_value.one_element=core::iter::once((0, 0)).collect()",
            ],
        ),
        (
            "(i32, u8)",
            &[
                "fn_value.tuple=(-1, 0)",
                "fn_value.tuple=(-1, 1)",
                "fn_value.tuple=(0, 0)",
                "fn_value.tuple=(0, 1)",
                "fn_value.tuple=(1, 0)",
                "fn_value.tuple=(1, 1)",
            ],
        ),
        ("(u32,)", &["fn_value.tuple=(0,)", "fn_value.tuple=(1,)"]),
    ];

    for (ty, expected) in cases {
        assert_eq!(values_for_return(ty), *expected, "{ty}");
    }
}

#[test]
fn every_iterator_trait_that_either_satisfies_is_served_iterator_values() {
    // `Either` implements exactly these four, which is what makes them safe to mutate; a
    // trait outside the set has no value this tool could write down.
    for ty in [
        "impl Iterator<Item = u32>",
        "impl DoubleEndedIterator<Item = u32>",
        "impl ExactSizeIterator<Item = u32>",
        "impl FusedIterator<Item = u32>",
    ] {
        assert_eq!(
            values_for_return(ty),
            [
                "fn_value.empty_collection=core::iter::empty()",
                "fn_value.one_element=core::iter::once(0)",
                "fn_value.one_element=core::iter::once(1)",
            ],
            "{ty}"
        );
    }

    assert!(values_for_return("impl Clone").is_empty());
}

#[test]
fn every_expression_kind_is_descended_into() {
    // Each source nests one mutable site inside one kind of expression. A visitor that
    // stopped recursing there would still collect everything else, so nothing but a case
    // per kind notices the loss.
    let cases: &[(&str, &str)] = &[
        ("binary", "fn f() { let _ = (a + b) * c; }"),
        ("unary", "fn f() { let _ = -(a + b); }"),
        ("if", "fn f() { if c { let _ = a + b; } }"),
        ("while", "fn f() { while c { let _ = a + b; } }"),
        ("match", "fn f(x: T) { match x { _ => { let _ = a + b; } } }"),
        ("struct", "fn f() { let _ = S { v: a + b }; }"),
        ("range", "fn f() { let _ = (a + b)..c; }"),
        ("break", "fn f() { loop { break a + b; } }"),
        ("call", "fn f() { g(a + b); }"),
        ("assign", "fn f() { x = a + b; }"),
        ("method_call", "fn f() { x.g(a + b); }"),
        ("index", "fn f() { let _ = x[a + b]; }"),
        ("return", "fn f() -> T { return a + b; }"),
        ("for_loop", "fn f() { for i in y { let _ = a + b; } }"),
    ];

    for (kind, source) in cases {
        let found = mutators(source, "arith.add_to_sub", &CfgSet::unconditional());

        assert_eq!(found, vec!["arith.add_to_sub"], "{kind}: {source}");
    }
}

#[test]
fn every_expression_kind_is_descended_into_while_looking_for_numeric_names() {
    // The pre-pass that decides which names hold numbers has its own visitor, and each of
    // these nests the evidence one level inside the kind being overridden, so only recursion
    // reaches it. Perturbing `a` where it is merely passed along is what proves the name was
    // learned; the arithmetic holding the evidence is perturbed either way.
    let cases: &[(&str, &str)] = &[
        ("binary", "fn f() { let a = g(); let _ = (a * b) + z; h(a); }"),
        ("index", "fn f() { let a = g(); let _ = v[a * b]; h(a); }"),
        ("method_call", "fn f() { let a = g(); let _ = x.q(a * b); h(a); }"),
        ("for_loop", "fn f() { let a = g(); for i in y { let _ = a * b; } h(a); }"),
    ];

    for (kind, source) in cases {
        let file = SourceFile::parse("test.rs", (*source).to_owned()).unwrap();
        let selection = Selection::parse("expr.increment").unwrap();

        let found: Vec<String> = collect_in(&file, &selection, &CfgSet::unconditional())
            .into_iter()
            .map(|candidate| candidate.replacement.to_string())
            .collect();

        assert!(found.contains(&"(a) + 1".to_owned()), "{kind}: {found:?}");
    }
}

#[test]
fn a_const_generic_argument_in_expression_position_is_never_mutated() {
    // A guard is a function call, and no const context will evaluate one, so a mutant here
    // could only ever be withdrawn as unviable after the whole tree had been built for it.
    for source in [
        "fn f() { let _ = Foo::<{ a + b }>::BAR; }",
        "fn f() { g::<{ a + b }>(); }",
        "fn f() { let _ = <T as Tr<{ a + b }>>::C; }",
    ] {
        let found = mutators(source, "arith", &CfgSet::unconditional());

        assert!(found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn a_generic_argument_that_is_not_const_is_still_reached() {
    // Only the const case is inert; a closure body written as an associated-type binding is
    // ordinary code that a guard can sit inside.
    let source = "fn f() { let _ = g::<[u8; 4]>(a + b); }";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::unconditional());

    assert_eq!(found, vec!["arith.add_to_sub"], "{found:?}");
}

/// A span that reaches past the file text is refused rather than spliced.
///
/// This is the macro-expansion case: the node is real, but its offsets do not describe the
/// bytes this file holds, so every edit derived from them would land in the wrong place.
#[test]
fn a_site_whose_span_runs_past_the_text_offers_nothing() {
    let source = "fn f(a: i32, b: i32) -> bool { a < b }\n";
    let whole = mutators(source, "relational", &CfgSet::unconditional());
    let truncated = mutators_past_the_end(source, 10, "relational");

    assert!(!whole.is_empty(), "the source offers nothing even intact");
    assert!(truncated.is_empty(), "{truncated:?}");
}

/// The struct-field omission refuses a literal whose span it cannot index.
#[test]
fn a_struct_literal_past_the_text_omits_no_field() {
    let source = "fn f(base: S) -> S { S { a: 1, b: 2, ..base } }\n";
    let whole = mutators(source, "struct_field.omit", &CfgSet::unconditional());
    let truncated = mutators_past_the_end(source, 22, "struct_field.omit");

    assert_eq!(whole, vec!["struct_field.omit", "struct_field.omit"]);
    assert!(truncated.is_empty(), "{truncated:?}");
}

/// A literal with no base expression has nothing to fall back on, so no field can be dropped.
#[test]
fn a_struct_literal_without_a_base_omits_no_field() {
    let source = "fn f() -> S { S { a: 1, b: 2 } }\n";
    let found = mutators(source, "struct_field.omit", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

/// The `vec!` element omission refuses a macro whose span it cannot index.
#[test]
fn a_vec_literal_past_the_text_omits_no_element() {
    let source = "fn f() -> Vec<i32> { vec![1, 2, 3] }\n";
    let whole = mutators(source, "collection.omit_element", &CfgSet::unconditional());
    let truncated = mutators_past_the_end(source, 24, "collection.omit_element");

    assert_eq!(whole.len(), 3, "{whole:?}");
    assert!(truncated.is_empty(), "{truncated:?}");
}

/// A method with no rename in the table is left alone rather than renamed to nothing.
#[test]
fn a_method_the_table_does_not_know_is_not_renamed() {
    let source = "fn f(x: Thing) -> i32 { x.frobnicate() }\n";
    let found = mutators(source, "iter,string", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

/// A perturbation needs the text of the expression, which a span past the file cannot give.
#[test]
fn a_perturbed_expression_past_the_text_offers_nothing() {
    let source = "fn f(a: i32) -> i32 { g(a) }\n";
    let whole = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());
    let truncated = mutators_past_the_end(source, 22, "expr.increment,expr.decrement");

    assert!(!whole.is_empty(), "the source offers nothing even intact");
    assert!(truncated.is_empty(), "{truncated:?}");
}

/// A block is a lexical scope, and its `let` must not outlive it.
///
/// A `let` inside `{ … }` must not overwrite the numeric evidence for a shadowed outer name beyond
/// the block. Both directions are wrong and in opposite ways: losing the evidence withholds valid
/// mutants and inflates the score, and gaining it emits mutants that cannot compile.
#[test]
fn a_block_local_binding_does_not_outlive_its_block() {
    // The outer `n` is numeric again once the block has closed, so its use is still perturbed.
    let kept = mutators(
        "fn f(n: usize) { { let n: String = g(); } h(n); }",
        "expr.increment",
        &CfgSet::unconditional(),
    );

    assert_eq!(kept, vec!["expr.increment"], "the inner `let` must not erase the outer evidence");

    // And the reverse: the inner `let` must not lend its numeric type to the outer `String`, which
    // would offer `(n) + 1` on a name that cannot be added to.
    let refused = mutators(
        "fn f(n: String) { { let n: usize = g(); } h(n); }",
        "expr.increment",
        &CfgSet::unconditional(),
    );

    assert!(refused.is_empty(), "the inner `let` must not leak numeric evidence: {refused:?}");
}

/// Scoping the evidence must not break the case it exists to serve.
///
/// A `let scanned;` settled by an assignment inside a nested block is the common shape, and it only
/// works because the nested block inherits the deferral its parent recorded. Clearing rather than
/// inheriting would offer the initialising assignment as a deletion candidate, whose mutant fails
/// to compile as E0381 at a *different* statement — the diagnostic this suppression exists to
/// avoid.
#[test]
fn a_deferred_binding_is_still_seen_from_inside_a_nested_block() {
    let source = "fn f(c: bool) -> i32 { let scanned; if c { scanned = 1; } else { scanned = 2; } scanned }";
    let found = mutators(source, "stmt.delete_assign", &CfgSet::unconditional());

    assert!(
        found.is_empty(),
        "an assignment that first initialises a deferred `let` must not be deletable: {found:?}"
    );
}

/// One module's `use` must not decide another module's question.
///
/// The import index is keyed by the bare name and spans the whole file, so a later `use` must not
/// overwrite an earlier one and leave a bare `Error` resolving to whichever was written last. That
/// would make the answer depend on the order the modules happen to appear in: reading `std::io`
/// first offers `Err(Default::default())` for both, reading `crate` first withholds it for both,
/// and in each case one of the two modules gets the other's answer.
#[test]
fn one_modules_import_does_not_decide_anothers() {
    let io_first = concat!(
        "mod a { use std::io::Error; pub fn f() -> Result<u8, Error> { Ok(0) } }\n",
        "mod b { use crate::Error; pub fn g() -> Result<u8, Error> { Ok(0) } }",
    );
    let crate_first = concat!(
        "mod b { use crate::Error; pub fn g() -> Result<u8, Error> { Ok(0) } }\n",
        "mod a { use std::io::Error; pub fn f() -> Result<u8, Error> { Ok(0) } }",
    );

    let first = mutators(io_first, "fn_value.err_default", &CfgSet::unconditional());
    let second = mutators(crate_first, "fn_value.err_default", &CfgSet::unconditional());

    assert_eq!(
        first, second,
        "the decision must not depend on which `use` the file happens to write last"
    );
}

/// Demoting a contested name must not cost the answer for a name nothing contests.
#[test]
fn an_uncontested_import_still_resolves_to_where_it_came_from() {
    // Foreign, so it has no `Default` and the mutant is withheld.
    let foreign = mutators(
        "mod a { use std::io::Error; pub fn f() -> Result<u8, Error> { Ok(0) } }",
        "fn_value.err_default",
        &CfgSet::unconditional(),
    );

    assert!(foreign.is_empty(), "a foreign error type has no `Default`: {foreign:?}");

    // Local, so `Default::default()` is a guess worth making.
    let local = mutators(
        "mod b { use crate::Error; pub fn g() -> Result<u8, Error> { Ok(0) } }",
        "fn_value.err_default",
        &CfgSet::unconditional(),
    );

    assert_eq!(local, vec!["fn_value.err_default"], "a workspace error type may have a `Default`");
}

/// Two modules importing the *same* path do not disagree, so neither loses its answer.
#[test]
fn importing_one_path_twice_is_not_a_disagreement() {
    let source = concat!(
        "mod a { use std::io::Error; pub fn f() -> Result<u8, Error> { Ok(0) } }\n",
        "mod c { use std::io::Error; pub fn h() -> Result<u8, Error> { Ok(0) } }",
    );
    let found = mutators(source, "fn_value.err_default", &CfgSet::unconditional());

    assert!(
        found.is_empty(),
        "a repeated identical import must not demote the name to unknown: {found:?}"
    );
}

/// A cast writes the type at the use site, so a cast argument is perturbed like any other number.
#[test]
fn a_cast_argument_is_perturbed_as_a_number() {
    let source = "fn f(x: usize) { g(x as i32); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.decrement", "expr.increment"], "{found:?}");
}

/// Negation is a number; `!`/`*` are excluded elsewhere because they may not be.
#[test]
fn a_negated_argument_is_perturbed_as_a_number() {
    let source = "fn f(x: i32) { g(-x); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.decrement", "expr.increment"], "{found:?}");
}

/// `String + &str` is addition too, so `+` asks each side rather than assuming both are numbers.
/// A numeric left side settles it without needing the literal on the right to answer as well.
#[test]
fn an_addition_is_numeric_when_either_side_is() {
    let source = "fn f(a: i32) { g(a + 1); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.decrement", "expr.increment"], "{found:?}");
}

/// A tuple field's type is not recorded anywhere this file reads, so it never answers "number".
#[test]
fn a_tuple_field_access_is_never_known_numeric() {
    let source = "fn f(t: (i32, i32)) { g(t.0); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

/// A trait path past the text cannot be read, so the scope falls back to naming itself after the
/// `Self` type alone rather than reporting `<S as >` from an unreadable trait.
#[test]
fn a_trait_path_past_the_text_names_the_scope_for_the_self_type_alone() {
    let source = "struct S; trait Trait { fn f(&self) -> i32; } impl Trait for S { fn f(&self) -> i32 { 1 + 2 } }\n";
    let truncated = mutators_past_the_end(source, 51, "arith.add_to_sub");

    assert!(truncated.is_empty(), "{truncated:?}");
}

/// A method call past the text is refused rather than renamed with a phantom suffix.
#[test]
fn a_method_call_past_the_text_is_not_renamed() {
    let source = "fn f(v: Vec<i32>) -> Option<&i32> { v.first() }\n";
    let whole = mutators(source, "iter.first_to_last", &CfgSet::unconditional());
    let truncated = mutators_past_the_end(source, 44, "iter.first_to_last");

    assert_eq!(whole, vec!["iter.first_to_last"], "{whole:?}");
    assert!(truncated.is_empty(), "{truncated:?}");
}

/// A trait definition the build does not contain offers nothing from any of its default bodies.
#[test]
fn a_trait_that_is_configured_out_is_not_mutated() {
    let source = "#[cfg(not(unix))] trait Hidden { fn f(&self) -> i32 { 1 + 2 } }\n";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

/// An expression the build does not contain is refused even when it sits inside a call argument
/// rather than as a whole statement, which is the position `visit_block` cannot filter for it.
#[test]
fn an_argument_expression_that_is_configured_out_is_not_mutated() {
    let source = "fn f() -> i32 { g(#[cfg(not(unix))] (1 + 2)) }\n";
    let found = mutators(source, "arith.add_to_sub", &CfgSet::parse("unix"));

    assert!(found.is_empty(), "{found:?}");
}

/// `omit_elements` refuses a macro with fewer than two elements, but its own caller already
/// filters that case before ever calling it. Calling it directly is the only way to reach the
/// guard that keeps the function safe on its own terms.
#[test]
fn omit_elements_refuses_a_macro_with_no_elements() {
    let file = SourceFile::parse("test.rs", "fn f() -> Vec<i32> { vec![] }".to_owned()).unwrap();
    let selection = Selection::parse("collection.omit_element").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);
    let macro_node: syn::Macro = syn::parse_str("vec![]").unwrap();
    let elements = Punctuated::new();

    collector.omit_elements(&macro_node, &elements);

    assert!(collector.finish().is_empty());
}

#[test]
fn omit_elements_refuses_element_spans_outside_the_macro() {
    let file = SourceFile::parse("test.rs", "fn f() { vec![1, 2]; }".to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Macro(statement) = &function.block.stmts[0] else {
        panic!("test source should contain a macro statement");
    };
    let node = statement.mac.clone();
    let mut elements = node.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated).unwrap();
    elements[0] = syn::parse_quote!(outside);
    let selection = Selection::parse("collection.omit_element").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.omit_elements(&node, &elements);

    assert!(collector.finish().is_empty());
}

#[test]
fn omit_update_refuses_field_spans_outside_the_struct() {
    let mut file = SourceFile::parse("test.rs", "fn f() { S { field: 1, ..base }; }".to_owned()).unwrap();
    let Item::Fn(function) = &mut file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Expr(Expr::Struct(expression), _) = &mut function.block.stmts[0] else {
        panic!("test source should contain a struct expression");
    };
    expression.fields[0].member = Member::Named(syn::Ident::new("outside", proc_macro2::Span::call_site()));
    let node = expression.clone();
    let selection = Selection::parse("struct_field.omit").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.struct_fields(&node);

    assert!(collector.finish().is_empty());
}

#[test]
fn rename_method_refuses_a_name_span_outside_the_call() {
    let mut file = SourceFile::parse("test.rs", "fn f(values: &[u8]) { values.first(); }".to_owned()).unwrap();
    let Item::Fn(function) = &mut file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Expr(Expr::MethodCall(expression), _) = &mut function.block.stmts[0] else {
        panic!("test source should contain a method call");
    };
    expression.method = syn::Ident::new("first", proc_macro2::Span::call_site());
    let node = expression.clone();
    let selection = Selection::parse("iter.first_to_last").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.rename_method(&node, "first");

    assert!(collector.finish().is_empty());
}

#[test]
fn direct_local_visit_refuses_a_configured_out_binding() {
    let file = SourceFile::parse("test.rs", "fn f() { #[cfg(not(unix))] let value: i32 = 1; }".to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Local(local) = &function.block.stmts[0] else {
        panic!("test source should contain a local");
    };
    let selection = Selection::parse("expr.increment").unwrap();
    let cfg = CfgSet::parse("unix");
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.visit_local(local);

    assert!(collector.bindings.is_empty());
    assert!(collector.finish().is_empty());
}

/// `==` and the other comparisons are not known to be numbers on either side, so a comparison
/// argument is never perturbed as one.
#[test]
fn a_comparison_argument_is_never_known_numeric() {
    let source = "fn f(a: i32, b: i32) { g(a == b); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

/// A comment inside a qualified trait path is trivia, not part of the trait's identity, so the
/// scope name built from it must not carry the comment along.
#[test]
fn a_comment_inside_a_trait_path_does_not_reach_the_item_path() {
    let source = "struct S; impl some/*noise*/::Trait for S { fn f(&self) -> i32 { 1 + 2 } }\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("arith.add_to_sub").unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());

    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(&*candidates[0].item_path, "<S as some::Trait>::f");
}

/// A literal inside a trait path -- a const generic argument -- is stepped over by the source
/// parser's lexer rather than by the whitespace filter, so it survives intact.
#[test]
fn a_literal_inside_a_trait_path_survives_compaction() {
    let source = "struct S; impl Trait<5> for S { fn f(&self) -> i32 { 1 + 2 } }\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("arith.add_to_sub").unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());

    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(&*candidates[0].item_path, "<S as Trait<5>>::f");
}

/// Lifetime and type tokens need a separator even though punctuation around them does not.
#[test]
fn an_impl_self_type_preserves_the_boundary_after_a_lifetime() {
    let source = "trait Trait { fn f(&self) -> i32; }\n\
                  impl<'a, T> Trait for &'a T { fn f(&self) -> i32 { 1 + 2 } }\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("arith.add_to_sub").unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());

    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(&*candidates[0].item_path, "<&'a T as Trait>::f");
}

/// A string literal is recognized by the source parser's lexer, not by the whitespace filter, so
/// its contents -- including a comment-shaped sequence and internal spaces -- pass through
/// `compact_path` untouched even though everything around it is compacted.
#[test]
fn compact_path_steps_over_a_string_literal_intact() {
    let compacted = compact_path(r#"Trait < "a /* not a comment */ b" >"#);

    assert_eq!(compacted, r#"Trait<"a /* not a comment */ b">"#);
}
