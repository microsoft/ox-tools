// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::Range;

use compact_str::CompactString;
use syn::parse_file;

use super::*;
use crate::cfg::CfgSet;
use crate::model::{MutantId, SiteIndex, mutant_id_with_discriminator, normalize_site_text};
use crate::ops::registry::{REGISTRY, Selection};
use crate::parse::SourceFile;
use crate::schema::{AssignedMutant, Ordinal, instrument};

fn candidates(source: &str, ops: &str) -> Vec<Candidate> {
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse(ops).unwrap();

    collect(&file, &selection)
}

fn mutators(source: &str, ops: &str) -> Vec<&'static str> {
    candidates(source, ops).into_iter().map(|c| c.mutator).collect()
}

fn with_errors(source: &str, errors: &[&str]) -> Vec<Candidate> {
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let mut selection = Selection::empty();

    selection.set_errors(errors.iter().map(|e| (*e).to_owned()).collect());
    collect(&file, &selection)
}

/// Two replacements offered at one site must be told apart by their identifiers.
///
/// `replacement_index` is the only thing distinguishing them: same file, same item, same
/// mutator, same site text, same occurrence. It reaches `mutant_id`, and an id is not a
/// cosmetic label — suppressions name mutants by id, and incremental execution decides what it has
/// already resolved by id. Collapsing the index makes two genuinely different mutants share
/// one identity, so a suppression aimed at either silences both and an incremental run credits a
/// verdict to a mutant that never earned it.
#[test]
fn two_replacements_at_one_site_get_distinct_identifiers() {
    let found = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &["MyError::Io", "MyError::Eof"]);

    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].span, found[1].span, "the premise is that only the index differs");
    assert_eq!(found[0].mutator, found[1].mutator);
    assert_ne!(found[0].replacement_index, found[1].replacement_index);

    let file = SourceFile::parse("test.rs", "fn f() -> Result<i32, MyError> { Ok(1) }".to_owned()).unwrap();
    let mutants = into_definitions(&file, found);

    assert_ne!(mutants[0].id, mutants[1].id, "two mutants sharing an id cannot be suppressed apart");
}

/// Multiple replacements targeting the same source span share one `MutationSite` allocation.
#[test]
fn replacements_at_same_span_share_a_site_allocation() {
    let found = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &["MyError::Io", "MyError::Eof"]);

    assert!(found.len() >= 2);
    assert_eq!(found[0].span, found[1].span, "premise: same span");

    let file = SourceFile::parse("test.rs", "fn f() -> Result<i32, MyError> { Ok(1) }".to_owned()).unwrap();
    let mutants = into_definitions(&file, found);

    assert!(
        std::sync::Arc::ptr_eq(&mutants[0].site, &mutants[1].site),
        "two definitions at the same span must share one MutationSite"
    );
    assert_eq!(mutants[0].site.original, mutants[1].site.original);
    assert_eq!(mutants[0].site.line, mutants[1].site.line);
}

/// The number of replacements at one site does not consume occurrence numbers belonging to later
/// identical sites.
#[test]
fn occurrence_counts_sites_instead_of_replacements() {
    let item = "fn f() -> Result<i32, MyError> { Ok(1) }";
    let source = format!("{item}\n{item}");
    let file = SourceFile::parse("test.rs", source).unwrap();
    let first = with_errors(item, &["MyError::Io", "MyError::Eof"]);
    let mut second = first.clone();

    for candidate in &mut second {
        candidate.span = candidate.span.start + item.len() + 1..candidate.span.end + item.len() + 1;
    }

    let mutants = into_definitions(&file, first.into_iter().chain(second).collect());
    let occurrences: Vec<u32> = mutants.iter().map(|mutant| mutant.occurrence).collect();

    assert_eq!(occurrences, vec![0, 0, 1, 1]);
}

/// The cached per-span normalized text must produce exactly the identity a fresh, uncached
/// normalization of the same site's bytes would — for every mutant the site offers, not just the
/// first one to reach the cache.
#[test]
fn cached_normalization_matches_a_direct_recomputation_for_every_replacement_at_a_site() {
    let source = "fn f() -> Result<i32, MyError> { Ok(1) }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let found = with_errors(source, &["MyError::Io", "MyError::Eof", "MyError::Closed"]);

    // Independently recomputed, from the same source bytes, with no cache in the loop.
    let expected: Vec<MutantId> = found
        .iter()
        .map(|candidate| {
            let text = file.slice(&candidate.span);
            let normalized = normalize_site_text(text);

            mutant_id_with_discriminator(
                &file.path,
                &candidate.item_path,
                candidate.mutator,
                &normalized,
                SiteIndex::new(0, candidate.replacement_index),
                (candidate.mutator == "fn_value.err_with").then_some(candidate.replacement.as_str()),
            )
        })
        .collect();

    let actual: Vec<MutantId> = into_definitions(&file, found).into_iter().map(|mutant| mutant.id).collect();

    assert_eq!(actual, expected);
}

#[test]
fn named_error_identity_changes_with_text_and_order() {
    let source = "fn f() -> Result<i32, MyError> { Ok(1) }";
    let identify = |errors: &[&str]| {
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();

        into_definitions(&file, with_errors(source, errors))
            .into_iter()
            .map(|mutant| (mutant.replacement.to_string(), mutant.id))
            .collect::<crate::HashMap<_, _>>()
    };
    let original = identify(&["MyError::Io", "MyError::Eof"]);
    let changed = identify(&["MyError::Io", "MyError::Closed"]);
    let reordered = identify(&["MyError::Eof", "MyError::Io"]);

    assert_ne!(original["Err(MyError::Eof)"], changed["Err(MyError::Closed)"]);
    assert_ne!(original["Err(MyError::Io)"], reordered["Err(MyError::Io)"]);
    assert_ne!(original["Err(MyError::Eof)"], reordered["Err(MyError::Eof)"]);
}

#[test]
fn each_named_error_value_becomes_its_own_mutant() {
    let found = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &["MyError::Io", "MyError::Eof"]);

    let replacements: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

    assert_eq!(replacements, vec!["Err(MyError::Io)", "Err(MyError::Eof)"]);
    assert!(found.iter().all(|c| c.mutator == "fn_value.err_with"));
}

#[test]
fn named_error_values_only_reach_functions_returning_result() {
    let found = with_errors("fn f() -> i32 { 1 }", &["MyError::Io"]);

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn naming_no_error_values_produces_no_error_mutants() {
    let found = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &[]);

    assert!(found.is_empty(), "{found:?}");
}

