// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{Candidate, Confidence, Defaults, collect_with};
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

fn discover(source: &str, selector: &str, mode: &str) -> Vec<Candidate> {
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let cfg = CfgSet::parse("");
    let defaults = match mode {
        "labeled" => {
            let mut defaults = Defaults::of_source_in(&file.ast, &cfg, "fixture.rs");
            defaults.bind_sources(&[("fixture.rs".into(), "fixture.rs".into(), Vec::new())]);
            defaults
        }
        "optimistic" => Defaults::optimistic_of_in(&file.ast, &cfg),
        _ => Defaults::of_in(&file.ast, &cfg),
    };
    collect_with(&file, &Selection::parse(selector).unwrap(), &cfg, &defaults)
}

fn replacements(found: &[Candidate], item: &str) -> Vec<(String, Confidence)> {
    let mut result = found
        .iter()
        .filter(|candidate| candidate.item_path.as_ref() == item)
        .map(|candidate| (candidate.replacement.to_string(), candidate.confidence))
        .collect::<Vec<_>>();
    result.sort_by(|left, right| left.0.cmp(&right.0));
    result
}

#[test]
fn alias_parameter_roles_survive_chaining_and_do_not_borrow_the_opposite_payload() {
    let source = "
        struct Missing;
        type Flipped<E, T> = Result<T, E>;
        type Chain<A, B> = Flipped<B, A>;
        type Identity<T> = T;
        fn success() -> Chain<bool, Missing> { original() }
        fn failure() -> Chain<Missing, u8> { original() }
        fn identity_positive() -> Identity<bool> { original() }
        fn identity_negative() -> Identity<Missing> { original() }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "fn_value", mode);
        assert_eq!(
            replacements(&found, "success"),
            [("Ok(false)".into(), Confidence::Proven), ("Ok(true)".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "failure"),
            [("Err(Default::default())".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "identity_positive"),
            [("false".into(), Confidence::Proven), ("true".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert!(replacements(&found, "identity_negative").is_empty(), "{mode}: {found:?}");
    }
    let found = discover(source, "fn_value", "optimistic");
    assert_eq!(
        replacements(&found, "success"),
        [
            ("Err(Default::default())".into(), Confidence::Optimistic),
            ("Ok(false)".into(), Confidence::Proven),
            ("Ok(true)".into(), Confidence::Proven),
        ],
        "{found:?}"
    );
    assert_eq!(
        replacements(&found, "failure"),
        [
            ("Err(Default::default())".into(), Confidence::Proven),
            ("Ok(Default::default())".into(), Confidence::Optimistic),
        ],
        "{found:?}"
    );
    assert_eq!(
        replacements(&found, "identity_positive"),
        [("false".into(), Confidence::Proven), ("true".into(), Confidence::Proven)],
        "{found:?}"
    );
    assert_eq!(
        replacements(&found, "identity_negative"),
        [("Default::default()".into(), Confidence::Optimistic)],
        "{found:?}"
    );
}

#[test]
fn renamed_standard_containers_preserve_shape_without_borrowing_foreign_identity() {
    let source = "
        use core::result::Result as Outcome;
        use core::option::Option as Maybe;
        fn result() -> Outcome<bool, u8> { original() }
        fn option() -> Maybe<bool> { original() }
        mod foreign {
            use dependency::Result as Outcome;
            fn result() -> Outcome<bool, u8> { original() }
        }
    ";
    for mode in ["complete", "labeled", "optimistic"] {
        let found = discover(source, "fn_value", mode);
        assert_eq!(
            replacements(&found, "result"),
            [
                ("Err(Default::default())".into(), Confidence::Proven),
                ("Ok(false)".into(), Confidence::Proven),
                ("Ok(true)".into(), Confidence::Proven),
            ],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "option"),
            [
                ("None".into(), Confidence::Proven),
                ("Some(false)".into(), Confidence::Proven),
                ("Some(true)".into(), Confidence::Proven),
            ],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "foreign::result"),
            [("Default::default()".into(), Confidence::Optimistic)],
            "{mode}: {found:?}"
        );
    }
}

#[test]
fn alias_declaration_identity_distinguishes_modules_shadows_and_cycles() {
    let source = "
        mod ready { #[derive(Default)] pub struct Same; pub type Alias = Same; }
        mod missing { pub struct Same; pub type Alias = Same; }
        use ready::Alias as Ready;
        use missing::Alias as Missing;
        fn positive() -> Ready { original() }
        fn negative() -> Missing { original() }
        type CycleA = CycleB;
        type CycleB = CycleA;
        fn cycle() -> CycleA { original() }
        type Alias<T> = Option<T>;
        fn alias_positive() -> Alias<bool> { original() }
        mod shadow { struct Alias; fn negative() -> Alias { original() } }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "fn_value", mode);
        assert_eq!(
            replacements(&found, "positive"),
            [("Default::default()".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert!(replacements(&found, "negative").is_empty(), "{mode}: {found:?}");
        assert!(replacements(&found, "shadow::negative").is_empty(), "{mode}: {found:?}");
        assert_eq!(
            replacements(&found, "alias_positive"),
            [
                ("None".into(), Confidence::Proven),
                ("Some(false)".into(), Confidence::Proven),
                ("Some(true)".into(), Confidence::Proven),
            ],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "cycle"),
            [("Default::default()".into(), Confidence::Optimistic)],
            "{mode}: {found:?}"
        );
    }
    let found = discover(source, "fn_value", "optimistic");
    assert_eq!(
        replacements(&found, "negative"),
        [("Default::default()".into(), Confidence::Optimistic)],
        "{found:?}"
    );
}

#[test]
fn generic_alias_defaults_are_instantiated_before_payload_admission() {
    let source = "
        struct Missing;
        type Wrapped<T> = Box<T>;
        type Twice<T> = Wrapped<T>;
        fn positive() -> Option<Twice<u8>> { None }
        fn negative() -> Option<Twice<Missing>> { None }
        fn bounded<T: Default>() -> Option<Twice<T>> { None }
        fn where_bounded<T>() -> Option<Twice<T>> where T: Default { None }
        fn unbounded<T>() -> Option<Twice<T>> { None }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "option.none_to_some", mode);
        for item in ["positive", "bounded", "where_bounded"] {
            assert_eq!(
                replacements(&found, item),
                [("Some(Default::default())".into(), Confidence::Proven)],
                "{mode}, {item}: {found:?}"
            );
        }
        for item in ["negative", "unbounded"] {
            assert!(replacements(&found, item).is_empty(), "{mode}, {item}: {found:?}");
        }
    }
}

