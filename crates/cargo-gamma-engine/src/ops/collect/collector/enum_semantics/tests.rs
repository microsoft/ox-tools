// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::spanned::Spanned as _;
use syn::token::Group;

use super::super::super::{Candidate, Defaults, check_stated_and_collect_with, collect_with, into_definitions};
use super::{Collector, Expr, control_flow_variant, variant};
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

fn candidates(source: &str, selection: &str) -> Vec<Candidate> {
    let file = SourceFile::parse("enums.rs", source.into()).unwrap();
    let selection = Selection::parse(selection).unwrap();
    let cfg = CfgSet::parse("");
    let defaults = Defaults::of(&file.ast);
    check_stated_and_collect_with(&file, &selection, &cfg, &defaults).unwrap()
}

#[test]
fn control_flow_constructors_require_defaultable_destination_payloads() {
    let found = candidates(
        r"
        use core::ops::ControlFlow::{self, Break, Continue};
        struct NoDefault;
        fn stop() -> ControlFlow<NoDefault> { Break(NoDefault) }
        fn proceed() -> ControlFlow<NoDefault> { Continue(()) }
        fn both(flag: bool) -> ControlFlow<u8, bool> {
            if flag { Break(7) } else { Continue(true) }
        }
        fn generic<T>(x: T) -> ControlFlow<T> { Break(x) }
        fn bounded<T: Default>() -> ControlFlow<T> { Continue(()) }
        ",
        "control_flow",
    );
    let actual = found
        .iter()
        .map(|c| (c.item_path.as_ref(), c.mutator, c.replacement.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            (
                "stop",
                "control_flow.break_to_continue",
                "::core::ops::ControlFlow::Continue(::core::default::Default::default())"
            ),
            (
                "both",
                "control_flow.break_to_continue",
                "::core::ops::ControlFlow::Continue(::core::default::Default::default())"
            ),
            (
                "both",
                "control_flow.continue_to_break",
                "::core::ops::ControlFlow::Break(::core::default::Default::default())"
            ),
            (
                "generic",
                "control_flow.break_to_continue",
                "::core::ops::ControlFlow::Continue(::core::default::Default::default())"
            ),
            (
                "bounded",
                "control_flow.continue_to_break",
                "::core::ops::ControlFlow::Break(::core::default::Default::default())"
            ),
        ]
    );
}

#[test]
fn gaining_destination_default_proof_preserves_later_constructor_ids() {
    for (variant, first_type, second_type) in [
        ("Continue", "ControlFlow<NoDefault>", "ControlFlow<u8>"),
        ("Break", "ControlFlow<(), NoDefault>", "ControlFlow<(), u8>"),
    ] {
        let definitions = |derive: &str| {
            let source = format!(
                "use core::ops::ControlFlow;
                 {derive} struct NoDefault;
                 fn body() {{
                     let _first: {first_type} = ControlFlow::{variant}(());
                     let _second: {second_type} = ControlFlow::{variant}(());
                 }}"
            );
            let file = SourceFile::parse("enums.rs", source).unwrap();
            let selection = Selection::parse("control_flow").unwrap();
            let cfg = CfgSet::parse("");
            let found = collect_with(&file, &selection, &cfg, &Defaults::of(&file.ast));
            assert_eq!(found.len(), if derive.is_empty() { 1 } else { 2 });
            assert_eq!(found.last().unwrap().identity_occurrence, Some(1));
            into_definitions(&file, found)
        };
        let before = definitions("");
        let after = definitions("#[derive(Default)]");
        assert_eq!(before[0].id, after[1].id, "{variant}");
        assert_ne!(after[0].id, after[1].id, "{variant}");
    }
}

#[test]
fn control_flow_defaults_require_proof_for_shadowed_standard_payload_names() {
    for (declaration, has_default) in [("struct String;", false), ("#[derive(Default)] struct String;", true)] {
        let source = [
            "use core::ops::ControlFlow;\n",
            declaration,
            r"
            fn explicit_break() { let _ = ControlFlow::<u8, String>::Break(1); }
            fn explicit_continue() { let _ = ControlFlow::<String, u8>::Continue(1); }
            fn expected_break() -> ControlFlow<u8, String> { ControlFlow::Break(1) }
            fn expected_continue() -> ControlFlow<String, u8> { ControlFlow::Continue(1) }
            ",
        ]
        .concat();
        let found = candidates(&source, "control_flow");
        let actual = found
            .iter()
            .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator))
            .collect::<Vec<_>>();
        let expected = if has_default {
            vec![
                ("explicit_break", "control_flow.break_to_continue"),
                ("explicit_continue", "control_flow.continue_to_break"),
                ("expected_break", "control_flow.break_to_continue"),
                ("expected_continue", "control_flow.continue_to_break"),
            ]
        } else {
            Vec::new()
        };
        assert_eq!(actual, expected, "{declaration}");
    }
}

