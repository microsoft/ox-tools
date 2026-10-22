// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cargo_gamma_engine::ops::collect::Confidence;

use super::{candidates, tokens, type_evidence_fixture};

const SOURCE: &str = include_str!("../fixtures/correctness/type_evidence.rs");

#[test]
fn exact_type_negative_population_is_removed_and_supported_mutants_still_compile() {
    let (file, found) = candidates(
        SOURCE,
        "fn_value,arith,iter.take_to_skip,option.some_to_none,loop.break_value_default",
    );
    let invalid = [
        ("qualified", "fn_value.default"),
        ("missing::local", "fn_value.default"),
        ("renamed", "fn_value.default"),
        ("linked", "fn_value.default"),
        ("reexported", "fn_value.default"),
        ("empty", "fn_value.one_element"),
        ("wrapper", "fn_value.default"),
        ("break_value", "loop.break_value_default"),
        ("custom_some", "option.some_to_none"),
        ("reader", "iter.take_to_skip"),
        ("buffered", "iter.take_to_skip"),
        ("reader_chain", "iter.take_to_skip"),
        ("opened", "iter.take_to_skip"),
        ("field_duration", "arith.mul_to_add"),
        ("propagated", "arith.mul_to_add"),
        ("duration_pair", "arith.add_to_mul"),
        ("duration_division", "arith.div_to_rem"),
        ("text", "arith.add_to_sub"),
        ("text", "arith.add_to_mul"),
    ];
    for (item, mutator) in invalid {
        assert!(super::Selection::parse(mutator).is_ok(), "unknown selector: {mutator}");
        assert!(
            !found
                .iter()
                .any(|candidate| candidate.item_path.as_ref() == item && candidate.mutator == mutator),
            "{item}: {mutator} remains in the invalid fixture population: {found:#?}"
        );
    }
    for (item, mutator) in [
        ("empty", "fn_value.empty_collection"),
        ("optional", "fn_value.none"),
        ("empty_wrapper", "fn_value.empty_collection"),
        ("defaulted", "fn_value.default"),
        ("ready::local", "fn_value.default"),
        ("configured", "fn_value.default"),
        ("alias_defaulted", "fn_value.default"),
        ("field_duration", "arith.mul_to_div"),
        ("field_numeric", "arith.mul_to_add"),
        ("propagated", "arith.mul_to_div"),
        ("duration_pair", "arith.add_to_sub"),
        ("duration_division", "arith.div_to_mul"),
        ("iterator", "iter.take_to_skip"),
        ("standard_some", "option.some_to_none"),
        ("alias_some", "option.some_to_none"),
        ("imported_some", "option.some_to_none"),
        ("custom_imported", "option.some_to_none"),
        ("custom_api", "iter.take_to_skip"),
    ] {
        assert!(
            found
                .iter()
                .any(|candidate| candidate.item_path.as_ref() == item && candidate.mutator == mutator),
            "{item}: supported {mutator} was lost: {found:#?}"
        );
    }
    assert!(found.iter().any(|candidate| {
        candidate.item_path.as_ref() == "field_duration"
            && candidate.mutator == "arith.mul_to_div"
            && tokens(&candidate.replacement) == tokens("(value.span) / (scalar)")
            && file.slice(&candidate.span) == "value.span * scalar"
    }));
    let (divided, subtracted, ratio, empty, none, skipped) =
        type_evidence_fixture::compiled_mutants(std::time::Duration::from_secs(6), std::time::Duration::from_secs(2), 3);
    assert_eq!(divided.as_secs(), 2);
    assert_eq!(subtracted.as_secs(), 4);
    assert!((ratio - 3.0).abs() < f64::EPSILON);
    assert!(empty.is_empty());
    assert!(none.is_none());
    assert_eq!(skipped, 2);
}

#[test]
fn unknown_dependencies_custom_operators_and_ambiguous_imports_stay_optimistic() {
    let source = r"
        use dependency::Unknown;
        fn unknown(value: Unknown) -> Unknown { value + value }
        mod one { pub struct Duration; }
        use one::Duration;
        fn custom(value: Duration) -> Duration { value * value }
        use dependency::*;
        fn ambiguous(value: Duration) -> Duration { value * value }
    ";
    let (_, found) = candidates(source, "arith");
    for item in ["unknown", "custom", "ambiguous"] {
        assert!(found.iter().any(|candidate| candidate.item_path.as_ref() == item), "{item}");
    }
}

#[test]
fn removing_an_unknown_overloaded_not_performs_no_operator_calls() {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::custom_not_fixture::{IncrementingNot, compiled_removed_not, overloaded_not};

    let (file, found) = candidates(include_str!("../fixtures/correctness/custom_not.rs"), "unary.remove_not");
    let selected = found
        .into_iter()
        .filter(|candidate| candidate.item_path.as_ref() == "overloaded_not")
        .collect::<Vec<_>>();
    let replacements = selected
        .iter()
        .map(|candidate| (candidate.replacement.as_str(), candidate.confidence))
        .collect::<Vec<_>>();
    assert_eq!(replacements, [("value.clone()", Confidence::Optimistic)]);
    let mutants = super::collect::into_mutants(&file, "subject", selected);
    assert_eq!(mutants.len(), 1);
    let calls = Rc::new(Cell::new(0));
    let value = IncrementingNot {
        value: 7,
        calls: Rc::clone(&calls),
    };
    assert_eq!(overloaded_not(&value).value, 8);
    assert_eq!(calls.get(), 1);
    calls.set(0);
    assert_eq!(compiled_removed_not(&value).value, 7);
    assert_eq!(calls.get(), 0);
}