#[test]
fn inherited_alias_targets_retain_their_declaring_imports_and_nominal_types() {
    let source = "
        use core::primitive::u32 as Number;
        type Inner = Number;
        type Outer = Inner;
        type Signed = core::primitive::i32;
        type Unknown = dependency::Number;
        #[derive(Default)] struct Ready;
        type Payload = Ready;
        fn unsigned() { type Number = i32; let _: Outer = 0; }
        fn signed() { type i32 = u32; let _: Signed = 0; }
        fn unresolved() { let _: Unknown = 0; }
        fn positive() {
            struct Ready;
            let _: Option<Payload> = None;
        }
        fn shadowed() {
            struct Payload;
            let _: Option<Payload> = None;
        }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "literal.int_decrement,option.none_to_some", mode);
        assert!(replacements(&found, "unsigned").is_empty(), "{mode}: {found:?}");
        assert_eq!(
            replacements(&found, "signed"),
            [("-1".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "unresolved"),
            [("-1".into(), Confidence::Optimistic)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "positive"),
            [("Some(Default::default())".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert!(replacements(&found, "shadowed").is_empty(), "{mode}: {found:?}");
    }
}

#[test]
fn empty_collections_do_not_require_payload_defaults_but_synthesized_elements_do() {
    let source = "
        struct Missing;
        #[derive(Default)] struct Present;
        fn empty() -> Vec<Missing> { original() }
        fn element() -> Vec<Present> { original() }
        fn map_missing() -> std::collections::HashMap<u8, Missing> { original() }
        fn map_present() -> std::collections::HashMap<u8, Present> { original() }
        fn ordered_unknown() -> std::collections::BTreeSet<Present> { original() }
        fn ordered_known() -> std::collections::BTreeSet<u8> { original() }
        fn nested_known() -> std::collections::HashMap<Option<bool>, Option<u8>> { original() }
        fn nested_unknown() -> std::collections::HashMap<Option<Present>, Option<u8>> { original() }
        fn foreign_nested() -> std::collections::HashMap<dependency::Option<bool>, Option<u8>> { original() }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "fn_value.empty_collection,fn_value.one_element", mode);
        assert_eq!(
            replacements(&found, "empty"),
            [("Vec::new()".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "element"),
            [
                ("Vec::new()".into(), Confidence::Proven),
                ("core::iter::once(Default::default()).collect()".into(), Confidence::Proven),
            ],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "map_missing"),
            [("std::collections::HashMap::new()".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert!(
            replacements(&found, "map_present")
                .contains(&("core::iter::once((0, Default::default())).collect()".into(), Confidence::Proven)),
            "{mode}: {found:?}"
        );
        let elements = |item| {
            found
                .iter()
                .filter(|candidate| candidate.item_path.as_ref() == item && candidate.mutator == "fn_value.one_element")
                .map(|candidate| candidate.confidence)
                .collect::<Vec<_>>()
        };
        assert_eq!(elements("ordered_unknown"), [Confidence::Optimistic], "{mode}: {found:?}");
        assert_eq!(
            elements("ordered_known"),
            [Confidence::Proven, Confidence::Proven],
            "{mode}: {found:?}"
        );
        for (item, confidence) in [
            ("nested_known", Confidence::Proven),
            ("nested_unknown", Confidence::Optimistic),
            ("foreign_nested", Confidence::Optimistic),
        ] {
            assert_eq!(elements(item), [confidence], "{mode}, {item}: {found:?}");
            assert!(
                replacements(&found, item).contains(&("core::iter::once((None, None)).collect()".into(), confidence)),
                "{mode}, {item}: {found:?}"
            );
            assert!(
                replacements(&found, item).contains(&("std::collections::HashMap::new()".into(), Confidence::Proven)),
                "{mode}, {item}: {found:?}"
            );
        }
    }
}

#[test]
fn conditional_bounds_and_custom_hashers_keep_the_compiler_oracle_honest() {
    let source = "
        struct Missing;
        #[derive(Default)] struct Present;
        #[derive(Default)] struct Derived<T>(T);
        struct Holder<T>(T);
        trait Marker {}
        impl<T: Default + Marker> Default for Holder<T> { fn default() -> Self { original() } }
        #[cfg_attr(feature = \"extra\", derive(Default))] struct Conditional<T>(T);
        fn uncertain() -> Holder<u8> { original() }
        fn conditional() -> Conditional<u8> { original() }
        fn derived_good() -> Derived<u8> { original() }
        fn derived_bad() -> Derived<Missing> { original() }
        fn good() -> std::collections::HashSet<u8, Present> { original() }
        fn bad() -> std::collections::HashSet<u8, Missing> { original() }
        fn foreign() -> std::collections::HashSet<u8, dependency::RandomState> { original() }
        fn standard() -> std::collections::HashSet<u8, std::collections::hash_map::RandomState> { original() }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "fn_value", mode);
        for item in ["uncertain", "conditional", "derived_bad"] {
            assert_eq!(
                replacements(&found, item),
                [("Default::default()".into(), Confidence::Optimistic)],
                "{mode}, {item}: {found:?}"
            );
        }
        assert_eq!(
            replacements(&found, "derived_good"),
            [("Default::default()".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        let good = replacements(&found, "good");
        assert!(
            good.contains(&("Default::default()".into(), Confidence::Proven)),
            "{mode}: {found:?}"
        );
        assert!(
            good.contains(&("core::iter::once(0).collect()".into(), Confidence::Optimistic)),
            "{mode}: {found:?}"
        );
        assert!(replacements(&found, "bad").is_empty(), "{mode}: {found:?}");
        assert_eq!(
            replacements(&found, "foreign"),
            [
                ("Default::default()".into(), Confidence::Optimistic),
                ("core::iter::once(0).collect()".into(), Confidence::Optimistic),
                ("core::iter::once(1).collect()".into(), Confidence::Optimistic),
            ],
            "{mode}: {found:?}"
        );
        assert!(
            replacements(&found, "standard").contains(&("Default::default()".into(), Confidence::Proven)),
            "{mode}: {found:?}"
        );
    }
}

#[test]
fn call_argument_evidence_respects_bindings_configuration_and_declaring_imports() {
    for (source, expected) in [
        ("fn take(_: u32) {} fn f() { take(0); }", None),
        ("fn take(_: i32) {} fn f() { take(0); }", Some(Confidence::Proven)),
        (
            "fn take(_: u32) {} fn f(take: impl Fn(i32)) { take(0); }",
            Some(Confidence::Optimistic),
        ),
        (
            "fn take(_: u32) {} fn take(_: i32) {} fn f() { take(0); }",
            Some(Confidence::Optimistic),
        ),
        (
            "fn take(_: u32) {} fn f() { fn take(_: i32) {} take(0); }",
            Some(Confidence::Proven),
        ),
        ("fn take(#[cfg(any())] _: i32, _: u32) {} fn f() { take(0); }", None),
        (
            "use core::primitive::u32 as Number; fn take(_: Number) {} fn f() { type Number = i32; take(0); }",
            None,
        ),
        (
            "use core::primitive::u32 as Number; fn f() { fn take(_: Number) {} take(0); }",
            None,
        ),
        ("fn take<T>(_: T) {} fn f<T: Default>() { take(0); }", Some(Confidence::Optimistic)),
    ] {
        let found = discover(source, "literal.int_decrement", "complete");
        let expected = expected.map(|confidence| vec![("-1".into(), confidence)]).unwrap_or_default();
        assert_eq!(replacements(&found, "f"), expected, "{source}: {found:?}");
    }
}

#[test]
fn call_results_do_not_borrow_duplicate_async_or_associated_type_evidence() {
    for (source, expected) in [
        ("fn make() -> u8 { 7 } fn f() { make(); }", true),
        ("async fn make() -> u8 { 7 } fn f() { make(); }", false),
        ("fn make() -> u8 { 7 } fn make() -> bool { true } fn f() { make(); }", false),
        ("fn make() -> u8 { 7 } fn f(make: impl Fn() -> String) { make(); }", false),
        (
            "trait Has { type Output; } fn make<T: Has>() -> T::Output { original() } fn f() { make(); }",
            false,
        ),
        ("fn make<T>() -> T { original() } fn f<T: Default>() { make(); }", false),
    ] {
        let found = discover(source, "call.replace_with_default,call_result.default", "complete");
        let actual = replacements(&found, "f");
        let expected = if expected {
            vec![
                ("::gamma_rt::default_after(make())".into(), Confidence::Proven),
                ("Default::default()".into(), Confidence::Proven),
            ]
        } else {
            Vec::new()
        };
        assert_eq!(actual, expected, "{source}: {found:?}");
    }
}

#[test]
fn same_named_calls_in_separate_modules_keep_their_own_signatures() {
    let source = "
        fn take(_: i32) {}
        fn f() { take(0); }
        mod inner { fn take(_: u32) {} fn f() { take(0); } }
        mod imported {
            use crate::take as renamed;
            fn f() { renamed(0); }
        }
    ";
    let found = discover(source, "literal.int_decrement", "complete");
    assert_eq!(replacements(&found, "f"), [("-1".into(), Confidence::Proven)], "{found:?}");
    assert!(replacements(&found, "inner::f").is_empty(), "{found:?}");
    assert_eq!(
        replacements(&found, "imported::f"),
        [("-1".into(), Confidence::Proven)],
        "{found:?}"
    );
}

#[test]
fn nested_expected_types_and_self_arguments_use_the_callee_not_the_caller() {
    let source = "
        struct Receiver;
        impl Receiver { fn take(_: Option<Self>) {} }
        #[derive(Default)] struct Caller;
        impl Caller { fn negative() { Receiver::take(None); } }
        #[derive(Default)] struct Ready;
        impl crate::Ready { fn take(_: Option<Self>) {} }
        fn positive() { Ready::take(None); }
        fn nested() -> Option<u8> {
            let _ = || { None };
            let _ = async { None };
            let _ = || -> Option<u8> { None };
            { if condition() { None } else { None } }
        }
    ";
    for mode in ["complete", "labeled"] {
        let found = discover(source, "option.none_to_some", mode);
        assert!(replacements(&found, "Caller::negative").is_empty(), "{mode}: {found:?}");
        assert_eq!(
            replacements(&found, "positive"),
            [("Some(Default::default())".into(), Confidence::Proven)],
            "{mode}: {found:?}"
        );
        assert_eq!(
            replacements(&found, "nested"),
            vec![("Some(Default::default())".into(), Confidence::Proven); 3],
            "{mode}: {found:?}"
        );
    }
}