#[test]
fn control_flow_collection_payload_defaults_require_genuine_external_roots() {
    for (root, payloads) in [
        (
            "std",
            &[
                "vec::Vec<u8>",
                "collections::VecDeque<u8>",
                "collections::HashSet<u8>",
                "collections::HashMap<u8, u8>",
                "collections::btree_map::BTreeMap<u8, u8>",
                "collections::btree_set::BTreeSet<u8>",
                "collections::binary_heap::BinaryHeap<u8>",
                "collections::linked_list::LinkedList<u8>",
                "string::String",
                "option::Option<u8>",
                "boxed::Box<u8>",
                "sync::Arc<u8>",
                "rc::Rc<u8>",
            ][..],
        ),
        (
            "alloc",
            &["vec::Vec<u8>", "collections::VecDeque<u8>", "string::String", "boxed::Box<u8>"][..],
        ),
    ] {
        for (declaration, admitted) in [
            (String::new(), true),
            (format!("extern crate {root};"), true),
            (format!("extern crate custom as {root};"), false),
            (format!("extern crate self as {root};"), false),
            (format!("#[cfg(all())] extern crate custom as {root};"), false),
            (format!("#[cfg(any())] extern crate custom as {root};"), true),
            (format!("mod {root} {{}}"), true),
            (format!("struct {root};"), true),
            (format!("type {root} = ();"), true),
        ] {
            for payload in payloads {
                for (variant, output, mutation, function_mutation) in [
                    (
                        "Break",
                        format!("u8, ::{root}::{payload}"),
                        "control_flow.break_to_continue",
                        "fn_value.continue_default",
                    ),
                    (
                        "Continue",
                        format!("::{root}::{payload}, u8"),
                        "control_flow.continue_to_break",
                        "fn_value.break_default",
                    ),
                ] {
                    let functions = format!(
                        "fn explicit(value: u8) {{ let _ = ::core::ops::ControlFlow::<{output}>::{variant}(value); }}
                         fn expected(value: u8) -> ::core::ops::ControlFlow<{output}> {{ ::core::ops::ControlFlow::{variant}(value) }}"
                    );
                    for nested in [false, true] {
                        let source = if nested {
                            format!("{declaration} mod nested {{ mod {root} {{}} {functions} }}")
                        } else {
                            format!("{declaration} {functions}")
                        };
                        let file = SourceFile::parse("enums.rs", source).unwrap();
                        let cfg = CfgSet::parse("");
                        let selection = Selection::parse(&format!("{mutation},{function_mutation}")).unwrap();
                        for bound in [false, true] {
                            let mut defaults = Defaults::of_source_in(&file.ast, &cfg, "enums.rs");
                            if bound {
                                defaults.bind_sources(&[("enums.rs".into(), "enums.rs".into(), Vec::new())]);
                            }
                            let found = collect_with(&file, &selection, &cfg, &defaults);
                            assert_eq!(
                                found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
                                if admitted {
                                    vec![mutation, function_mutation, mutation]
                                } else {
                                    Vec::new()
                                },
                                "{declaration}; {payload}; {variant}; nested={nested}; bound={bound}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn control_flow_external_root_evidence_is_scoped_and_survives_collection_aliases() {
    let found = candidates(
        r"
        mod foreign {
            extern crate custom as std;
            use ::std::vec::Vec;
            type Payload<T> = Vec<T>;
            fn rejected(value: u8) -> ::core::ops::ControlFlow<u8, Payload<u8>> {
                ::core::ops::ControlFlow::Break(value)
            }
            mod child {
                use ::std::vec::Vec;
                type Payload<T> = Vec<T>;
                fn rejected(value: u8) -> ::core::ops::ControlFlow<u8, Payload<u8>> {
                    ::core::ops::ControlFlow::Break(value)
                }
            }
        }
        mod sibling {
            use ::std::vec::Vec;
            type Payload<T> = Vec<T>;
            fn accepted(value: u8) -> ::core::ops::ControlFlow<u8, Payload<u8>> {
                ::core::ops::ControlFlow::Break(value)
            }
        }
        fn accepted_block(value: u8) {
            {
                extern crate custom as std;
                let _ = ::core::ops::ControlFlow::<u8, ::std::vec::Vec<u8>>::Break(value);
            }
            let _ = ::core::ops::ControlFlow::<u8, ::std::vec::Vec<u8>>::Break(value);
        }
        ",
        "control_flow.break_to_continue,fn_value.continue_default",
    );
    assert_eq!(
        found
            .iter()
            .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator))
            .collect::<Vec<_>>(),
        [
            ("sibling::accepted", "fn_value.continue_default"),
            ("sibling::accepted", "control_flow.break_to_continue"),
            ("accepted_block", "control_flow.break_to_continue"),
        ]
    );
}

#[test]
fn labeled_control_flow_payload_proof_respects_custom_default_traits_and_roots() {
    for (declarations, expected) in [
        (
            "mod custom { pub trait Default {} } use custom::*; struct Shape; impl Default for Shape {}",
            0,
        ),
        (
            "mod core { pub mod default { pub trait Default {} } } struct Shape; impl core::default::Default for Shape {}",
            0,
        ),
        ("mod core {} struct Shape; impl ::core::default::Default for Shape {}", 1),
        (
            "extern crate custom as core; struct Shape; impl core::default::Default for Shape {}",
            0,
        ),
        (
            "extern crate custom as core; struct Shape; impl ::core::default::Default for Shape {}",
            0,
        ),
        (
            "use ::core::default::Default as D; extern crate custom as core; struct Shape; impl D for Shape {}",
            0,
        ),
        ("extern crate core; struct Shape; impl ::core::default::Default for Shape {}", 1),
        (
            "#[cfg(any())] extern crate custom as core; struct Shape; impl ::core::default::Default for Shape {}",
            1,
        ),
        (
            "mod custom { pub trait Default {} } use custom::*; use ::core::default::Default as D; struct Shape; impl D for Shape {}",
            1,
        ),
    ] {
        let source =
            format!("{declarations} fn body() -> ::std::ops::ControlFlow<crate::Shape, u8> {{ ::std::ops::ControlFlow::Continue(1) }}");
        let file = SourceFile::parse("enums.rs", source).unwrap();
        let cfg = CfgSet::parse("");
        let mut defaults = Defaults::of_source_in(&file.ast, &cfg, "enums.rs");
        defaults.bind_sources(&[("enums.rs".into(), "enums.rs".into(), Vec::new())]);
        let selection = Selection::parse("control_flow.continue_to_break").unwrap();
        let found = collect_with(&file, &selection, &cfg, &defaults);
        assert_eq!(found.len(), expected, "{declarations}");
    }
}

#[test]
fn generic_payloads_do_not_inherit_nominal_defaults_with_or_without_source_labels() {
    for bound in ["", ": Default"] {
        let source = format!(
            r"
            use core::ops::ControlFlow;
            #[derive(Default)]
            struct Payload;
            fn explicit_continue<Payload{bound}>() {{
                let _ = ControlFlow::<Payload>::Continue(());
            }}
            fn expected_continue<Payload{bound}>() -> ControlFlow<Payload> {{
                ControlFlow::Continue(())
            }}
            fn explicit_break<Payload{bound}>() {{
                let _ = ControlFlow::<u8, Payload>::Break(1);
            }}
            fn expected_break<Payload{bound}>() -> ControlFlow<u8, Payload> {{
                ControlFlow::Break(1)
            }}
            fn concrete() -> ControlFlow<Payload> {{
                ControlFlow::Continue(())
            }}
            fn generic_source<Payload>(value: Payload) -> ControlFlow<Payload> {{
                ControlFlow::Break(value)
            }}
            "
        );
        for labeled in [false, true] {
            let file = SourceFile::parse("enums.rs", source.clone()).unwrap();
            let cfg = CfgSet::parse("");
            let mut defaults = if labeled {
                Defaults::of_source_in(&file.ast, &cfg, "enums.rs")
            } else {
                Defaults::of(&file.ast)
            };
            if labeled {
                defaults.bind_sources(&[("enums.rs".into(), "enums.rs".into(), Vec::new())]);
            }
            let selection = Selection::parse("control_flow").unwrap();
            let found = collect_with(&file, &selection, &cfg, &defaults);
            let mut expected = Vec::new();
            if !bound.is_empty() {
                expected.extend([
                    ("explicit_continue", "control_flow.continue_to_break"),
                    ("expected_continue", "control_flow.continue_to_break"),
                    ("explicit_break", "control_flow.break_to_continue"),
                    ("expected_break", "control_flow.break_to_continue"),
                ]);
            }
            expected.extend([
                ("concrete", "control_flow.continue_to_break"),
                ("generic_source", "control_flow.break_to_continue"),
            ]);
            assert_eq!(
                found
                    .iter()
                    .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator))
                    .collect::<Vec<_>>(),
                expected,
                "bound={bound:?}, labeled={labeled}"
            );
        }
    }
}

#[test]
fn enum_variants_resolve_imports_aliases_and_exclude_patterns_and_constants() {
    let found = candidates(
        r"
        use std::cmp::Ordering::{self as Comparison, Less as Before, Equal, Greater};
        use std::ops::ControlFlow as Flow;
        type Outcome = Flow<u8>;
        const FIXED: Comparison = Before;
        fn compare() -> Comparison { Before }
        fn equal() -> Comparison { Equal }
        fn greater() -> Comparison { Greater }
        fn stop() -> Outcome { Outcome::Break(1) }
        fn check(value: Comparison) -> bool {
            match value { Before => true, Equal => false, Greater => true }
        }
        ",
        "ordering,control_flow",
    );
    assert_eq!(found.len(), 7, "{found:?}");
    assert_eq!(
        found
            .iter()
            .take(6)
            .map(|c| (c.item_path.as_ref(), c.mutator, c.replacement.as_str()))
            .collect::<Vec<_>>(),
        [
            ("compare", "ordering.less_to_equal", "::core::cmp::Ordering::Equal"),
            ("compare", "ordering.less_to_greater", "::core::cmp::Ordering::Greater"),
            ("equal", "ordering.equal_to_greater", "::core::cmp::Ordering::Greater"),
            ("equal", "ordering.equal_to_less", "::core::cmp::Ordering::Less"),
            ("greater", "ordering.greater_to_equal", "::core::cmp::Ordering::Equal"),
            ("greater", "ordering.greater_to_less", "::core::cmp::Ordering::Less"),
        ]
    );
    assert_eq!(found.last().unwrap().mutator, "control_flow.break_to_continue");
    assert_eq!(
        found.iter().map(|candidate| candidate.replacement_index).collect::<Vec<_>>(),
        [0; 7]
    );
    assert!(
        found
            .iter()
            .all(|c| c.item_path.as_ref() != "check" && c.item_path.as_ref() != "FIXED")
    );
}

#[test]
fn enum_replacement_qualifiers_reject_foreign_standard_crate_bindings() {
    for (declarations, namespace, replacement_root) in [
        ("", "std", Some("core")),
        ("extern crate custom as core;", "std", Some("std")),
        ("extern crate custom as std;", "core", Some("core")),
        ("extern crate custom as core; extern crate other as std;", "std", None),
    ] {
        let source = format!(
            "{declarations}
             use {namespace}::cmp::Ordering;
             use {namespace}::ops::ControlFlow;
             fn order() -> Ordering {{ Ordering::Less }}
             fn stop() -> ControlFlow<u8, u8> {{ ControlFlow::Break(1) }}
             fn proceed() -> ControlFlow<u8, u8> {{ ControlFlow::Continue(1) }}
             fn nested(value: Option<Ordering>) -> Option<Ordering> {{ value }}"
        );
        let found = candidates(&source, "ordering,control_flow,fn_value");
        let Some(root) = replacement_root else {
            assert_eq!(
                found
                    .iter()
                    .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator, candidate.replacement.as_str()))
                    .collect::<Vec<_>>(),
                [("nested", "fn_value.none", "None")]
            );
            continue;
        };
        let ordering = |variant| format!("::{root}::cmp::Ordering::{variant}");
        let flow = |variant| format!("::{root}::ops::ControlFlow::{variant}(::{root}::default::Default::default())");
        for (function, mut expected) in [
            (
                "order",
                vec![ordering("Equal"), ordering("Equal"), ordering("Greater"), ordering("Greater")],
            ),
            ("stop", vec![flow("Break"), flow("Continue"), flow("Continue")]),
            ("proceed", vec![flow("Break"), flow("Break"), flow("Continue")]),
            (
                "nested",
                vec![
                    "None".to_owned(),
                    format!("Some({})", ordering("Less")),
                    format!("Some({})", ordering("Equal")),
                    format!("Some({})", ordering("Greater")),
                ],
            ),
        ] {
            let mut actual = found
                .iter()
                .filter(|candidate| candidate.item_path.as_ref() == function)
                .map(|candidate| candidate.replacement.to_string())
                .collect::<Vec<_>>();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{declarations}: {function}");
        }
    }
}

#[test]
fn absolute_ordering_paths_and_imports_bypass_local_standard_root_shadows() {
    for declarations in [
        "mod core {}",
        "struct core;",
        "type core = ();",
        "mod std {}",
        "struct std;",
        "type std = ();",
        "mod core {} mod std {}",
    ] {
        for root in ["core", "std"] {
            for (imports, expression) in [
                (String::new(), format!("::{root}::cmp::Ordering::Less")),
                (format!("use ::{root}::cmp::Ordering as O;"), "O::Less".to_owned()),
                (format!("use ::{root}::cmp::Ordering::Less as V;"), "V".to_owned()),
                (format!("type O = ::{root}::cmp::Ordering;"), "O::Less".to_owned()),
                (format!("extern crate {root} as real;"), "real::cmp::Ordering::Less".to_owned()),
            ] {
                for nested in [false, true] {
                    let body = format!("{imports} fn order() -> ::std::cmp::Ordering {{ {expression} }}");
                    let source = if nested {
                        format!("{declarations} mod nested {{ {body} }}")
                    } else {
                        format!("{declarations} {body}")
                    };
                    let mut actual = candidates(&source, "ordering,fn_value.less,fn_value.equal,fn_value.greater")
                        .into_iter()
                        .map(|candidate| (candidate.mutator, candidate.replacement.to_string()))
                        .collect::<Vec<_>>();
                    actual.sort();
                    assert_eq!(
                        actual,
                        [
                            ("fn_value.equal", "::core::cmp::Ordering::Equal".to_owned()),
                            ("fn_value.greater", "::core::cmp::Ordering::Greater".to_owned()),
                            ("ordering.less_to_equal", "::core::cmp::Ordering::Equal".to_owned()),
                            ("ordering.less_to_greater", "::core::cmp::Ordering::Greater".to_owned()),
                        ],
                        "{source}"
                    );
                }
            }
        }
    }
}

#[test]
fn absolute_control_flow_paths_keep_default_identity_under_local_root_shadows() {
    for declarations in ["mod core {}", "mod std {}", "mod core {} mod std {}"] {
        for root in ["core", "std"] {
            for original in ["Break", "Continue"] {
                for payload in [
                    "0".to_owned(),
                    "::core::default::Default::default()".to_owned(),
                    "::std::default::Default::default()".to_owned(),
                ] {
                    for (imports, constructor) in [
                        (String::new(), format!("::{root}::ops::ControlFlow::{original}")),
                        (format!("use ::{root}::ops::ControlFlow as C;"), format!("C::{original}")),
                        (format!("use ::{root}::ops::ControlFlow::{original} as V;"), "V".to_owned()),
                        (
                            format!("extern crate {root} as real;"),
                            format!("real::ops::ControlFlow::{original}"),
                        ),
                    ] {
                        let source = format!(
                            "{declarations} {imports}
                             fn flow() -> ::std::ops::ControlFlow<u8, u8> {{ {constructor}({payload}) }}"
                        );
                        let found = candidates(&source, "control_flow,fn_value.break_default,fn_value.continue_default");
                        let expected = if original == "Break" {
                            ["fn_value.continue_default", "control_flow.break_to_continue"]
                        } else {
                            ["fn_value.break_default", "control_flow.continue_to_break"]
                        };
                        assert_eq!(
                            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
                            expected,
                            "{source}"
                        );
                        let opposite = if original == "Break" { "Continue" } else { "Break" };
                        let replacement = format!("::core::ops::ControlFlow::{opposite}(::core::default::Default::default())");
                        assert_eq!(
                            found.iter().map(|candidate| candidate.replacement.as_str()).collect::<Vec<_>>(),
                            [replacement.as_str(); 2],
                            "{source}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn absolute_enum_imports_still_reject_foreign_external_roots() {
    for (declarations, namespace, replacement_root) in [
        ("extern crate custom as core;", "std", Some("std")),
        ("extern crate custom as std;", "core", Some("core")),
        ("extern crate custom as core;", "core", None),
        ("extern crate custom as std;", "std", None),
        ("extern crate custom as core; extern crate other as std;", "std", None),
    ] {
        let source = format!(
            "{declarations}
             use ::{namespace}::cmp::Ordering as O;
             use ::{namespace}::cmp::Ordering::Less as V;
             fn order() -> ::{namespace}::cmp::Ordering {{ V }}
             fn aliased() -> O {{ O::Less }}"
        );
        let found = candidates(&source, "ordering,fn_value.less,fn_value.equal,fn_value.greater");
        let expected = replacement_root.map_or_else(Vec::new, |root| {
            vec![
                format!("::{root}::cmp::Ordering::Equal"),
                format!("::{root}::cmp::Ordering::Greater"),
                format!("::{root}::cmp::Ordering::Equal"),
                format!("::{root}::cmp::Ordering::Greater"),
            ]
        });
        for function in ["order", "aliased"] {
            assert_eq!(
                found
                    .iter()
                    .filter(|candidate| candidate.item_path.as_ref() == function)
                    .map(|candidate| candidate.replacement.to_string())
                    .collect::<Vec<_>>(),
                expected,
                "{source}"
            );
        }
    }
}

#[test]
fn generic_enum_roots_shadow_standard_imports_before_variant_resolution() {
    let source = r"
        use core::cmp::Ordering;
        use core::cmp::Ordering as Comparison;
        use core::ops::ControlFlow;
        trait Source { const Less: u8; }
        trait FlowSource {
            fn Break(value: u8) -> core::ops::ControlFlow<u8, u8>;
        }
        fn read<Ordering: Source>() -> u8 { Ordering::Less }
        fn bounded<Ordering: Source + Default>() -> u8 { Ordering::Less }
        fn renamed<Comparison: Source>() -> u8 { Comparison::Less }
        fn flow<ControlFlow: FlowSource>() -> core::ops::ControlFlow<u8, u8> {
            ControlFlow::Break(1)
        }
        fn absolute<Ordering: Source>() -> core::cmp::Ordering {
            ::core::cmp::Ordering::Less
        }
        fn standard() -> Ordering { Ordering::Less }
    ";
    let found = candidates(source, "ordering,control_flow");
    assert_eq!(
        found
            .iter()
            .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator))
            .collect::<Vec<_>>(),
        [
            ("absolute", "ordering.less_to_equal"),
            ("absolute", "ordering.less_to_greater"),
            ("standard", "ordering.less_to_equal"),
            ("standard", "ordering.less_to_greater"),
        ]
    );
}

#[test]
fn const_parameters_shadow_imported_value_variants_and_restore_each_scope() {
    let source = r"
        use core::cmp::Ordering;
        use core::cmp::Ordering::{Less, Equal};
        fn before() -> Ordering { Less }
        fn read<const Less: u8>() -> u8 { Less }
        struct Container<const Less: u8>;
        impl<const Less: u8> Container<Less> {
            fn read() -> u8 { Less }
            fn known() -> Ordering { Equal }
        }
        trait Reader<const Less: u8> {
            fn read() -> u8 { Less }
            fn known() -> Ordering { Equal }
        }
        fn typed_namespace<const Ordering: u8>() -> core::cmp::Ordering {
            Ordering::Less
        }
        fn after() -> Ordering { Less }
    ";
    let found = candidates(source, "ordering");
    assert_eq!(found.len(), 10, "{found:?}");
    assert!(found.iter().all(|candidate| !candidate.item_path.ends_with("read")), "{found:?}");
    assert_eq!(
        found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
        [
            "ordering.less_to_equal",
            "ordering.less_to_greater",
            "ordering.equal_to_greater",
            "ordering.equal_to_less",
            "ordering.equal_to_greater",
            "ordering.equal_to_less",
            "ordering.less_to_equal",
            "ordering.less_to_greater",
            "ordering.less_to_equal",
            "ordering.less_to_greater",
        ]
    );
    let returns = candidates(source, "fn_value.less,fn_value.equal,fn_value.greater");
    assert_eq!(
        returns
            .iter()
            .filter(|candidate| candidate.item_path.as_ref() == "typed_namespace")
            .map(|candidate| candidate.mutator)
            .collect::<Vec<_>>(),
        ["fn_value.equal", "fn_value.greater"]
    );
}

#[test]
fn default_bounded_enum_names_keep_generic_defaults_instead_of_concrete_variants() {
    let source = r"
        use core::cmp::Ordering;
        use core::cmp::Ordering as Comparison;
        use core::ops::ControlFlow;
        type Identity<T> = T;
        trait Check {
            fn is_lt(&self) -> bool;
            fn is_break(&self) -> bool;
        }
        fn ordering<Ordering: Default>(value: Ordering) -> Ordering { value }
        fn renamed<Comparison: Default>(value: Comparison) -> Comparison { value }
        fn flow<ControlFlow: Default>(value: ControlFlow) -> ControlFlow { value }
        fn aliased<Ordering: Default>(value: Identity<Ordering>) -> Identity<Ordering> { value }
        fn aliased_flow<ControlFlow: Default>(value: Identity<ControlFlow>) -> Identity<ControlFlow> { value }
        fn predicate<Ordering: Default + Check>(value: Ordering) -> bool { value.is_lt() }
        fn flow_predicate<ControlFlow: Default + Check>(value: ControlFlow) -> bool { value.is_break() }
    ";
    let found = candidates(source, "fn_value");
    assert_eq!(
        found
            .iter()
            .filter(|candidate| !candidate.item_path.contains("predicate"))
            .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator, candidate.replacement.as_str()))
            .collect::<Vec<_>>(),
        [
            ("ordering", "fn_value.default", "Default::default()"),
            ("renamed", "fn_value.default", "Default::default()"),
            ("flow", "fn_value.default", "Default::default()"),
            ("aliased", "fn_value.default", "Default::default()"),
            ("aliased_flow", "fn_value.default", "Default::default()"),
        ]
    );
    assert!(candidates(source, "ordering,control_flow").is_empty());
}

#[test]
fn control_flow_return_noops_resolve_default_literals_and_constructor_paths() {
    for (original, retained) in [("Break", "fn_value.continue_default"), ("Continue", "fn_value.break_default")] {
        for (ty, payload) in [("()", "()"), ("u8", "0"), ("bool", "false")] {
            for expression in [
                format!("ControlFlow::{original}({payload})"),
                format!("core::ops::ControlFlow::{original}({payload})"),
                format!("::std::ops::ControlFlow::{original}({payload})"),
                format!("Alias::{original}({payload})"),
                format!("imported({payload})"),
                format!("((ControlFlow::{original})(({payload})))"),
                format!("return ControlFlow::{original}({payload});"),
            ] {
                let source = format!(
                    "use core::ops::ControlFlow;
                     use core::ops::ControlFlow::{original} as imported;
                     type Alias = ControlFlow<{ty}, {ty}>;
                     fn flow() -> ControlFlow<{ty}, {ty}> {{ {expression} }}"
                );
                let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
                assert_eq!(
                    found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
                    [retained],
                    "{source}"
                );
                assert_eq!(found[0].confidence, crate::ops::collect::Confidence::Proven);
            }
        }
    }
}

#[test]
fn control_flow_return_noops_recognize_only_resolved_default_trait_calls() {
    for original in ["Break", "Continue"] {
        for (declarations, payload, identity) in [
            ("", "Default::default()", true),
            ("", "::core::default::Default::default()", true),
            ("", "std::default::Default::default()", true),
            ("use core::default::Default as D;", "(D::default)()", true),
            (
                "struct Default; impl Default { fn default() -> u8 { 7 } }",
                "Default::default()",
                false,
            ),
            ("trait Default { fn default() -> u8; }", "Default::default()", false),
            ("use custom::Default;", "Default::default()", false),
            ("use custom::Default;", "::core::default::Default::default()", true),
            ("extern crate custom as core;", "core::default::Default::default()", false),
            ("extern crate custom as core;", "::std::default::Default::default()", true),
            ("", "u8::default()", false),
            ("", "Default::default::<u8>()", false),
            ("", "Default::default(effect())", false),
        ] {
            let source = format!(
                "{declarations}
                 use ::std::ops::ControlFlow;
                 fn flow() -> ControlFlow<u8, u8> {{ ControlFlow::{original}({payload}) }}"
            );
            let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
            let actual = found
                .iter()
                .filter(|candidate| candidate.item_path.as_ref() == "flow")
                .map(|candidate| candidate.mutator)
                .collect::<Vec<_>>();
            let expected = match (identity, original) {
                (true, "Break") => vec!["fn_value.continue_default"],
                (true, _) => vec!["fn_value.break_default"],
                (false, _) => vec!["fn_value.break_default", "fn_value.continue_default"],
            };
            assert_eq!(actual, expected, "{source}");
        }
    }
}

#[test]
fn control_flow_return_noop_filter_preserves_nondefault_unknown_and_effectful_bodies() {
    for variant in ["Break", "Continue"] {
        let source = format!("use core::ops::ControlFlow; fn flow() -> ControlFlow<bool, bool> {{ ControlFlow::{variant}(true) }}");
        let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
        assert_eq!(
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
            ["fn_value.break_default", "fn_value.continue_default"],
            "{source}"
        );
    }
    for body in [
        "ControlFlow::Break(7)",
        "ControlFlow::Continue(1)",
        "ControlFlow::Break(payload)",
        "ControlFlow::Continue(effect())",
        "ControlFlow::Break({ effect(); 0 })",
        "effect(); ControlFlow::Break(0)",
        "ControlFlow::Continue(0);",
        "return;",
        "#[opaque] ControlFlow::Break(0)",
        "#[opaque] (ControlFlow::Break(0))",
        "#[opaque] return ControlFlow::Break(0);",
        "ControlFlow::Break(#[opaque] 0)",
        "ControlFlow::Break(#[opaque] (0))",
        "ControlFlow::Break(#[opaque] Default::default())",
        "ControlFlow::Break((#[opaque] Default::default)())",
    ] {
        let source = format!("use core::ops::ControlFlow; fn flow(payload: u8) -> ControlFlow<u8, u8> {{ {body} }}");
        let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
        assert_eq!(
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
            ["fn_value.break_default", "fn_value.continue_default"],
            "{source}"
        );
    }
}

#[test]
fn control_flow_return_noop_filter_respects_constructor_and_generic_shadows() {
    for (declarations, parameters, body) in [
        ("use core::ops::ControlFlow::Break;", "Break: fn(u8) -> ControlFlow<u8>", "Break(0)"),
        (
            "use core::ops::ControlFlow::Break;",
            "(Break, _): (fn(u8) -> ControlFlow<u8>, u8)",
            "Break(0)",
        ),
        ("fn Break(_: u8) -> ControlFlow<u8> { panic!() }", "", "Break(0)"),
        ("use custom::ControlFlow as Custom;", "", "Custom::Break(0)"),
    ] {
        let source = format!(
            "use core::ops::ControlFlow; {declarations}
             fn flow({parameters}) -> ControlFlow<u8> {{ {body} }}"
        );
        let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
        assert_eq!(
            found
                .iter()
                .filter(|candidate| candidate.item_path.as_ref() == "flow")
                .map(|candidate| candidate.mutator)
                .collect::<Vec<_>>(),
            ["fn_value.break_default", "fn_value.continue_default"],
            "{source}"
        );
    }
    for (parameters, body) in [
        ("ControlFlow: Source", "ControlFlow::Break(0)"),
        ("ControlFlow: Source + Default", "ControlFlow::Continue(())"),
        ("Default: Source", "::core::ops::ControlFlow::Break(Default::default())"),
        (
            "Default: Source + ::core::default::Default",
            "::core::ops::ControlFlow::Break(Default::default())",
        ),
        ("T: Source + Default", "::core::ops::ControlFlow::Break(T::default())"),
    ] {
        let source = format!(
            "use core::ops::ControlFlow;
             trait Source {{}}
             fn flow<{parameters}>() -> ::core::ops::ControlFlow<u8> {{ {body} }}"
        );
        let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
        assert_eq!(
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
            ["fn_value.break_default", "fn_value.continue_default"],
            "{source}"
        );
    }
}

#[test]
fn control_flow_return_noops_include_implicit_unit_and_preserve_generic_value_namespaces() {
    for (body, retained, index) in [
        ("ControlFlow::Continue(())", "fn_value.break_default", 0),
        ("return ControlFlow::Continue(());", "fn_value.break_default", 0),
        ("ControlFlow::Break(0)", "fn_value.continue_default", 1),
        ("return ControlFlow::Break(0);", "fn_value.continue_default", 1),
    ] {
        let source = format!(
            "use core::ops::ControlFlow;
             fn flow<const ControlFlow: u8>() -> ::core::ops::ControlFlow<u8> {{ {body} }}"
        );
        let found = candidates(&source, "fn_value.break_default,fn_value.continue_default");
        assert_eq!(
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
            [retained],
            "{source}"
        );
        assert_eq!(found[0].replacement_index, index);
    }
}

#[test]
fn control_flow_return_identity_recognizes_grouped_empty_payloads_without_expanding_default_proofs() {
    with_collector(|collector| {
        let types = collector.types();
        for (payload, ty, expected) in [
            (syn::parse_quote!(""), syn::parse_quote!(&'static str), Some("Break".into())),
            (group(syn::parse_quote!("")), syn::parse_quote!(&'static str), Some("Break".into())),
            (group(syn::parse_quote!(())), syn::parse_quote!(()), Some("Break".into())),
            (syn::parse_quote!("nonempty"), syn::parse_quote!(&'static str), None),
        ] {
            let ty: syn::Type = ty;
            let signature = syn::parse_quote!(fn flow() -> core::ops::ControlFlow<#ty, #ty>);
            let mut call: syn::ExprCall = syn::parse_quote!(core::ops::ControlFlow::Break(#payload));
            *call.func = group((*call.func).clone());
            let expression = group(Expr::Call(call));
            let body = syn::parse_quote!({ #expression });
            assert_eq!(collector.constant_default_control_flow_return(&signature, &body, &types), expected);
        }
    });
    let found = candidates(
        r#"fn flow() -> core::ops::ControlFlow<&'static str> { core::ops::ControlFlow::Break("") }"#,
        "fn_value.break_default,fn_value.continue_default",
    );
    assert_eq!(
        found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
        ["fn_value.continue_default"]
    );
}

#[test]
fn ordering_return_noops_resolve_imported_qualified_and_aliased_variants() {
    for (original, mutators) in [
        ("Less", ["fn_value.equal", "fn_value.greater"]),
        ("Equal", ["fn_value.greater", "fn_value.less"]),
        ("Greater", ["fn_value.equal", "fn_value.less"]),
    ] {
        for expression in [
            format!("Ordering::{original}"),
            format!("core::cmp::Ordering::{original}"),
            format!("::std::cmp::Ordering::{original}"),
            format!("Alias::{original}"),
            format!("imported_{original}"),
            format!("(Ordering::{original})"),
            format!("return Ordering::{original};"),
        ] {
            let source = format!(
                "use core::cmp::Ordering;
                 use core::cmp::Ordering::{original} as imported_{original};
                 type Alias = Ordering;
                 fn order<'a, T, const N: usize>(unused: u8) -> Ordering {{ {expression} }}"
            );
            let found = candidates(&source, "fn_value.less,fn_value.equal,fn_value.greater");
            let actual = found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>();
            assert_eq!(actual, mutators, "{expression}");
        }
    }
}

#[test]
fn ordering_return_noop_filter_preserves_effectful_and_shadowed_alternatives() {
    for source in [
        "use core::cmp::Ordering; fn order() -> Ordering { effect(); Ordering::Less }",
        "use core::cmp::Ordering; fn order() -> Ordering { make_ordering() }",
        "use core::cmp::Ordering; fn order() -> Ordering { return; }",
        "use core::cmp::Ordering; fn order() -> Ordering { Ordering::Less; }",
        "use core::cmp::Ordering; fn order() -> Ordering { #[opaque] Ordering::Less }",
        "use core::cmp::Ordering; fn order() -> Ordering { #[opaque] (Ordering::Less) }",
        "use core::cmp::Ordering; fn order() -> Ordering { #[opaque] return Ordering::Less; }",
        "use core::cmp::Ordering; use core::cmp::Ordering::Less; fn order(Less: Ordering) -> Ordering { Less }",
        "use core::cmp::Ordering; use core::cmp::Ordering::Less; fn order((Less, _): (Ordering, u8)) -> Ordering { Less }",
        "use core::cmp::Ordering; use core::cmp::Ordering::Less; fn order(Less @ _: Ordering) -> Ordering { Less }",
        "use core::cmp::Ordering; use core::cmp::Ordering::Less; fn order(head @ Less: Ordering) -> Ordering { Less }",
        "use core::cmp::Ordering; trait Source { const Less: core::cmp::Ordering; } fn order<Ordering: Source>() -> core::cmp::Ordering { Ordering::Less }",
        "use core::cmp::Ordering; trait Source { const Less: core::cmp::Ordering; } fn order<Ordering: Source + Default>() -> core::cmp::Ordering { Ordering::Less }",
        "use core::cmp::Ordering; fn order() -> Ordering { const VALUE: Ordering = Ordering::Less; VALUE }",
    ] {
        let found = candidates(source, "fn_value.less,fn_value.equal,fn_value.greater");
        assert_eq!(
            found.iter().map(|candidate| candidate.mutator).collect::<Vec<_>>(),
            ["fn_value.equal", "fn_value.greater", "fn_value.less"],
            "{source}"
        );
    }
}

#[test]
fn enum_predicates_preserve_receiver_and_invert_complements() {
    let found = candidates(
        r"
        use core::cmp::Ordering;
        use core::ops::ControlFlow;
        fn checks(order: Ordering, flow: &ControlFlow<u8>) {
            order.is_lt(); order.is_le(); order.is_gt();
            order.is_ge(); order.is_eq(); order.is_ne();
            flow.is_break(); flow.is_continue();
        }
        ",
        "ordering,control_flow",
    );
    let actual = found.iter().map(|c| (c.mutator, c.replacement.as_str())).collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            ("ordering.is_lt_to_is_ge", "order.is_ge()"),
            ("ordering.is_le_to_is_gt", "order.is_gt()"),
            ("ordering.is_gt_to_is_le", "order.is_le()"),
            ("ordering.is_ge_to_is_lt", "order.is_lt()"),
            ("ordering.is_eq_to_is_ne", "order.is_ne()"),
            ("ordering.is_ne_to_is_eq", "order.is_eq()"),
            ("control_flow.is_break_to_is_continue", "flow.is_continue()"),
            ("control_flow.is_continue_to_is_break", "flow.is_break()"),
        ]
    );
}

#[test]
fn enum_mutations_reject_user_enums_atomic_ordering_and_shadowed_variants() {
    let found = candidates(
        r"
        enum Ordering { Less, Equal, Greater }
        enum ControlFlow<B, C> { Break(B), Continue(C) }
        fn local(order: Ordering, flow: ControlFlow<u8, ()>) -> Ordering {
            order.is_lt(); flow.is_break(); Ordering::Less
        }
        fn local_flow() -> ControlFlow<u8, ()> { ControlFlow::Break(1) }
        fn atomic() { consume(core::sync::atomic::Ordering::Relaxed); }
        fn shadow() {
            use core::cmp::Ordering::Less;
            let Less = 7;
            consume(Less);
        }
        ",
        "ordering,control_flow,fn_value.less,fn_value.equal,fn_value.greater,fn_value.break_default,fn_value.continue_default",
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn item_value_shadows_apply_before_declarations_and_inside_nested_functions() {
    let found = candidates(
        r"
        fn consume<T>(_: T) {}
        fn constant_before() { consume(before); const before: u8 = 7; }
        fn constant_after() { const before: u8 = 7; consume(before); }
        fn static_before() { consume(same); static same: u8 = 7; }
        fn static_after() { static same: u8 = 7; consume(same); }
        fn function_before() -> ControlFlow<u8> {
            stop(1);
            fn stop(_: u8) -> ControlFlow<u8> { panic!() }
            stop(2)
        }
        fn function_after() -> ControlFlow<u8> {
            fn stop(_: u8) -> ControlFlow<u8> { panic!() }
            stop(1)
        }
        fn nested() -> ControlFlow<u8> {
            fn stop(_: u8) -> ControlFlow<u8> { panic!() }
            fn inner() -> ControlFlow<u8> { stop(1) }
            inner()
        }
        fn pointer(_: u8) -> ControlFlow<u8> { panic!() }
        fn constant_constructor() -> ControlFlow<u8> {
            (stop)(1);
            const stop: fn(u8) -> ControlFlow<u8> = pointer;
            stop(2)
        }
        fn static_constructor() -> ControlFlow<u8> {
            static next: fn(u8) -> ControlFlow<u8> = pointer;
            next(1)
        }
        use core::cmp::Ordering::{Less as before, Equal as same};
        use core::ops::ControlFlow::{self, Break as stop, Continue as next};
        ",
        "ordering,control_flow",
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn generic_function_value_shadows_do_not_borrow_imported_enum_identity() {
    for (import, declaration, argument) in [
        ("core::cmp::Ordering::Less", "fn Less<T>() -> u8 { 7 }", ""),
        ("core::ops::ControlFlow::Break", "fn Break<T>(value: u8) -> u8 { value }", "1"),
    ] {
        let name = import.rsplit("::").next().unwrap();
        for callee in [format!("{name}::<u8>"), format!("({name}::<u8>)")] {
            let source = format!(
                "use {import};
                 fn before() -> u8 {{ let value = {callee}({argument}); {declaration} value }}
                 fn after() -> u8 {{ {declaration} {callee}({argument}) }}"
            );
            let found = candidates(&source, "ordering,control_flow");
            assert!(found.is_empty(), "{source}: {found:?}");
        }
    }
}

#[test]
fn item_value_shadows_respect_import_overrides_cfg_and_namespace_boundaries() {
    let found = candidates(
        r"
        use core::cmp::Ordering::{self, Less as before};
        use core::ops::ControlFlow::{self, Break as stop};
        fn consume<T>(_: T) {}
        fn restored() -> Ordering {
            { const before: u8 = 7; consume(before); }
            before
        }
        fn imported_again() -> Ordering {
            const before: u8 = 7;
            { use core::cmp::Ordering::Less as before; before }
        }
        fn inactive_shadow() -> Ordering {
            #[cfg(any())] const before: u8 = 7;
            before
        }
        fn type_namespace() -> Ordering {
            const Ordering: u8 = 7;
            Ordering::Less
        }
        fn constructor_imported_again() -> ControlFlow<u8> {
            fn stop(_: u8) -> ControlFlow<u8> { panic!() }
            { use core::ops::ControlFlow::Break as stop; stop(1) }
        }
        mod isolated {
            const before: u8 = 7;
            mod nested {
                use core::cmp::Ordering::Less as before;
                fn compare() -> core::cmp::Ordering { before }
            }
        }
        ",
        "ordering,control_flow",
    );
    assert_eq!(
        found.iter().map(|c| (c.item_path.as_ref(), c.mutator)).collect::<Vec<_>>(),
        [
            ("restored", "ordering.less_to_equal"),
            ("restored", "ordering.less_to_greater"),
            ("imported_again", "ordering.less_to_equal"),
            ("imported_again", "ordering.less_to_greater"),
            ("inactive_shadow", "ordering.less_to_equal"),
            ("inactive_shadow", "ordering.less_to_greater"),
            ("type_namespace", "ordering.less_to_equal"),
            ("type_namespace", "ordering.less_to_greater"),
            ("constructor_imported_again", "control_flow.break_to_continue"),
            ("isolated::nested::compare", "ordering.less_to_equal"),
            ("isolated::nested::compare", "ordering.less_to_greater"),
        ]
    );
}

#[test]
fn enum_return_values_compose_with_options_and_default_continuations() {
    let found = candidates(
        r"
        use core::cmp::Ordering;
        use core::ops::ControlFlow;
        struct NoDefault;
        fn compare(a: u8, b: u8) -> Ordering { a.cmp(&b) }
        fn partial(value: Option<Ordering>) -> Option<Ordering> { value }
        fn control(value: ControlFlow<NoDefault>) -> ControlFlow<NoDefault> { value }
        fn both() -> ControlFlow<u8, bool> { ControlFlow::Break(1) }
        ",
        "fn_value",
    );
    let returns = |name| {
        found
            .iter()
            .filter(|c| c.item_path.as_ref() == name)
            .map(|c| c.replacement.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        returns("compare"),
        [
            "::core::cmp::Ordering::Equal",
            "::core::cmp::Ordering::Greater",
            "::core::cmp::Ordering::Less"
        ]
    );
    assert_eq!(
        returns("partial"),
        [
            "None",
            "Some(::core::cmp::Ordering::Less)",
            "Some(::core::cmp::Ordering::Equal)",
            "Some(::core::cmp::Ordering::Greater)"
        ]
    );
    assert_eq!(
        returns("control"),
        ["::core::ops::ControlFlow::Continue(::core::default::Default::default())"]
    );
    assert_eq!(
        returns("both"),
        [
            "::core::ops::ControlFlow::Break(::core::default::Default::default())",
            "::core::ops::ControlFlow::Continue(::core::default::Default::default())"
        ]
    );
}

#[test]
fn enum_families_are_default_enabled_and_available_in_presets() {
    let defaults = Selection::parse("@default").unwrap();
    let control = Selection::parse("@control").unwrap();
    let boundary = Selection::parse("@boundary").unwrap();
    let semantics = Selection::parse("@semantics").unwrap();
    for mutator in crate::ops::registry::REGISTRY {
        if mutator.name.starts_with("control_flow.") {
            assert!(defaults.contains(mutator.name));
            assert!(control.contains(mutator.name));
            assert!(semantics.contains(mutator.name));
        } else if mutator.name.starts_with("ordering.") {
            assert!(defaults.contains(mutator.name));
            assert!(boundary.contains(mutator.name));
            assert!(semantics.contains(mutator.name));
        }
    }
}

#[test]
fn generic_control_flow_aliases_and_explicit_constructor_arguments_retain_payload_evidence() {
    let found = candidates(
        r"
        use core::ops::ControlFlow;
        type Flow<T> = ControlFlow<T>;
        fn alias() -> Flow<u8> { Flow::Continue(()) }
        fn explicit() { let value = ControlFlow::<u8, bool>::Break(7); }
        fn predicate(value: Flow<u8>) -> bool { value.is_break() }
        ",
        "control_flow",
    );
    assert_eq!(
        found.iter().map(|c| (c.item_path.as_ref(), c.mutator)).collect::<Vec<_>>(),
        [
            ("alias", "control_flow.continue_to_break"),
            ("explicit", "control_flow.break_to_continue"),
            ("predicate", "control_flow.is_break_to_is_continue"),
        ]
    );
}

#[test]
fn explicit_constructor_defaults_use_the_destination_not_the_source_payload() {
    let found = candidates(
        r"
        use core::ops::ControlFlow;
        struct NoDefault;
        fn consume<T>(_: T) {}
        fn stop() { consume(ControlFlow::<NoDefault, u8>::Break(NoDefault)); }
        fn proceed() { consume(ControlFlow::<u8, NoDefault>::Continue(NoDefault)); }
        fn parenthesized_stop() { consume((ControlFlow::<NoDefault, u8>::Break)(NoDefault)); }
        fn parenthesized_proceed() { consume((ControlFlow::<u8, NoDefault>::Continue)(NoDefault)); }
        fn blocked() { consume(ControlFlow::<NoDefault, NoDefault>::Break(NoDefault)); }
        fn parenthesized_blocked() { consume((ControlFlow::<NoDefault, u8>::Continue)(7)); }
        ",
        "control_flow",
    );
    assert_eq!(
        found
            .iter()
            .map(|c| (c.item_path.as_ref(), c.mutator, c.replacement.as_str(), c.replacement_index))
            .collect::<Vec<_>>(),
        [
            (
                "stop",
                "control_flow.break_to_continue",
                "::core::ops::ControlFlow::Continue(::core::default::Default::default())",
                0,
            ),
            (
                "proceed",
                "control_flow.continue_to_break",
                "::core::ops::ControlFlow::Break(::core::default::Default::default())",
                0,
            ),
            (
                "parenthesized_stop",
                "control_flow.break_to_continue",
                "::core::ops::ControlFlow::Continue(::core::default::Default::default())",
                0,
            ),
            (
                "parenthesized_proceed",
                "control_flow.continue_to_break",
                "::core::ops::ControlFlow::Break(::core::default::Default::default())",
                0,
            ),
        ]
    );
}

#[test]
fn token_grouped_constructor_defaults_retain_explicit_destination_evidence() {
    let mut file = SourceFile::parse(
        "enums.rs",
        r"
        struct NoDefault;
        fn stop() { let _ = (core::ops::ControlFlow::<NoDefault, u8>::Break)(NoDefault); }
        fn proceed() { let _ = (core::ops::ControlFlow::<u8, NoDefault>::Continue)(NoDefault); }
        fn blocked_stop() { let _ = (core::ops::ControlFlow::<u8, NoDefault>::Break)(7); }
        fn blocked_proceed() { let _ = (core::ops::ControlFlow::<NoDefault, u8>::Continue)(7); }
        "
        .into(),
    )
    .unwrap();
    for item in &mut file.ast.items {
        let syn::Item::Fn(function) = item else { continue };
        let syn::Stmt::Local(local) = &mut function.block.stmts[0] else {
            panic!()
        };
        let Expr::Call(call) = &mut *local.init.as_mut().unwrap().expr else {
            panic!()
        };
        *call.func = group((*call.func).clone());
    }
    let found = collect_with(
        &file,
        &Selection::parse("control_flow").unwrap(),
        &CfgSet::unconditional(),
        &Defaults::of(&file.ast),
    );
    assert_eq!(
        found.iter().map(|c| (c.item_path.as_ref(), c.mutator)).collect::<Vec<_>>(),
        [
            ("stop", "control_flow.break_to_continue"),
            ("proceed", "control_flow.continue_to_break"),
        ]
    );
}

#[test]
fn constructor_shadowing_and_malformed_arity_do_not_produce_enum_replacements() {
    let found = candidates(
        r"
        use core::ops::ControlFlow::{self, Break};
        fn shadow(Break: fn(u8) -> ControlFlow<u8>) -> ControlFlow<u8> { Break(1) }
        fn paren_shadow(Break: fn(u8) -> ControlFlow<u8>) -> ControlFlow<u8> { (Break)(1) }
        fn missing() { consume(ControlFlow::<u8>::Break()); }
        fn extra() { consume(ControlFlow::<u8>::Break(1, 2)); }
        ",
        "control_flow",
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn predicates_require_the_matching_enum_and_an_empty_argument_list() {
    let found = candidates(
        r"
        use core::cmp::Ordering;
        use core::ops::ControlFlow;
        fn checks(order: Ordering, flow: ControlFlow<u8>) {
            order.reverse(); flow.break_value();
            order.is_lt(1); flow.is_break(1);
            order.is_break(); flow.is_lt();
        }
        ",
        "ordering,control_flow",
    );
    assert!(found.is_empty(), "{found:?}");
}

fn with_collector(check: impl FnOnce(&mut Collector<'_>)) {
    let file = SourceFile::parse(
        "enums.rs",
        "use core::cmp::Ordering::Less; use core::ops::ControlFlow::Break;".into(),
    )
    .unwrap();
    let selection = Selection::parse("ordering,control_flow").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    check(&mut Collector::new(&file, &selection, selection.errors(), &cfg, &defaults));
}

#[test]
fn direct_enum_generators_reject_unrecognized_variants_and_predicate_names() {
    with_collector(|collector| {
        collector.control_flow_constructor(&syn::parse_quote!(core::ops::ControlFlow::Unknown(1)));
        collector.ordering_variant(&syn::parse_quote!(core::cmp::Ordering::Unknown));
        collector.enum_predicate(&syn::parse_quote!(value.reverse()), "reverse");
        assert_eq!(collector.candidates.len(), 0);
    });
}

fn group(expression: Expr) -> Expr {
    let span = expression.span();
    Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: Group { span },
        expr: Box::new(expression),
    })
}

#[test]
fn constant_enum_identity_rejects_attributed_groups_and_nondefault_tuple_payloads() {
    let attribute: syn::Attribute = syn::parse_quote!(#[opaque]);
    with_collector(|collector| {
        let signature = syn::parse_quote!(fn flow() -> core::ops::ControlFlow<(), ()>);
        let plain: Expr = syn::parse_quote!(core::ops::ControlFlow::Break(()));
        let mut attributed_group = group(plain.clone());
        let Expr::Group(node) = &mut attributed_group else { unreachable!() };
        node.attrs.push(attribute.clone());
        let mut attributed_tuple: syn::ExprTuple = syn::parse_quote!(());
        attributed_tuple.attrs.push(attribute.clone());
        let mut attributed_call: syn::ExprCall = syn::parse_quote!(core::ops::ControlFlow::Break(()));
        attributed_call.attrs.push(attribute.clone());
        let mut attributed_default: syn::ExprCall = syn::parse_quote!(Default::default());
        attributed_default.attrs.push(attribute);
        for expression in [
            attributed_group,
            Expr::Call(attributed_call),
            syn::parse_quote!(loop {}),
            syn::parse_quote!(return),
            syn::parse_quote!(core::ops::ControlFlow::Break()),
            syn::parse_quote!(core::ops::ControlFlow::Break((), ())),
            syn::parse_quote!(core::ops::ControlFlow::Break((1,))),
            syn::parse_quote!(core::ops::ControlFlow::Break(#attributed_tuple)),
            syn::parse_quote!(core::ops::ControlFlow::Break(#attributed_default)),
            syn::parse_quote!(({ core::ops::ControlFlow::Break })(())),
        ] {
            let body = syn::parse_quote!({ #expression });
            assert_eq!(
                collector.constant_default_control_flow_return(&signature, &body, &collector.types()),
                None,
                "{expression:?}"
            );
        }
        let body = syn::parse_quote!({ #plain });
        assert_eq!(
            collector
                .constant_default_control_flow_return(&signature, &body, &collector.types())
                .as_deref(),
            Some("Break")
        );
    });
}

#[test]
fn constant_enum_identity_checks_value_bindings_even_when_called_without_collection_prefilters() {
    with_collector(|collector| {
        let body = syn::parse_quote!({ Break(()) });
        let signature = syn::parse_quote!(fn flow() -> core::ops::ControlFlow<(), ()>);
        assert_eq!(
            collector
                .constant_default_control_flow_return(&signature, &body, &collector.types())
                .as_deref(),
            Some("Break")
        );
        let parameterized = syn::parse_quote!(fn flow(Break: fn(()) -> core::ops::ControlFlow<(), ()>));
        assert_eq!(
            collector.constant_default_control_flow_return(&parameterized, &body, &collector.types()),
            None
        );
        collector.bind("Break".into(), false);
        assert_eq!(
            collector.constant_default_control_flow_return(&signature, &body, &collector.types()),
            None
        );
    });
}

#[test]
fn qualified_self_paths_do_not_borrow_an_unrelated_imported_variant() {
    with_collector(|collector| {
        let types = collector.types();
        assert_eq!(variant(&syn::parse_quote!(<Local>::Less), &types, "cmp", "Ordering"), None);
        assert_eq!(control_flow_variant(&syn::parse_quote!(<Local>::Break), &types), None);
    });
}

#[test]
fn grouped_and_parenthesized_variant_callees_retain_their_identity() {
    with_collector(|collector| {
        let types = collector.types();
        for (expression, expected) in [
            (syn::parse_quote!((Break)), "Break"),
            (group(syn::parse_quote!(Break)), "Break"),
            (group(syn::parse_quote!((core::ops::ControlFlow::Continue))), "Continue"),
        ] {
            assert_eq!(control_flow_variant(&expression, &types).as_deref(), Some(expected));
        }
        assert_eq!(control_flow_variant(&syn::parse_quote!(make()), &types), None);
        assert_eq!(control_flow_variant(&syn::parse_quote!(Local::Break), &types), None);
    });
}

#[test]
fn binding_detection_peels_groups_without_classifying_other_expressions_as_bindings() {
    with_collector(|collector| {
        collector.bind("Break".into(), false);
        for expression in [
            syn::parse_quote!(Break),
            syn::parse_quote!(Break::<u8>),
            syn::parse_quote!((Break)),
            group(syn::parse_quote!(Break)),
        ] {
            assert!(collector.callee_is_binding(&expression));
        }
        for expression in [
            syn::parse_quote!(core::ops::ControlFlow::Break),
            syn::parse_quote!(core::ops::ControlFlow::Break::<u8>),
            syn::parse_quote!(::Break::<u8>),
            syn::parse_quote!(Break::Variant),
            syn::parse_quote!(<Owner>::Break),
            syn::parse_quote!(make()),
            syn::parse_quote!(Continue),
        ] {
            assert!(!collector.callee_is_binding(&expression));
        }
    });
}