#[test]
fn custom_operator_outputs_and_range_obligations_are_exact() {
    use super::custom_operator_fixture::{Number, compiled_changed_result, compiled_endpoint, compiled_subtraction};

    let (_, found) = candidates(include_str!("../fixtures/correctness/custom_operators.rs"), "arith,range");
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "positions" && candidate.mutator.starts_with("range."))
    );
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "numbers" && candidate.mutator.starts_with("range."))
    );
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "overloaded" && candidate.mutator == "arith.add_to_sub")
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "overloaded" && candidate.mutator == "arith.add_to_mul")
    );
    assert_eq!(compiled_subtraction(Number(8), 3).0, 5);
    assert_eq!(compiled_changed_result(Number(8), 3), 24);
    assert_eq!(compiled_endpoint(Number(2), Number(5)).end.0, 6);
}

#[test]
fn temporal_operator_results_match_compiled_counterparts() {
    use super::temporal_operator_fixture::{compiled_naive_shift, compiled_scale, compiled_shift};

    let (_, found) = candidates(include_str!("../fixtures/correctness/temporal_operators.rs"), "arith,range");
    for (item, selector) in [
        ("difference", "arith.sub_to_add"),
        ("difference", "arith.sub_to_div"),
        ("shifted", "arith.add_to_mul"),
        ("naive_shifted", "arith.add_to_mul"),
        ("scaled", "arith.div_to_rem"),
        ("endpoint", "range.exclusive_to_inclusive"),
    ] {
        assert!(super::Selection::parse(selector).is_ok(), "unknown selector: {selector}");
        assert!(
            !found
                .iter()
                .any(|candidate| candidate.item_path.as_ref() == item && candidate.mutator == selector),
            "{item}: {selector}"
        );
    }
    for (item, selector) in [
        ("shifted", "arith.add_to_sub"),
        ("naive_shifted", "arith.add_to_sub"),
        ("scaled", "arith.div_to_mul"),
    ] {
        assert!(
            found
                .iter()
                .any(|candidate| candidate.item_path.as_ref() == item && candidate.mutator == selector),
            "{item}: {selector}"
        );
    }
    let instant = chrono::DateTime::<chrono::Utc>::from_timestamp(100, 0).unwrap();
    let delta = chrono::TimeDelta::seconds(4);
    assert_eq!(compiled_shift(instant, delta).timestamp(), 96);
    assert_eq!(
        compiled_naive_shift(instant.naive_utc(), std::time::Duration::from_secs(4))
            .and_utc()
            .timestamp(),
        96
    );
    assert_eq!(compiled_scale(delta, 3).num_seconds(), 12);
}

#[test]
fn checked_in_multifile_counterparts_compile_and_preserve_valid_container_and_alias_values() {
    use super::package_identity_fixture::consumers::{compiled_alias_none, compiled_alternate, compiled_custom_none, compiled_empty};

    assert!(compiled_empty().is_empty());
    let _defaulted = compiled_alternate();
    assert!(compiled_alias_none().is_none());
    assert!(matches!(
        compiled_custom_none(),
        super::package_identity_fixture::features::Optional::None
    ));

    let (_, found) = candidates(
        "use unresolved_dependency::MetadataFeatureOption; fn unknown() { let _ = MetadataFeatureOption::Some(1); }",
        "option.some_to_none",
    );
    assert!(found.iter().any(|candidate| candidate.mutator == "option.some_to_none"));
}

#[test]
fn read_trait_identity_does_not_override_supported_file_cursor_or_wrapper_extensions() {
    use super::reader_api_fixture::{Wrapper, compiled_cursor_skip, compiled_wrapper_skip, cursor_take, wrapper_take};

    let (_, found) = candidates(include_str!("../fixtures/correctness/reader_api_identity.rs"), "iter.take_to_skip");
    for item in ["file_take", "cursor_take", "wrapper_take", "custom_view::iterator_take"] {
        assert!(
            found.iter().any(|candidate| candidate.item_path.as_ref() == item),
            "{item}: {found:#?}"
        );
    }
    for item in ["read_cases::read_take", "custom_view::direct_read"] {
        assert!(
            !found.iter().any(|candidate| candidate.item_path.as_ref() == item),
            "{item}: {found:#?}"
        );
    }
    let input = || std::io::Cursor::new(vec![0, 1, 2, 3]);
    assert_eq!(cursor_take(input()).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(compiled_cursor_skip(input()).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(wrapper_take(Wrapper(input())).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(compiled_wrapper_skip(Wrapper(input())).collect::<Vec<_>>(), [2, 3]);
}