/// One operator's source, the selection that reaches it, and the exact replacements it must emit.
type OperatorCase = (&'static str, &'static str, &'static [(&'static str, &'static str)]);

/// Every binary and compound-assignment operator, paired with the exact `(mutator, replacement)`
/// set it must produce.
///
/// The replacement is the text of the *whole* expression, which is why the operands appear: a
/// mutant is a `(span, text)` pair over the entire construct, not a patched operator token.
const OPERATOR_ORACLE: &[OperatorCase] = &[
    (
        "fn f(a: i32, b: i32) -> bool { a < b }",
        "relational",
        &[("relational.lt_to_le", "(a) <= (b)"), ("relational.lt_to_gt", "(a) > (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> bool { a <= b }",
        "relational",
        &[("relational.le_to_lt", "(a) < (b)"), ("relational.le_to_ge", "(a) >= (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> bool { a > b }",
        "relational",
        &[("relational.gt_to_ge", "(a) >= (b)"), ("relational.gt_to_lt", "(a) < (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> bool { a >= b }",
        "relational",
        &[("relational.ge_to_gt", "(a) > (b)"), ("relational.ge_to_le", "(a) <= (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> bool { a == b }",
        "relational",
        &[("relational.eq_to_ne", "(a) != (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> bool { a != b }",
        "relational",
        &[("relational.ne_to_eq", "(a) == (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a + b }",
        "arith",
        &[("arith.add_to_sub", "(a) - (b)"), ("arith.add_to_mul", "(a) * (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a - b }",
        "arith",
        &[("arith.sub_to_add", "(a) + (b)"), ("arith.sub_to_div", "(a) / (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a * b }",
        "arith",
        &[("arith.mul_to_div", "(a) / (b)"), ("arith.mul_to_add", "(a) + (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a / b }",
        "arith",
        &[("arith.div_to_mul", "(a) * (b)"), ("arith.div_to_rem", "(a) % (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a % b }",
        "arith",
        &[("arith.rem_to_div", "(a) / (b)"), ("arith.rem_to_mul", "(a) * (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a & b }",
        "bitwise",
        &[("bitwise.and_to_or", "(a) | (b)"), ("bitwise.and_to_xor", "(a) ^ (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a | b }",
        "bitwise",
        &[("bitwise.or_to_and", "(a) & (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a ^ b }",
        "bitwise",
        &[("bitwise.xor_to_and", "(a) & (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a << b }",
        "shift",
        &[("shift.shl_to_shr", "(a) >> (b)")],
    ),
    (
        "fn f(a: i32, b: i32) -> i32 { a >> b }",
        "shift",
        &[("shift.shr_to_shl", "(a) << (b)")],
    ),
    (
        "fn f(x: bool, y: bool) -> bool { x && y }",
        "logical",
        &[("logical.and_to_or", "(x) || (y)")],
    ),
    (
        "fn f(x: bool, y: bool) -> bool { x || y }",
        "logical",
        &[("logical.or_to_and", "(x) && (y)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a += b; }",
        "assign",
        &[("assign.add_to_sub", "*a -= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a -= b; }",
        "assign",
        &[("assign.sub_to_add", "*a += (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a *= b; }",
        "assign",
        &[("assign.mul_to_div", "*a /= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a /= b; }",
        "assign",
        &[("assign.div_to_mul", "*a *= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a %= b; }",
        "assign",
        &[("assign.rem_to_div", "*a /= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a &= b; }",
        "assign",
        &[("assign.and_to_or", "*a |= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a |= b; }",
        "assign",
        &[("assign.or_to_and", "*a &= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a ^= b; }",
        "assign",
        &[("assign.xor_to_and", "*a &= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a <<= b; }",
        "assign",
        &[("assign.shl_to_shr", "*a >>= (b)")],
    ),
    (
        "fn f(a: &mut i32, b: i32) { *a >>= b; }",
        "assign",
        &[("assign.shr_to_shl", "*a <<= (b)")],
    ),
];

/// The mutation operators are the product, so the token each one emits is part of its contract and
/// not an implementation detail.
///
/// Checking these families by mutator *name* alone — which is all that happened before — lets a
/// wrong or swapped replacement token through untouched: the tool would emit a `<=` mutant, label
/// it `lt_to_gt`, and report a survivor against an edit it never made. Worse, a swap can collapse
/// two entries onto the same token, silently turning two mutants into one duplicate. The oracle is
/// therefore the exact `(mutator, replacement)` set, asserted for every entry in the table.
#[test]
fn every_binary_and_compound_assignment_operator_emits_the_replacement_its_name_promises() {
    for (source, ops, expected) in OPERATOR_ORACLE {
        let mut found: Vec<(&str, String)> = candidates(source, ops)
            .into_iter()
            .map(|candidate| (candidate.mutator, candidate.replacement.to_string()))
            .collect();

        let mut expected: Vec<(&str, String)> = expected
            .iter()
            .map(|(mutator, replacement)| (*mutator, (*replacement).to_owned()))
            .collect();

        found.sort_unstable();
        expected.sort_unstable();

        assert_eq!(found, expected, "{source}");
    }
}

/// The table is only worth pinning if it is the whole table: an entry deleted from
/// `binary_replacements` would otherwise vanish from both the code and its oracle at once.
#[test]
fn the_operator_oracle_covers_every_replacement_the_tables_offer() {
    let pairs: usize = OPERATOR_ORACLE.iter().map(|(_, _, expected)| expected.len()).sum();

    assert_eq!(OPERATOR_ORACLE.len(), 28, "one row per binary and compound-assignment operator");
    assert_eq!(pairs, 38, "one assertion per `binary_replacements` entry");
}

#[test]
fn unselected_mutators_produce_nothing() {
    assert!(mutators("fn f(a: i32, b: i32) -> bool { a < b }", "arith").is_empty());
}

#[test]
fn spans_cover_the_whole_binary_expression() {
    let source = "fn f(a: i32, b: i32) -> bool { a < b }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let found = collect(&file, &Selection::parse("relational").unwrap());

    assert_eq!(file.slice(&found[0].span), "a < b");
}

#[test]
fn nested_binary_expressions_are_all_found() {
    let found = mutators("fn f(a: i32, b: i32, c: i32) -> i32 { a + b * c }", "arith");

    assert!(found.contains(&"arith.add_to_sub"));
    assert!(found.contains(&"arith.mul_to_div"));
}

#[test]
fn candidates_come_back_in_source_order() {
    let source = "fn f(a: i32, b: i32) -> i32 { let x = a - b; x * a }";
    let found = candidates(source, "arith");

    for pair in found.windows(2) {
        assert!(pair.len() > 1);
        assert!(pair[0].span.start <= pair[1].span.start);
    }
}

#[test]
fn the_largest_integer_literal_does_not_overflow() {
    // `i64::MAX` has no increment. Computing one unchecked panics in a debug build and wraps to
    // `i64::MIN` in a release build, which would offer a "+1" mutant that is smaller than the
    // literal it replaces.
    let found = mutators("fn f() -> i64 { 9223372036854775807 }", "literal");

    assert!(found.contains(&"literal.int_decrement"), "{found:?}");
    assert!(!found.contains(&"literal.int_increment"), "{found:?}");
}

#[test]
fn item_paths_include_the_enclosing_function() {
    let found = candidates("fn outer(a: i32) -> i32 { a + 1 }", "arith.add_to_sub");

    assert_eq!(&*found[0].item_path, "outer");
}

#[test]
fn item_paths_include_module_impl_and_method() {
    let source = "mod m { struct S; impl S { fn go(&self, a: i32) -> i32 { a + 1 } } }";
    let found = candidates(source, "arith.add_to_sub");

    assert_eq!(&*found[0].item_path, "m::S::go");
    assert!(found[0].trait_impl.is_none());
}

#[test]
fn item_paths_distinguish_trait_defaults_from_each_other_and_free_functions() {
    let source = "fn f() -> i32 { 1 + 2 }
        trait First { fn f() -> i32 { 1 + 2 } }
        trait Second { fn f() -> i32 { 1 + 2 } }";
    let found = candidates(source, "arith.add_to_sub");
    let mut paths: Vec<&str> = found.iter().map(|candidate| &*candidate.item_path).collect();

    paths.sort_unstable();

    assert_eq!(paths, vec!["First::f", "Second::f", "f"]);
}

#[test]
fn impl_paths_retain_qualification_generics_and_references() {
    let source = "struct S;
        impl a::S { fn go(&self) -> i32 { 1 + 1 } }
        impl b::S { fn go(&self) -> i32 { 2 + 2 } }
        impl S<u8> { fn go(&self) -> i32 { 3 + 3 } }
        impl S<u16> { fn go(&self) -> i32 { 4 + 4 } }
        impl S { fn go(&self) -> i32 { 5 + 5 } }
        trait Go { fn go(&self) -> i32; }
        impl Go for &S { fn go(&self) -> i32 { 6 + 6 } }
        impl Go for S { fn go(&self) -> i32 { 7 + 7 } }";
    let found = candidates(source, "arith.add_to_sub");
    let paths: Vec<&str> = found.iter().map(|candidate| &*candidate.item_path).collect();

    assert_eq!(
        paths,
        vec![
            "a::S::go",
            "b::S::go",
            "S<u8>::go",
            "S<u16>::go",
            "S::go",
            "<&S as Go>::go",
            "<S as Go>::go",
        ]
    );
}

#[test]
fn impl_identities_survive_reordering_across_qualified_instantiated_and_reference_self_types() {
    let first = "impl a::S { fn go(&self) -> i32 { 1 + 1 } }
        impl S<u8> { fn go(&self) -> i32 { 1 + 1 } }
        impl S<u16> { fn go(&self) -> i32 { 1 + 1 } }
        trait Go { fn go(&self) -> i32; }
        impl Go for &S { fn go(&self) -> i32 { 1 + 1 } }
        impl Go for S { fn go(&self) -> i32 { 1 + 1 } }";
    let second = "trait Go { fn go(&self) -> i32; }
        impl Go for S { fn go(&self) -> i32 { 1 + 1 } }
        impl Go for &S { fn go(&self) -> i32 { 1 + 1 } }
        impl S<u16> { fn go(&self) -> i32 { 1 + 1 } }
        impl S<u8> { fn go(&self) -> i32 { 1 + 1 } }
        impl a::S { fn go(&self) -> i32 { 1 + 1 } }";

    let identities = |source: &str| {
        let file = SourceFile::parse("test.rs", source.to_owned()).expect("the fixture parses");
        let selection = Selection::parse("arith.add_to_sub").expect("the mutator exists");
        let mut ids: Vec<(String, String)> = into_definitions(&file, collect(&file, &selection))
            .into_iter()
            .map(|mutant| (mutant.item_path.to_string(), mutant.id.to_string()))
            .collect();

        ids.sort_unstable();
        ids
    };

    let before = identities(first);
    let after = identities(second);

    assert_eq!(
        before, after,
        "every qualified, instantiated, or reference self type must keep its own id regardless of \
         impl block order"
    );
    assert_eq!(before.len(), 5, "each distinct self type must produce its own entry, not share one");
}

#[test]
fn impl_identities_preserve_word_boundaries_when_blocks_are_reordered() {
    let first = "trait Marker {}
        trait Subject { fn f(&self) -> i32; }
        struct dynMarker;
        impl Subject for dyn Marker { fn f(&self) -> i32 { 1 + 1 } }
        impl Subject for dynMarker { fn f(&self) -> i32 { 1 + 1 } }";
    let second = "trait Marker {}
        trait Subject { fn f(&self) -> i32; }
        struct dynMarker;
        impl Subject for dynMarker { fn f(&self) -> i32 { 1 + 1 } }
        impl Subject for dyn Marker { fn f(&self) -> i32 { 1 + 1 } }";

    let identities = |source: &str| {
        let file = SourceFile::parse("test.rs", source.to_owned()).expect("the fixture parses");
        let selection = Selection::parse("arith.add_to_sub").expect("the mutator exists");
        let mut ids: Vec<(String, String)> = into_definitions(&file, collect(&file, &selection))
            .into_iter()
            .map(|mutant| (mutant.item_path.to_string(), mutant.id.to_string()))
            .collect();

        ids.sort_unstable();
        ids
    };

    let before = identities(first);

    assert_eq!(before, identities(second));
    assert_ne!(before[0].0, before[1].0);
    assert_ne!(before[0].1, before[1].1);
}

#[test]
fn trait_implementation_paths_keep_identical_methods_stable_when_reordered() {
    let first = "struct S;
        trait First { fn f(&self) -> i32; }
        trait Second { fn f(&self) -> i32; }
        impl S { fn f(&self) -> i32 { 1 + 2 } }
        impl First for S { fn f(&self) -> i32 { 1 + 2 } }
        impl Second for S { fn f(&self) -> i32 { 1 + 2 } }";
    let second = "struct S;
        trait First { fn f(&self) -> i32; }
        trait Second { fn f(&self) -> i32; }
        impl S { fn f(&self) -> i32 { 1 + 2 } }
        impl Second for S { fn f(&self) -> i32 { 1 + 2 } }
        impl First for S { fn f(&self) -> i32 { 1 + 2 } }";

    let identities = |source: &str| {
        let file = SourceFile::parse("test.rs", source.to_owned()).expect("the fixture parses");
        let selection = Selection::parse("arith.add_to_sub").expect("the mutator exists");
        let mut ids: Vec<(String, String)> = into_definitions(&file, collect(&file, &selection))
            .into_iter()
            .map(|mutant| (mutant.item_path.to_string(), mutant.id.to_string()))
            .collect();

        ids.sort_unstable();
        ids
    };

    let before = identities(first);
    let after = identities(second);
    let paths: Vec<&str> = before.iter().map(|(path, _id)| path.as_str()).collect();

    assert_eq!(before, after, "each trait method keeps its id when implementation order changes");
    assert!(paths.contains(&"S::f"), "{paths:?}");
    assert!(paths.contains(&"<S as First>::f"), "{paths:?}");
    assert!(paths.contains(&"<S as Second>::f"), "{paths:?}");
}

#[test]
fn trait_implementations_record_the_terminal_trait_name() {
    let source = "struct A; struct B; struct C;
        impl Debug for A { fn fmt(&self) -> i32 { 1 + 1 } }
        impl fmt::Debug for B { fn fmt(&self) -> i32 { 2 + 2 } }
        impl core::fmt::Debug for C { fn fmt(&self) -> i32 { 3 + 3 } }
        fn outside() -> i32 { 4 + 4 }";
    let found = candidates(source, "arith.add_to_sub");

    assert_eq!(found.len(), 4);
    assert!(found[..3].iter().all(|candidate| candidate.trait_impl.as_deref() == Some("Debug")));
    assert!(found[3].trait_impl.is_none(), "trait context must not escape its implementation");
}

#[test]
fn test_functions_are_not_mutated() {
    let source = "#[test] fn t() { assert_eq!(1 + 1, 2); }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn test_impl_methods_are_not_mutated() {
    let source = "struct S; impl S { #[test] fn t(&self) { let _ = 1 + 1; } }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn cfg_test_modules_are_not_mutated() {
    let source = "#[cfg(test)] mod tests { fn helper(a: i32) -> i32 { a + 1 } }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn non_test_cfg_modules_are_mutated() {
    let source = "#[cfg(unix)] mod platform { fn helper(a: i32) -> i32 { a + 1 } }";

    assert_eq!(candidates(source, "arith.add_to_sub").len(), 1);
}

#[test]
fn a_compound_gate_that_only_a_test_build_satisfies_is_not_mutated() {
    // The parser this replaced looked only at the top level of the predicate, so `all(test, unix)`
    // read as an ordinary platform gate and the test helper below it entered the population.
    for source in [
        "#[cfg(all(test, unix))] mod tests { fn helper(a: i32) -> i32 { a + 1 } }",
        "#[cfg(all(unix, all(test, feature = \"x\")))] mod tests { fn helper(a: i32) -> i32 { a + 1 } }",
        "#[cfg(not(feature = \"x\"))] #[cfg(test)] mod tests { fn helper(a: i32) -> i32 { a + 1 } }",
    ] {
        assert!(candidates(source, "arith").is_empty(), "{source}");
    }
}

#[test]
fn a_gate_a_production_build_can_also_satisfy_is_still_mutated() {
    // `any(test, feature = "…")` holds whenever the feature does, so this code is compiled into the
    // library the run measures. Reading the bare `test` as decisive would drop every mutant in it.
    let source = "#[cfg(any(test, feature = \"runtime\"))] mod support { fn helper(a: i32) -> i32 { a + 1 } }";

    assert_eq!(candidates(source, "arith.add_to_sub").len(), 1);
}

#[test]
fn tokio_test_functions_are_not_mutated() {
    let source = "#[tokio::test] async fn t() { let _ = 1 + 1; }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn const_initializers_are_not_mutated() {
    // The encoding wraps expressions in an `if` over a function call, which const contexts
    // reject, so generating these would produce guaranteed compile failures.
    assert!(candidates("const N: i32 = 1 + 2;", "arith").is_empty());
    assert!(candidates("static N: i32 = 1 + 2;", "arith").is_empty());
}

#[test]
fn const_fn_bodies_are_not_mutated() {
    // Every expression inside a `const fn` is in a const context, not just the body value, so
    // the whole subtree has to stay inert. Mutating one of these compiles nowhere.
    assert!(candidates("const fn f(a: i32, b: i32) -> i32 { a + b }", "arith").is_empty());
    assert!(candidates("const fn f(a: usize, b: &[u8]) -> bool { a < b.len() }", "relational").is_empty());

    // A non-const function in the same file must still be mutated.
    let source = "const fn f(a: i32) -> i32 { a + 1 }\nfn g(a: i32) -> i32 { a + 2 }";

    assert!(!candidates(source, "arith").is_empty());
}

#[test]
fn array_lengths_in_types_are_not_mutated() {
    // `[u8; 200]` in a type is a const context, and the guard is a function call. This is not
    // the same position as the length in the *value* `[0u8; 32]`, which was already inert, and
    // the difference cost a real crate a build that could not compile and could not be blamed
    // on any one mutant.
    assert!(candidates("struct Pairs([u8; 200]);", "literal").is_empty());
    assert!(candidates("struct Pairs([u8; 100 * 2]);", "arith").is_empty());
    assert!(candidates("fn f() -> [u8; 4] { todo!() }", "literal").is_empty());
    assert!(candidates("fn f(a: [u8; 4]) -> usize { a.len() }", "literal").is_empty());

    // The element of an array *value* is an ordinary expression and must stay mutable; it is
    // only the length beside it that cannot hold a guard.
    assert!(!candidates("fn f() -> [u8; 4] { [7; 4] }", "literal").is_empty());
    assert!(candidates("type Row = [u8; 16];", "literal").is_empty());
}

#[test]
fn const_generic_arguments_are_not_mutated() {
    // Same reason as an array length: the argument is a const expression, and it can sit
    // arbitrarily deep inside a type.
    assert!(candidates("struct Grid(Matrix<3>);", "literal").is_empty());
    assert!(candidates("fn f() -> Wrapper<Inner<8>> { todo!() }", "literal").is_empty());
}

#[test]
fn a_value_beside_an_inert_type_is_still_mutated() {
    // Making types inert must not swallow the function they belong to; the array length is a
    // const context but the body around it is ordinary code.
    let source = "fn f(a: [u8; 4], b: i32) -> i32 { b + 1 }";

    assert!(!candidates(source, "arith").is_empty());
}

#[test]
fn macro_interiors_are_not_mutated() {
    let source = "fn f() { println!(\"{}\", 1 + 2); }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn if_conditions_can_be_negated() {
    let source = "fn f(a: bool) -> i32 { if a { 1 } else { 2 } }";
    let found = mutators(source, "cond.negate");

    assert_eq!(found, vec!["cond.negate"]);
}

#[test]
fn a_negated_condition_is_parenthesized() {
    // `!` binds tighter than any binary operator, so `!a == b` is a different expression that
    // usually does not even type-check.
    let source = "fn f(a: i32, b: i32) -> i32 { if a == b { 1 } else { 2 } }";
    let found = candidates(source, "cond.negate");

    assert_eq!(found[0].replacement, "!(a == b)");
}

#[test]
fn removing_a_unary_operator_leaves_the_operand() {
    let found = candidates("fn f(a: i32) -> i32 { -a }", "unary.remove_neg");

    assert_eq!(found[0].replacement, "a");
}

#[test]
fn if_let_conditions_are_left_alone() {
    let source = "fn f(a: Option<i32>) -> i32 { if let Some(x) = a { x } else { 2 } }";

    assert!(candidates(source, "cond").is_empty());
}

#[test]
fn while_conditions_can_be_negated() {
    let source = "fn f(mut a: i32) { while a > 0 { a -= 1; } }";
    let found = mutators(source, "cond.negate");

    assert_eq!(found, vec!["cond.negate"]);
}

#[test]
fn integer_literals_yield_boundary_replacements() {
    let found = mutators("fn f() -> i32 { 5 }", "literal");

    assert!(found.contains(&"literal.int_to_zero"));
    assert!(found.contains(&"literal.int_to_one"));
    assert!(found.contains(&"literal.int_increment"));
    assert!(found.contains(&"literal.int_decrement"));
}

#[test]
fn a_literal_zero_is_not_replaced_by_zero() {
    let found = mutators("fn f() -> i32 { 0 }", "literal");

    assert!(!found.contains(&"literal.int_to_zero"));
}

#[test]
fn a_literal_one_is_not_replaced_by_one() {
    let found = mutators("fn f() -> i32 { 1 }", "literal");

    assert!(!found.contains(&"literal.int_to_one"));
}

#[test]
fn an_error_type_from_another_crate_is_not_given_a_default() {
    for source in [
        "fn f() -> Result<u8, std::io::Error> { todo!() }",
        "use std::io; fn f() -> Result<u8, io::Error> { todo!() }",
        "use std::io::Error; fn f() -> Result<u8, Error> { todo!() }",
        "use anyhow::Error; fn f() -> Result<u8, Error> { todo!() }",
    ] {
        let found = candidates(source, "fn_value");
        let texts: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

        assert!(!texts.contains(&"Err(Default::default())"), "{source}: {texts:?}");
    }
}

#[test]
fn an_ok_is_not_flipped_to_an_error_the_signature_cannot_default() {
    let found = mutators("use std::io; fn f() -> Result<u8, io::Error> { Ok(1) }", "result");

    assert!(!found.contains(&"result.ok_to_err"), "{found:?}");
}

#[test]
fn an_ok_is_still_flipped_when_the_error_type_is_the_workspace_s_own() {
    let found = mutators("fn f() -> Result<u8, crate::Error> { Ok(1) }", "result");

    assert!(found.contains(&"result.ok_to_err"), "{found:?}");
}

#[test]
fn an_ok_inside_an_aliased_result_stays_optimistic() {
    let found = mutators("fn f() -> Result<u8> { Ok(1) }", "result");

    assert!(found.contains(&"result.ok_to_err"), "{found:?}");
}

#[test]
fn a_workspace_error_type_is_still_given_a_default() {
    for source in [
        "fn f() -> Result<u8, crate::Error> { todo!() }",
        "fn f() -> Result<u8, super::Error> { todo!() }",
        "fn f() -> Result<u8, MyError> { todo!() }",
    ] {
        let found = candidates(source, "fn_value");
        let texts: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

        assert!(texts.contains(&"Err(Default::default())"), "{source}: {texts:?}");
    }
}

#[test]
fn the_one_formatting_error_that_does_have_a_default_keeps_its_mutant() {
    for source in [
        "fn f() -> Result<u8, core::fmt::Error> { todo!() }",
        "use core::fmt; fn f() -> Result<u8, fmt::Error> { todo!() }",
        "use core::fmt::Error; fn f() -> Result<u8, Error> { todo!() }",
    ] {
        let found = candidates(source, "fn_value");
        let texts: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

        assert!(texts.contains(&"Err(Default::default())"), "{source}: {texts:?}");
    }
}

#[test]
fn a_small_literal_is_not_mutated_to_the_same_value_twice() {
    for source in ["fn f() -> i32 { 0 }", "fn f() -> i32 { 1 }", "fn f() -> i32 { 2 }"] {
        let found = candidates(source, "literal");
        let mut edits: Vec<(usize, usize, &str)> = found.iter().map(|c| (c.span.start, c.span.end, c.replacement.as_str())).collect();
        let count = edits.len();

        edits.sort_unstable();
        edits.dedup();
        assert_eq!(edits.len(), count, "{source} produced a duplicate edit");
    }
}

#[test]
fn a_collision_keeps_the_perturbation_rather_than_the_value() {
    let found = mutators("fn f() -> i32 { 0 }", "literal");

    assert!(found.contains(&"literal.int_increment"));
    assert!(!found.contains(&"literal.int_to_one"));
}

#[test]
fn a_collision_still_yields_a_mutant_when_only_the_value_mutator_is_selected() {
    let found = mutators("fn f() -> i32 { 0 }", "literal.int_to_one");

    assert_eq!(found, vec!["literal.int_to_one"]);
}

#[test]
fn increment_replacements_are_the_neighbouring_values() {
    let found = candidates("fn f() -> i32 { 5 }", "literal.int_increment,literal.int_decrement");
    let mut replacements: Vec<&str> = found.iter().map(|c| c.replacement.as_str()).collect();

    replacements.sort_unstable();
    assert_eq!(replacements, vec!["4", "6"]);
}

#[test]
fn an_explicitly_unsigned_zero_is_not_decremented_below_its_range() {
    assert!(candidates("fn f() -> u32 { 0u32 }", "literal.int_decrement").is_empty());
}

#[test]
fn an_unsuffixed_zero_keeps_its_decrement_candidate() {
    let found = candidates("fn f() -> i32 { 0 }", "literal.int_decrement");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].replacement, "-1");
}

#[test]
fn a_borrowed_literal_array_is_left_alone_so_it_can_still_be_promoted() {
    let source = "fn f(k: &str) -> Option<&'static [&'static str]> { Some(match k { \"a\" => &[\"id\", \"name\"], _ => return None }) }";
    let found = mutators(source, "literal");

    assert!(found.is_empty(), "promotable borrow was instrumented: {found:?}");
}

#[test]
fn a_borrowed_array_of_computed_values_is_still_mutated() {
    let found = mutators("fn f(n: i32) -> i32 { let v = &[n + 1, n * 2]; v[0] }", "arith");

    assert!(found.contains(&"arith.add_to_sub"));
}

#[test]
fn a_let_chain_condition_is_not_negated_or_replaced() {
    let source = "fn f(x: Option<i32>, y: bool) -> i32 { if let Some(n) = x && y { n } else { 0 } }";
    let found = mutators(source, "cond,logical");

    assert!(found.is_empty(), "let-chain condition was mutated: {found:?}");
}

#[test]
fn a_binding_at_the_end_of_a_let_chain_is_also_recognized() {
    let source = "fn f(x: Option<i32>, y: bool) -> i32 { if y && let Some(n) = x { n } else { 0 } }";
    let found = mutators(source, "cond,logical");

    assert!(found.is_empty(), "trailing let-chain binding was mutated: {found:?}");
}

#[test]
fn a_while_let_chain_condition_is_not_negated() {
    let source = "fn f(mut x: Option<i32>, y: bool) -> i32 { while let Some(_n) = x && y { x = None; } 0 }";
    let found = mutators(source, "cond");

    assert!(!found.contains(&"cond.negate"));
}

#[test]
fn an_ordinary_compound_condition_is_still_mutated() {
    let found = mutators(
        "fn f(a: bool, b: bool) -> bool { if a && b { true } else { false } }",
        "cond,logical",
    );

    assert!(found.contains(&"cond.negate"));
    assert!(found.contains(&"logical.and_to_or"));
}

#[test]
fn an_empty_string_literal_is_not_replaced_by_an_empty_string() {
    let found = mutators("fn f() -> &'static str { \"\" }", "literal");

    assert!(!found.contains(&"literal.str_to_empty"));
    assert!(found.contains(&"literal.str_to_xyzzy"));
}

#[test]
fn the_marker_string_is_not_replaced_by_itself() {
    // The mutant would be the original program, so it could never be killed and would be
    // reported as a survivor on every run.
    let found = mutators("fn f() -> &'static str { \"xyzzy\" }", "literal");

    assert!(!found.contains(&"literal.str_to_xyzzy"), "{found:?}");
    assert!(found.contains(&"literal.str_to_empty"), "{found:?}");
}

#[test]
fn a_panic_message_is_not_rewritten() {
    let found = mutators("fn f(x: Option<i32>) -> i32 { x.expect(\"x must be present\") }", "literal");

    assert!(!found.contains(&"literal.str_to_empty"), "{found:?}");
    assert!(!found.contains(&"literal.str_to_xyzzy"), "{found:?}");
}

#[test]
fn an_expected_error_message_is_not_rewritten() {
    let found = mutators("fn f(x: Result<i32, String>) -> String { x.expect_err(\"must fail\") }", "literal");

    assert!(!found.contains(&"literal.str_to_xyzzy"), "{found:?}");
}

#[test]
fn a_value_a_panic_message_is_asked_of_is_still_mutated() {
    // Only the message is exempt. The receiver is ordinary code and stays in the population.
    let found = mutators(
        "fn f(x: &str) -> i32 { x.strip_prefix(\"go\").expect(\"prefixed\").len() as i32 }",
        "literal",
    );

    assert!(found.contains(&"literal.str_to_xyzzy"), "{found:?}");
}

#[test]
fn a_call_that_merely_shares_the_name_keeps_its_arguments() {
    // `expect` with any other arity is somebody else's method, not the standard library's.
    let found = mutators(
        "struct S; impl S { fn expect(&self, _a: &str, _b: &str) {} } fn f(s: S) { s.expect(\"one\", \"two\") }",
        "literal",
    );

    assert!(found.contains(&"literal.str_to_xyzzy"), "{found:?}");
}

#[test]
fn a_condition_that_is_already_a_literal_is_not_replaced_by_that_literal() {
    let found = mutators("fn f() -> i32 { if true { 1 } else { 2 } }", "cond");

    assert!(!found.contains(&"cond.always_true"), "{found:?}");
    assert!(found.contains(&"cond.always_false"), "{found:?}");

    let found = mutators("fn f() -> i32 { if false { 1 } else { 2 } }", "cond");

    assert!(found.contains(&"cond.always_true"), "{found:?}");
    assert!(!found.contains(&"cond.always_false"), "{found:?}");
}

#[test]
fn negated_integer_zero_has_no_mutant_but_negative_float_zero_does() {
    assert!(candidates("fn f() -> i32 { -0 }", "unary.remove_neg").is_empty());
    assert!(!candidates("fn f() -> f64 { -0.0 }", "unary.remove_neg").is_empty());
    assert!(!candidates("fn f() -> i32 { -1 }", "unary.remove_neg").is_empty());
}

#[test]
fn a_parenthesised_zero_is_still_recognised_under_negation() {
    // `-(0)` is exactly as much its own negation as `-0` is; a reader would not expect the
    // redundant parentheses to turn a no-op mutant into a real one.
    assert!(candidates("fn f() -> i32 { -(0) }", "unary.remove_neg").is_empty());
}

#[test]
fn associated_const_initializers_are_not_mutated() {
    // A guard cannot be called in a const-evaluation context, so the mutant would not compile.
    let source = "struct S; impl S { const N: i32 = 1 + 2; }";

    assert!(candidates(source, "").is_empty(), "{:?}", candidates(source, ""));
}

#[test]
fn trait_const_defaults_are_not_mutated() {
    let source = "trait T { const N: i32 = 1 + 2; }";

    assert!(candidates(source, "").is_empty(), "{:?}", candidates(source, ""));
}

#[test]
fn a_string_returning_function_gets_an_owned_marker() {
    // `"xyzzy"` is a `&'static str`, so a `String`-returning function needs the owned form or
    // every one of these mutants is withdrawn as unviable.
    let found = candidates("fn f() -> String { String::new() }", "fn_value.xyzzy_string");

    assert_eq!(found[0].replacement, "\"xyzzy\".to_owned()");
}

#[test]
fn booleans_flip_to_the_other_value() {
    let found = candidates("fn f() -> bool { true }", "literal.bool_flip");

    assert_eq!(found[0].replacement, "false");
}

#[test]
fn compound_assignment_is_mutated() {
    let found = mutators("fn f(a: &mut i32) { *a += 1; }", "assign");

    assert_eq!(found, vec!["assign.add_to_sub"]);
}

#[test]
fn unary_operators_can_be_removed() {
    let found = mutators("fn f(a: i32) -> i32 { -a }", "unary");

    assert_eq!(found, vec!["unary.remove_neg"]);
}

#[test]
fn logical_not_can_be_removed() {
    let found = candidates("fn f(a: bool) -> bool { !a }", "unary.remove_not");

    assert_eq!(found[0].replacement, "a");
}

#[test]
fn statement_deletion_covers_calls_assignments_and_ignored_statements() {
    let source = "fn f(v: &mut Vec<i32>, mut a: i32) {
        v.push(1);
        a = 2;
        a += 3;
        a + 4;
    }";
    let found = mutators(source, "stmt");

    assert!(found.contains(&"stmt.delete_call"));
    assert_eq!(found.iter().filter(|name| **name == "stmt.delete_assign").count(), 2);
}

/// A `let` with no value makes the assignment that settles it load-bearing, and deleting it
/// produces a mutant that cannot compile. The error rustc raises, E0381, is reported at the
/// binding's first *use* rather than at the deleted statement, so withdrawal cannot match the
/// diagnostic to the mutant that caused it and abandons the entire run. The assignment must
/// therefore never become a candidate in the first place.
#[test]
fn the_assignment_settling_a_deferred_let_is_not_deletable() {
    let source = "fn f(flag: bool) -> i32 {
        let scanned;
        if flag {
            scanned = 1;
        } else {
            scanned = 2;
        }
        scanned
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert!(found.is_empty(), "{found:?}");
}

/// The exemption is about the binding being empty, not about the name. A `let` that supplies a
/// value leaves its later assignments as ordinary overwrites, which a test that reads the
/// variable afterwards should notice going missing.
#[test]
fn assigning_to_an_initialised_binding_stays_deletable() {
    let source = "fn f() -> i32 {
        let mut scanned = 0;
        scanned = 1;
        scanned
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert_eq!(found, vec!["stmt.delete_assign"]);
}

/// Shadowing re-uses the name for a different binding, and the new one is initialised. Because
/// declarations are recorded in source order, the second `let` must lift the exemption the
/// first one installed rather than leaving the name exempt for the rest of the function.
#[test]
fn a_later_initialised_let_lifts_the_deferral_for_that_name() {
    let source = "fn f() -> i32 {
        let scanned;
        scanned = 1;
        let mut scanned = scanned;
        scanned = 2;
        scanned
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert_eq!(found, vec!["stmt.delete_assign"]);
}

/// Deferred names are per-function: a nested function cannot see the outer one's locals, so an
/// assignment to its own fully-initialised binding must not inherit the outer exemption.
#[test]
fn a_nested_function_does_not_inherit_deferred_names() {
    let source = "fn outer() -> i32 {
        let scanned;
        scanned = 1;
        fn inner() -> i32 {
            let mut scanned = 0;
            scanned = 5;
            scanned
        }
        scanned + inner()
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert_eq!(found, vec!["stmt.delete_assign"]);
}

/// The exemption is keyed on a bare name because only a bare name can be what a deferred `let`
/// is waiting on. Assigning through a field presupposes the binding it belongs to already holds
/// a value, so that assignment carries no such load-bearing weight and stays an ordinary
/// candidate a test should notice the loss of.
#[test]
fn assigning_through_a_field_is_never_treated_as_settling_a_deferred_let() {
    let source = "fn f() -> i32 {
        let mut pair = (0, 0);
        pair.0 = 1;
        pair.0
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert_eq!(found, vec!["stmt.delete_assign"]);
}

/// Leaving a block restores the type evidence a shadow inside it displaced. `total` is a
/// `String` inside the block and the numeric parameter after it, so the perturbation family is
/// offered on the post-block use and withheld on the in-block one. If the inner shadow outlived
/// the block, the later use would be judged a `String` and lose its perturbation — the reverse
/// of the failure the block scoping exists to prevent, and just as wrong.
#[test]
fn a_block_shadow_of_a_binding_does_not_outlive_the_block() {
    let source = "fn f(total: i32) { { let total: String = String::new(); sink(total); } sink(total); }";
    let found = mutators(source, "expr");

    assert_eq!(
        found.iter().filter(|name| **name == "expr.increment").count(),
        1,
        "only the post-block use of the numeric `total` should be perturbed: {found:?}"
    );
    assert_eq!(found.iter().filter(|name| **name == "expr.decrement").count(), 1, "{found:?}");
}

/// Leaving a block also restores the deferral state it changed. The first block re-declares
/// `scanned` with a value, lifting the deferral inside itself; the second then settles the
/// still-deferred outer `scanned`, an assignment that must stay exempt from deletion. If the
/// first block's removal outlived it, the outer `scanned` would look initialised and the
/// settling assignment would wrongly become a `stmt.delete_assign` candidate.
#[test]
fn a_block_change_to_a_deferral_does_not_outlive_the_block() {
    let source = "fn f() -> i32 {
        let scanned;
        { let scanned = 0; let _ = scanned; }
        { scanned = 1; let _ = scanned; }
        scanned
    }";
    let found = mutators(source, "stmt.delete_assign");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn repeat_lengths_and_enum_discriminants_are_const_contexts() {
    let source = "enum E { A = 1 + 2 } fn f(n: i32) { let _ = [n + 1; 2 + 3]; }";
    let found = mutators(source, "arith");

    assert!(found.contains(&"arith.add_to_sub"), "{found:?}");
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn trait_default_methods_are_mutated_but_excluded_ones_are_not() {
    let source = "trait T {
        fn f(&self) -> i32 { 1 }
        #[cfg(test)]
        fn helper(&self) -> i32 { 2 }
    }";
    let found = candidates(source, "fn_value.zero,literal.int_to_zero");

    assert!(found.iter().any(|candidate| &*candidate.item_path == "T::f"));
    assert!(found.iter().all(|candidate| &*candidate.item_path != "T::helper"));
}

#[test]
fn const_trait_defaults_are_left_inert() {
    let source = "trait T { const fn f(&self) -> i32 { 1 + 2 } }";

    assert!(candidates(source, "arith,fn_value").is_empty());
}

#[test]
fn impl_paths_handle_reference_and_non_path_self_types() {
    let source = "trait T { fn f(&self) -> i32; }
        struct S;
        impl T for &S { fn f(&self) -> i32 { 1 + 1 } }
        impl T for (S,) { fn f(&self) -> i32 { 2 + 2 } }";
    let found = candidates(source, "arith.add_to_sub");
    let paths: Vec<&str> = found.iter().map(|candidate| &*candidate.item_path).collect();

    assert!(paths.contains(&"<&S as T>::f"), "{paths:?}");
    assert!(paths.contains(&"<(S,) as T>::f"), "{paths:?}");
}

#[test]
fn borrowed_promotable_shapes_are_classified_without_touching_unary_minus() {
    let source = "fn f() -> &'static (i32, [i32; 2], &'static i32, i32) {
        &((-1), [0; 2], &3, 4)
    }";
    let found = mutators(source, "literal,unary");

    assert!(!found.contains(&"unary.remove_neg"), "{found:?}");
}

#[test]
fn function_value_replacements_cover_return_type_shapes() {
    let source = "fn unit() { work(); }
        fn explicit_unit() -> () { work(); }
        fn unsigned() -> usize { 3 }
        fn float() -> f64 { 3.0 }
        fn owned_string() -> String { make() }
        fn vec_deque() -> std::collections::VecDeque<i32> { make() }
        fn reference(x: &i32) -> &i32 { x }
        fn array() -> [i32; 1] { [1] }
        fn unknown() -> Custom { make() }";
    let found = mutators(source, "fn_value");

    for expected in [
        "fn_value.unit",
        "fn_value.zero",
        "fn_value.one",
        "fn_value.minus_one",
        "fn_value.empty_string",
        "fn_value.xyzzy_string",
        "fn_value.empty_collection",
        "fn_value.one_element",
        "fn_value.default",
    ] {
        assert!(found.contains(&expected), "{expected} not in {found:?}");
    }
}

#[test]
fn a_map_return_offers_an_empty_map_and_a_single_pairing() {
    // A map's element is a pair, so unlike an ordinary collection the one-element form has to
    // supply both a key and a value; without a test naming a `HashMap` return type directly,
    // nothing would ever exercise the branch that builds that pair rather than a bare element.
    let source = "fn f() -> std::collections::HashMap<i32, bool> { make() }";
    let found = candidates(source, "fn_value");
    let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

    assert!(texts.iter().any(|text| text.contains("HashMap::new()")), "{texts:?}");
    assert!(
        texts
            .iter()
            .any(|text| text.contains("core::iter::once((") && text.contains(").collect()")),
        "{texts:?}"
    );
}

#[test]
fn a_map_whose_key_is_a_type_the_caller_chooses_offers_only_the_empty_map() {
    // When nothing is known about the key, there is no value to pair with one that is, and the
    // one-element form has to be withheld entirely rather than invented with half a pair
    // missing; a `Default::default()` key would compile only by accident of the type the
    // caller happened to choose.
    let source = "fn f<D>() -> std::collections::HashMap<D, i32> { make() }";
    let found = candidates(source, "fn_value");
    let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

    assert!(texts.iter().any(|text| text.contains("HashMap::new()")), "{texts:?}");
    assert!(!texts.iter().any(|text| text.contains("core::iter::once")), "{texts:?}");
}

#[test]
fn unselected_function_values_are_filtered_at_emit_time() {
    let found = candidates("fn f() -> i32 { 2 }", "fn_value.one");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].mutator, "fn_value.one");
}

#[test]
fn literals_without_literal_mutators_are_ignored() {
    let found = candidates("fn f() -> char { 'x' }", "literal");

    assert!(found.is_empty());
}

#[test]
fn huge_integer_literals_skip_neighbour_replacements() {
    let found = mutators("fn f() -> u128 { 340282366920938463463374607431768211455 }", "literal");

    assert!(found.contains(&"literal.int_to_zero"));
    assert!(!found.contains(&"literal.int_increment"));
    assert!(!found.contains(&"literal.int_decrement"));
}

#[test]
fn ids_are_stable_across_reformatting() {
    let compact = "fn f(a: i32, b: i32) -> bool { a < b }";
    let spaced = "fn f(a: i32, b: i32) -> bool {\n\n    a  <  b\n\n}\n";

    let left = SourceFile::parse("test.rs", compact.to_owned()).unwrap();
    let right = SourceFile::parse("test.rs", spaced.to_owned()).unwrap();
    let selection = Selection::parse("relational").unwrap();

    let left_ids: Vec<String> = into_definitions(&left, collect(&left, &selection))
        .into_iter()
        .map(|m| m.id.to_string())
        .collect();

    let right_ids: Vec<String> = into_definitions(&right, collect(&right, &selection))
        .into_iter()
        .map(|m| m.id.to_string())
        .collect();

    assert_eq!(left_ids, right_ids);
}

#[test]
fn ids_survive_a_line_inserted_above() {
    let before = "fn f(a: i32, b: i32) -> bool { a < b }";
    let after = "// a new comment\n\nfn f(a: i32, b: i32) -> bool { a < b }";

    let left = SourceFile::parse("test.rs", before.to_owned()).unwrap();
    let right = SourceFile::parse("test.rs", after.to_owned()).unwrap();
    let selection = Selection::parse("relational").unwrap();

    let left_ids: Vec<String> = into_definitions(&left, collect(&left, &selection))
        .into_iter()
        .map(|m| m.id.to_string())
        .collect();

    let right_ids: Vec<String> = into_definitions(&right, collect(&right, &selection))
        .into_iter()
        .map(|m| m.id.to_string())
        .collect();

    assert_eq!(left_ids, right_ids);
}

#[test]
fn identical_sites_in_one_function_get_distinct_ids() {
    let source = "fn f(a: i32, b: i32) -> bool { (a < b) && (a < b) }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let mutants = into_definitions(&file, collect(&file, &selection));

    assert_eq!(mutants.len(), 2);
    assert_ne!(mutants[0].id, mutants[1].id);
    assert_eq!(mutants[0].occurrence, 0);
    assert_eq!(mutants[1].occurrence, 1);
}

#[test]
fn identical_sites_in_different_functions_get_distinct_ids() {
    let source = "fn f(a: i32, b: i32) -> bool { a < b }\nfn g(a: i32, b: i32) -> bool { a < b }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let mutants = into_definitions(&file, collect(&file, &selection));

    assert_eq!(mutants.len(), 2);
    assert_ne!(mutants[0].id, mutants[1].id);
    assert_eq!(mutants[0].occurrence, 0);
    assert_eq!(mutants[1].occurrence, 0);
}

#[test]
fn different_replacements_at_one_site_get_distinct_ids() {
    let source = "fn f(a: i32, b: i32) -> bool { a < b }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("relational").unwrap();
    let mutants = into_definitions(&file, collect(&file, &selection));

    assert_eq!(mutants.len(), 2);
    assert_ne!(mutants[0].id, mutants[1].id);
}

#[test]
fn mutants_carry_line_and_column() {
    let source = "fn f(a: i32, b: i32) -> bool {\n    a < b\n}";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let mutants = into_definitions(&file, collect(&file, &selection));

    assert_eq!(mutants[0].site.line, 2);
    assert_eq!(mutants[0].site.column, 5);
}

#[test]
fn doc_comments_are_not_string_literals() {
    // A doc comment is desugared into `#[doc = "..."]`, so a visitor that walks attributes
    // reports every line of documentation in the tree as a mutable string.
    let source = "/// documentation\nfn f() {}";

    assert!(candidates(source, "literal").is_empty());
}

#[test]
fn attribute_arguments_are_not_mutated() {
    let source = "#[deprecated(note = \"use g instead\", since = \"1.0\")]\nfn f() {}";

    assert!(candidates(source, "all").is_empty());
}

#[test]
fn string_literals_in_real_code_are_still_mutated() {
    let source = "/// documentation\nfn f() -> &'static str { \"hello\" }";
    let found = mutators(source, "literal.str_to_empty");

    assert_eq!(found, vec!["literal.str_to_empty"]);
}

#[test]
fn an_empty_file_yields_nothing() {
    assert!(candidates("", "all").is_empty());
}

#[test]
fn a_file_of_only_types_yields_nothing() {
    assert!(candidates("struct S { a: i32 } enum E { A, B }", "all").is_empty());
}

// ---- Match guards. -----------------------------------------------------------------------

#[test]
fn a_match_guard_is_mutated_the_way_a_branch_condition_is() {
    let source = "fn f(v: i32) -> i32 { match v { n if n > 0 => n, _ => 0 } }";
    let found = mutators(source, "match_guard");

    // Without this family a guard is the one condition in the language nothing asks about, so a
    // suite that never exercises the guarded case scores as though it had.
    assert!(found.contains(&"match_guard.negate"), "{found:?}");
    assert!(found.contains(&"match_guard.always_true"), "{found:?}");
    assert!(found.contains(&"match_guard.always_false"), "{found:?}");
}

#[test]
fn an_unguarded_arm_offers_no_guard_mutants() {
    let source = "fn f(v: i32) -> i32 { match v { 1 => 1, _ => 0 } }";

    assert!(candidates(source, "match_guard").is_empty());
}

#[test]
fn a_guard_that_is_already_a_literal_is_not_replaced_by_that_literal() {
    let source = "fn f(v: i32) -> i32 { match v { n if true => n, _ => 0 } }";
    let found = mutators(source, "match_guard");

    // Replacing `true` with `true` is the original program, which can never be caught and
    // would sit in the report as a permanent survivor.
    assert!(!found.contains(&"match_guard.always_true"), "{found:?}");
    assert!(found.contains(&"match_guard.always_false"), "{found:?}");
}

// ---- Match arms. -------------------------------------------------------------------------

#[test]
fn an_arm_before_a_wildcard_can_be_stopped_from_matching() {
    let source = "fn f(v: i32) -> i32 { match v { 1 => 10, 2 => 20, _ => 0 } }";
    let found = candidates(source, "match_arm");

    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().all(|c| c.shape == Shape::Arm), "{found:?}");
}

#[test]
fn the_wildcard_itself_is_never_stopped_from_matching() {
    let source = "fn f(v: i32) -> i32 { match v { 1 => 10, _ => 0 } }";
    let found = candidates(source, "match_arm");

    // Guarding the wildcard leaves the match non-exhaustive, which is a compile error rather
    // than a question about the tests: the compiler does not count a guarded arm.
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        found[0].span,
        span_of(source, "1 => 10").start..span_of(source, "1 => 10").start + 1
    );
}

#[test]
fn a_match_without_a_wildcard_offers_no_arm_mutants() {
    let source = "fn f(v: bool) -> i32 { match v { true => 1, false => 0 } }";

    assert!(
        candidates(source, "match_arm").is_empty(),
        "an exhaustive match has nothing to fall through to"
    );
}

#[test]
fn an_arm_after_the_wildcard_offers_nothing() {
    let source = "fn f(v: i32) -> i32 { match v { _ => 0, 1 => 10 } }";

    // Nothing falls through to an arm the wildcard already swallowed, so the mutant would be
    // an equivalent one that survives forever.
    assert!(candidates(source, "match_arm").is_empty());
}

#[test]
fn a_guarded_arm_is_disabled_by_its_guard_rather_than_by_a_second_mutant() {
    let source = "fn f(v: i32) -> i32 { match v { n if n > 0 => n, _ => 0 } }";

    // `match_guard.always_false` already stops the arm matching. A second mutant saying the
    // same thing would double the cost of one question.
    assert!(candidates(source, "match_arm").is_empty());
}

// ---- Struct literal fields. --------------------------------------------------------------

#[test]
fn a_struct_field_is_omitted_only_when_a_base_supplies_it() {
    let source = "fn f() -> C { C { a: 1, b: 2, ..Default::default() } }";
    let found = candidates(source, "struct_field");

    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found
            .iter()
            .any(|c| c.replacement.contains("b: 2") && !c.replacement.contains("a: 1"))
    );
    assert!(
        found
            .iter()
            .any(|c| c.replacement.contains("a: 1") && !c.replacement.contains("b: 2"))
    );
}

#[test]
fn a_struct_literal_without_a_base_offers_nothing() {
    let source = "fn f() -> C { C { a: 1, b: 2 } }";

    // Removing a field from a literal that names every one of them does not compile.
    assert!(candidates(source, "struct_field").is_empty());
}

#[test]
fn omitting_the_last_field_leaves_the_base_intact() {
    let source = "fn f() -> C { C { a: 1, ..Default::default() } }";
    let found = candidates(source, "struct_field");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].replacement, "C { ..Default::default() }");
}

// ---- Ranges. -----------------------------------------------------------------------------

#[test]
fn a_half_open_range_offers_its_inclusive_form() {
    let source = "fn f(n: usize) -> usize { let mut t = 0; for i in 0..n { t += i; } t }";
    let found = candidates(source, "range");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].mutator, "range.exclusive_to_inclusive");

    // Spelled as arithmetic on the endpoint rather than as `..=`, because the mutant and the
    // original share the arms of an `if` and so have to share a type.
    assert_eq!(found[0].replacement, "(0)..((n) + 1)");
}

#[test]
fn an_inclusive_range_offers_its_half_open_form() {
    let source = "fn f(n: usize) -> usize { let mut t = 0; for i in 0..=n { t += i; } t }";
    let found = candidates(source, "range");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].mutator, "range.inclusive_to_exclusive");
    assert_eq!(found[0].replacement, "(0)..=((n) - 1)");
}

#[test]
fn a_range_with_no_end_has_no_inclusive_form_to_offer() {
    let source = "fn f(v: &[u8]) -> &[u8] { &v[1..] }";

    assert!(candidates(source, "range").is_empty());
}

#[test]
fn a_range_with_no_start_still_moves_its_boundary() {
    let source = "fn f(v: &[u8], n: usize) -> &[u8] { &v[..n] }";
    let found = candidates(source, "range");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].replacement, "..((n) + 1)");
}

// ---- Loop exits. -------------------------------------------------------------------------

#[test]
fn break_and_continue_are_swapped_for_each_other() {
    let source = "fn f(v: &[i32]) { for x in v { if *x == 0 { continue; } if *x == 1 { break; } } }";
    let found = mutators(source, "loop.break_to_continue,loop.continue_to_break");

    assert!(found.contains(&"loop.break_to_continue"), "{found:?}");
    assert!(found.contains(&"loop.continue_to_break"), "{found:?}");
}

#[test]
fn a_break_carrying_a_value_is_left_alone() {
    let source = "fn f() -> i32 { loop { break 1; } }";

    // `continue` produces no value, so the loop would no longer have the type its context
    // requires and the mutant would be withdrawn as unviable rather than measured.
    assert!(!mutators(source, "loop.break_to_continue").contains(&"loop.break_to_continue"));
}

#[test]
fn a_labelled_break_is_left_alone_but_a_labelled_continue_is_not() {
    let source = "fn f(v: &[i32]) { 'outer: for x in v { for y in v { if x == y { continue 'outer; } break 'outer; } } }";
    let found = candidates(source, "loop");

    // A label on `continue` can only name a loop, so `break` accepts it. A label on `break`
    // may name a labelled block, which `continue` cannot leave at all.
    assert!(
        found
            .iter()
            .any(|c| c.mutator == "loop.continue_to_break" && c.replacement == "break 'outer")
    );
    assert!(!found.iter().any(|c| c.mutator == "loop.break_to_continue"));
}

#[test]
fn a_continue_is_not_changed_to_a_valueless_break_in_a_value_producing_loop() {
    let source = "fn f(flag: bool) -> i32 { loop { if flag { continue; } break 1; } }";

    assert!(candidates(source, "loop.continue_to_break").is_empty());
}

#[test]
fn a_labelled_continue_is_not_changed_to_a_valueless_break_in_its_value_producing_loop() {
    let source = "fn f(flag: bool) -> i32 { 'outer: loop { while flag { continue 'outer; } break 'outer 1; } }";

    assert!(candidates(source, "loop.continue_to_break").is_empty());
}

#[test]
fn a_break_or_continue_statement_can_be_deleted() {
    let source = "fn f(v: &[i32]) { for x in v { if *x == 0 { continue; } } }";
    let found = candidates(source, "loop.delete_continue");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].shape, Shape::Stmt);
}

// ---- Focused numeric perturbation. --------------------------------------------------------

#[test]
fn a_call_argument_is_perturbed_by_one_in_both_directions() {
    let source = "fn f(n: usize) { g(n); }";
    let found = candidates(source, "expr");

    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|c| c.replacement == "(n) + 1"));
    assert!(found.iter().any(|c| c.replacement == "(n) - 1"));
}

#[test]
fn a_literal_argument_is_left_to_the_literal_family() {
    let source = "fn f() { g(3); }";

    // `literal.int_increment` already offers `4` here. Offering `(3) + 1` beside it would buy
    // a second run of the whole suite for an answer already in hand.
    assert!(candidates(source, "expr").is_empty());
}

#[test]
fn a_capacity_argument_is_not_perturbed() {
    let source = "fn f(n: usize) -> Vec<u8> { Vec::with_capacity(n) }";

    // A test that noticed would be a test pinning an allocation strategy, so reporting the
    // survivor would accuse the suite of a gap it should not be asked to fill.
    assert!(candidates(source, "expr").is_empty());
}

#[test]
fn an_index_and_a_range_bound_are_perturbed() {
    let indexed = candidates("fn f(v: &[u8], i: usize) -> u8 { v[i] }", "expr");
    let bounded = candidates("fn f(v: &[u8], n: usize) -> &[u8] { &v[..n] }", "expr");

    // Two positions overlap here and both are wanted: the subscript, where being wrong by one
    // reads the neighbouring element, and the returned `u8`, where being wrong by one is the
    // classic off-by-one in the answer itself.
    assert!(indexed.iter().any(|c| c.replacement == "(i) + 1"), "{indexed:?}");

    // The element is offered too, but on the signature's word rather than on a guess about
    // what subscripting yields: `-> u8` says the returned value is a number.
    assert!(indexed.iter().any(|c| c.replacement == "(v[i]) + 1"), "{indexed:?}");

    // Take that word away and it falls silent, because nothing else here says what `v[i]` is.
    let unsaid = candidates("fn f(v: &[u8], i: usize) { g(v[i]); }", "expr");

    assert!(!unsaid.iter().any(|c| c.replacement == "(v[i]) + 1"), "{unsaid:?}");

    // The returned `&[u8]` is not a number, so only the range bound is offered.
    assert_eq!(bounded.len(), 2, "{bounded:?}");
    assert!(bounded.iter().any(|c| c.replacement == "(n) - 1"), "{bounded:?}");
}

#[test]
fn a_returned_value_is_perturbed_however_it_is_returned() {
    let trailing = candidates("fn f(n: usize) -> usize { n }", "expr");
    let explicit = candidates("fn f(n: usize) -> usize { return n; }", "expr");

    assert_eq!(trailing.len(), 2, "{trailing:?}");
    assert_eq!(explicit.len(), 2, "{explicit:?}");
}

#[test]
fn integer_literal_tails_and_explicit_returns_have_identical_mutants() {
    let describe = |source| {
        let mut found: Vec<(&'static str, String)> = candidates(source, "literal,expr")
            .into_iter()
            .map(|candidate| (candidate.mutator, candidate.replacement.to_string()))
            .collect();
        found.sort();
        found
    };

    assert_eq!(describe("fn f() -> i32 { 3 }"), describe("fn f() -> i32 { return 3 }"));
}

#[test]
fn a_returned_number_in_tail_position_perturbs_nothing() {
    // `return 5` as the body's tail has type `!`, so it passes the signature-only proof that a
    // tail is a number, but `(return 5) + 1` returns before the `+ 1` is ever reached: the
    // mutant is a twin of the original that no test could distinguish. The bare literal `5` is
    // still the literal family's to perturb; this asks only about the increment/decrement pair.
    let found = candidates("fn f() -> i32 { return 5 }", "expr.increment,expr.decrement");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn statically_divergent_tails_perturb_nothing() {
    // Each of these leaves the function -- or spins forever -- before its supposed value is
    // used, so wrapping it in `+ 1` or `- 1` changes nothing a test could observe. The proof
    // that a numeric tail is worth perturbing has to exclude the tails that never yield one:
    // `return`, the never-returning macros, a breakless `loop`, and branches that all diverge.
    let divergent = [
        "fn f() -> i32 { panic!() }",
        "fn f() -> i32 { unreachable!() }",
        "fn f() -> i32 { todo!() }",
        "fn f() -> i32 { unimplemented!() }",
        "fn f() -> i32 { loop {} }",
        "fn f(c: bool) -> i32 { if c { return 1 } else { return 2 } }",
    ];

    for source in divergent {
        let found = candidates(source, "expr.increment,expr.decrement");

        assert!(found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn an_ordinary_numeric_tail_still_perturbs_both_ways() {
    // The guard rejects only the tails that diverge. A plain value-producing tail is the whole
    // point of the family, and dropping its neighbours would blind the suite to an off-by-one
    // in the one place the signature guarantees a number.
    let found: Vec<String> = candidates("fn f(value: i32) -> i32 { value }", "expr.increment,expr.decrement")
        .into_iter()
        .map(|candidate| candidate.replacement.to_string())
        .collect();

    assert!(found.contains(&"(value) + 1".to_owned()), "{found:?}");
    assert!(found.contains(&"(value) - 1".to_owned()), "{found:?}");
}

#[test]
fn a_loop_that_can_break_with_a_value_is_still_perturbed() {
    // A `loop` is only rejected when it truly cannot terminate. One that breaks with a value
    // does hand a number back, so the family still owes it the increment/decrement pair; the
    // breakless check must not swallow it.
    let found = mutators("fn f() -> i32 { loop { break 5; } }", "expr.increment,expr.decrement");

    assert!(found.contains(&"expr.increment"), "{found:?}");
    assert!(found.contains(&"expr.decrement"), "{found:?}");
}

#[test]
fn a_non_numeric_argument_is_not_perturbed() {
    let source = "fn f(s: &str, c: bool, n: usize) { g(s, c, &s, |x| x, \"lit\", n); }";
    let found = candidates(source, "expr");

    // Nothing here adds to an integer except `n`. Every mutant that cannot compile costs a
    // share of a rebuild that finds nothing, so the filter is what keeps the family
    // affordable — but the one argument that does add has to survive it, or the filter has
    // bought its saving by hiding a gap in the suite.
    assert!(found.iter().all(|c| c.replacement.starts_with("(n)")), "{found:?}");
    assert!(found.iter().any(|c| c.replacement == "(n) + 1"), "{found:?}");
}

#[test]
fn a_parameter_the_source_declares_a_number_is_still_perturbed_through_a_reference() {
    // `&usize + 1` compiles, so treating every reference as unaddable would throw away a
    // mutant that builds, runs and can genuinely be missed.
    let found = candidates("fn f(n: &usize) { g(n); }", "expr");

    assert!(found.iter().any(|c| c.replacement == "(n) + 1"), "{found:?}");
}

#[test]
fn an_annotated_local_is_judged_by_the_type_the_source_wrote_down() {
    let source = "fn f() { let name: String = h(); let count: u32 = h(); g(name); g(count); }";
    let found = candidates(source, "expr");

    assert!(found.iter().any(|c| c.replacement == "(count) + 1"), "{found:?}");
    assert!(!found.iter().any(|c| c.replacement.starts_with("(name)")), "{found:?}");
}

#[test]
fn a_local_whose_type_was_never_written_down_is_left_alone_rather_than_guessed_at() {
    // Guessing that anything unaccounted for is a number was measured on this repository and
    // was wrong three times in four, which made the two perturbation operators alone 78% of
    // every mutant that failed to build. Silence is the answer when the source says nothing.
    let guessed = candidates("fn f() { let total = h(); g(total); }", "expr");

    assert!(guessed.is_empty(), "{guessed:?}");

    // But the source says a great deal short of an annotation, and the point of refusing to
    // guess is that reading what it does say has to make up the difference. An initialiser
    // this collector can type answers exactly what the annotation would have.
    let inferred = candidates("fn f(v: &[u8]) { let total = v.len(); g(total); }", "expr");

    assert!(inferred.iter().any(|c| c.replacement == "(total) + 1"), "{inferred:?}");
}

#[test]
fn a_local_used_where_only_a_number_fits_is_perturbed_without_an_annotation() {
    // Most locals carry no type and no initialiser worth reading, so the last evidence left is
    // how the name is used. Each of these uses admits nothing but a number.
    for source in [
        "fn f() { let i = h(); let _ = v[i]; g(i); }",
        "fn f() { let i = h(); let _ = i - 1; g(i); }",
        "fn f() { let i = h(); if i > 0 { g(i); } }",
        "fn f() { let i = h(); let _ = i.saturating_sub(1); g(i); }",
        "fn f() { for i in 0..8 { g(i); } }",
    ] {
        let found = candidates(source, "expr");

        assert!(found.iter().any(|c| c.replacement == "(i) + 1"), "{source}: {found:?}");
    }
}

#[test]
fn a_field_is_judged_by_the_struct_that_declares_it() {
    // A field is very often read above its own `struct`, so the declarations are gathered in a
    // pass of their own before anything is offered.
    let source = "fn f(s: S) { g(s.count, s.name); } struct S { name: String, count: usize }";
    let found = candidates(source, "expr");

    assert!(found.iter().any(|c| c.replacement == "(s.count) + 1"), "{found:?}");
    assert!(!found.iter().any(|c| c.replacement.starts_with("(s.name)")), "{found:?}");
}

#[test]
fn a_type_named_where_a_value_is_expected_is_not_perturbed() {
    let source = "fn f() { g(PhantomData, Vec::new(), items.iter(), MAX); }";
    let found = candidates(source, "expr");

    // `MAX` is the point of the exception: constants are spelled in the screaming case and are
    // among the most worthwhile things this family has to offer, so the camel-case rule that
    // rejects `PhantomData` must not reject them too.
    assert!(found.iter().any(|c| c.replacement == "(MAX) + 1"), "{found:?}");
    assert!(found.iter().all(|c| c.replacement.starts_with("(MAX)")), "{found:?}");
}

#[test]
fn a_local_binding_does_not_leak_into_a_nested_function() {
    // A function defined inside another cannot see the outer one's locals, so reasoning from
    // them would reach a confident conclusion about a completely unrelated name.
    let source = "fn outer() { let value: String = h(); fn inner(value: u32) { g(value); } }";
    let found = candidates(source, "expr");

    assert!(found.iter().any(|c| c.replacement == "(value) + 1"), "{found:?}");
}

#[test]
fn a_default_is_not_invented_for_a_type_the_caller_chooses() {
    // `D::Error` is whatever the caller's deserializer says it is, and nothing promises it has
    // a `Default`. On a serde-shaped API this was the single largest source of mutants that
    // could not compile.
    let source = "fn f<D: Reader>(d: D) -> Result<usize, D::Error> { g(d); Ok(1) }";
    let found = candidates(source, "fn_value");

    assert!(
        !found.iter().any(|c| c.replacement.contains("Err(Default::default())")),
        "{found:?}"
    );

    // The other half of the return type is concrete, so it keeps everything it had. A rule
    // that took the whole signature out would stop asking whether the value is tested at all.
    assert!(found.iter().any(|c| c.replacement == "Ok(0)"), "{found:?}");
}

#[test]
fn a_default_is_still_invented_for_a_parameter_declared_to_have_one() {
    // The promise this rule looks for was made explicitly, so the mutant it would otherwise
    // withhold compiles and is worth offering.
    let source = "fn f<T: Default>(t: T) -> Result<usize, T> { g(t); Ok(1) }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement == "Err(Default::default())"), "{found:?}");
}

#[test]
fn a_default_is_not_invented_for_a_trait_object() {
    // `dyn Reader` names a capability rather than a type; there is no `default()` to call.
    let found = candidates("fn f() -> Box<dyn Reader> { h() }", "fn_value");

    assert!(!found.iter().any(|c| c.replacement.contains("Default::default()")), "{found:?}");
}

#[test]
fn an_associated_type_of_self_is_still_given_a_default() {
    // `Self::Value` looks like `D::Error` but is not: inside an `impl` it resolves to a type
    // that block chose, which often does have a `Default`. Treating it as abstract cost six
    // mutants a real suite had caught.
    let source = "impl Visitor for V { fn visit(self) -> Result<Self::Value, u8> { h() } }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement == "Ok(Default::default())"), "{found:?}");
}

#[test]
fn perturbation_is_on_by_default() {
    let source = "fn f(n: usize) { g(n); }";
    let found = mutators(source, "@default");

    assert!(found.contains(&"expr.increment"), "{found:?}");
}

#[test]
fn option_and_result_construction_is_mutated_both_ways() {
    let found = mutators("fn f(flag: bool) { let _ = if flag { Some(1) } else { None }; }", "option");

    assert!(found.contains(&"option.some_to_none"), "{found:?}");
    assert!(found.contains(&"option.none_to_some"), "{found:?}");

    let found = mutators("fn f(flag: bool) { let _ = if flag { Ok(1) } else { Err(2) }; }", "result");

    assert!(found.contains(&"result.ok_to_err"), "{found:?}");
    assert!(found.contains(&"result.err_to_ok"), "{found:?}");
}

#[test]
fn iterator_methods_swap_only_where_the_types_agree() {
    let found = mutators("fn f(v: &[u32]) { let _ = v.iter().any(|n| *n > 0); }", "iter");

    assert!(found.contains(&"iter.any_to_all"), "{found:?}");

    // `take` and `skip` return different types, so no mutant may be offered for them. This
    // would otherwise be generated on every chain in a codebase and withdrawn on every run.
    let found = mutators("fn f(v: &[u32], n: usize) { let _ = v.iter().take(n).count(); }", "iter");

    assert!(!found.contains(&"iter.take_to_skip"), "{found:?}");
}

#[test]
fn a_method_rename_needs_the_arity_that_identifies_it() {
    // Without type resolution, the count is the only evidence that this `take` belongs to
    // `Iterator` rather than to `Option` or `Cell`, where the rename would be nonsense.
    let found = mutators(
        "fn f(v: &[String], s: &str) { let _ = v.iter().any(|w| w.starts_with(s)); }",
        "string",
    );

    assert!(found.contains(&"string.starts_with_to_ends_with"), "{found:?}");

    let found = mutators("fn f(o: &mut Option<u32>) { let _ = o.take(); }", "iter,string");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_vec_literal_offers_each_element_for_omission() {
    let found = candidates("fn f() { let _ = vec![1, 2, 3]; }", "collection.omit_element");

    assert_eq!(found.len(), 3);

    // The replacement must still be an expression, because it becomes one arm of an `if`.
    assert!(found.iter().all(|candidate| candidate.replacement.starts_with("vec!")), "{found:?}");
}

#[test]
fn an_assignment_offers_a_default_value() {
    let found = mutators("fn f(mut n: u32) { n = n + 1; }", "assign_value");

    assert!(found.contains(&"assign_value.default"), "{found:?}");
}

#[test]
fn nested_return_types_recurse_into_their_payloads() {
    let source = "fn f() -> Result<Option<bool>, String> { compute() }";
    let found = candidates(source, "fn_value");
    let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

    assert!(texts.contains(&"Ok(None)"), "{texts:?}");
    assert!(texts.contains(&"Ok(Some(true))"), "{texts:?}");
    assert!(texts.contains(&"Ok(Some(false))"), "{texts:?}");
}

#[test]
fn an_impl_iterator_return_is_mutated_through_the_either_wrapper() {
    // An `impl Trait` return is one concrete type picked by the body, and a mutant shares an
    // `if` with that body, so a bare replacement would be withdrawn after a wasted build.
    // `Shape::IterBlock` wraps both arms so they agree on a type, which is what makes these
    // mutants viable rather than a waste of a build.
    let found = candidates("fn f() -> impl Iterator<Item = u32> { core::iter::once(1) }", "fn_value");
    let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

    assert!(texts.contains(&"core::iter::empty()"), "{texts:?}");
    assert!(texts.contains(&"core::iter::once(0)"), "{texts:?}");

    assert!(
        found.iter().all(|candidate| candidate.shape == Shape::IterBlock),
        "the wrapper is what makes them compile, so every one must ask for it: {found:?}"
    );
}

#[test]
fn an_impl_iterator_return_without_an_item_type_still_offers_the_empty_case() {
    // `empty()` needs no item type, because the wrapper infers it from the arm holding the
    // original. `once(v)` needs a value, and there is no type here to name one of.
    let found = candidates("fn f() -> impl Iterator { core::iter::once(1) }", "fn_value");
    let texts: Vec<_> = found.iter().map(|candidate| candidate.replacement.as_str()).collect();

    assert_eq!(texts, vec!["core::iter::empty()"], "{texts:?}");
}

#[test]
fn an_impl_trait_return_that_is_not_an_iterator_still_offers_nothing() {
    // `Either` only unifies iterators. A future, a closure or a writer has no expression this
    // tool can name that is guaranteed to satisfy the signature.
    let found = mutators("fn f() -> impl core::future::Future<Output = u32> { async { 1 } }", "fn_value");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_vec_element_is_never_offered_when_the_mutator_is_not_selected() {
    // `omit_elements` splices a candidate directly rather than going through the usual
    // `emit_shaped` helper, so it carries its own `wants` check. Without one, disabling
    // `collection.omit_element` would still leave the mutant in the population, and a user who
    // turned the family off to shrink a run would get exactly the mutants they asked to skip.
    let found = mutators("fn f() { let _ = vec![1, 2, 3]; }", "arith");

    assert!(!found.contains(&"collection.omit_element"), "{found:?}");
}

#[test]
fn a_vec_that_uses_the_repeat_syntax_is_not_a_comma_separated_list() {
    // `vec![value; count]` does not parse as `Punctuated<Expr, Comma>`, so the body parse
    // fails outright. Treating that as "nothing to offer" rather than propagating the error
    // is what lets an ordinary array-style vec sit next to a repeat-style one in the same file
    // without the whole file failing to collect.
    let found = mutators("fn f() { let _ = vec![0; 5]; }", "collection.omit_element");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_single_element_vec_is_left_alone() {
    // Omitting the one element a single-item `vec!` has would leave an empty collection, which
    // is a different question — whether the collection is needed at all — and one that
    // `Vec::new()` already asks on the function's behalf. Offering it here would be asking the
    // same thing twice under two different names.
    let found = mutators("fn f() { let _ = vec![1]; }", "collection.omit_element");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_cfg_test_match_arm_is_left_out_of_the_never_matches_family() {
    // An arm that only exists under `#[cfg(test)]` is not compiled into the program a normal
    // test run exercises, so a guard placed on it would sit on code that was never there and
    // could never be activated by any test.
    let source = "fn f(x: i32) -> i32 { match x { #[cfg(test)] 1 => 10, _ => 0 } }";
    let found = mutators(source, "match_arm.never_matches");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_struct_field_omission_is_withheld_when_its_mutator_is_not_selected() {
    // Like the `vec!` element family, field omission splices its candidate directly and has
    // to check the selection itself; skipping that check would ignore a selection that
    // deliberately left the mutator out.
    let found = mutators("fn f() -> C { C { a: 1, ..Default::default() } }", "arith");

    assert!(!found.contains(&"struct_field.omit"), "{found:?}");
}

#[test]
fn a_struct_literal_with_no_base_offers_no_field_omissions() {
    // Omitting a field only leaves a well-formed value when a `..base` supplies whatever was
    // taken out. Without one, "omission" would just be a missing field, which is a compile
    // error rather than a mutant.
    let found = mutators("fn f() -> C { C { a: 1, b: 2 } }", "struct_field.omit");

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_cfg_test_struct_field_is_never_offered_for_omission() {
    // A field that only exists under `#[cfg(test)]` is not part of the struct a normal build
    // sees, so a mutant that omits it there would be omitting something that was never there.
    let source = "fn f() -> C { C { #[cfg(test)] a: 1, b: 2, ..Default::default() } }";
    let found = candidates(source, "struct_field.omit");

    assert_eq!(found.len(), 1, "{found:?}");
    assert!(!found[0].replacement.contains('b'), "{found:?}");
}

#[test]
fn a_cfg_test_impl_block_is_not_mutated() {
    // `#[cfg(test)]` on the `impl` itself takes every method in it out of the build, the same
    // way the attribute does on a module or a function; the check has to be repeated at the
    // `impl` because none of those other sites would have caught it.
    let source = "#[cfg(test)] impl S { fn f(&self) -> i32 { 1 + 1 } }";

    assert!(candidates(source, "arith").is_empty());
}

#[test]
fn parenthesised_bindings_still_answer_whether_a_value_is_a_number() {
    // Redundant parentheses are common after a refactor or a macro expansion site, and a
    // reader would not expect them to change what is offered. Without seeing through them
    // here, a parenthesised reference to a `String` parameter would be perturbed as though it
    // were a number, offering a mutant that could never compile.
    let found = mutators("fn f(name: String) { g((name)); }", "expr");

    assert!(!found.contains(&"expr.increment"), "{found:?}");
    assert!(!found.contains(&"expr.decrement"), "{found:?}");
}

#[test]
fn a_parenthesised_true_is_not_offered_true_again() {
    // `if (true)` is the same condition as `if true`, so `cond.always_true` would replace it
    // with a copy of itself — a mutant that compiles to the original program and can never be
    // caught, forever occupying a line on the report as a survivor nothing could kill.
    let found = mutators("fn f() { if (true) { g(); } }", "cond");

    assert!(!found.contains(&"cond.always_true"), "{found:?}");
    assert!(found.contains(&"cond.always_false"), "{found:?}");
}

#[test]
fn a_parenthesised_arithmetic_expression_is_still_perturbable() {
    // A caller sometimes parenthesises an argument for its own clarity, and that should not
    // hide the arithmetic inside it from the family whose whole purpose is asking whether an
    // off-by-one there would be caught.
    let found = mutators("fn f(a: i32, b: i32) { g((a + b)); }", "expr");

    assert!(found.contains(&"expr.increment"), "{found:?}");
}

/// A screaming-case name whose declaration is not a number is not perturbed.
///
/// The screaming case is read as evidence of a constant, and a constant is one of the best
/// things this family has to offer — but the spelling says nothing about the type, and this
/// codebase alone is full of `const PREFIX: &str`. Adding one to those is `E0369` every time: a
/// mutant that cannot compile, and a share of a rollback round spent finding that out.
#[test]
fn a_constant_the_file_declares_as_text_is_not_perturbed() {
    let found = mutators(r#"const PREFIX: &str = "x"; fn f() { g(PREFIX); }"#, "expr");

    assert!(!found.contains(&"expr.increment"), "{found:?}");
}

/// The declaration is read in both directions, so a quiet name that is a number still perturbs.
#[test]
fn a_constant_the_file_declares_as_a_number_is_still_perturbed() {
    let found = mutators("const LIMIT: usize = 4; fn f() { g(LIMIT); }", "expr");

    assert!(found.contains(&"expr.increment"), "{found:?}");
}

/// An associated constant is declared just as plainly as a free one.
#[test]
fn an_associated_constant_declared_as_text_is_not_perturbed() {
    let found = mutators(
        r#"struct S; impl S { const NAME: &'static str = "s"; } fn f() { g(S::NAME); }"#,
        "expr",
    );

    assert!(!found.contains(&"expr.increment"), "{found:?}");
}

/// Two declarations disagreeing about a name leave it unknown rather than letting one win.
///
/// Without type resolution there is no way to say which `MAX` a bare `MAX` reached, so the
/// screaming-case guess would be as likely to be wrong as right — and being wrong here costs a
/// build. Neither answer is taken, which for this family means no perturbation.
#[test]
fn a_constant_two_declarations_disagree_about_is_not_perturbed() {
    let source = r#"struct A; struct B; impl A { const CAP: usize = 1; } impl B { const CAP: &'static str = "b"; } fn f() { g(CAP); }"#;
    let found = mutators(source, "expr");

    assert!(!found.contains(&"expr.increment"), "{found:?}");
}

/// Text the source builds on the spot is never perturbed, whatever else looked numeric.
///
/// `format!` has one result type, and the sum of a `String` and a `&str` is a `String` however
/// numeric either side looked. Both are decidable from the syntax alone, which is what makes
/// them worth refusing rather than paying a build to discover.
#[test]
fn text_the_source_builds_on_the_spot_is_not_perturbed() {
    for source in [
        r#"fn f(n: usize) { g(format!("{n}")); }"#,
        "fn f(n: usize) { g(n.to_string()); }",
        r#"fn f() { g("a".to_owned()); }"#,
        r#"const NAME: &str = "n"; fn f(s: String) { g(s + NAME); }"#,
    ] {
        let found = mutators(source, "expr");

        assert!(!found.contains(&"expr.increment"), "{source}: {found:?}");
        assert!(!found.contains(&"expr.decrement"), "{source}: {found:?}");
    }
}

#[test]
fn a_parenthesised_callee_is_still_recognised_by_its_type() {
    // `(Vec::new)()` calls a parenthesised path, which is unusual but legal Rust, and the
    // qualifying type still has to be read through the parentheses. Missing it here would
    // offer `+ 1` on a `Vec`, a mutant that can never compile.
    let found = mutators("fn f() { g((Vec::new)()); }", "expr");

    assert!(!found.contains(&"expr.increment"), "{found:?}");
}

#[test]
fn an_assignment_already_holding_a_default_is_left_alone() {
    // Replacing `Default::default()` with `Default::default()` reproduces the original
    // program exactly, so the mutant could never be caught and would sit in every report as a
    // permanent, uninformative survivor.
    let found = mutators("fn f(mut n: i32) { n = Default::default(); }", "assign_value");

    assert!(!found.contains(&"assign_value.default"), "{found:?}");
}

#[test]
fn a_parenthesised_default_call_is_still_recognised() {
    let found = mutators("fn f(mut n: i32) { n = (Default::default()); }", "assign_value");

    assert!(!found.contains(&"assign_value.default"), "{found:?}");
}

#[test]
fn a_trait_default_bound_suppresses_an_identical_assignment_mutant() {
    let source = "trait Builder<T: Default> { fn build(mut value: T) { value = T::default(); } }";
    let found = mutators(source, "assign_value");

    assert!(!found.contains(&"assign_value.default"), "{found:?}");
}

#[test]
fn a_bare_call_to_a_capacity_named_function_is_not_perturbed() {
    // `with_capacity(5)` names an allocation strategy rather than a behavior even when it is
    // not qualified by the type it belongs to, and perturbing its result would ask a question
    // about a performance decision rather than about the program.
    let found = mutators("fn f() { h(with_capacity(5)); }", "expr");

    assert!(!found.contains(&"expr.increment"), "{found:?}");
}

#[test]
fn a_parenthesised_type_the_caller_chooses_is_still_treated_as_abstract() {
    // `D::Error` wrapped in redundant parentheses is exactly as unconstrained as the bare
    // form, and a rule that stopped recognising it once parenthesised would start inventing
    // `Default::default()` values for a type nothing promises has one.
    let source = "fn f<D: Reader>(d: D) -> Result<usize, (D::Error)> { g(d); Ok(1) }";
    let found = candidates(source, "fn_value");

    assert!(
        !found.iter().any(|c| c.replacement.contains("Err(Default::default())")),
        "{found:?}"
    );
}

#[test]
fn a_parenthesised_numeric_type_annotation_still_marks_a_binding_numeric() {
    // `let x: (i32) = 1;` writes down a numeric type just as plainly as `let x: i32 = 1;`
    // does, and a reader would not expect the redundant parentheses to hide that fact from the
    // family that decides whether a bare identifier is worth perturbing.
    let found = mutators("fn f() { let x: (i32) = 1; g(x); }", "expr");

    assert!(found.contains(&"expr.increment"), "{found:?}");
}

#[test]
fn a_parenthesised_callee_path_still_names_the_variant_it_constructs() {
    // `(Some)(1)` calls a parenthesised path, which is legal Rust and behaves exactly like
    // `Some(1)`; missing the wrapped path here would quietly drop `option.some_to_none` for
    // every call written this way.
    let found = mutators("fn f() { let _ = (Some)(1); }", "option");

    assert!(found.contains(&"option.some_to_none"), "{found:?}");
}

#[test]
fn a_lifetime_generic_parameter_never_needs_a_default_bound() {
    // Only a type parameter can carry a `Default` bound in the first place, so a lifetime
    // sitting beside one in the same parameter list has to be skipped rather than treated as
    // an undefaulted type parameter, or it would be reported as a type this tool cannot
    // assume a `Default` for.
    let source = "fn f<'a, D: Reader>(d: &'a D) -> Result<usize, D::Error> { g(d); Ok(1) }";
    let found = candidates(source, "fn_value");

    assert!(
        !found.iter().any(|c| c.replacement.contains("Err(Default::default())")),
        "{found:?}"
    );
}

/// Without type resolution the classification is by name, so a locally defined type spelled
/// `Vec` cannot be told apart by its name alone. It is told apart by its shape: the standard
/// `Vec` carries an element type and this one carries nothing, so it is not read as the
/// standard one and the family falls back to the guess that fits any type.
#[test]
fn a_local_type_that_shares_a_collection_name_but_has_no_generics_falls_back_to_default() {
    let source = "struct Vec; fn f() -> Vec { Vec }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().all(|c| c.replacement == "Default::default()"), "{found:?}");
}

#[test]
fn a_bare_box_with_no_generic_argument_is_not_treated_as_unconstructable() {
    // `Box` is only as unconstructable as whatever it wraps, and asking what it wraps when
    // nothing was written down should be answered with "nothing", not with a guess. A `Box`
    // spelled without its argument at all — a locally shadowed name, since the real type
    // always requires one — must not be mistaken for `Box<dyn Trait>`, which is why it holds
    // the `err_default` mutant rather than losing it.
    let source = "struct Box; fn f() -> Result<i32, Box> { g() }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement == "Err(Default::default())"), "{found:?}");
}

#[test]
fn a_return_type_nested_deeper_than_the_recursion_bound_falls_back_to_default() {
    // The recursion is bounded so that a deeply nested return type costs a constant number of
    // mutants rather than one for every level of nesting; past that bound, the family still
    // has to offer something, and `Default::default()` is the one expression that type-checks
    // regardless of how deep the type turned out to be.
    let source = "fn f() -> Option<Option<Option<Option<bool>>>> { None }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement.contains("Default::default()")), "{found:?}");
}

#[test]
fn an_option_whose_payload_is_a_type_the_caller_chooses_offers_only_none() {
    // Just as with `Result`, an `Option<D::Error>` cannot be given a `Some(..)` mutant without
    // guessing at a `Default` nothing promises exists, so the abstract payload has to leave
    // the family with nothing to offer beyond the value it can always name: absence itself.
    let source = "fn f<D: Reader>(d: D) -> Option<D::Error> { g(d) }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement == "None"), "{found:?}");
    assert!(!found.iter().any(|c| c.replacement.contains("Some")), "{found:?}");
}

#[test]
fn a_result_whose_success_payload_is_abstract_offers_no_ok_mutant() {
    // The mirror image of the caller-chosen-error case: when it is the success side that
    // cannot be given a `Default`, the family still owes the reader the error mutant it can
    // name, but must not fabricate a value for the side it cannot.
    let source = "fn f<D: Reader>(d: D) -> Result<D::Error, u8> { g(d) }";
    let found = candidates(source, "fn_value");

    assert!(!found.iter().any(|c| c.replacement.starts_with("Ok(")), "{found:?}");
    assert!(found.iter().any(|c| c.replacement == "Err(Default::default())"), "{found:?}");
}

#[test]
fn a_wide_tuple_return_is_capped_rather_than_multiplied_out_in_full() {
    // Every element of a tuple return multiplies the count of mutants by its own number of
    // values, so a tuple of several booleans would otherwise cost dozens of mutants for one
    // function. The cap exists precisely to stop that multiplication, and this is the fixture
    // that forces the running total past it mid-build.
    let source = "fn f() -> (bool, bool, bool, bool, bool) { (true, true, true, true, true) }";
    let found = candidates(source, "fn_value.tuple");

    assert!(found.len() <= 8, "{found:?}");
    assert!(!found.is_empty(), "{found:?}");
}

#[test]
fn a_destructured_parameter_contributes_no_binding_to_perturb() {
    // A tuple-pattern parameter never binds a plain identifier the way `a: i32` does, so there
    // is no name this family could look up later when deciding whether some later expression
    // is the numeric parameter it appears to be. Recording nothing for it, rather than
    // guessing at one of its parts, is what keeps that later lookup honest.
    let source = "fn f((a, b): (i32, i32)) -> i32 { let a = a; a + b }";
    let found = mutators(source, "expr");

    assert!(found.contains(&"expr.increment"), "{found:?}");
}

#[test]
fn a_parenthesised_return_type_is_still_classified_by_the_type_it_wraps() {
    // Redundant parentheses around a return type are legal and occasionally left behind by a
    // refactor; a reader would expect `(Vec<i32>)` to offer exactly what `Vec<i32>` offers; if
    // the parentheses hid the type from classification, the whole `fn_value` family would fall
    // silent for a function that has plenty to offer.
    let found = mutators("fn f() -> (Vec<i32>) { make() }", "fn_value");

    assert!(found.contains(&"fn_value.empty_collection"), "{found:?}");
}

#[test]
fn a_lifetime_argument_ahead_of_a_result_payload_is_skipped_rather_than_counted() {
    // `Result`'s two payload types are found by position among a path's generic arguments, but
    // a written-out lifetime shares that argument list. Counting it as though it were a type
    // would shift every position after it by one and hand the wrong payload to the wrong side
    // of the `Result`.
    let found = candidates("fn f() -> Result<'static, u8> { Ok(1) }", "fn_value.ok");

    assert!(found.iter().any(|c| c.replacement == "Ok(0)"), "{found:?}");
}

/// A body that already spells one of the values the family offers would be replaced by itself.
/// The compiled program is identical, so no test can tell the mutant from the original and it
/// survives every suite that will ever exist — a permanent accusation against tests that had
/// nothing to answer. The sibling values are still worth offering, so only the duplicate goes.
#[test]
fn a_replacement_identical_to_the_body_it_replaces_is_not_offered() {
    let found = candidates("fn f() -> bool { true }", "fn_value");

    assert!(
        found.iter().all(|c| c.replacement != "true"),
        "a body of `true` must not be replaced by `true`: {found:?}"
    );
    assert!(
        found.iter().any(|c| c.replacement == "false"),
        "the other value must survive: {found:?}"
    );
}

/// Layout must not decide the answer, since the question is whether the compiled program
/// changes. A body spread over several lines is the same program as one on a single line, and
/// comparing the text rather than the tokens would call the second a no-op and the first a
/// mutant.
#[test]
fn the_no_op_test_reads_tokens_rather_than_layout() {
    let found = candidates("fn f() -> Option<u8> {\n    // a note\n    None\n}", "fn_value.none");

    assert!(found.is_empty(), "{found:?}");
}

/// A body doing more than producing the value is not reproduced by that value alone, however
/// the two end. Dropping the statement is exactly what the mutant is asking about, so this one
/// must be kept.
#[test]
fn a_body_ending_in_the_offered_value_is_still_mutated() {
    let found = candidates("fn f(c: &Cell<u8>) -> bool { c.set(1); true }", "fn_value.bool_true");

    assert!(found.iter().any(|c| c.replacement == "true"), "{found:?}");
}

/// `Default::default()` inside `impl Default` names the function it is replacing, so the
/// mutant is unbounded recursion. It cannot be killed by a test seeing a wrong value, only by
/// the stack running out, which costs a full timeout to reach and is the slowest verdict there
/// is. Mutants elsewhere in the same body are ordinary and must be kept — which is why the
/// whole `impl` is not simply skipped.
#[test]
fn the_default_method_of_a_default_impl_is_not_replaced_by_a_call_to_itself() {
    let source = "impl Default for Thing { fn default() -> Self { Thing { n: 7 } } }";
    let found = candidates(source, "all");

    assert!(
        found.iter().all(|c| c.replacement != "Default::default()"),
        "`default` must not be replaced by a call to itself: {found:?}"
    );
    assert!(
        found.iter().any(|c| c.mutator == "literal.int_increment"),
        "other mutants in the same body must be kept: {found:?}"
    );
}

/// The rule is about the trait, not the name. An inherent `default` does not shadow
/// `Default::default`, so replacing its body with one is an ordinary mutant, and a differently
/// named method inside `impl Default` is not what `Default::default()` resolves to either.
#[test]
fn only_the_default_method_of_a_default_impl_loses_that_replacement() {
    let inherent = candidates("impl Thing { fn default() -> Self { Thing { n: 7 } } }", "fn_value.default");

    assert!(!inherent.is_empty(), "an inherent `default` is not the trait method: {inherent:?}");

    let sibling = candidates(
        "impl Default for Thing { fn helper() -> Other { Other { n: 7 } } }",
        "fn_value.default",
    );

    assert!(!sibling.is_empty(), "a sibling method is not the trait method: {sibling:?}");
}

#[test]
fn a_concrete_self_without_default_is_not_replaced_with_default() {
    let source = "struct Thing; impl Thing { fn make() -> Self { Self } }";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("fn_value.default").unwrap();
    let defaults = Defaults::of(&file.ast);
    let found = collect_with(&file, &selection, &CfgSet::unconditional(), &defaults);

    assert!(found.is_empty(), "`Thing` does not implement `Default`: {found:?}");
}

#[test]
fn a_concrete_self_associated_type_without_default_is_not_defaulted() {
    let source = "
        struct Error;
        struct Thing;
        trait Make { type Err; fn make() -> Result<Self, Self::Err>; }
        impl Make for Thing {
            type Err = Error;
            fn make() -> Result<Self, Self::Err> { unreachable!() }
        }
    ";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("fn_value.err_default").unwrap();
    let defaults = Defaults::of(&file.ast);
    let found = collect_with(&file, &selection, &CfgSet::unconditional(), &defaults);

    assert!(found.is_empty(), "`Self::Err` resolves to non-defaultable `Error`: {found:?}");
}

#[test]
fn the_standard_fmt_result_alias_gets_a_compiling_result_value() {
    let found = candidates(
        "use std::fmt::{self, Display}; impl Display for Thing { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(\"x\") } }",
        "fn_value",
    );

    assert!(
        found.iter().any(|candidate| candidate.replacement == "Ok(Default::default())"),
        "`fmt::Result` should be recognized as `Result<(), fmt::Error>`: {found:?}"
    );
    assert!(
        found.iter().all(|candidate| candidate.replacement != "Default::default()"),
        "`Result` itself does not implement `Default`: {found:?}"
    );
}

#[test]
fn standard_time_types_are_not_replaced_with_default() {
    for source in [
        "use std::time::Instant; fn now() -> Instant { Instant::now() }",
        "fn epoch() -> std::time::SystemTime { std::time::UNIX_EPOCH }",
    ] {
        let found = candidates(source, "fn_value.default");

        assert!(found.is_empty(), "the standard time type has no `Default`: {found:?}");
    }
}

/// A reference cannot point at a temporary, so `&Default::default()` would not compile and the
/// family would otherwise pass over every reference-returning function in silence. Leaking a box gives
/// a `&'static mut T`, which lives long enough for any signature, and the values are the
/// element type's own.
///
/// A shared reference is reborrowed rather than left to coerce. Coercion is enough in a return
/// position, but not where the value is what a type is *inferred* from — an
/// `impl Iterator<Item = &T>` would infer `Once<&mut T>` and be withdrawn as unviable.
#[test]
fn a_shared_reference_return_is_served_by_reborrowing_a_leaked_box() {
    let found = candidates("fn f(v: &Vec<String>) -> &String { &v[0] }", "fn_value");

    assert!(
        found.iter().any(|c| c.replacement == "&*Box::leak(Box::new(String::new()))"),
        "{found:?}"
    );
}

/// A mutable reference is not reborrowed, because `Box::leak` already yields exactly that.
#[test]
fn a_mutable_reference_return_is_served_by_the_leak_alone() {
    let found = candidates("fn f(v: &mut Vec<u8>) -> &mut u8 { &mut v[0] }", "fn_value");

    assert!(found.iter().any(|c| c.replacement == "Box::leak(Box::new(1))"), "{found:?}");
}

/// A reference to something abstract still yields nothing. `Box::new` needs a value of the
/// pointee type, and a trait object names a capability rather than a type, so there is none to
/// make — the leak does not rescue this case and must not be offered for it.
#[test]
fn a_reference_to_a_trait_object_is_still_left_alone() {
    let found = candidates("fn f(&self) -> &dyn Debug { &self.inner }", "fn_value");

    assert!(found.is_empty(), "{found:?}");
}

/// Only the last segment of a path is compared against the standard names, so a local type is
/// told apart by the type arguments it carries. `mine::Vec` takes none where the standard `Vec`
/// takes one, and calling `mine::Vec::new()` would not compile; the generic guess is offered
/// instead, which at least stands a chance.
#[test]
fn a_local_type_wearing_a_standard_name_is_not_treated_as_the_standard_one() {
    let source = "mod mine { pub struct Vec { pub n: i32 } } fn f() -> mine::Vec { mine::Vec { n: 4 } }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().all(|c| c.replacement == "Default::default()"), "{found:?}");
}

/// The same rule applies to `Cow`, where the trap is sharper: a replacement that hard-codes
/// `std::borrow::Cow` names a different type from the one being returned.
#[test]
fn a_local_cow_is_not_given_the_standard_cow_constructor() {
    let source = "mod mine { pub struct Cow { pub n: i32 } } fn f() -> mine::Cow { mine::Cow { n: 5 } }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().all(|c| !c.replacement.contains("std::borrow")), "{found:?}");
}

/// Qualification must keep working, since that is why only the last segment is compared. A
/// fully written `std::vec::Vec` is the standard one and keeps every value it had.
#[test]
fn a_fully_qualified_standard_type_is_still_recognised() {
    let source = "fn f() -> std::vec::Vec<i32> { std::vec::Vec::from([1]) }";
    let found = candidates(source, "fn_value");

    assert!(found.iter().any(|c| c.replacement == "std::vec::Vec::new()"), "{found:?}");
}

/// `type Result<T> = core::result::Result<T, MyError>` is everywhere in real crates. The error
/// is whatever the alias fixed it to, which is almost never something with a `Default`, so
/// guessing at it produced a mutant that did not compile. The `Ok` values are unaffected,
/// because those come from the argument the alias does spell.
#[test]
fn an_aliased_result_is_not_given_an_error_it_cannot_name() {
    let found = candidates("fn f() -> Result<i32> { g() }", "fn_value");

    assert!(found.iter().all(|c| c.mutator != "fn_value.err_default"), "{found:?}");
    assert!(found.iter().any(|c| c.replacement == "Ok(0)"), "{found:?}");
}

/// The error value is still offered when the path names the error, which is the case the rule
/// above must not disturb.
#[test]
fn a_result_that_names_its_error_still_offers_one() {
    let found = candidates("fn f() -> Result<i32, String> { g() }", "fn_value");

    assert!(found.iter().any(|c| c.mutator == "fn_value.err_default"), "{found:?}");
}

/// A lifetime is not a type argument, so `Cow<'a, str>` names one type and must still be
/// recognised despite carrying two arguments in the source.
#[test]
fn a_lifetime_does_not_count_towards_the_type_arguments() {
    let found = candidates("fn f() -> Cow<'static, str> { borrow() }", "fn_value");

    assert!(found.iter().any(|c| c.replacement.contains("Cow::Owned")), "{found:?}");
}

fn span_of(text: &str, needle: &str) -> core::ops::Range<usize> {
    let start = text.find(needle).expect("the needle must be in the text");

    start..start + needle.len()
}

/// A selection that reads none of the pre-pass indexes must collect exactly what a full one does.
///
/// The whole-file pre-pass is skipped when no selected mutator consults it, and skipping work is
/// only safe if it is invisible: the candidates a narrow selection yields have to be the ones a
/// selection that *does* build the indexes yields for those same mutators. When the gate was
/// first written it missed `result.ok_to_err`, which reads the import index to decide whether an
/// `Err` is constructible, and five mutants changed — a gate on an index is wrong exactly when
/// its output stops matching the ungated walk.
#[test]
fn a_selection_that_needs_no_pre_pass_collects_what_a_full_one_does() {
    let source = "\
use std::io::Error;
const CAP: usize = 8;
struct Held { count: usize }
fn f(held: &Held) -> Result<usize, Error> {
for i in 0..CAP {
    if held.count < i && i > 1 {
        return Ok(i);
    }
}
Ok(0)
}
";

    let shape = |found: Vec<Candidate>| -> Vec<String> {
        found
            .into_iter()
            .filter(|c| c.mutator.starts_with("relational."))
            .map(|c| format!("{c:?}"))
            .collect()
    };

    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let narrow = shape(collect(&file, &Selection::parse("relational").unwrap()));
    let full = shape(collect(&file, &Selection::everything()));

    assert!(
        !narrow.is_empty(),
        "the fixture must offer relational sites for this to prove anything"
    );
    assert_eq!(narrow, full);
}
/// A file that is not plain ASCII survives collection with its spans, text and columns intact.
///
/// Every other fixture in this workspace is an ASCII string literal with Unix line endings,
/// and every stage downstream of the parser is byte-offset arithmetic. A byte-offset slip on a
/// multi-byte identifier does not fail loudly — it records the wrong `original`, reports the
/// wrong column, and splices the guard through the middle of a character, which is a build
/// failure blamed on the tree rather than on the tool. Real repositories contain all three of
/// these: a byte-order mark, `\r\n` terminators and non-ASCII identifiers and literals.
///
/// The oracle is deliberately not `file.slice`, which is the same arithmetic the recording
/// used. It is `find` over the text, which knows nothing about spans.
#[test]
fn a_file_with_a_byte_order_mark_crlf_endings_and_multibyte_text_keeps_its_spans() {
    let source = "\u{feff}fn tälle(gröÿe: usize) -> bool {\r\n    let ändern = \"ünïcøde\";\r\n\r\n    gröÿe < ändern.len()\r\n}\r\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let found = into_definitions(&file, collect(&file, &Selection::parse("relational").unwrap()));

    assert!(!found.is_empty(), "nothing was collected from a non-ASCII file");

    assert!(
        !file.text.starts_with('\u{feff}'),
        "the mark has to be gone, or every span below is three bytes out"
    );

    for mutant in &found {
        assert_eq!(
            file.text.get(mutant.site.span.clone()),
            Some(mutant.site.original.as_str()),
            "the recorded original is not the text at the span"
        );

        let line = file.text.lines().nth(mutant.site.line - 1).expect("the line exists");

        assert_eq!(
            line.chars().nth(mutant.site.column - 1),
            mutant.site.original.chars().next(),
            "the column does not point at the start of `{}` on line {}",
            mutant.site.original,
            mutant.site.line
        );
    }

    // The site itself, found without going anywhere near a span.
    let comparison = found
        .iter()
        .find(|mutant| mutant.site.original == "gröÿe < ändern.len()")
        .expect("the comparison over multi-byte operands was collected");

    assert_eq!(comparison.site.line, 4, "the CRLF terminators shifted the line number");
    assert_eq!(comparison.site.column, 5, "the multi-byte indent shifted the column");
}

/// The same file instruments into something that still parses and still says what it said.
///
/// Collection recording the right bytes is half of it; the splice has to put them back. A cut
/// on a byte that is not a character boundary produces invalid UTF-8 in the middle of a source
/// file, and a splice that is merely off by a byte silently changes an identifier.
#[test]
fn a_non_ascii_file_instruments_into_source_that_still_parses() {
    let source = "\u{feff}fn tälle(gröÿe: usize) -> bool {\r\n    let ändern = \"ünïcøde\";\r\n\r\n    gröÿe < ändern.len()\r\n}\r\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let found = into_definitions(&file, collect(&file, &Selection::parse("relational").unwrap()));
    let mutations: Vec<_> = found
        .iter()
        .enumerate()
        .map(|(ordinal, mutant)| AssignedMutant::new(Ordinal::new(u32::try_from(ordinal).unwrap()), mutant))
        .collect();
    let instrumented = instrument(&file.text, &mutations).expect("instruments");

    let _ = parse_file(&instrumented).expect("the instrumented file no longer parses");

    for identifier in ["tälle", "gröÿe", "ändern", "ünïcøde"] {
        assert!(
            instrumented.contains(identifier),
            "`{identifier}` did not survive instrumentation:\n{instrumented}"
        );
    }
}

// ---- Values the site states for itself. --------------------------------------------------

/// Every replacement collected under the `fn_value` family, in emission order.
fn fn_values(source: &str) -> Vec<(&'static str, String)> {
    candidates(source, "fn_value")
        .into_iter()
        .map(|c| (c.mutator, c.replacement.to_string()))
        .collect()
}

/// A stated value replaces what the return type made the tool guess.
///
/// The point of the attribute: the author knows a value that is wrong in a way their suite should
/// notice, and `false`, `0` or `String::new()` was not it. Emitting the guess as well would ask
/// the same question twice at one site and charge the suite for failing to answer the version its
/// author had already rejected.
///
/// The last two are the guess that most often turns out not to compile: behind an alias or an
/// associated type there is a concrete type the collector cannot see, and `Default::default()` is
/// a hope that it implements `Default`. A stated value is how that hope becomes knowledge.
#[test]
fn a_stated_value_displaces_the_guessed_ones() {
    for (source, stated, guessed) in [
        ("fn f() -> bool { g() }", "#[gamma::value(h())] fn f() -> bool { g() }", "false"),
        ("fn f() -> i32 { g() }", "#[gamma::value(h())] fn f() -> i32 { g() }", "0"),
        (
            "fn f() -> String { g() }",
            "#[gamma::value(h())] fn f() -> String { g() }",
            "String::new()",
        ),
        (
            "fn f() -> MyAlias { g() }",
            "#[gamma::value(h())] fn f() -> MyAlias { g() }",
            "Default::default()",
        ),
        (
            "impl Trait for S { type Item = u8; fn f(&self) -> Self::Item { g() } }",
            "impl Trait for S { type Item = u8; #[gamma::value(h())] fn f(&self) -> Self::Item { g() } }",
            "Default::default()",
        ),
    ] {
        assert!(
            fn_values(source).iter().any(|(_, value)| value == guessed),
            "the premise is that `{guessed}` is guessed"
        );
        assert_eq!(fn_values(stated), vec![("fn_value.stated", "h()".to_owned())], "for `{source}`");
    }
}

/// The shapes whose types the tool refuses to guess a value for, each with a value stated.
///
/// A type parameter, an associated type reached through one, a trait object and an opaque return
/// are all types the collector cannot see a constructor behind — it parses, it does not resolve —
/// so each one silently costs a site its `fn_value` mutant. The attribute is the way back: the
/// author can see the constructor, and says it.
const WITHHELD: &[(&str, &str)] = &[
    ("fn f<T: Bound>(t: T) -> T { g(t) }", "t"),
    ("fn f<I: Iterator>(i: I) -> I::Item { g(i) }", "i.next().unwrap()"),
    ("fn f() -> Box<dyn Reader> { g() }", "Box::new(Empty)"),
    ("fn f() -> impl Reader { g() }", "Empty"),
];

/// A site the tool declines to guess for gets a mutant once the value is stated.
///
/// Half of F4's reason to exist: these sites are not hard to mutate, they are hard to *guess* a
/// mutant for, and the difference showed up as a hole in the population rather than as a question.
#[test]
fn a_stated_value_adds_a_mutant_where_none_was_guessed() {
    for (source, expression) in WITHHELD {
        assert!(
            fn_values(source).is_empty(),
            "the premise is that `{source}` yields no guessed value"
        );

        let stated = format!("#[gamma::value({expression})]\n{source}");

        assert_eq!(
            fn_values(&stated),
            vec![("fn_value.stated", (*expression).to_owned())],
            "for `{source}`"
        );
    }
}

/// An `impl Iterator` return keeps the splice its type needs when the value is stated.
///
/// Both arms of the guard have to be wrapped so they share one type, and that is decided by the
/// signature rather than by where the value came from. Emitting a stated value as a plain block
/// would produce a mutant that cannot compile for a reason the author had no way to see.
#[test]
fn a_stated_value_on_an_iterator_return_keeps_the_iterator_shape() {
    let found = candidates(
        "#[gamma::value(core::iter::empty())]\nfn f() -> impl Iterator<Item = u8> { g() }",
        "fn_value",
    );

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].shape, Shape::IterBlock);
}

/// The expression reaches the mutant exactly as written.
///
/// It is shown in reports and spliced into the file, so a re-rendering that turns `Config { n: 1 }`
/// into `Config { n : 1 }` puts text in the author's diff that the author did not write.
#[test]
fn the_stated_expression_reaches_the_mutant_verbatim() {
    let found = fn_values("#[gamma::value(Some(Config { n: 1 }))]\nfn f() -> Option<Config> { g() }");

    assert_eq!(found, vec![("fn_value.stated", "Some(Config { n: 1 })".to_owned())]);
}

/// A stated value that cannot possibly type-check is still collected.
///
/// F4's rule about trust: the tool does not type-check, so it cannot tell a wrong value from a
/// right one, and guessing would mean quietly dropping the mutants of authors who were right.
/// A wrong one becomes a mutant that fails to build and is withdrawn by the same rollback that
/// withdraws every other unviable mutant, which is a reported outcome rather than a silence.
#[test]
fn a_stated_value_of_the_wrong_type_is_collected_rather_than_dropped() {
    let found = fn_values("#[gamma::value(\"not a number\")]\nfn f() -> i32 { g() }");

    assert_eq!(found, vec![("fn_value.stated", "\"not a number\"".to_owned())]);
}

/// Stating a value adds a mutant; it never takes one away.
///
/// The other half of F4's rule: the attribute is not a suppression channel. If stating a value
/// could remove mutants, an author could quietly delete the site's other questions by answering
/// one of them, and the mutation score would rise for a reason nobody reviewed.
#[test]
fn stating_a_value_leaves_every_other_family_alone() {
    let body = "fn f(a: i32, b: i32) -> i32 { if a < b { a + 1 } else { b * 2 } }";
    let plain = mutators(body, "all");
    let stated = mutators(&format!("#[gamma::value(41)]\n{body}"), "all");

    for mutator in &plain {
        if mutator.starts_with("fn_value") {
            continue;
        }

        assert_eq!(
            plain.iter().filter(|m| *m == mutator).count(),
            stated.iter().filter(|m| *m == mutator).count(),
            "`{mutator}` changed when a value was stated"
        );
    }
}

/// The named error values keep the replacement indices they had before.
///
/// Their indices continue the guessed list's, so if a stated value shortened that list the
/// `--error` mutants at every annotated site would be renumbered, and a renumbered mutant is a
/// new id: suppressions by id stop matching and an incremental run re-runs work it had settled.
#[test]
fn a_stated_value_does_not_renumber_the_named_error_mutants() {
    let plain = with_errors("fn f() -> Result<i32, MyError> { Ok(1) }", &["MyError::Io"]);
    let stated = with_errors("#[gamma::value(Ok(7))]\nfn f() -> Result<i32, MyError> { Ok(1) }", &["MyError::Io"]);

    let indices = |found: &[Candidate]| -> Vec<u32> {
        found
            .iter()
            .filter(|c| c.mutator == "fn_value.err_with")
            .map(|c| c.replacement_index)
            .collect()
    };

    assert_eq!(indices(&plain), vec![4], "the premise is that the errors follow the guessed values");
    assert_eq!(indices(&stated), indices(&plain));
}

/// A stated mutant does not inherit the identity of the guess it displaced.
///
/// Identity is a hash of the mutator name and the site, not of the replacement text, so reusing
/// `fn_value.default` for a stated value would give the two the same id — and a cached
/// `CompileError` against the guess would then withhold the very mutant the author wrote the
/// attribute to obtain, without a word about why.
#[test]
fn a_stated_mutant_has_an_identity_of_its_own() {
    let identify = |source: &str| -> String {
        let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
        let found = collect(&file, &Selection::parse("fn_value").unwrap());
        let mutants = into_definitions(&file, found);

        mutants
            .into_iter()
            .map(|m| m.id.to_string())
            .next()
            .expect("the fixture yields a mutant")
    };

    let guessed = identify("fn f() -> bool { g() }");
    let stated = identify("#[gamma::value(true)]\nfn f() -> bool { g() }");

    assert_ne!(guessed, stated);
}

/// A file with no attribute remains pinned to the site-counted identity baseline.
///
/// The baseline includes the corrected rule that replacements at one site share an occurrence.
/// Once established, these ids must remain stable across runs because stored verdicts and
/// suppressions name mutants by id.
#[test]
fn a_file_that_states_nothing_uses_the_site_counted_identity_baseline() {
    let source = "fn a() -> bool { g() }\nfn b() -> Result<i32, MyError> { Ok(1) }\nfn c(x: i32) -> i32 { x + 1 }\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let found = collect(&file, &Selection::parse("all").unwrap());
    let ids: Vec<String> = into_definitions(&file, found).into_iter().map(|m| m.id.to_string()).collect();

    assert_eq!(
        ids,
        vec![
            "2b643388f655",
            "6ecb982becec",
            "c0fc1efe0f11",
            "3c5e85253765",
            "0a42ad003f48",
            "496b6928efc8",
            "2c30ededa685",
            "6b3d0f1f4c8a",
            "7db3aae5c3f8",
            "14e9e208d250",
            "1a5c3cad1dc5",
            "e7e1046f74d6",
            "bee0a88bd144",
            "3e4f9ce3258f",
            "03ddbb807ce2",
            "680f8f10ad99",
            "1ad3b0366e51"
        ],
        "collected ids moved for a file that states no values"
    );
}

/// Methods and trait methods are annotated the same way free functions are.
///
/// The three positions are three different `syn` nodes and three different call sites in the
/// collector, so "it works on a function" says nothing about the two places most trait-shaped
/// code actually lives.
#[test]
fn a_value_can_be_stated_on_a_method_or_a_trait_method() {
    let sources = [
        "impl S { #[gamma::value(9)] fn f(&self) -> u8 { g() } }",
        "trait T { #[gamma::value(9)] fn f(&self) -> u8 { g() } }",
    ];

    for source in sources {
        assert_eq!(fn_values(source), vec![("fn_value.stated", "9".to_owned())], "for `{source}`");
    }
}

/// A stated value obeys the selection like every other mutator.
///
/// The attribute states which expression to substitute, not that the substitution must happen:
/// a run narrowed to arithmetic asked for arithmetic, and a mutator that ignored the selection
/// because a source file asked nicely would make `--mutators` mean something different per file.
#[test]
fn a_stated_value_is_still_subject_to_the_selection() {
    let source = "#[gamma::value(9)]\nfn f(a: u8) -> u8 { a + 1 }";

    assert!(
        mutators(source, "arith").iter().all(|m| *m != "fn_value.stated"),
        "{:?}",
        mutators(source, "arith")
    );
    assert!(mutators(source, "fn_value.stated").contains(&"fn_value.stated"));

    // The other half of the rule, and the one the attribute must never break: a selection holding
    // a sibling `fn_value` mutator but not the stated one still gets that sibling's mutant. The
    // attribute says which expression a site substitutes; a narrowing flag must not turn it into a
    // way of taking the site's only `fn_value` mutant away, because that shrinks the denominator
    // and raises the score with nothing said about it anywhere.
    let narrowed = mutators(source, "fn_value.zero");

    assert_eq!(narrowed, vec!["fn_value.zero"], "{narrowed:?}");
}

/// A stated value on a `const fn` produces nothing, as every function value does there.
///
/// The guard around the body cannot be called in a const context, so the mutant could not be
/// built whatever its value is. Honouring the attribute here would emit a mutant that fails to
/// compile for a reason that has nothing to do with what the author stated.
#[test]
fn a_stated_value_on_a_const_fn_produces_nothing() {
    assert!(fn_values("#[gamma::value(9)]\nconst fn f() -> u8 { g() }").is_empty());
}

// ---- Every catalog entry must actually fire. ---------------------------------------------
/// A composite fixture holding at least one construct for every mutator family in the registry.
///
/// One function per shape rather than one dense function, so that a mutator that fails to fire
/// can be diagnosed by reading the single function that was supposed to feed it. Undefined
/// callees and types are deliberate: collection only parses, it never type-checks, so the
/// fixture can name the exact return shapes the `fn_value` family keys on without carrying the
/// machinery to make them real.
const EVERY_FAMILY_FIXTURE: &str = r#"
fn returns_unit() { do_work(); }
fn returns_bool() -> bool { compute() }
fn returns_signed() -> i32 { compute() }
fn returns_unsigned() -> usize { compute() }
fn returns_float() -> f64 { compute() }
fn returns_nonzero() -> core::num::NonZeroU32 { compute() }
fn returns_string() -> String { compute() }
fn returns_static_str() -> &'static str { compute() }
fn returns_option() -> Option<i32> { compute() }
fn returns_option_ref() -> Option<&'static dyn core::fmt::Debug> { compute() }
fn returns_result() -> Result<i32, MyError> { compute() }
fn returns_result_ref() -> Result<&'static dyn core::fmt::Debug, MyError> { compute() }
fn returns_vec() -> Vec<i32> { compute() }
fn returns_tuple() -> (i32, bool) { compute() }
fn returns_custom() -> Custom { compute() }

#[gamma::value(Custom::EMPTY)]
fn returns_a_stated_value() -> Custom { compute() }

fn relational_ops(a: i32, b: i32) -> bool {
    a < b && a <= b && a > b && a >= b && a == b && a != b
}

fn arithmetic_ops(a: i32, b: i32) -> i32 {
    a + b - a * b / a % b
}

fn bitwise_ops(a: i32, b: i32) -> i32 {
    a & b | a ^ b
}

fn shift_ops(a: i32, b: i32) -> i32 {
    (a << b) >> b
}

fn logical_ops(x: bool, y: bool) -> bool {
    x && y || x
}

fn assign_ops(a: &mut i32, b: i32) {
    *a += b;
    *a -= b;
    *a *= b;
    *a /= b;
    *a %= b;
    *a &= b;
    *a |= b;
    *a ^= b;
    *a <<= b;
    *a >>= b;
}

fn cond_op(flag: bool) -> i32 {
    if flag { 1 } else { 2 }
}

fn guard_op(v: i32) -> i32 {
    match v {
        n if n > 0 => n,
        _ => 0,
    }
}

fn arm_op(v: i32) -> i32 {
    match v {
        1 => 10,
        2 => 20,
        _ => 0,
    }
}

fn struct_op() -> C {
    C { a: 1, b: 2, ..Default::default() }
}

fn range_op(n: usize) -> usize {
    let mut t = 0;
    for i in 0..n {
        t += i;
    }
    for j in 0..=n {
        t += j;
    }
    t
}

fn loop_op(v: &[i32]) {
    for x in v {
        if *x == 0 {
            continue;
        }
        if *x == 1 {
            break;
        }
    }
}

fn unary_op(a: i32, c: bool) -> i32 {
    let _ = !c;
    -a
}

fn literal_op() -> i32 {
    let _b = true;
    let _s = "hello";
    5
}

fn stmt_op(v: &mut Vec<i32>) {
    let mut a = 0;
    v.push(1);
    a = 2;
    let _ = a;
}

fn expr_op(n: usize) {
    sink(n);
}

fn option_op(flag: bool) {
    let _ = if flag { Some(1) } else { None };
}

fn result_op(flag: bool) {
    let _ = if flag { Ok(1) } else { Err(2) };
}

fn iter_op(v: &[u32], mut w: Vec<u32>) {
    let _ = v.iter().any(|n| *n > 0);
    let _ = v.iter().all(|n| *n > 0);
    let _ = v.iter().min();
    let _ = v.iter().max();
    let _ = v.first();
    let _ = v.last();
    w.sort();
    w.dedup();
}

fn string_op(s: &str) {
    let _ = s.starts_with("a");
    let _ = s.ends_with("b");
    let _ = s.to_lowercase();
    let _ = s.to_uppercase();
    let _ = s.trim_start();
    let _ = s.trim_end();
}

fn collection_op() {
    let _ = vec![1, 2, 3];
}

fn assign_value_op(mut n: u32) {
    n = n + 1;
    let _ = n;
}
"#;

/// Every entry in `REGISTRY` must produce at least one candidate that is attributed to it.
///
/// This is the one guard the aggregate tests structurally cannot be: a mutator that silently
/// fires zero times is invisible everywhere else. It compiles, it passes
/// `every_mutator_has_a_description`, it passes `registry_names_are_unique`, it shows up in
/// `cargo gamma list mutators` and in the generated documentation tables — and it never produces
/// a mutant. The consequence is the worst failure this tool has: a non-firing entry shrinks the
/// mutant population, which shrinks the denominator, which makes the mutation *score go up*, so a
/// tool whose entire purpose is to tell a user their tests are weaker than they think reports
/// that they are stronger. The same failure hides a mutator that regresses to producing nothing
/// after a refactor of the collectors.
///
/// Because only the single entry under test is selected, every candidate collected is already
/// attributed to it, so `any(mutator == name)` asserts both that something fired and that it is
/// the right thing. There is deliberately no skip-list: an entry that will not fire is either a
/// gap in this fixture (extend the fixture) or a real defect in its collector (report it), and
/// the failure message names the entry, its family and its description so a maintainer can tell
/// the two apart at a glance rather than being told only that "some entry" is broken.
#[test]
fn every_registry_entry_produces_at_least_one_candidate_against_the_composite_fixture() {
    let file = SourceFile::parse("fixture.rs", EVERY_FAMILY_FIXTURE.to_owned()).unwrap();

    for mutator in REGISTRY {
        let mut selection = Selection::parse(mutator.name).unwrap();

        // `fn_value.err_with` consumes caller-supplied `Err(...)` payloads and produces nothing
        // until some are named, exactly as it does on the command line.
        if mutator.name == "fn_value.err_with" {
            selection.set_errors(vec!["MyError::Boom".to_owned()]);
        }

        let found = collect(&file, &selection);
        let family = mutator.name.split('.').next().unwrap_or(mutator.name);

        assert!(
            found.iter().any(|candidate| candidate.mutator == mutator.name),
            "registry entry `{}` (family `{family}`: {}) fired against nothing in the composite fixture. \
             Either the fixture lacks the syntax this mutator needs — extend `EVERY_FAMILY_FIXTURE` — or the \
             collector has regressed to emitting nothing for it, which silently shrinks the mutant population \
             and inflates the mutation score. Candidates found under this selection: {:?}",
            mutator.name,
            mutator.description,
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
        );
    }
}

/// The identifying fields returned by [`candidate_key`].
///
/// The alias lets [`separately_and_fused`] return both candidate lists without repeating the tuple
/// type.
type CandidateKey = (Range<usize>, &'static str, CompactString, u32, String, Shape);

/// A candidate reduced to the fields that identify it, so two independently produced `Vec`s can be
/// compared without `Candidate` needing `PartialEq` for production code that never asks two of them
/// whether they are equal.
fn candidate_key(candidate: &Candidate) -> CandidateKey {
    (
        candidate.span.clone(),
        candidate.mutator,
        candidate.replacement.clone(),
        candidate.replacement_index,
        candidate.item_path.to_string(),
        candidate.shape,
    )
}

/// The fused pre-pass exists to audit stated values while building collection indexes; it must
/// never produce a different answer. Every family in [`EVERY_FAMILY_FIXTURE`] — including the
/// numeric evidence and import indexes only [`check_stated_and_collect_with`]'s pre-pass builds —
/// is selected here, so the candidate pass reads the same indexes [`collect_with`] would have
/// built separately.
///
/// Both sides are compared through [`candidate_key`] rather than by ordering the raw `Vec`s: both
/// already come out of [`finish`](super::traversal) sorted by the same span-then-mutator-then-index
/// rule, so the two lists line up position for position without this test inventing an ordering of
/// its own.
#[test]
fn the_fused_pass_produces_the_same_candidates_as_check_stated_then_collect_with() {
    let file = SourceFile::parse("fixture.rs", EVERY_FAMILY_FIXTURE.to_owned()).unwrap();
    let selection = {
        let names = REGISTRY.iter().map(|mutator| mutator.name).collect::<Vec<_>>().join(",");
        let mut selection = Selection::parse(&names).unwrap();
        selection.set_errors(vec!["MyError::Boom".to_owned()]);
        selection
    };
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);

    check_stated(&file).expect("the composite fixture states nothing that cannot be honoured");
    let separately = collect_with(&file, &selection, &cfg, &defaults);
    let fused = check_stated_and_collect_with(&file, &selection, &cfg, &defaults).expect("the composite fixture has no fault to report");

    assert!(
        !fused.is_empty(),
        "the fixture is expected to produce candidates under the full registry selection"
    );
    assert_eq!(
        fused.len(),
        separately.len(),
        "the fused pass must offer as many candidates as the two separate passes would"
    );

    let separately: Vec<_> = separately.iter().map(candidate_key).collect();
    let fused: Vec<_> = fused.iter().map(candidate_key).collect();

    assert_eq!(
        fused, separately,
        "the fused pass must offer exactly the candidates the two separate passes would, in the same order"
    );
}

/// The fault the fused pass reports for a misplaced or malformed stated value must be the one
/// [`check_stated`] alone would have reported, in the same wording — a caller switching to the
/// fused entry point must never see its error messages, or its decision to stop before collecting
/// any candidates, change.
#[test]
fn the_fused_pass_reports_the_same_fault_as_check_stated_and_collects_nothing() {
    let source = "#[gamma::value(0)]\n#[gamma::value(1)]\nfn f() -> u32 { 2 }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("arith.add_to_sub").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);

    let expected = check_stated(&file)
        .expect_err("the fixture states two values on one item")
        .to_string();
    let actual = check_stated_and_collect_with(&file, &selection, &cfg, &defaults)
        .expect_err("the fused pass must reject what check_stated alone rejects")
        .to_string();

    assert_eq!(
        actual, expected,
        "the fused pass must report the identical fault check_stated would have reported alone"
    );
}

/// Runs both entry points over one fixture and returns their candidates side by side, reduced to
/// the fields that identify each.
///
/// Written once rather than per fixture because the invariant the tests below check is always the
/// same one: the pre-pass the fused entry point runs must not learn anything the pass
/// [`collect_with`] runs internally would not, or the two disagree about which guesses an active
/// site is entitled to.
fn separately_and_fused(source: &str, ops: &str, cfg: &CfgSet) -> (Vec<CandidateKey>, Vec<CandidateKey>) {
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::parse(ops).unwrap();
    let defaults = Defaults::of(&file.ast);

    let separately = collect_with(&file, &selection, cfg, &defaults);
    let fused = check_stated_and_collect_with(&file, &selection, cfg, &defaults).expect("the fixture has no fault to report");

    (
        separately.iter().map(candidate_key).collect(),
        fused.iter().map(candidate_key).collect(),
    )
}

/// Code the selected build strips is not code either pass may learn from, and the two passes must
/// strip exactly the same code.
///
/// The gate is written twice — once in `indexes::Walk`'s own visitor, which `collect_with` drives,
/// and once in `phase_one::PhaseOne`'s, which the fused entry point drives — so the two can drift
/// apart with nothing else noticing. The fixture makes such a drift visible in the candidates
/// rather than only in the indexes: `count` is a number in the active struct and a `String` in the
/// inactive one, so whichever side indexed the inactive field would demote the name to unknown and
/// withhold the perturbation the active site is entitled to.
///
/// An enforced set is essential — [`CfgSet::unconditional`] answers every predicate `true`, so
/// nothing would be stripped and the fixture would test nothing.
#[test]
fn the_fused_pass_strips_the_same_inactive_code_the_separate_passes_do() {
    let source = r#"
        struct Inactive {
            #[cfg(windows)]
            count: String,
        }

        struct Active {
            count: u32,
        }

        #[cfg(windows)]
        const LIMIT: &str = "not built";

        const LIMIT: u32 = 1;

        fn f(record: &Active) -> u32 {
            record.count + LIMIT
        }
    "#;
    let cfg = CfgSet::parse("unix\n");

    let (separately, fused) = separately_and_fused(source, "expr.increment,expr.decrement,arith.add_to_sub", &cfg);

    assert!(!fused.is_empty(), "the fixture is expected to produce candidates");
    assert_eq!(
        fused, separately,
        "the fused pass must strip exactly what the separate passes strip"
    );
}

/// A field-level gate applies before the stated-value audit sees the field's attributes.
#[test]
fn the_fused_pass_does_not_audit_stated_values_on_inactive_fields() {
    let source = r"
        struct Record {
            #[cfg(windows)]
            #[gamma::value(not a valid value)]
            count: u32,
            active: u32,
        }

        fn f(record: &Record) -> u32 {
            record.active + 1
        }
    ";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("arith.add_to_sub").unwrap();
    let cfg = CfgSet::parse("unix\n");
    let defaults = Defaults::of_in(&file.ast, &cfg);

    let candidates = check_stated_and_collect_with(&file, &selection, &cfg, &defaults)
        .expect("a malformed stated value on an inactive field is outside the selected build");

    assert!(!candidates.is_empty(), "the active function still supplies a candidate");
}

/// The collector offers no mutant in test code, so neither pass may draw evidence from it. This is
/// the case `CfgSet::holds_for` alone got wrong: `cfg(test)` *holds* for the instrumented build,
/// so a pre-pass reading it indexed the helper below while the collector skipped it, and the
/// guesses made about the active `count` were drawn from a struct the measured code has nothing to
/// do with.
///
/// No enforced set is needed here, because the test gate is decided without consulting whether
/// predicates are enforced at all — which is also why this fixture was wrong for every caller that
/// passes an unconditional set.
#[test]
fn the_fused_pass_strips_the_same_test_gated_code_the_separate_passes_do() {
    let source = r#"
        #[cfg(test)]
        mod tests {
            struct Helper {
                count: String,
            }

            const LIMIT: &str = "fixture";
        }

        struct Active {
            count: u32,
        }

        const LIMIT: u32 = 1;

        fn f(record: &Active) -> u32 {
            record.count + LIMIT
        }
    "#;
    let cfg = CfgSet::unconditional();

    let (separately, fused) = separately_and_fused(source, "expr.increment,expr.decrement,arith.add_to_sub", &cfg);

    assert!(!fused.is_empty(), "the fixture is expected to produce candidates");
    assert_eq!(
        fused, separately,
        "the fused pass must strip exactly what the separate passes strip"
    );
}

/// Conditional compilation inside a body is written on statements and locals, which no item
/// visitor reaches — so this is the level the two visitors are most likely to drift apart at, and
/// the level `indexes_in`'s own unit tests pin against the collector's rule.
#[test]
fn the_fused_pass_strips_the_same_inactive_statements_the_separate_passes_do() {
    let source = r"
        fn f(data: &[u8]) -> usize {
            #[cfg(windows)]
            let _ = 1 < only_on_windows;

            let count: usize = 1;

            count + data.len()
        }
    ";
    let cfg = CfgSet::parse("unix\n");

    let (separately, fused) = separately_and_fused(source, "expr.increment,expr.decrement,arith.add_to_sub", &cfg);

    assert!(!fused.is_empty(), "the fixture is expected to produce candidates");
    assert_eq!(
        fused, separately,
        "the fused pass must strip exactly what the separate passes strip"
    );
}

/// Every stated value the two channels accept has to reach a candidate, or the attribute is a hint
/// that reads as working and measures nothing — the failure the `const fn` and empty-body
/// rejections in `stated::check` exist to make impossible.
///
/// Every position in which a function is written is covered, because a gate applied to `ItemFn`
/// alone would leave methods and trait methods silent.
#[test]
fn every_accepted_stated_value_emits_a_candidate() {
    let sources = [
        "#[gamma::value(7)]\nfn f() -> u32 { 1 }",
        "struct S;\nimpl S {\n#[gamma::value(7)]\nfn f(&self) -> u32 { 1 }\n}",
        "trait T {\n#[gamma::value(7)]\nfn f(&self) -> u32 { 1 }\n}",
    ];

    for source in sources {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();

        check_stated(&file).unwrap_or_else(|error| panic!("`{source}` states a value on a function: {error}"));

        let found = candidates(source, "fn_value.stated");

        assert_eq!(
            found.iter().map(|candidate| candidate.replacement.as_str()).collect::<Vec<_>>(),
            vec!["7"],
            "`{source}` states a value that must become a mutant"
        );
    }
}

/// The converse of the test above: the two positions collection returns from before it reads a
/// stated value emit nothing, which is exactly why `stated::check` refuses the attribute there.
/// If either of these ever started producing a candidate, that refusal would have become wrong.
#[test]
fn the_positions_a_stated_value_is_refused_on_emit_no_candidate() {
    for source in ["#[gamma::value(7)]\nconst fn f() -> u32 { 1 }", "#[gamma::value(())]\nfn f() {}"] {
        assert!(
            candidates(source, "fn_value.stated").is_empty(),
            "`{source}` must emit nothing, which is what makes refusing the attribute correct"
        );

        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();

        let _rejected = check_stated(&file).expect_err("an inert position must be reported rather than silently ignored");
    }
}
