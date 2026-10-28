// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Expr, Item, Member, Stmt};

use super::super::{Defaults, collect_in, collect_with};
use super::{Collector, Confidence, Shape, compact_path};
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

// Initializes source-derived facts without traversal; callbacks drive selected visitor and binding transitions.
fn with_source_facts(source: &str, selector: &str, check: impl FnOnce(&mut Collector<'_>)) {
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::parse(selector).unwrap();
    let cfg = CfgSet::parse("");
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);
    check(&mut collector);
}

#[test]
fn campaign_generic_types_without_default_keep_only_constructible_return_alternatives() {
    let source = r"
        struct Region<'a>(&'a str);
        struct Holder<T>(T);
        fn region(text: &str) -> Region<'_> { Region(text) }
        fn optional(text: &str) -> Option<Region<'_>> { Some(Region(text)) }
        fn regions(text: &str) -> Vec<Region<'_>> { vec![Region(text)] }
        fn holder(value: u8) -> Holder<u8> { Holder(value) }
        fn unknown() -> dependency::Value { dependency::make() }
    ";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_with(
        &file,
        &Selection::parse("fn_value").unwrap(),
        &CfgSet::unconditional(),
        &Defaults::of(&file.ast),
    );
    let actual = found
        .iter()
        .map(|candidate| (candidate.item_path.as_ref(), candidate.mutator, candidate.replacement.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            ("optional", "fn_value.none", "None"),
            ("regions", "fn_value.empty_collection", "Vec::new()"),
            ("unknown", "fn_value.default", "Default::default()"),
        ]
    );
    assert_eq!(found.last().unwrap().confidence, super::Confidence::Optimistic);
}

#[test]
fn campaign_conditional_generic_defaults_remain_eligible_without_unconditional_proof() {
    let source = r"
        struct Holder<T>(T);
        impl<T: Default> Default for Holder<T> {
            fn default() -> Self { Self(T::default()) }
        }
        fn holder(value: u8) -> Holder<u8> { Holder(value) }
    ";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_with(
        &file,
        &Selection::parse("fn_value.default").unwrap(),
        &CfgSet::unconditional(),
        &Defaults::of(&file.ast),
    );
    assert!(
        found.iter().any(|candidate| candidate.item_path.as_ref() == "holder"),
        "a bounded generic implementation must not be mistaken for absence: {found:?}"
    );
}

#[test]
fn campaign_reference_arithmetic_rejects_mutable_references_without_removing_values() {
    let source = "fn next(remaining: &mut usize) { consume(remaining); consume(*remaining); } fn consume<T>(_: T) {}";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_in(
        &file,
        &Selection::parse("expr.increment,expr.decrement").unwrap(),
        &CfgSet::unconditional(),
    );
    assert!(
        found.iter().all(|candidate| file.slice(&candidate.span) != "remaining"),
        "numeric-use evidence must not turn an exclusive reference into an arithmetic operand: {found:?}"
    );
    let numeric = "fn next(remaining: &mut usize) -> usize { *remaining }";
    let file = SourceFile::parse("fixture.rs", numeric.into()).unwrap();
    let found = collect_in(
        &file,
        &Selection::parse("expr.increment,expr.decrement").unwrap(),
        &CfgSet::unconditional(),
    );
    assert_eq!(found.len(), 2, "dereferenced numeric values must remain eligible: {found:?}");
    assert!(found.iter().all(|candidate| file.slice(&candidate.span) == "*remaining"));
}

#[test]
fn campaign_reader_bounds_and_adapters_are_not_iterator_take_calls() {
    for source in [
        "fn f(reader: impl std::io::Read) { reader.take(10); }",
        "use std::io::Read; fn f(mut reader: impl Read) { reader.by_ref().take(10); }",
        "use std::io::BufRead; fn f(reader: &mut impl BufRead) { reader.by_ref().take(10); }",
        "use std::io::Read; fn f<R: Read>(reader: R) { reader.take(10); }",
        "use std::io; fn f<R>(reader: R) where R: io::Read { reader.take(10); }",
        "use std::io::Read; fn f(reader: &mut std::fs::File) { reader.by_ref().take(10); }",
    ] {
        let found = mutators(source, "iter.take_to_skip", &CfgSet::unconditional());
        assert!(found.is_empty(), "{source}: {found:?}");
    }
    for source in [
        "fn f(reader: impl Iterator<Item = u8>) { reader.take(10); }",
        "trait Read {} fn f(reader: impl Read) { reader.take(10); }",
        "fn f<R: std::io::Read>(reader: R) { fn nested<R>(reader: R) { reader.take(10); } }",
    ] {
        assert_eq!(
            mutators(source, "iter.take_to_skip", &CfgSet::unconditional()),
            ["iter.take_to_skip"],
            "{source}"
        );
    }
}

#[test]
fn campaign_result_breaks_do_not_request_a_nonexistent_default() {
    let source = "fn f() -> Result<Option<u8>, ()> { loop { break Ok(Some(1)); } }";
    assert!(
        mutators(source, "loop.break_value_default", &CfgSet::unconditional()).is_empty(),
        "Result has no Default implementation even when both payloads do"
    );
    let control = "fn f() -> u8 { loop { break 1u8; } }";
    assert_eq!(
        mutators(control, "loop.break_value_default", &CfgSet::unconditional()),
        ["loop.break_value_default"]
    );
}

#[test]
fn campaign_match_and_destructuring_propagate_reader_payloads_without_guessing_custom_variants() {
    for statement in [
        "match opened() { Ok(mut reader) => { reader.take(10); }, Err(_) => {} }",
        "let Ok(mut reader) = opened() else { return; }; reader.take(10);",
        "let (mut reader, _) = (file, 1u8); reader.take(10);",
    ] {
        let source = format!(
            "use std::io::Read; use std::fs::File;
             fn opened() -> std::io::Result<File> {{ todo!() }}
             fn f(file: File) {{ {statement} }}"
        );
        assert!(
            mutators(&source, "iter.take_to_skip", &CfgSet::unconditional()).is_empty(),
            "{source}"
        );
    }
    let source = "struct Ok<T>(T); fn f(value: Unknown) { match value { Ok(reader) => { reader.take(10); } } }";
    assert_eq!(
        mutators(source, "iter.take_to_skip", &CfgSet::unconditional()),
        ["iter.take_to_skip"]
    );
}

#[test]
fn campaign_inferred_character_casing_keeps_mutations_with_iterator_unification() {
    for source in [
        "fn f(text: &str) { for character in text.chars() { character.to_lowercase(); } }",
        "fn f(text: &str) { let mut chars = text.chars(); chars.next().map_or_else(String::new, |first| first.to_uppercase().collect::<String>()); }",
        "fn f(text: &str) { let mut chars = text.chars(); let Some(first) = chars.next() else { return; }; first.to_uppercase(); }",
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse("string").unwrap(), &CfgSet::unconditional());
        let casing = found
            .iter()
            .filter(|candidate| matches!(candidate.mutator, "string.lower_to_upper" | "string.upper_to_lower"))
            .collect::<Vec<_>>();
        assert_eq!(casing.len(), 1, "{source}: {found:?}");
        assert_eq!(casing[0].shape, super::Shape::IterExpr, "{source}");
    }
    let source = "struct Text; impl Text { fn chars(&self) -> Custom { todo!() } } fn f(text: Text) { for character in text.chars() { character.to_lowercase(); } }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_in(&file, &Selection::parse("string").unwrap(), &CfgSet::unconditional());
    let casing = found
        .iter()
        .filter(|candidate| matches!(candidate.mutator, "string.lower_to_upper" | "string.upper_to_lower"))
        .collect::<Vec<_>>();
    assert_eq!(casing.len(), 1);
    assert_eq!(casing[0].shape, super::Shape::Expr);
}

#[test]
fn campaign_result_breaks_propagate_through_matches_and_blocks() {
    for value in ["{ Ok(Some(1u8)) }", "match value { true => Ok(Some(1u8)), false => Err(()) }"] {
        let source = format!("fn f(value: bool) -> Result<Option<u8>, ()> {{ loop {{ break {value}; }} }}");
        assert!(
            mutators(&source, "loop.break_value_default", &CfgSet::unconditional()).is_empty(),
            "{source}"
        );
    }
    let source = "fn f(value: bool) -> u8 { loop { break match value { true => 1u8, false => 2u8 }; } }";
    assert_eq!(
        mutators(source, "loop.break_value_default", &CfgSet::unconditional()),
        ["loop.break_value_default"]
    );
}

#[test]
fn campaign_open_options_propagate_file_payloads_without_defaulting_unknown_tuple_members() {
    let source = r#"
        use std::fs::OpenOptions;
        fn f() {
            loop {
                match OpenOptions::new().create_new(true).write(true).open("file") {
                    Ok(file) => break (unknown(), file),
                    Err(_) => {}
                }
            }
        }
    "#;
    assert!(mutators(source, "loop.break_value_default", &CfgSet::unconditional()).is_empty());
    let control = "struct OpenOptions; fn f() { loop { break (unknown(), OpenOptions::new().open(\"file\")); } }";
    assert_eq!(
        mutators(control, "loop.break_value_default", &CfgSet::unconditional()),
        ["loop.break_value_default"]
    );
}

#[test]
fn campaign_structural_ownership_does_not_require_a_closed_crate_or_treat_copy_fields_as_moves() {
    let source = r#"
        mod dependency;
        #[derive(Debug)] struct Owned { value: String }
        struct CopyValue { value: u8 }
        struct Borrowed { value: &'static str }
        #[transform] struct Transformed { value: String }
        #[transform] mod altered { pub struct Nested { value: String } }
        struct Conditional { #[cfg(feature = "owning")] value: String }
    "#;
    with_source_facts(source, "stmt.delete_assign", |collector| {
        for (ty, expected) in [
            (syn::parse_quote!(Owned), true),
            (syn::parse_quote!(CopyValue), false),
            (syn::parse_quote!(Borrowed), false),
            (syn::parse_quote!(Transformed), false),
            (syn::parse_quote!(altered::Nested), false),
            (syn::parse_quote!(Conditional), false),
        ] {
            assert_eq!(collector.noncopy_type(&ty, 8), expected, "{ty:?}");
        }
    });
}

#[test]
fn campaign_method_self_results_and_tuple_rest_patterns_keep_their_actual_owners() {
    let source = r"
        struct Other;
        impl Other { fn owned(self) -> Self { self } }
        struct Caller;
        impl Caller { fn run(value: Other) { let result = value.owned(); } }
        fn tuple() { let (.., last) = (false, 1u8, String::new()); }
    ";
    with_source_facts(source, "fn_value.default", |collector| {
        collector.impl_self_type = Some(syn::parse_quote!(Caller));
        collector.bind_type("value".into(), syn::parse_quote!(Other));
        assert_eq!(
            collector.inferred_expression_type(&syn::parse_quote!(value.owned())),
            Some(syn::parse_quote!(crate::Other)),
        );
        let pattern: syn::Pat = syn::parse_quote!((.., last));
        collector.bind_pattern(&pattern, Some(&syn::parse_quote!((bool, u8, String))));
        assert_eq!(collector.binding_types["last"].ty, syn::parse_quote!(String));
    });
}

#[test]
fn local_literal_and_range_patterns_do_not_emit_expression_mutations() {
    for (source, selector, original, replacement, mutator) in [
        (
            "fn f(value:u8){let 7=value else{return;};let _=9;}",
            "literal.int_to_zero",
            "9",
            "0",
            "literal.int_to_zero",
        ),
        (
            "fn f(value:u8){let 3..=5=value else{return;};let _=9;}",
            "literal.int_to_zero,range.inclusive_to_exclusive",
            "9",
            "0",
            "literal.int_to_zero",
        ),
        (
            "fn f(value:bool){let true=value else{return;};let _=false;}",
            "literal.bool_flip",
            "false",
            "true",
            "literal.bool_flip",
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(found.len(), 1, "{source}: {found:?}");
        let candidate = &found[0];
        let start = source.rfind(original).unwrap();
        assert_eq!(candidate.span, start..start + original.len());
        assert_eq!(file.slice(&candidate.span), original);
        assert_eq!(candidate.mutator, mutator);
        assert_eq!(candidate.replacement, replacement);
        assert_eq!(candidate.replacement_index, 0);
        assert_eq!(candidate.shape, super::Shape::Expr);
        assert_eq!(candidate.confidence, super::Confidence::Proven);
    }
}

#[test]
fn local_pattern_exclusion_preserves_initializer_divergence_and_sibling_candidates() {
    let source = "fn f(value:u8){let 7=std::hint::black_box(value+13) else{std::hint::black_box(11);return;};let _=9;}";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::unconditional());
    let originals: Vec<_> = found.iter().map(|candidate| file.slice(&candidate.span)).collect();
    assert_eq!(originals, ["13", "11", "9"]);
    for (candidate, original) in found.iter().zip(["13", "11", "9"]) {
        let start = source.find(original).unwrap();
        assert_eq!(candidate.span, start..start + original.len());
        assert_eq!(candidate.replacement, "0");
        assert_eq!(candidate.replacement_index, 0);
        assert_eq!(candidate.shape, super::Shape::Expr);
        assert_eq!(candidate.confidence, super::Confidence::Proven);
    }
}

#[test]
fn local_binding_evidence_preserves_pattern_trust_and_clears_shadowed_types() {
    for (pattern, trusted) in [("value", true), ("ref value", false), ("value @ _", false)] {
        let source = format!("fn f() {{let {pattern}:u32=7;}}");
        with_source_facts(&source, "literal.int_to_zero", |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            let Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            collector.visit_local(local);
            assert_eq!(collector.binding_types["value"].resource_trusted, trusted, "{pattern}");
        });
    }
    for statement in ["let value=unknown;", "let value;"] {
        let source = format!("fn f(){{{statement}}}");
        with_source_facts(&source, "literal.int_to_zero", |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            let Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            collector.bind("value".into(), true);
            collector.bind_type("value".into(), syn::parse_quote!(u32));
            collector.visit_local(local);
            assert!(!collector.binding_types.contains_key("value"), "{statement}");
            assert!(!collector.inferred_unsigned.contains("value"), "{statement}");
            assert_eq!(collector.bindings.get("value"), Some(&false), "{statement}");
        });
    }
}

#[test]
fn untyped_shadowing_restores_outer_type_evidence_after_leaving_its_scope() {
    with_source_facts("fn f() {}", "expr.increment", |collector| {
        collector.bind_type("value".into(), syn::parse_quote!(u32));
        collector.in_scope(|collector| {
            collector.bind_unknown_type("value".into());
            assert!(!collector.binding_types.contains_key("value"));
            collector.in_scope(|collector| {
                collector.bind_type("value".into(), syn::parse_quote!(bool));
                assert_eq!(collector.binding_types["value"].ty, syn::parse_quote!(bool));
            });
            assert!(!collector.binding_types.contains_key("value"));
        });
        assert_eq!(collector.binding_types["value"].ty, syn::parse_quote!(u32));
        assert!(collector.is_known_unsigned(&syn::parse_quote!(value)));
    });
}

#[test]
fn inferred_locals_keep_unsigned_evidence_separate_from_resource_trust() {
    with_source_facts(
        "fn f(source:u32){let value=u32::from(source);let copy=source;}",
        "literal.int_to_zero",
        |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            collector.in_function(&function.sig, |collector| {
                for statement in &function.block.stmts {
                    let Stmt::Local(local) = statement else { unreachable!() };
                    collector.visit_local(local);
                }
                assert!(collector.inferred_unsigned.contains("value"));
                assert!(!collector.binding_types["copy"].resource_trusted);
                assert_eq!(collector.binding_types["copy"].ty, syn::parse_quote!(u32));
            });
        },
    );
}

#[test]
fn iterator_bindings_restore_prior_membership_and_cannot_escape_nested_functions() {
    let source = "fn f(text:&str){let iter=text.chars().peekable();let iter=text.chars().peekable();fn nested(){}}";
    for had_prior in [false, true] {
        with_source_facts(source, "literal.int_to_zero", |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            collector.in_function(&function.sig, |collector| {
                let Stmt::Local(first) = &function.block.stmts[0] else {
                    unreachable!()
                };
                let Stmt::Local(second) = &function.block.stmts[1] else {
                    unreachable!()
                };
                let Stmt::Item(Item::Fn(nested)) = &function.block.stmts[2] else {
                    unreachable!()
                };
                if had_prior {
                    collector.visit_local(first);
                }
                collector.in_scope(|collector| {
                    collector.visit_local(second);
                    assert!(collector.resource_iterators.contains("iter"));
                    collector.in_function(&nested.sig, |collector| {
                        assert!(!collector.resource_iterators.contains("iter"));
                    });
                    assert!(collector.resource_iterators.contains("iter"));
                });
                assert_eq!(collector.resource_iterators.contains("iter"), had_prior);
            });
        });
    }
}

#[test]
fn diverging_let_else_initializers_do_not_supply_resource_iterator_evidence() {
    let source = "fn f(text:&str){let iter=text.chars().peekable() else {return;};}";
    with_source_facts(source, "literal.int_to_zero", |collector| {
        let file = collector.file;
        let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
        collector.in_function(&function.sig, |collector| {
            let Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            collector.visit_local(local);
            assert!(!collector.resource_iterators.contains("iter"));
        });
    });
}

#[test]
fn default_and_payload_evidence_requires_the_correct_variant_family_and_parameter_type() {
    for (source, selector) in [
        ("fn Err(_:())->Option<u8>{Some(7)} fn f()->Option<u8>{Err(())}", "result.err_to_ok"),
        (
            "fn Err(_:())->Option<&'static u8>{None} fn f(value:&'static u8)->Option<&'static u8>{Err(())}",
            "result.err_to_ok",
        ),
        ("fn f(other:&'static u16)->Option<&'static u8>{None}", "option.none_to_some"),
        (
            "const None:Result<&'static u8,()>=Err(()); fn f(value:&'static u8)->Result<&'static u8,()>{None}",
            "option.none_to_some",
        ),
    ] {
        with_source_facts(source, selector, |collector| {
            collector.visit_file(&collector.file.ast);
            assert!(collector.candidates.is_empty(), "{source}: {:?}", collector.candidates);
        });
    }
    with_source_facts(
        "type Output=Option<&'static u8>;fn f(value:&'static u8)->Output{None}",
        "option.none_to_some",
        |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), 1);
            assert_eq!(collector.candidates[0].replacement, "Some(value)");
            assert_eq!(collector.candidates[0].replacement_index, 0);
        },
    );
}

#[test]
fn typed_variant_evidence_uses_associated_types_and_lexically_visible_imports() {
    for (source, replacement) in [
        (
            "trait Has{type Error;fn f()->Result<u8,Self::Error>;}struct S;impl Has for S{type Error=u16;fn f()->Result<u8,Self::Error>{Ok(7)}}",
            "Err(Default::default())",
        ),
        (
            "use foreign::HashMap;mod child{use std::collections::HashMap;fn f()->Option<HashMap<u8,u8>>{None}}",
            "Some(Default::default())",
        ),
    ] {
        with_source_facts(source, "option.none_to_some,result.ok_to_err", |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), 1, "{source}: {:?}", collector.candidates);
            assert_eq!(collector.candidates[0].replacement, replacement);
        });
    }
}

#[test]
fn nested_return_context_keeps_associated_nondefault_error_evidence() {
    let source = "struct String;struct S;trait Has{type Error;fn f();}impl Has for S{type Error=String;fn f(){let _=||->Result<u8,Self::Error>{Ok(7)};}}";
    with_source_facts(source, "result.ok_to_err", |collector| {
        collector.visit_file(&collector.file.ast);
        assert!(collector.candidates.is_empty(), "{:?}", collector.candidates);
    });
}

#[test]
fn assignment_default_evidence_resolves_aliases_and_associated_types_but_requires_a_type() {
    for source in [
        "type Value=Option<u8>;fn f(mut target:Value){target=Some(7);}",
        "trait Has{type Value;fn f(target:Self::Value);}struct S;impl Has for S{type Value=u32;fn f(mut target:Self::Value){target=7;}}",
    ] {
        with_source_facts(source, "assign_value.default", |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), 1, "{source}: {:?}", collector.candidates);
            assert_eq!(collector.candidates[0].replacement, "Default::default()");
            assert_eq!(collector.candidates[0].replacement_index, 0);
        });
    }
    with_source_facts("fn f(){unknown=7;}", "assign_value.default", |collector| {
        collector.visit_file(&collector.file.ast);
        assert!(collector.candidates.is_empty());
    });
}

#[test]
fn configured_out_struct_fields_do_not_hide_later_active_field_expressions() {
    let source = "struct S{#[cfg(any())] hidden:u32,kept:u32}fn f(){let _=S{#[cfg(any())] hidden:7,kept:9};}";
    with_source_facts(source, "literal.int_to_zero", |collector| {
        collector.visit_file(&collector.file.ast);
        assert_eq!(collector.candidates.len(), 1);
        assert_eq!(collector.file.slice(&collector.candidates[0].span), "9");
        assert_eq!(collector.candidates[0].replacement, "0");
    });
}

#[test]
fn unsigned_zero_collection_records_only_zero_literals_and_real_sum_sources() {
    for (text, expected) in [
        ("0", &["0"][..]),
        ("7", &[][..]),
        ("false", &[][..]),
        ("'x'", &[][..]),
        ("iter.map(|_|0).sum()", &["0"][..]),
        ("iter.map(|_|0).count()", &[][..]),
    ] {
        let expression: Expr = syn::parse_str(text).unwrap();
        let mut spans = Vec::new();
        super::unsigned_zero_spans(&expression, &mut spans);
        let actual: Vec<_> = spans.iter().map(|span| &text[span.clone()]).collect();
        assert_eq!(actual, expected, "{text}");
    }
}

#[test]
fn expression_type_proofs_keep_parentheses_inferred_time_and_conservative_bare_option_vetoes() {
    with_source_facts("use std::time::Duration;fn f<Option>(value:u32){}", "all", |collector| {
        let Item::Fn(function) = collector.file.ast.items[1].clone() else {
            unreachable!()
        };
        collector.in_function(&function.sig, |collector| {
            assert_eq!(
                collector.type_of_expression(&syn::parse_quote!((value))),
                Some(&syn::parse_quote!(u32))
            );
            assert!(collector.is_known_textual_or_temporal(&syn::parse_quote!(Duration::from_secs(7))));
            assert!(collector.is_known_textual_or_temporal_type(&syn::parse_quote!(Option)));
            assert!(!collector.is_known_textual_or_temporal_type(&syn::parse_quote!(xyzzy)));
            assert!(!collector.is_known_textual_or_temporal_type(&syn::parse_quote!(u32)));
        });
    });
}

#[test]
fn textual_ordering_results_and_capacity_arguments_do_not_gain_numeric_perturbations() {
    for source in ["fn f(){consume(\"a\".min(\"b\"));}", "fn f(){consume(\"a\".max(\"b\"));}"] {
        with_source_facts(source, "expr.increment", |collector| {
            collector.visit_file(&collector.file.ast);
            assert!(collector.candidates.is_empty(), "{source}: {:?}", collector.candidates);
        });
    }
    let source = "fn f(mut values:Vec<u8>,count:usize){values.reserve(count);values.get(count);}";
    with_source_facts(source, "expr.increment", |collector| {
        collector.visit_file(&collector.file.ast);
        assert_eq!(collector.candidates.len(), 1);
        let candidate = &collector.candidates[0];
        assert_eq!(collector.file.slice(&candidate.span), "count");
        assert_eq!(candidate.span.start, source.rfind("count").unwrap());
    });
}

#[test]
fn invalid_original_regexes_cannot_be_repaired_into_mutation_candidates() {
    assert_eq!(super::regex_mutations(r"\x?41"), [("regex.optional_to_required", r"\x41".into())]);
    for (pattern, expected) in [(r"\x?41", 0), ("^a", 1)] {
        let source = format!("fn f(){{let _=Regex::new({pattern:?});}}");
        with_source_facts(&source, "regex.optional_to_required,regex.remove_start_anchor", |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), expected, "{pattern}: {:?}", collector.candidates);
            if expected != 0 {
                assert_eq!(collector.candidates[0].replacement, "\"a\"");
            }
        });
    }
}

#[test]
fn condition_range_and_option_rewrites_preserve_their_assigned_zero_ordinals() {
    for (source, selector, original, replacement) in [
        ("fn f(){if ready{}}", "cond.negate", "ready", "!(ready)"),
        ("fn f(){while ready{}}", "cond.negate", "ready", "!(ready)"),
        (
            "fn f(){let _=lo..hi;}",
            "range.exclusive_to_inclusive",
            "lo..hi",
            "(lo)..((hi) + 1)",
        ),
        (
            "fn f(){let _=lo..=hi;}",
            "range.inclusive_to_exclusive",
            "lo..=hi",
            "(lo)..=((hi) - 1)",
        ),
        (
            "fn f()->Option<u8>{None}",
            "option.none_to_some",
            "None",
            "Some(Default::default())",
        ),
    ] {
        with_source_facts(source, selector, |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), 1, "{source}: {:?}", collector.candidates);
            let candidate = &collector.candidates[0];
            assert_eq!(collector.file.slice(&candidate.span), original);
            assert_eq!(candidate.replacement, replacement);
            assert_eq!(candidate.replacement_index, 0);
        });
    }
    with_source_facts("fn f(){if (false){}}", "cond.always_false", |collector| {
        collector.visit_file(&collector.file.ast);
        assert!(collector.candidates.is_empty());
    });
}

#[test]
fn range_start_and_endpoint_are_independent_numeric_perturbation_sites() {
    with_source_facts("fn f(lo:u32,hi:u32){let _=lo..hi;}", "expr.increment", |collector| {
        collector.visit_file(&collector.file.ast);
        let originals: Vec<_> = collector
            .candidates
            .iter()
            .map(|candidate| collector.file.slice(&candidate.span))
            .collect();
        assert_eq!(originals, ["hi", "lo"]);
        assert_eq!(collector.candidates[0].replacement, "(hi) + 1");
        assert_eq!(collector.candidates[1].replacement, "(lo) + 1");
    });
}

#[test]
fn unknown_assignment_types_cannot_claim_signed_zero_confidence() {
    with_source_facts(
        "fn f(mut known:i32,mut unknown:dependency::Integer){known=0;unknown=0;}",
        "literal.int_decrement",
        |collector| {
            collector.visit_file(&collector.file.ast);
            let confidences: Vec<_> = collector.candidates.iter().map(|candidate| candidate.confidence).collect();
            assert_eq!(confidences, [super::Confidence::Proven, super::Confidence::Optimistic]);
            assert!(collector.candidates.iter().all(|candidate| candidate.replacement == "-1"));
        },
    );
}

#[test]
fn value_bearing_breaks_and_diverging_else_loops_keep_their_control_flow_obligations() {
    for (source, selector, expected) in [
        ("fn f(){let _=loop{break 7;};}", "loop.delete_break,loop.break_to_continue", 0),
        (
            "fn f(value:Option<u8>){let Some(value)=value else{loop{continue;}};}",
            "loop.continue_to_break",
            0,
        ),
        ("fn f(){let _=loop{for _ in 0..1{continue;}break 7;};}", "loop.continue_to_break", 1),
        ("fn f(work:fn()){loop{work();break;}}", "loop.break_to_continue", 1),
    ] {
        with_source_facts(source, selector, |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), expected, "{source}: {:?}", collector.candidates);
            for candidate in &collector.candidates {
                assert_eq!(
                    candidate.replacement,
                    if selector == "loop.continue_to_break" {
                        "break"
                    } else {
                        "continue"
                    }
                );
            }
        });
    }
}

#[test]
fn parameter_patterns_and_impl_generic_contexts_keep_independent_proof_evidence() {
    for (pattern, trusted) in [("value", true), ("ref value", false), ("value @ _", false)] {
        let source = format!("fn f({pattern}:u32){{}}");
        with_source_facts(&source, "all", |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            collector.in_function(&function.sig, |collector| {
                assert_eq!(collector.binding_types["value"].resource_trusted, trusted);
            });
        });
    }
    for (source, selector, expected) in [
        ("struct S<T>(T);impl<T>S<T>{fn f()->T{panic!()}}", "fn_value.default", 0),
        ("struct S<T>(T);impl<T:Default>S<T>{fn f()->T{panic!()}}", "fn_value.default", 1),
        (
            "struct S<T>(T);impl<T:Iterator>S<T>{fn f(value:T){let _=value.filter(|_|true);}}",
            "iter.remove_filter",
            1,
        ),
    ] {
        with_source_facts(source, selector, |collector| {
            collector.visit_file(&collector.file.ast);
            assert_eq!(collector.candidates.len(), expected, "{source}: {:?}", collector.candidates);
            assert!(
                collector
                    .candidates
                    .iter()
                    .all(|candidate| candidate.confidence == super::Confidence::Proven),
                "{source}: {:?}",
                collector.candidates
            );
        });
    }
}

#[test]
fn default_recursion_filter_does_not_remove_nonrecursive_value_families() {
    let source = "struct String;impl Default for String{fn default()->Self{String}}";
    with_source_facts(
        source,
        "fn_value.empty_string,fn_value.xyzzy_string,fn_value.default",
        |collector| {
            collector.visit_file(&collector.file.ast);
            let replacements: Vec<_> = collector
                .candidates
                .iter()
                .map(|candidate| candidate.replacement.as_str())
                .collect();
            assert_eq!(replacements, ["String::new()", "\"xyzzy\".to_owned()"]);
            assert!(
                collector
                    .candidates
                    .iter()
                    .all(|candidate| candidate.confidence == super::Confidence::Optimistic)
            );
        },
    );
}

#[test]
fn ordinal_conversion_preserves_small_values_and_the_largest_representable_value() {
    assert_eq!(super::saturating_ordinal(0), 0);
    assert_eq!(super::saturating_ordinal(7), 7);
    if let Ok(maximum) = usize::try_from(u32::MAX) {
        assert_eq!(super::saturating_ordinal(maximum - 1), u32::MAX - 1);
        assert_eq!(super::saturating_ordinal(maximum), u32::MAX);
    }
}

#[test]
#[cfg(target_pointer_width = "64")]
fn ordinal_conversion_saturates_above_the_u32_boundary_without_allocating_a_huge_collection() {
    let maximum = usize::try_from(u32::MAX).unwrap();
    assert_eq!(super::saturating_ordinal(maximum + 1), u32::MAX);
    assert_eq!(super::saturating_ordinal(usize::MAX), u32::MAX);
}

#[test]
fn numeric_and_iterator_proofs_preserve_qualification_parentheses_and_negative_controls() {
    with_source_facts("use core::u32 as Number; const COUNT:u32=7; fn f() {}", "all", |collector| {
        assert!(collector.is_primitive_constant(&syn::parse_quote!(Number::MAX)));
        assert!(collector.is_known_numeric(&syn::parse_quote!(Number::MAX)));
        assert!(!collector.is_known_numeric(&syn::parse_quote!(<Other>::COUNT)));
        assert!(collector.is_known_numeric(&syn::parse_quote!(COUNT)));
        assert_eq!(
            collector.numeric_confidence(&syn::parse_quote!(!1u8)),
            super::Confidence::Optimistic
        );
        assert_eq!(collector.numeric_confidence(&syn::parse_quote!(-1i8)), super::Confidence::Proven);
        for (text, expected) in [
            ("u32::from(value)", true),
            ("<Other>::u32::from(value)", false),
            ("from(value)", false),
        ] {
            let call = syn::parse_str(text).unwrap();
            assert_eq!(collector.is_primitive_numeric_associated_call(&call), expected, "{text}");
        }
        assert!(collector.is_iterator_receiver(&syn::parse_quote!((0..3))));
        assert!(!collector.is_iterator_receiver(&syn::parse_quote!((unknown))));
    });
}

#[test]
fn standard_paths_require_both_complete_qualifiers_and_the_requested_terminal_name() {
    for (text, expected) in [
        ("std::env::var", true),
        ("std::env", false),
        ("std::env::var::extra", false),
        ("other::env::var", false),
    ] {
        assert_eq!(
            super::path_matches(&syn::parse_str(text).unwrap(), &["std", "env", "var"]),
            expected,
            "{text}"
        );
    }
    let imports = crate::HashMap::from_iter([("Clock".into(), Some(vec!["std".into(), "time".into(), "Instant".into()]))]);
    assert!(super::imported_item_matches(
        &imports,
        &syn::parse_quote!(Clock),
        "Instant",
        &["std", "time"]
    ));
    assert!(!super::imported_item_matches(
        &imports,
        &syn::parse_quote!(Clock),
        "Duration",
        &["std", "time"]
    ));
    assert!(!super::standard_type_path(
        &syn::parse_quote!(std::time::Instant),
        &imports,
        "Duration",
        &["std", "time"]
    ));
    assert!(super::standard_type_path(
        &syn::parse_quote!(std::time::Duration),
        &imports,
        "Duration",
        &["std", "time"]
    ));
    with_source_facts("use std::vec::Vec as Buffer; fn f() {}", "all", |collector| {
        assert!(!collector.type_has_standard_iterator_methods(&syn::parse_quote!(Buffer<u8>), collector.current_imports()));
        assert!(collector.type_has_standard_iterator_methods(&syn::parse_quote!(Vec<u8>), collector.current_imports()));
    });
}

#[test]
fn tuple_default_proofs_require_every_element_and_none_requires_an_option_context() {
    with_source_facts("const None:bool=true; fn f() {}", "all", |collector| {
        for (text, expected) in [("(1, false)", true), ("()", true), ("(1, unknown)", false), ("(unknown, 1)", false)] {
            assert_eq!(
                collector.expression_can_directly_default(&syn::parse_str(text).unwrap()),
                expected,
                "{text}"
            );
        }
        assert!(!collector.expression_is_known_default_for_type(&syn::parse_quote!(None), &syn::parse_quote!(bool)));
        assert!(collector.expression_is_known_default_for_type(&syn::parse_quote!(None), &syn::parse_quote!(Option<u32>)));
    });
}

#[test]
fn typed_boolean_rewrites_never_negate_a_pattern_binding_condition() {
    with_source_facts(
        "fn f() { if let Some(value)=option {} if ready {} }",
        "bool_expr.negate",
        |collector| {
            let file = collector.file;
            let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
            for statement in &function.block.stmts {
                let Stmt::Expr(Expr::If(branch), _) = statement else {
                    unreachable!()
                };
                collector.boolean_expression(&branch.cond, &syn::parse_quote!(bool));
            }
            assert_eq!(collector.candidates.len(), 1);
            assert_eq!(collector.file.slice(&collector.candidates[0].span), "ready");
            assert_eq!(collector.candidates[0].replacement, "!(ready)");
        },
    );
}

#[test]
fn lexical_default_imports_do_not_erase_the_independent_root_default_path_veto() {
    with_source_facts("use foreign::Default; fn f() {use core::default::Default;}", "all", |collector| {
        let Item::Fn(function) = &collector.file.ast.items[1] else {
            unreachable!()
        };
        collector.scope_path.push(super::indexes::block_id(&function.block));
        assert!(collector.types().supports_bare_default_trait());
        assert!(!collector.standard_default_replacement());
    });
}

#[test]
fn unresolved_textual_operands_preserve_custom_operator_possibilities_and_comparisons() {
    for source in [
        "fn f(){let _=String::from(\"a\")+suffix();}",
        "fn f(){let _=prefix()+\"suffix\";}",
        "fn f(left:String){let _=left+suffix();}",
        "fn f(right:&str){let _=prefix()+right;}",
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        assert!(
            collect_in(&file, &Selection::parse("arith.add_to_sub").unwrap(), &CfgSet::unconditional()).len() == 1,
            "{source}"
        );
    }
    let file = SourceFile::parse("fixture.rs", "fn f(left:String,right:String)->bool{left==right}".into()).unwrap();
    let found = collect_in(&file, &Selection::parse("relational.eq_to_ne").unwrap(), &CfgSet::unconditional());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].replacement, "(left) != (right)");
}

#[test]
fn right_hand_temporal_evidence_does_not_resolve_an_unknown_left_operand() {
    let file = SourceFile::parse("fixture.rs", "fn f(right:std::time::Duration){let _=left+right;}".into()).unwrap();
    let found = collect_in(&file, &Selection::parse("arith").unwrap(), &CfgSet::unconditional());
    assert_eq!(found.len(), 2);
    let found = found.iter().find(|candidate| candidate.mutator == "arith.add_to_sub").unwrap();
    assert_eq!(found.replacement, "(left) - (right)");
    assert_eq!(found.replacement_index, 0);
}

#[test]
fn configured_out_statements_and_unrelated_macros_do_not_stop_or_join_active_collection() {
    for (source, selector, expected, original) in [
        ("fn f(){#[cfg(any())] hidden(); active();}", "stmt.delete_call", 1, "active();"),
        (
            "fn f(){#[cfg(any())] vec![1,2]; other![5,6]; vec![3,4];}",
            "collection.omit_element",
            2,
            "vec![3,4]",
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::parse(""));
        assert_eq!(found.len(), expected, "{source}: {found:?}");
        for candidate in &found {
            assert_eq!(file.slice(&candidate.span), original);
        }
    }
}

#[test]
fn every_unsigned_and_nested_value_traversal_keeps_observable_nonzero_children() {
    for (source, expected) in [
        ("fn callee(value:u32){} fn f(){callee(7);}", vec!["7"]),
        ("fn f(){std::time::Duration::from_secs(7);}", vec!["7"]),
        ("fn f(){let value:u32=7;}", vec!["7"]),
        ("fn f(values:&[u8]){let index={7}; let _=values[index];}", vec!["7"]),
        ("fn f()->u32{return 7;}", vec!["7"]),
        ("fn f(mut value:u32){value=7;}", vec!["7"]),
        ("fn f(values:&mut[u8]){values[7]=9;}", vec!["7", "9"]),
        ("fn f(){[7].as_slice(9);}", vec!["7", "9"]),
        ("fn f(){let _=[7][0];}", vec!["7"]),
        ("fn f(){let _=(7 as u32)==unknown;let _=unknown==(9 as u32);}", vec!["7", "9"]),
        ("fn f(){let _=fallible(7)?;}", vec!["7"]),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::unconditional());
        assert_eq!(
            found.iter().map(|candidate| file.slice(&candidate.span)).collect::<Vec<_>>(),
            expected,
            "{source}: {found:?}"
        );
        for candidate in &found {
            assert_eq!(candidate.replacement, "0");
            assert_eq!(candidate.replacement_index, 0);
        }
    }
}

#[test]
fn numeric_conversion_proofs_require_a_primitive_numeric_owner() {
    for (source, expected) in [
        ("u32::from(value)", true),
        ("i64::from_be_bytes(bytes)", true),
        ("f32::from(value)", true),
        ("String::from(value)", false),
        ("Custom::from_be_bytes(bytes)", false),
        ("u32::new(value)", false),
        ("from(value)", false),
    ] {
        let call: syn::ExprCall = syn::parse_str(source).unwrap();
        assert_eq!(super::numeric_associated_call(&call), expected, "{source}");
    }
}

#[test]
fn candidate_emission_cannot_bypass_the_selected_mutator() {
    let file = SourceFile::parse("fixture.rs", "fn f()->bool { true }".into()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("fixture contains one function");
    };
    let selection = Selection::parse("literal.bool_flip").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);
    for mutator in ["fn_value.default", "literal.bool_flip"] {
        collector.emit_shaped_with_confidence(
            mutator,
            function.block.span(),
            "false",
            3,
            super::Shape::Block,
            super::Confidence::Proven,
        );
    }
    let candidates = collector.finish();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].mutator, "literal.bool_flip");
    assert_eq!(candidates[0].replacement_index, 3);
}

#[test]
fn inherent_impl_identity_keeps_the_exact_qualified_self_type() {
    for (owner, expected) in [
        ("a::Owner<u8>", "a::Owner<u8>"),
        ("b::Owner<u16>", "b::Owner<u16>"),
        ("&Owner", "&Owner"),
    ] {
        let source = format!("impl {owner} {{ fn value()->u32 {{ 7 }} }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let candidates = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1);
        assert_eq!(&*candidates[0].item_path, format!("{expected}::value"));
    }
}

#[test]
fn declaration_const_positions_stay_inert_while_method_trait_and_repeat_values_are_visited() {
    let source = r#"
        const CONSTANT: usize = 3;
        static STATIC: usize = 3;
        enum Discriminant { Value = 3 }
        type Bytes = [u8; 3];
        struct Owner;
        struct Holder<const N: usize> { field: [u8; 2] }
        impl Owner {
            const ASSOCIATED: usize = 3;
            fn method() -> usize { 3 }
        }
        trait Behavior {
            const ASSOCIATED: usize = 3;
            fn default_method() -> usize { 3 }
        }
        const fn constant_function() -> usize { 3 }
        unsafe extern "C" {
            fn foreign(value: [u8; 3]);
            static FOREIGN: [u8; 3];
        }
        fn f() {
            const LOCAL: usize = 3;
            let _ = [3; 2];
            callee::<{3}>();
            let _ = Holder::<{3}> { field: [3; 2] };
        }
    "#;
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let found = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::unconditional());
    assert_eq!(found.len(), 4, "{found:?}");
    assert_eq!(
        found.iter().map(|value| value.item_path.as_ref()).collect::<Vec<_>>(),
        ["Owner::method", "Behavior::default_method", "f", "f"]
    );
    assert_eq!(
        found.iter().map(|value| value.identity_occurrence).collect::<Vec<_>>(),
        [Some(0), Some(0), Some(0), Some(1)]
    );
    for candidate in &found {
        assert_eq!(file.slice(&candidate.span), "3");
        assert_eq!(candidate.replacement, "0");
        assert_eq!(candidate.replacement_index, 0);
        assert_eq!(candidate.shape, super::Shape::Expr);
        assert_eq!(candidate.confidence, super::Confidence::Proven);
    }
}

#[test]
fn unsigned_context_tracks_both_comparison_sides_without_leaking_into_closures_or_async_blocks() {
    for (source, expected) in [
        ("fn f(u:u32) {let _=u==0;}", None),
        ("fn f(u:u32) {let _=0==u;}", None),
        ("fn f(s:i32) {let _=s==0;}", Some(super::Confidence::Proven)),
        ("fn f(s:i32) {let _=0==s;}", Some(super::Confidence::Proven)),
        ("fn f() {let _=0==unknown;}", Some(super::Confidence::Optimistic)),
        ("fn f() {let n:u32={0};}", None),
        ("fn f() {let _=0 as u32;}", None),
        ("fn f()->u32 {if choose() {0}else{0}}", None),
        ("fn f()->u32 {let _=||->u32 {0}; 0}", None),
        ("fn f()->u32 {let _=|| {0}; 0}", Some(super::Confidence::Optimistic)),
        ("fn f()->u32 {let _=async {0}; 0}", Some(super::Confidence::Optimistic)),
        ("fn f() {let _=array[0];}", None),
        ("fn f() {let _=0..=0;}", Some(super::Confidence::Optimistic)),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse("literal.int_decrement").unwrap(), &CfgSet::unconditional());
        let count = if source.contains("0..=0") {
            2
        } else {
            usize::from(expected.is_some())
        };
        assert_eq!(found.len(), count, "{source}: {found:?}");
        for candidate in &found {
            assert_eq!(file.slice(&candidate.span), "0");
            assert_eq!(candidate.replacement, "-1");
            assert_eq!(candidate.replacement_index, 3);
            assert_eq!(Some(candidate.confidence), expected, "{source}");
            assert_eq!(candidate.shape, super::Shape::Expr);
        }
        if source.contains("|| {0}") || source.contains("async {0}") {
            assert_eq!(found[0].span.start, source.find("{0}").unwrap() + 1, "{source}");
        }
    }
}

#[test]
fn skipped_parameter_kinds_do_not_hide_later_mutable_or_method_parameter_shadows() {
    for (source, replacement, index, path) in [
        (
            "struct Plain; fn f((a,b):(u8,u8), plain:Plain, mut count:u32)->u32 { count }",
            "{ let mut count: u32 = Default::default(); count }",
            2,
            "f",
        ),
        (
            "struct Owner; impl Owner { fn f(&self, count:u32)->u32 { count } }",
            "{ let count: u32 = Default::default(); count }",
            1,
            "Owner::f",
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_with(
            &file,
            &Selection::parse("parameter.default_shadow").unwrap(),
            &CfgSet::unconditional(),
            &Defaults::of(&file.ast),
        );
        assert_eq!(found.len(), 1, "{source}: {found:?}");
        assert_eq!(found[0].replacement, replacement);
        assert_eq!(found[0].replacement_index, index);
        assert_eq!(found[0].shape, super::Shape::Block);
        assert_eq!(found[0].confidence, super::Confidence::Proven);
        assert_eq!(&*found[0].item_path, path);
        assert_eq!(file.slice(&found[0].span), "{ count }");
    }
}

#[test]
fn fallback_and_iterator_rewrites_keep_their_exact_shape_index_and_confidence() {
    for (source, selector, replacement, shape, confidence) in [
        (
            "fn f() { let _ = option.unwrap_or(9); }",
            "fallback.unwrap_or_to_default",
            "(option).unwrap_or_default()",
            super::Shape::Expr,
            super::Confidence::Proven,
        ),
        (
            "fn f() { let _ = mystery.filter(|_| true); }",
            "iter.remove_filter",
            "mystery",
            super::Shape::IterExpr,
            super::Confidence::Optimistic,
        ),
        (
            "fn f(xs:Vec<u8>) { let _ = xs.iter().filter(|_| true); }",
            "iter.remove_filter",
            "xs.iter()",
            super::Shape::IterExpr,
            super::Confidence::Proven,
        ),
        (
            "fn f() { let _ = vec![[1].as_slice(), [2].as_slice()]; }",
            "collection.reverse_vec",
            "vec![[2].as_slice(), [1].as_slice()]",
            super::Shape::Expr,
            super::Confidence::Proven,
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(found.len(), 1, "{selector}: {found:?}");
        assert_eq!(found[0].replacement, replacement);
        assert_eq!(found[0].replacement_index, 0);
        assert_eq!(found[0].shape, shape);
        assert_eq!(found[0].confidence, confidence);
    }
    let file = SourceFile::parse("fixture.rs", "fn f(){let _=[[1].as_slice(),[2].as_slice()];}".into()).unwrap();
    assert!(
        collect_in(
            &file,
            &Selection::parse("collection.reverse_array").unwrap(),
            &CfgSet::unconditional()
        )
        .is_empty()
    );
    for expression in ["true", "!unknown()"] {
        let file = SourceFile::parse("fixture.rs", format!("fn f()->bool {{ {expression} }}")).unwrap();
        assert!(
            collect_in(&file, &Selection::parse("bool_expr.negate").unwrap(), &CfgSet::unconditional()).is_empty(),
            "{expression}"
        );
    }
}

#[test]
fn regex_splices_distinguish_escaping_lazy_quantifiers_and_character_class_contents() {
    assert_eq!(
        super::regex_mutations(r"^a?[+*?]b*?c+d\$$"),
        [
            ("regex.remove_start_anchor", r"a?[+*?]b*?c+d\$$".into()),
            ("regex.remove_end_anchor", r"^a?[+*?]b*?c+d\$".into()),
            ("regex.optional_to_required", r"^a[+*?]b*?c+d\$$".into()),
            ("regex.negate_character_class", r"^a?[^+*?]b*?c+d\$$".into()),
            ("regex.star_to_plus", r"^a?[+*?]b+?c+d\$$".into()),
            ("regex.plus_to_star", r"^a?[+*?]b*?c*d\$$".into()),
        ]
    );
    assert_eq!(super::regex_mutations("[^a]"), [("regex.negate_character_class", "[a]".into())]);
    assert_eq!(super::regex_mutations("🦀a?"), [("regex.optional_to_required", "🦀a".into())]);
    assert_eq!(super::regex_mutations(r"\\$"), [("regex.remove_end_anchor", r"\\".into())]);
    assert_eq!(super::regex_mutations("^?a"), [("regex.optional_to_required", "^a".into())]);
    for pattern in [r"\?\+\*", r"\$", "?a", "[a*+?]", "a{2}?"] {
        let found = super::regex_mutations(pattern);
        if pattern == "[a*+?]" {
            assert_eq!(found, [("regex.negate_character_class", "[^a*+?]".into())]);
        } else {
            assert!(found.is_empty(), "{pattern}: {found:?}");
        }
    }
}

#[test]
fn standard_unsigned_signatures_require_exact_qualifier_import_and_argument_positions() {
    let imports = crate::HashMap::from_iter([
        ("Clock".into(), Some(vec!["std".into(), "time".into(), "Duration".into()])),
        ("Small".into(), Some(vec!["core".into(), "num".into(), "NonZeroU8".into()])),
        (
            "Atomic".into(),
            Some(vec!["core".into(), "sync".into(), "atomic".into(), "AtomicU32".into()]),
        ),
        ("Foreign".into(), Some(vec!["foreign".into(), "Duration".into()])),
        (
            "TooLong".into(),
            Some(vec!["std".into(), "time".into(), "nested".into(), "Duration".into()]),
        ),
    ]);
    for (callee, expected) in [
        ("std::time::Duration::new", [true, true, false]),
        ("core::time::Duration::new", [true, true, false]),
        ("Clock::new", [true, true, false]),
        ("Clock::from_secs", [true, false, false]),
        ("Clock::other", [false, false, false]),
        ("Duration::new", [false, false, false]),
        ("Foreign::new", [false, false, false]),
        ("TooLong::new", [false, false, false]),
        ("foreign::Clock::new", [false, false, false]),
        ("std::num::NonZeroU8::new", [true, false, false]),
        ("Small::new", [true, false, false]),
        ("Small::other", [false, false, false]),
        ("Atomic::new", [true, false, false]),
        ("Atomic::other", [false, false, false]),
        ("char::from_u32", [true, false, false]),
        ("char::other", [false, false, false]),
        ("foreign::char::from_u32", [false, false, false]),
        ("single", [false, false, false]),
        ("(Clock::new)", [false, false, false]),
    ] {
        let expression = syn::parse_str(callee).unwrap();
        for (index, expected) in expected.into_iter().enumerate() {
            assert_eq!(
                super::standard_unsigned_argument(&expression, index, &imports),
                expected,
                "{callee} argument {index}"
            );
        }
    }
}

#[test]
fn binding_evidence_undo_restores_import_provenance_and_each_unsigned_transition() {
    let file = SourceFile::parse("fixture.rs", "use std::time::Duration as Clock; fn f() {}".into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind_type("outer".into(), syn::parse_quote!(Clock));
    collector.bind_inferred_unsigned("unsigned".into());
    let original_imports = collector.binding_types["outer"].imports.clone();
    collector.in_scope(|collector| {
        collector.scope_path.push((77, 3));
        collector.bind_inferred_unsigned("outer".into());
        assert!(!collector.binding_types.contains_key("outer"));
        assert!(collector.inferred_unsigned.contains("outer"));
        collector.bind_type_with_trust("unsigned".into(), syn::parse_quote!(bool), false);
        assert!(!collector.inferred_unsigned.contains("unsigned"));
        assert!(!collector.binding_types["unsigned"].resource_trusted);
        assert_eq!(collector.binding_types["unsigned"].scope, [(77, 3)]);
        collector.bind_unknown_type("unsigned".into());
        assert!(!collector.binding_types.contains_key("unsigned"));
        assert!(!collector.inferred_unsigned.contains("unsigned"));
        collector.bind_inferred_unsigned("unsigned".into());
        collector.bind_unknown_type("unsigned".into());
        assert!(!collector.inferred_unsigned.contains("unsigned"));
        let _scope = collector.scope_path.pop();
    });
    assert_eq!(collector.binding_types["outer"].ty, syn::parse_quote!(Clock));
    assert_eq!(collector.binding_types["outer"].imports, original_imports);
    assert!(collector.binding_types["outer"].scope.is_empty());
    assert!(collector.binding_types["outer"].resource_trusted);
    assert!(!collector.inferred_unsigned.contains("outer"));
    assert!(!collector.binding_types.contains_key("unsigned"));
    assert!(collector.inferred_unsigned.contains("unsigned"));
}

#[test]
fn noncanonical_boundary_literals_keep_exact_population_and_directional_confidence() {
    for (literal, selector, expected) in [
        ("0u8", "literal.int_decrement", None),
        ("1u8", "literal.int_decrement", Some(("0", 3))),
        ("0i8", "literal.int_decrement", Some(("-1", 3))),
        ("0u16", "literal.int_to_zero", None),
        ("1u16", "literal.int_to_one", None),
        ("0f64", "literal.float_to_zero", None),
        ("1f64", "literal.float_to_one", None),
        ("0.0f32", "literal.float_to_zero", None),
        ("0.0_f32", "literal.float_to_zero", None),
        ("1.0f32", "literal.float_to_one", None),
        ("1.0_f32", "literal.float_to_one", None),
        ("2.0f64", "literal.float_to_zero", Some(("0.0f64", 0))),
        ("2.0f64", "literal.float_to_one", Some(("1.0f64", 1))),
        ("r#\"\"#", "literal.str_to_empty", None),
        ("'\\u{0}'", "literal.char_to_nul", None),
        ("b'\\x00'", "literal.byte_to_nul", None),
        ("b'a'", "literal.byte_to_nul", Some(("b'\\0'", 0))),
    ] {
        let file = SourceFile::parse("fixture.rs", format!("fn f() {{ let _ = {literal}; }}")).unwrap();
        let found = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(found.len(), usize::from(expected.is_some()), "{literal} {selector}: {found:?}");
        if let Some((replacement, ordinal)) = expected {
            let candidate = &found[0];
            assert_eq!(candidate.mutator, selector);
            assert_eq!(file.slice(&candidate.span), literal);
            assert_eq!(candidate.replacement, replacement);
            assert_eq!(candidate.replacement_index, ordinal);
            assert_eq!(candidate.confidence, Confidence::Proven);
            assert_eq!(candidate.shape, Shape::Expr);
            assert_eq!(candidate.identity_occurrence, Some(0));
            assert_eq!(&*candidate.item_path, "f");
        }
    }
}

#[test]
fn resource_iterator_initializers_reject_attributes_on_each_receiver_layer() {
    let file = SourceFile::parse("fixture.rs", "fn f() {}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind_type("text".into(), syn::parse_quote!(&str));
    collector.bind_type("owned".into(), syn::parse_quote!(String));
    for text in [
        "#[transform] text.chars().peekable()",
        "(#[transform] text.chars()).peekable()",
        "(#[transform] text).chars().peekable()",
        "<Other>::text.chars().peekable()",
    ] {
        let expression: Expr = syn::parse_str(text).unwrap();
        assert!(!collector.resource_iterator_initializer(&expression), "{text}");
    }
    for (text, expected) in [
        ("text.chars().peekable()", true),
        ("text.chars().peekable::<u8>()", false),
        ("text.chars::<u8>().peekable()", false),
        ("text.chars(1).peekable()", false),
        ("text.chars().peekable(1)", false),
        ("text.chars().other()", false),
        ("text.bytes().peekable()", false),
        ("<T>::text.chars().peekable()", false),
        ("owned.chars().peekable()", false),
        ("unknown.chars().peekable()", false),
        ("module::text.chars().peekable()", false),
        ("(text).chars().peekable()", false),
        ("text.peekable()", false),
        ("true", false),
    ] {
        assert_eq!(
            collector.resource_iterator_initializer(&syn::parse_str(text).unwrap()),
            expected,
            "{text}"
        );
    }
    let _inserted = collector.resource_iterators.insert("outer".into());
    collector.in_function(&syn::parse_quote!(fn nested(item @ _: &str)), |collector| {
        assert!(collector.resource_iterators.is_empty());
        assert!(!collector.binding_types["item"].resource_trusted);
        let _inserted = collector.resource_iterators.insert("inner".into());
    });
    assert!(collector.resource_iterators.contains("outer"));
    assert!(!collector.resource_iterators.contains("inner"));
}

#[test]
fn temporal_constructor_inference_checks_each_qualifier_import_and_method() {
    let source = "use chrono::{Utc, DateTime, NaiveDateTime}; \
                  use std::time::{Instant, SystemTime, Duration}; fn f() {}";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (text, ty) in [
        ("Utc::now()", Some("chrono::DateTime")),
        ("Utc::other()", None),
        ("DateTime::from_timestamp(1, 0)", Some("chrono::DateTime")),
        ("NaiveDateTime::from_timestamp_opt(1, 0)", Some("chrono::NaiveDateTime")),
        ("Instant::now()", Some("std::time::Instant")),
        ("SystemTime::now()", Some("std::time::SystemTime")),
        ("Duration::from_secs(1)", Some("std::time::Duration")),
        ("Other::now()", None),
        ("other::now()", None),
        ("now()", None),
        ("(factory())()", None),
        ("1", None),
    ] {
        let expression = syn::parse_str(text).unwrap();
        assert_eq!(
            collector.inferred_expression_type(&expression),
            ty.map(|ty| syn::parse_str(ty).unwrap()),
            "{text}"
        );
        assert_eq!(collector.is_known_textual_or_temporal(&expression), ty.is_some(), "{text}");
        assert_eq!(collector.is_known_temporal(&expression), ty.is_some(), "{text}");
    }
    for name in ["Utc", "DateTime", "NaiveDateTime"] {
        let _previous = collector
            .root_imports
            .insert(name.into(), Some(vec!["foreign".into(), name.into()]));
        let text = format!("{name}::now()");
        assert_eq!(collector.inferred_expression_type(&syn::parse_str(&text).unwrap()), None, "{text}");
    }
}

#[test]
fn numeric_confidence_separates_exact_evidence_from_same_named_temporal_methods() {
    let file = SourceFile::parse("fixture.rs", "const LIMIT:u32=7; fn count()->u32 {7} fn f() {}".into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (name, ty) in [("number", "i32"), ("text", "String"), ("duration", "std::time::Duration")] {
        collector.bind_type(name.into(), syn::parse_str(ty).unwrap());
    }
    for (text, confidence) in [
        ("(LIMIT)", Confidence::Proven),
        ("(number)", Confidence::Proven),
        ("-number", Confidence::Proven),
        ("-(17)", Confidence::Proven),
        ("-unknown", Confidence::Optimistic),
        ("!unknown", Confidence::Optimistic),
        ("text.len()", Confidence::Proven),
        ("text.capacity()", Confidence::Proven),
        ("text.unknown()", Confidence::Optimistic),
        ("text.as_secs()", Confidence::Optimistic),
        ("duration.as_secs()", Confidence::Proven),
        ("duration.as_millis()", Confidence::Proven),
        ("duration.len()", Confidence::Optimistic),
        ("duration.unknown()", Confidence::Optimistic),
        ("unknown.len()", Confidence::Optimistic),
        ("u32::from(3)", Confidence::Proven),
        ("other::from(3)", Confidence::Optimistic),
        ("<u32>::from(3)", Confidence::Optimistic),
        ("u32::MAX", Confidence::Proven),
        ("LIMIT", Confidence::Proven),
        ("unknown", Confidence::Optimistic),
        ("<T>::LIMIT", Confidence::Optimistic),
    ] {
        assert_eq!(collector.numeric_confidence(&syn::parse_str(text).unwrap()), confidence, "{text}");
    }
    for (text, unsigned) in [
        ("u32::from(3)", true),
        ("count()", true),
        ("unknown()", false),
        ("i32::from(3)", false),
    ] {
        assert_eq!(collector.is_known_unsigned(&syn::parse_str(text).unwrap()), unsigned, "{text}");
    }
    for (text, numeric) in [("u32::MAX", true), ("LIMIT", true), ("<T>::LIMIT", false)] {
        assert_eq!(collector.is_known_numeric(&syn::parse_str(text).unwrap()), numeric, "{text}");
    }
}

#[test]
fn textual_type_evidence_rejects_imported_and_local_lookalikes_independently() {
    for source in [
        "use foreign::{String, Option}; fn f() {}",
        "struct String; struct Option; fn f() {}",
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let selection = Selection::everything();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::of(&file.ast);
        let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        for text in ["String", "Option", "foreign::String", "foreign::Option", "()"] {
            assert!(
                !collector.is_known_textual_or_temporal_type(&syn::parse_str(text).unwrap()),
                "{source}: {text}"
            );
        }
        for text in [
            "std::string::String",
            "alloc::string::String",
            "std::time::Duration",
            "chrono::DateTime",
        ] {
            assert!(
                collector.is_known_textual_or_temporal_type(&syn::parse_str(text).unwrap()),
                "{source}: {text}"
            );
        }
    }
}

#[test]
fn resource_transparency_restores_after_independently_opaque_item_and_expression_scopes() {
    for opaque in [
        "#[transform] fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}}",
        "struct S; #[transform] impl S {fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}}}",
        "struct S; impl S { #[transform] fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}} }",
        "trait S { #[transform] fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}} }",
        "fn affected(n:usize) { let mut i:usize=0; #[transform] while i<n {i+=1;} }",
        "#[no_implicit_prelude] mod no_prelude {fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}}}",
    ] {
        let source = format!("{opaque} fn clean(n:usize) {{ let mut i:usize=0; while i<n {{i+=1;}} }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
        let root_is_opaque = opaque.starts_with("#[transform] fn");
        assert_eq!(found.len(), if root_is_opaque { 2 } else { 1 }, "{opaque}: {found:?}");
        assert!(found[0].item_path.ends_with("affected"), "{opaque}: {found:?}");
        assert_eq!(file.slice(&found[0].span), "i+=1;");
        assert_eq!(found[0].replacement, "");
        assert_eq!(found[0].replacement_index, 0);
        assert_eq!(found[0].identity_occurrence, Some(0));
        if root_is_opaque {
            assert_eq!(&*found[1].item_path, "clean");
        }
    }
    let file = SourceFile::parse(
        "fixture.rs",
        "#![no_implicit_prelude] fn affected(n:usize) {let mut i:usize=0;while i<n {i+=1;}}".into(),
    )
    .unwrap();
    let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    assert_eq!(found.len(), 1);
    assert_eq!(file.slice(&found[0].span), "i+=1;");
}

#[test]
fn capture_and_pattern_expression_boundaries_disable_proofs_without_poisoning_later_siblings() {
    for opaque in [
        "(||{while i<n {i+=1;}})();",
        "async {while i<n {i+=1;}};",
        "match option {Some(_) => {while i<n {i+=1;}}, None => ()};",
        "for _ in values {while i<n {i+=1;}}",
        "while let Some(_) = option {while i<n {i+=1;}}",
    ] {
        let source = format!("fn f(n:usize,option:Option<u8>,values:&[u8]) {{let mut i:usize=0;{opaque} while i<n {{i+=1;}}}}");
        let file = SourceFile::parse("fixture.rs", source.clone()).unwrap();
        let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
        assert_eq!(found.len(), 1, "{opaque}: {found:?}");
        assert_eq!(found[0].span.start, source.find("i+=1;").unwrap());
        assert_eq!(found[0].replacement, "");
        assert_eq!(found[0].identity_occurrence, Some(0));
    }
}

#[test]
fn a_conservative_then_block_does_not_disable_resource_proofs_in_its_else_block() {
    let file = SourceFile::parse(
        "fixture.rs",
        "fn f(ready:bool,n:usize){let mut i:usize=0;if ready{let _=0;}else{while i<n{i+=1;}}}".into(),
    )
    .unwrap();
    let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn module_prelude_policy_is_inherited_and_restored_for_unit_result_proofs() {
    for (source, expected) in [
        (
            "#[no_implicit_prelude] mod limited{use core::default::Default;fn inner_unit(){}fn inner(){inner_unit();}} \
             fn root_unit(){} fn root(){root_unit();}",
            "limited::inner",
        ),
        (
            "#![no_implicit_prelude] mod limited{use core::default::Default;fn inner_unit(){}fn inner(){inner_unit();}}",
            "limited::inner",
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse("call_result.default").unwrap(), &CfgSet::unconditional());
        assert_eq!(found.len(), 1, "{source}: {found:?}");
        assert_eq!(&*found[0].item_path, expected);
        assert_eq!(file.slice(&found[0].span), "inner_unit()");
        assert_eq!(found[0].replacement, "::gamma_rt::default_after(inner_unit())");
    }
}

#[test]
fn resource_binding_and_iterator_initialization_require_exact_trusted_evidence() {
    use super::resource::Binding;

    let source = "type Text = &'static str; type Cycle = Cycle; type Generic<T> = T; fn f() {}";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (text, expected) in [
        ("bool", Some(Binding::Bool)),
        ("u32", Some(Binding::Integer)),
        ("i32", Some(Binding::Integer)),
        ("String", Some(Binding::String)),
        ("Text", Some(Binding::StrReference)),
        ("Cycle", None),
        ("Generic<u8>", None),
        ("&u32", None),
        ("()", None),
        ("Unknown", None),
        ("&mut std::iter::Peekable<std::str::Chars<'static>>", Some(Binding::PeekableChars)),
    ] {
        assert_eq!(
            collector.resource_binding(&syn::parse_str(text).unwrap(), &Vec::new()),
            expected,
            "{text}"
        );
    }
    collector.bind_type("text".to_owned(), syn::parse_quote!(&str));
    collector.bind_type_with_trust("untrusted".to_owned(), syn::parse_quote!(&str), false);
    for (text, expected) in [
        ("text.chars().peekable()", true),
        ("untrusted.chars().peekable()", false),
        ("missing.chars().peekable()", false),
        ("text.chars()", false),
        ("text.chars().peekable(1)", false),
        ("text.chars().peekable::<u8>()", false),
        ("text.chars(1).peekable()", false),
        ("text.chars::<u8>().peekable()", false),
        ("text.bytes().peekable()", false),
        ("text.peekable()", false),
        ("(text).chars().peekable()", false),
        ("module::text.chars().peekable()", false),
        ("7", false),
    ] {
        assert_eq!(
            collector.resource_iterator_initializer(&syn::parse_str(text).unwrap()),
            expected,
            "{text}"
        );
    }
    collector.generics.push("T".to_owned());
    assert_eq!(collector.resource_binding(&syn::parse_quote!(T), &Vec::new()), None);
}

#[test]
fn repeated_resource_probes_preserve_source_map_and_original_spans() {
    let text = format!(
        "fn f(n: usize, mut s: String) {{ let mut i: usize = 0; while i < n {{ {} i += 1; }} }}",
        "s.push_str(\"x\");".repeat(80)
    );
    let file = SourceFile::parse("fixture.rs", text).unwrap();
    let marker = || {
        "marker"
            .parse::<proc_macro2::TokenStream>()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .span()
            .byte_range()
            .start
    };
    let first = marker();
    let second = marker();
    let selection = Selection::parse("stmt.delete_assign,stmt.delete_call,literal.int_to_zero").unwrap();
    for _ in 0..8 {
        let found = collect_in(&file, &selection, &CfgSet::unconditional());
        assert!(found.iter().all(|candidate| file.slice(&candidate.span) != "i += 1;"));
        assert!(found.iter().any(|candidate| file.slice(&candidate.span) == "s.push_str(\"x\");"));
    }
    assert_eq!(marker() - second, second - first);
    assert_eq!(file.slice(&file.ast.items[0].span().byte_range()), file.text.as_str());
}

#[test]
fn scoped_resource_aliases_keep_nominal_comparisons_and_do_not_cycle() {
    for text in [
        "mod one { pub struct B; pub type A = &'static mut B; } \
         mod two { pub struct A; pub type B = &'static mut A; } \
         use one::A; fn f(a: A) { while true { break; } }",
        "mod numbers { type Counter = usize; } \
         mod custom { pub struct Counter(std::cell::Cell<usize>); \
         impl PartialEq for Counter { fn eq(&self, other: &Self) -> bool { self.0.get() == other.0.get() } } \
         impl PartialOrd for Counter { fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { \
         let n = self.0.get(); self.0.set(n.saturating_sub(1)); n.partial_cmp(&other.0.get()) } } } \
         use custom::Counter; fn f(n: Counter, mut i: usize) { while n > n { i += 1; } }",
        "type Counter = usize; fn f() { struct Counter; let n: Counter = Counter; let mut i: usize = 0; while n > n { i += 1; } }",
        "type Counter = usize; fn f() { enum Counter { A } let n: Counter = Counter::A; let mut i: usize = 0; while n > n { i += 1; } }",
        "type Counter = usize; fn f() { union Counter { a: usize } let n: Counter = Counter { a: 0 }; let mut i: usize = 0; while n > n { i += 1; } }",
    ] {
        let file = SourceFile::parse("fixture.rs", text.to_owned()).unwrap();
        let defaults = Defaults::of(&file.ast);
        for selector in ["all", "stmt.delete_assign,loop.delete_break"] {
            let selection = Selection::parse(selector).unwrap();
            let standalone = collect_with(&file, &selection, &CfgSet::unconditional(), &defaults);
            let fused = super::super::check_stated_and_collect_with(&file, &selection, &CfgSet::unconditional(), &defaults).unwrap();
            assert_eq!(standalone.len(), fused.len());
            if text.contains("while n > n") {
                for candidates in [&standalone, &fused] {
                    assert!(candidates.iter().any(|candidate| candidate.mutator == "stmt.delete_assign"));
                }
            }
        }
    }
    for text in [
        "type Count = usize; fn f(n: Count) { let mut i: Count = 0; while i < n { i += 1; } }",
        "mod m { type Count = usize; fn f(n: Count) { let mut i: Count = 0; while i < n { i += 1; } } }",
        "type Count = usize; fn f(n: Count) { { let mut i: Count = 0; while i < n { i += 1; } } }",
        "type Text = String; fn f(mut s: Text) { loop { s.push_str(\"x\"); break; } }",
    ] {
        assert!(
            mutators(text, "stmt.delete_assign,loop.delete_break", &CfgSet::unconditional()).is_empty(),
            "{text}"
        );
    }
}

#[test]
fn aliases_and_reexports_keep_imported_default_candidates_despite_nominal_collisions() {
    for export in [
        "pub type Artifact = std::time::Duration;",
        "pub use std::time::Duration as Artifact;",
        "pub use std::time::*;",
    ] {
        let subject = SourceFile::parse(
            "subject.rs",
            "use crate::exports::Artifact; fn f() -> Artifact { Artifact::from_secs(5) }".to_owned(),
        )
        .unwrap();
        let mut defaults = Defaults::of(&syn::parse_file("pub struct Artifact;").unwrap());
        defaults.absorb(Defaults::of(&syn::parse_file(export).unwrap()));
        defaults.absorb(Defaults::of(&subject.ast));
        for selector in ["all", "fn_value.default"] {
            let found = collect_with(&subject, &Selection::parse(selector).unwrap(), &CfgSet::unconditional(), &defaults);
            assert!(
                found.iter().any(|candidate| candidate.mutator == "fn_value.default"),
                "{export}: {found:?}"
            );
        }
    }
}

#[test]
fn resource_filters_omit_deleted_and_zeroed_scalar_progress_under_every_selector() {
    for selector in ["stmt.delete_assign,literal.int_decrement,literal.int_to_zero", "all"] {
        let source = "fn f(n: usize) { let mut i: usize = 0; while i < n { i += 1; } }";
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let selection = Selection::parse(selector).unwrap();
        let found = collect_in(&file, &selection, &CfgSet::unconditional());
        assert!(
            !found.iter().any(|candidate| {
                file.slice(&candidate.span) == "i += 1;" || (file.slice(&candidate.span) == "1" && candidate.replacement == "0")
            }),
            "{selector}: {found:?}"
        );
        assert!(
            found.iter().any(|candidate| candidate.mutator == "cond.negate") || selector != "all",
            "mutations that retain progress remain available: {found:?}"
        );
    }
}

#[test]
fn resource_filters_omit_removed_sole_exits_and_growth_loops() {
    for source in [
        "fn f() { loop { break; } }",
        "fn f() { loop { if true { break; } } }",
        "fn f(mut s: String) { loop { s.push_str(\"x\"); break; } }",
        "fn f(mut s: std::string::String) { loop { s.push('x'); break; } }",
        "fn f(n: usize, mut s: String) { let mut i: usize = 0; while i < n { s.push_str(\"x\"); i += 1; } }",
        "fn f(n: usize, mut s: String) { let mut i: usize = 0; while i < n { s.push_str(\"\"); i += 1; } }",
    ] {
        let found = mutators(
            source,
            "loop.delete_break,loop.break_to_continue,stmt.delete_assign",
            &CfgSet::unconditional(),
        );
        assert!(found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn resource_filters_prove_standard_character_iterator_stalls_and_exhaustion() {
    for source in [
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(|c| c.is_whitespace()) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(|c| !c.is_whitespace()) { let _ = chars.next(); } }",
        "fn f(mut chars: std::iter::Peekable<std::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>, mut s: String) { while chars.peek().is_some() { chars.next(); s.push_str(\"x\"); } }",
        "fn f(text: &str) { let mut chars = text.chars().peekable(); while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(text: &str) { let mut chars = text.chars().peekable(); while chars.peek().is_some_and(|c| c.is_whitespace()) { let _ = chars.next(); } }",
        "type Text<'a> = &'a str; fn f(text: Text<'_>) { let mut chars = text.chars().peekable(); while chars.peek().is_some() { chars.next(); } }",
    ] {
        let found = mutators(source, "cond.negate,stmt.delete_call", &CfgSet::unconditional());
        assert!(!found.contains(&"cond.negate"), "{source}: {found:?}");
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let found = collect_in(&file, &Selection::parse("stmt.delete_call").unwrap(), &CfgSet::unconditional());
        assert!(
            !found.iter().any(|candidate| file.slice(&candidate.span) == "chars.next();"),
            "{source}: {found:?}"
        );
    }
}

#[test]
fn resource_filters_keep_unknown_iterators_callbacks_and_restart_effects() {
    for source in [
        "fn f<I: Iterator<Item = char>>(mut chars: core::iter::Peekable<I>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "struct Chars; fn f(mut chars: std::iter::Peekable<Chars>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(make_callback()) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(|c| side_effect(c)) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { reset(chars); chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next().unwrap(); } }",
        "mod std {} fn f(mut chars: std::iter::Peekable<std::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "struct char; fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) { while chars.peek().is_some_and(char::is_ascii_digit) { if stop() { break; } chars.next(); } }",
        "fn f(text: Custom) { let mut chars = text.chars().peekable(); while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(text: &str) { let mut chars = text.chars().peekable(); let mut chars = custom(); while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "fn f(text: &str) { let mut chars = text.chars().peekable(); { let mut chars = custom(); while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } } }",
        "use core::str::Chars; fn f<Chars>(chars: &mut core::iter::Peekable<Chars>) { while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "type Text = &str; fn f<Text>(text: Text) { let mut chars = text.chars().peekable(); while chars.peek().is_some_and(char::is_ascii_digit) { chars.next(); } }",
        "type Text = &str; mod m { fn f(text: Text) { let mut chars = text.chars().peekable(); while chars.peek().is_some() { chars.next(); } } }",
        "type Text = &str; fn f() { struct Text; let text: Text = Text; let mut chars = text.chars().peekable(); while chars.peek().is_some() { chars.next(); } }",
    ] {
        let found = mutators(source, "cond.negate", &CfgSet::unconditional());
        assert!(found.contains(&"cond.negate"), "{source}: {found:?}");
    }
}

#[test]
fn resource_filters_keep_uncertain_effects_escapes_types_and_progress() {
    for source in [
        "fn f(n: usize) { let mut i: usize = 0; while i < n { work(); i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { if i == 0 { break; } i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { return; i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { panic!(); i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { let guard = acquire(); i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { i += 1; i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { i *= 2; i += 1; } }",
        "fn f(n: &usize) { let mut i: usize = 0; while i < *n { i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { unsafe { alter(&mut i); } i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { (|| work())(); i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { #[instrument] { work(); } i += 1; } }",
        "fn f<U>(n: U) { let mut i: U = start(); while i < n { i += 1; } }",
        "fn f<u8>(n: u8) { let mut i: u8 = start(); while i < n { i += 1; } }",
        "struct String; fn f(n: usize, mut s: String) { let mut i: usize = 0; while i < n { s.push_str(\"x\"); i += 1; } }",
        "fn f(n: usize, s: &mut String) { let mut i: usize = 0; while i < n { s.push_str(\"x\"); i += 1; } }",
        "fn f(n: usize, mut s: String) { let mut i: usize = 0; while i < n { s.clear(); s.push_str(\"x\"); i += 1; } }",
        "fn f(n: usize, mut s: String) { let mut i: usize = 0; while i < n { s.push_str(&make()); i += 1; } }",
        "#![no_implicit_prelude] fn f(n: usize) { let mut i: usize = 0; while i < n { i += 1; } }",
        "#[instrument] fn f(n: usize) { let mut i: usize = 0; while i < n { i += 1; } }",
        "fn n() -> usize { 0 } fn f() { let n = || custom(); let mut i = n(); while i < n() { i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; let (n,) = (custom(),); while i < n { i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; if let Some(n) = custom() { while i < n { i += 1; } } }",
        "fn f(ref n: usize) { let mut i: usize = 0; while i < n { i += 1; } }",
        "fn f(n: usize) { let mut i: usize = 0; opaque!(); while i < n { i += 1; } }",
    ] {
        let found = mutators(source, "stmt.delete_assign", &CfgSet::unconditional());
        assert!(!found.is_empty(), "{source}: uncertain deletion must remain eligible");
    }
}

#[test]
fn resource_filters_keep_finite_loops_and_preexisting_infinite_loops() {
    for source in [
        "fn f(v: &[u8]) { for _ in v { break; } }",
        "fn f() { while false { break; } }",
        "fn f() { loop { continue; } }",
        "fn f(mut s: String) { loop { s.push_str(\"x\"); } }",
    ] {
        let found = mutators(
            source,
            "loop.delete_break,loop.break_to_continue,loop.delete_continue,stmt.delete_call",
            &CfgSet::unconditional(),
        );
        assert!(!found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn resource_filters_reserve_identity_occurrences_for_omitted_sites() {
    let file = SourceFile::parse(
        "fixture.rs",
        "fn f(n: usize) { let mut i: usize = 0; while i < n { i += 1; } i += 1; }".to_owned(),
    )
    .unwrap();
    let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(file.slice(&found[0].span), "i += 1;");
    assert_eq!(found[0].identity_occurrence, Some(1));
}

#[test]
fn resource_filters_bound_proof_work_and_restore_iterator_evidence() {
    let oversized = format!("fn f() {{ loop {{ /*{}*/ break; }} }}", "x".repeat(4096));
    assert_eq!(
        mutators(&oversized, "loop.delete_break", &CfgSet::unconditional()),
        ["loop.delete_break"]
    );
    let source = "fn f(text: &str) { let mut chars = text.chars().peekable(); { let chars = custom(); } while chars.peek().is_some() { chars.next(); } }";
    assert!(mutators(source, "cond.negate", &CfgSet::unconditional()).is_empty());
}

#[test]
fn lexical_evidence_undo_restores_every_shadow_and_new_binding() {
    let declaration = |statement: Stmt| {
        let Stmt::Local(local) = statement else {
            panic!("test fixture statements are let declarations");
        };
        local
    };
    let file = SourceFile::parse("fixture.rs", "fn f() {}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind("number".into(), true);
    collector.bind_type("number".into(), syn::parse_quote!(u32));
    collector.bind("unsigned".into(), true);
    collector.bind_inferred_unsigned("unsigned".into());
    collector.record_declaration(&declaration(syn::parse_quote!(let deferred;)));
    let mark = collector.undo.len();
    let result = collector.in_scope(|collector| {
        assert!(collector.deferred.contains("deferred"));
        collector.bind("number".into(), false);
        collector.bind_type("number".into(), syn::parse_quote!(String));
        collector.bind_inferred_unsigned("number".into());
        collector.bind_unknown_type("number".into());
        assert!(!collector.binding_types.contains_key("number"));
        assert!(!collector.inferred_unsigned.contains("number"));
        collector.bind_unknown_type("unsigned".into());
        collector.bind_type("unsigned".into(), syn::parse_quote!(bool));
        collector.bind("new".into(), true);
        collector.bind_type("new".into(), syn::parse_quote!(i16));
        collector.record_declaration(&declaration(syn::parse_quote!(let deferred = 1;)));
        collector.record_declaration(&declaration(syn::parse_quote!(let new_deferred;)));
        assert!(!collector.deferred.contains("deferred"));
        collector.in_scope(|collector| {
            collector.bind("number".into(), true);
            collector.bind_type("number".into(), syn::parse_quote!(i8));
            collector.record_declaration(&declaration(syn::parse_quote!(let deferred;)));
        });
        assert!(!collector.bindings["number"]);
        assert!(!collector.binding_types.contains_key("number"));
        assert!(!collector.deferred.contains("deferred"));
        73
    });
    assert_eq!(result, 73);
    assert_eq!(collector.undo.len(), mark);
    assert_eq!(collector.bindings.len(), 2);
    assert!(collector.bindings["number"]);
    assert_eq!(collector.binding_types.len(), 1);
    assert_eq!(collector.binding_types["number"].ty, syn::parse_quote!(u32));
    assert_eq!(collector.binding_types["number"].imports, collector.root_imports);
    assert_eq!(collector.inferred_unsigned.iter().collect::<Vec<_>>(), [&"unsigned".to_owned()]);
    assert_eq!(collector.deferred.iter().collect::<Vec<_>>(), [&"deferred".to_owned()]);
}

#[test]
fn initial_state_and_identity_occurrences_are_independent_of_emission_order() {
    let file = SourceFile::parse("fixture.rs", "fn f(){same;same;same;}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    assert_eq!(&*collector.outermost, "");
    assert!(!collector.in_default_impl);
    assert!(!collector.numeric_return);
    assert!(!collector.foreign_error_return);
    assert_eq!(collector.function_bound_depth, None);
    let spans = file
        .text
        .match_indices("same")
        .map(|(start, text)| start..start + text.len())
        .collect::<Vec<_>>();
    for index in [2, 0, 1] {
        collector.emit_at(
            "literal.int_to_zero",
            spans[index].clone(),
            "0",
            0,
            super::super::Shape::Expr,
            super::super::Confidence::Proven,
        );
    }
    let candidates = collector.finish();
    assert_eq!(
        candidates.iter().map(|candidate| candidate.identity_occurrence).collect::<Vec<_>>(),
        [Some(2), Some(0), Some(1)]
    );
}

#[test]
fn exact_numeric_unsigned_signed_and_alias_receiver_evidence_is_preserved() {
    let file = SourceFile::parse(
        "fixture.rs",
        "type Text = String; type Time = std::time::Duration; type Sequence = Vec<u8>; fn f(){}".to_owned(),
    )
    .unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (name, ty) in [
        ("unsigned", "u32"),
        ("signed", "i32"),
        ("text", "Text"),
        ("time", "Time"),
        ("sequence", "Sequence"),
    ] {
        collector.bind_type(name.into(), syn::parse_str(ty).unwrap());
    }
    let _old = collector.declared_types.insert("textual".into(), Some(syn::parse_quote!(Text)));
    let _old = collector
        .declared_types
        .insert("unsigned_field".into(), Some(syn::parse_quote!(u32)));
    for (text, expected) in [
        ("unsigned", true),
        ("(unsigned)", true),
        ("object.unsigned_field", true),
        ("object.0", false),
        ("signed", false),
        ("false", false),
    ] {
        assert_eq!(collector.is_known_unsigned(&syn::parse_str(text).unwrap()), expected, "{text}");
    }
    let signed: Expr = syn::parse_quote!(signed);
    let grouped = Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: syn::token::Group::default(),
        expr: Box::new(signed),
    });
    assert!(collector.is_known_signed(&grouped));
    assert!(!collector.is_known_signed(&syn::parse_quote!(true)));
    assert!(!collector.is_known_textual_or_temporal(&syn::parse_quote!(object.textual)));
    assert!(collector.is_known_textual_or_temporal(&syn::parse_quote!(text)));
    assert!(collector.is_known_temporal(&syn::parse_quote!(time)));
    assert!(collector.receiver_has_first(&syn::parse_quote!(sequence)));
    for (text, expected) in [
        ("u8::from(1)", true),
        ("NonZeroU8::new(1)", true),
        ("NonZeroThing::new()", true),
        ("ThingNonZero::new()", false),
        ("unknown()", false),
        ("usize::MAX", true),
    ] {
        assert_eq!(collector.is_known_numeric(&syn::parse_str(text).unwrap()), expected, "{text}");
    }
    let inferred: Expr = syn::parse_quote!(inferred);
    let _old = collector.numeric_evidence.numeric.insert(inferred.span().byte_range());
    assert!(collector.is_known_numeric(&inferred));
    assert!(!collector.is_known_numeric(&syn::parse_quote!(<T>::inferred)));
    for name in ["u8", "u16", "u32", "u64", "u128", "usize"] {
        assert!(super::is_unsigned_type_name(name), "{name}");
    }
    for name in ["i8", "f32", "U8", "NonZeroU8", "u256", ""] {
        assert!(!super::is_unsigned_type_name(name), "{name}");
    }
}

#[test]
fn parenthesized_calls_and_import_prefixes_have_exact_helper_boundaries() {
    let expression: Expr = syn::parse_quote!((u8::from));
    assert_eq!(super::called_path(&expression), Some(&syn::parse_quote!(u8::from)));
    assert_eq!(super::called_path(&syn::parse_quote!(true)), None);
    assert!(!super::is_numeric_literal_expression(&syn::parse_quote!(!1)));
    assert!(!super::is_numeric_literal_expression(&syn::parse_quote!(*1)));
    let imports = crate::HashMap::from_iter([
        ("Relative".into(), Some(vec!["std".into(), "time".into(), "Duration".into()])),
        (
            "Absolute".into(),
            Some(vec![
                super::indexes::ABSOLUTE_ROOT.into(),
                "std".into(),
                "time".into(),
                "Duration".into(),
            ]),
        ),
        (
            "Extra".into(),
            Some(vec!["std".into(), "time".into(), "nested".into(), "Duration".into()]),
        ),
    ]);
    for (name, expected) in [("Relative", true), ("Absolute", true), ("Extra", false)] {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        assert_eq!(
            super::imported_item_matches(&imports, &ident, "Duration", &["std", "time"]),
            expected,
            "{name}"
        );
    }
}

#[test]
fn foreign_error_context_resolves_aliases_and_self_associated_types() {
    let file = SourceFile::parse("fixture.rs", "struct Missing; type ErrorAlias = Missing; fn f(){}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let _previous = collector.impl_self_associated.insert("Error".into(), syn::parse_quote!(Missing));
    for signature in [
        syn::parse_quote!(fn nested() -> Result<(),ErrorAlias>),
        syn::parse_quote!(fn nested() -> Result<(),Self::Error>),
    ] {
        collector.in_function(&signature, |collector| assert!(collector.foreign_error_return));
        collector.in_nested_return_context(Some(&signature.output), |collector| assert!(collector.foreign_error_return));
        assert!(!collector.foreign_error_return);
    }
    let _previous = collector.impl_self_associated.insert("Error".into(), syn::parse_quote!(u8));
    let signature: syn::Signature = syn::parse_quote!(fn nested() -> Result<(),Self::Error>);
    collector.in_function(&signature, |collector| assert!(!collector.foreign_error_return));
}

#[test]
fn seeded_result_and_option_admission_preserves_exact_payloads_and_ordinals() {
    for (text, defaultable, reusable, expected) in [
        ("Some(7)", false, false, Some("None")),
        ("Some(7,8)", false, false, None),
        ("Ok(7)", true, false, Some("Err(Default::default())")),
        ("Ok(7)", false, true, Some("Err(reused)")),
        ("Err(7)", true, false, Some("Ok(Default::default())")),
        ("Err(7)", false, true, Some("Ok(reused)")),
    ] {
        let file = SourceFile::parse("fixture.rs", format!("fn f(){{ {text}; }}")).unwrap();
        let Item::Fn(function) = &file.ast.items[0] else {
            panic!("the fixture contains only a function");
        };
        let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
            panic!("the fixture function begins with an expression");
        };
        let selection = Selection::parse("option.some_to_none,result.ok_to_err,result.err_to_ok").unwrap();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::of(&file.ast);
        let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        if defaultable {
            collector.admissible_defaults.push(expression.span().byte_range());
        }
        if reusable {
            collector.reusable_payloads.push((expression.span().byte_range(), "reused".into()));
            let _previous = collector
                .parameter_types
                .insert("reused".into(), syn::parse_quote!(core::num::NonZeroU8));
            collector.bind_type("reused".into(), syn::parse_quote!(core::num::NonZeroU8));
        }
        collector.visit_expr(expression);
        let candidates = collector.finish();
        let expected = expected.map(|replacement| (replacement, 0)).into_iter().collect::<Vec<_>>();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.replacement.as_str(), candidate.replacement_index))
                .collect::<Vec<_>>(),
            expected,
            "{text}"
        );
    }
}

#[test]
fn let_else_divergence_restores_nonempty_protected_statement_and_loop_stacks() {
    let file = SourceFile::parse("fixture.rs", "fn f(){ let Some(x) = value else { loop { break; } }; }".to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("the fixture contains a function");
    };
    let Stmt::Local(local) = &function.block.stmts[0] else {
        panic!("the fixture function begins with a let declaration");
    };
    let (_, diverge) = local.init.as_ref().unwrap().diverge.as_ref().unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.value_required_loops.push(0..1);
    collector.undeletable_statements.push(1..2);
    collector.visit_let_else_diverge(diverge);
    assert_eq!(collector.value_required_loops.as_slice(), std::slice::from_ref(&(0..1)));
    assert_eq!(collector.undeletable_statements.as_slice(), std::slice::from_ref(&(1..2)));
}

#[test]
fn emission_admission_deduplication_and_reserved_occurrences_keep_full_metadata() {
    use super::super::{Confidence, Shape};

    let file = SourceFile::parse("fixture.rs", "fn f(){ left; left; right; }".to_owned()).unwrap();
    let selection = Selection::parse("literal.int_to_zero,literal.int_to_one").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.outermost = "fixture".into();
    let spans = file
        .text
        .match_indices("left")
        .map(|(start, text)| start..start + text.len())
        .collect::<Vec<_>>();
    collector.emit_at_conditionally(
        "literal.int_to_zero",
        spans[0].clone(),
        "0",
        7,
        Shape::Expr,
        Confidence::Optimistic,
        false,
    );
    collector.emit_at("literal.int_to_zero", spans[1].clone(), "0", 9, Shape::Block, Confidence::Explicit);
    collector.emit_at("literal.int_to_one", spans[1].clone(), "0", 11, Shape::Expr, Confidence::Proven);
    collector.emit_at("literal.int_to_one", spans[1].clone(), "1", 13, Shape::Expr, Confidence::Proven);
    for range in [0..0, file.text.len()..file.text.len() + 1, 0..file.text.len() + 1] {
        collector.emit_at("literal.int_to_zero", range, "0", 17, Shape::Expr, Confidence::Proven);
    }
    collector.emit_at("literal.bool_flip", spans[0].clone(), "true", 19, Shape::Expr, Confidence::Proven);
    collector.emit_at("literal.int_to_zero", spans[0].clone(), "left", 23, Shape::Expr, Confidence::Proven);
    let candidates = collector.finish();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].mutator, "literal.int_to_zero");
    assert_eq!(candidates[0].span, spans[1]);
    assert_eq!(candidates[0].replacement, "0");
    assert_eq!(candidates[0].replacement_index, 9);
    assert_eq!(candidates[0].identity_occurrence, Some(1));
    assert_eq!(&*candidates[0].item_path, "fixture");
    assert_eq!(candidates[0].shape, Shape::Block);
    assert_eq!(candidates[0].confidence, Confidence::Explicit);
    assert_eq!(candidates[1].mutator, "literal.int_to_one");
    assert_eq!(candidates[1].replacement, "1");
    assert_eq!(candidates[1].replacement_index, 13);
    assert_eq!(candidates[1].identity_occurrence, Some(0));
}

#[test]
fn nested_function_and_closure_contexts_restore_all_outer_evidence() {
    use super::UnsignedReturn;

    let file = SourceFile::parse("fixture.rs", "fn f() {}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind("outer".into(), true);
    collector.bind_type("outer".into(), syn::parse_quote!(u32));
    collector.bind_inferred_unsigned("inferred".into());
    let _prior = collector.parameter_types.insert("outer".into(), syn::parse_quote!(u32));
    let _added = collector.deferred.insert("pending".into());
    collector.numeric_return = true;
    collector.unsigned_return = UnsignedReturn::BodyEntered;
    collector.foreign_error_return = true;
    collector.expected_return = Some(syn::parse_quote!(u32));
    collector.generics.push("Outer".into());
    let undo = collector.undo.len();
    let signature: syn::Signature = syn::parse_quote!(fn nested<T>(&self, inner: bool, last: i16) -> bool);
    let result = collector.in_function(&signature, |collector| {
        assert!(!collector.numeric_return);
        assert!(matches!(collector.unsigned_return, UnsignedReturn::Other));
        assert!(!collector.foreign_error_return);
        assert_eq!(collector.expected_return, Some(syn::parse_quote!(bool)));
        assert_eq!(collector.generics, ["Outer", "T"]);
        assert!(!collector.bindings.contains_key("outer"));
        assert_eq!(collector.bindings.len(), 2);
        assert!(!collector.bindings["inner"]);
        assert!(collector.bindings["last"]);
        assert_eq!(collector.binding_types["inner"].ty, syn::parse_quote!(bool));
        assert_eq!(collector.parameter_types["last"], syn::parse_quote!(i16));
        assert!(collector.inferred_unsigned.is_empty());
        assert!(collector.deferred.is_empty());
        for output in [None, Some(syn::ReturnType::Default), Some(syn::parse_quote!(-> u16))] {
            collector.in_nested_return_context(output.as_ref(), |collector| {
                let explicit = matches!(output, Some(syn::ReturnType::Type(_, _)));
                assert_eq!(collector.numeric_return, explicit);
                assert_eq!(collector.foreign_error_return, !explicit);
                assert_eq!(collector.expected_return.is_some(), explicit);
                assert_eq!(matches!(collector.unsigned_return, UnsignedReturn::BodyPending), explicit);
                collector.numeric_return = true;
                collector.expected_return = Some(syn::parse_quote!(String));
                collector.foreign_error_return = true;
            });
            assert!(!collector.numeric_return);
            assert!(!collector.foreign_error_return);
            assert_eq!(collector.expected_return, Some(syn::parse_quote!(bool)));
            assert!(matches!(collector.unsigned_return, UnsignedReturn::Other));
        }
        91
    });
    assert_eq!(result, 91);
    assert_eq!(collector.undo.len(), undo);
    assert_eq!(collector.generics, ["Outer"]);
    assert_eq!(collector.bindings.len(), 1);
    assert!(collector.bindings["outer"]);
    assert_eq!(collector.binding_types["outer"].ty, syn::parse_quote!(u32));
    assert_eq!(collector.parameter_types.len(), 1);
    assert!(collector.parameter_types.contains_key("outer"));
    assert!(collector.inferred_unsigned.contains("inferred"));
    assert!(collector.deferred.contains("pending"));
    assert!(collector.numeric_return);
    assert!(collector.foreign_error_return);
    assert!(matches!(collector.unsigned_return, UnsignedReturn::BodyEntered));
    assert_eq!(collector.expected_return, Some(syn::parse_quote!(u32)));
}

#[test]
fn impl_traversal_restores_outer_generic_and_associated_type_state() {
    let source = "struct S; trait Trait{} impl<T:Default,I:Iterator> Trait for S { type Value = u32; fn f()->Self::Value { 7 } }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("fn_value.zero").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.generics.push("Outer".into());
    collector.defaulted.push("OldDefault".into());
    collector.iterator_bounded.push("OldIterator".into());
    collector.in_default_impl = true;
    collector.impl_self_type = Some(syn::parse_quote!(bool));
    let _prior = collector.impl_self_associated.insert("Old".into(), syn::parse_quote!(i8));
    let Item::Impl(implementation) = &file.ast.items[2] else {
        unreachable!()
    };
    collector.visit_item_impl(implementation);
    assert_eq!(collector.generics, ["Outer"]);
    assert_eq!(collector.defaulted, ["OldDefault"]);
    assert_eq!(collector.iterator_bounded, ["OldIterator"]);
    assert!(collector.in_default_impl);
    assert_eq!(collector.impl_self_type, Some(syn::parse_quote!(bool)));
    assert_eq!(collector.impl_self_associated.len(), 1);
    assert_eq!(collector.impl_self_associated["Old"], syn::parse_quote!(i8));
    let candidates = collector.finish();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    assert_eq!(candidates[0].replacement, "0");
    assert_eq!(&*candidates[0].item_path, "<S as Trait>::f");
}

#[test]
fn typed_zero_and_loop_evidence_is_bounded_by_each_expression_context() {
    let file = SourceFile::parse("fixture.rs", "fn f() { if ready { 0 } else { 0 } }".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
    let Stmt::Expr(expression, None) = &function.block.stmts[0] else {
        unreachable!()
    };
    collector.unsigned_zeros.push(1..2);
    collector.signed_zeros.push(3..4);
    let result = collector.in_unsigned_expression(expression, |collector| {
        assert_eq!(collector.unsigned_zeros.len(), 3);
        assert!(collector.unsigned_zeros[1..].iter().all(|span| file.slice(span) == "0"));
        collector.in_signed_expression(expression, |collector| {
            assert_eq!(collector.signed_zeros.len(), 3);
            assert!(collector.signed_zeros[1..].iter().all(|span| file.slice(span) == "0"));
            19
        })
    });
    assert_eq!(result, 19);
    assert_eq!(collector.unsigned_zeros.as_slice(), std::slice::from_ref(&(1..2)));
    assert_eq!(collector.signed_zeros.as_slice(), std::slice::from_ref(&(3..4)));
    collector.admissible_defaults.push(5..6);
    collector.reusable_payloads.push((7..8, "existing".into()));
    collector.value_required_loops.push(9..10);
    let loop_expression: Expr = syn::parse_quote!(loop {
        break 0;
    });
    for (ty, count) in [(syn::parse_quote!(u32), 2), (syn::parse_quote!(()), 1)] {
        collector.in_typed_expression(&loop_expression, &ty, |collector| {
            assert_eq!(collector.value_required_loops.len(), count);
            collector.admissible_defaults.push(11..12);
            collector.reusable_payloads.push((13..14, "temporary".into()));
        });
        assert_eq!(collector.admissible_defaults.as_slice(), std::slice::from_ref(&(5..6)));
        assert_eq!(collector.reusable_payloads.len(), 1);
        assert_eq!(collector.reusable_payloads[0], (7..8, "existing".into()));
        assert_eq!(collector.value_required_loops.as_slice(), std::slice::from_ref(&(9..10)));
    }
}

#[test]
fn call_default_candidates_and_regex_rewrites_match_exact_output_matrices() {
    use super::super::{Confidence, Shape};

    for (selector, replacement) in [
        ("call.replace_with_default", "Default::default()"),
        ("call_result.default", "::gamma_rt::default_after(helper(7))"),
    ] {
        let source = "fn helper(v:u8)->u8{v} fn f(){helper(7)}";
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1, "{selector}");
        let candidate = &candidates[0];
        assert_eq!(candidate.mutator, selector);
        assert_eq!(file.slice(&candidate.span), "helper(7)");
        assert_eq!(candidate.replacement, replacement);
        assert_eq!(candidate.replacement_index, 0);
        assert_eq!(candidate.shape, Shape::Expr);
        assert_eq!(candidate.confidence, Confidence::Proven);
        assert_eq!(&*candidate.item_path, "f");
        assert_eq!(candidate.identity_occurrence, Some(0));
    }
    for source in [
        "fn helper()->&'static u8 { &1 } fn f(){helper()}",
        "fn helper<T>()->T {loop{}} fn f(){helper()}",
        "fn Some()->u8 {1} fn f(){Some()}",
        "fn Ok()->u8 {1} fn f(){Ok()}",
        "fn Err()->u8 {1} fn f(){Err()}",
        "fn drop()->u8 {1} fn f(){drop()}",
        "fn default()->u8 {1} fn f(){default()}",
        "fn f(){unknown()}",
        "mod nested {fn helper()->u8{1}} fn f(){nested::helper()}",
    ] {
        assert!(
            mutators(source, "call.replace_with_default,call_result.default", &CfgSet::unconditional()).is_empty(),
            "{source}"
        );
    }
    for (pattern, selector, expected) in [
        ("^abc", "regex.remove_start_anchor", "\"abc\""),
        ("abc$", "regex.remove_end_anchor", "\"abc\""),
        ("[ab]", "regex.negate_character_class", "\"[^ab]\""),
        ("a*", "regex.star_to_plus", "\"a+\""),
        ("a+", "regex.plus_to_star", "\"a*\""),
        ("a?", "regex.optional_to_required", "\"a\""),
    ] {
        let source = format!("fn f() {{ Regex::new({pattern:?}) }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1, "{pattern}: {candidates:?}");
        assert_eq!(candidates[0].replacement, expected);
        assert_eq!(candidates[0].replacement_index, 0);
        assert_eq!(candidates[0].shape, Shape::Expr);
        assert_eq!(file.slice(&candidates[0].span), format!("{pattern:?}"));
    }
    for source in [
        "fn f() { Regex::new(\"[\") }",
        "fn f() { Regex::new(\"^a\", \"b\") }",
        "fn f() { Regex::new(pattern) }",
        "fn f() { Regex::other(\"^a\") }",
        "fn f() { helper(\"^a\") }",
        "fn f() { Regex::new() }",
    ] {
        assert!(mutators(source, "regex", &CfgSet::unconditional()).is_empty(), "{source}");
    }
}

#[test]
fn regex_rewrite_matrix_distinguishes_escaped_delimiters_and_quantifier_context() {
    use super::regex_mutations;
    for (pattern, expected) in [
        (
            "^a*$",
            vec![
                ("regex.remove_start_anchor", "a*$"),
                ("regex.remove_end_anchor", "^a*"),
                ("regex.star_to_plus", "^a+$"),
            ],
        ),
        ("[^ab]", vec![("regex.negate_character_class", "[ab]")]),
        ("[a*+?]", vec![("regex.negate_character_class", "[^a*+?]")]),
        (r"\[a*\]", vec![("regex.star_to_plus", r"\[a+\]")]),
        (r"a\$", vec![]),
        (r"a\\$", vec![("regex.remove_end_anchor", r"a\\")]),
        (r"a\?", vec![]),
        ("(?:a)", vec![]),
        ("a+?", vec![("regex.plus_to_star", "a*?")]),
        ("a{1,2}?", vec![]),
        ("a??", vec![("regex.optional_to_required", "a?")]),
    ] {
        assert_eq!(
            regex_mutations(pattern)
                .iter()
                .map(|(name, text)| (*name, text.as_str()))
                .collect::<Vec<_>>(),
            expected,
            "{pattern}"
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the semantic candidate matrix keeps exact text, shape, and provenance oracles together"
)]
fn semantic_candidates_preserve_exact_text_shape_indices_and_provenance() {
    use super::super::{Confidence, Shape};

    let cases: &[(&str, &str, &str, &str, Shape)] = &[
        (
            "fn f(v: Option<u8>) { v.is_some() }",
            "option.is_some_to_is_none",
            "v.is_some()",
            "v.is_none()",
            Shape::Expr,
        ),
        (
            "fn f(v: Option<u8>) { v.is_none() }",
            "option.is_none_to_is_some",
            "v.is_none()",
            "v.is_some()",
            Shape::Expr,
        ),
        (
            "fn f(v: Result<u8,u8>) { v.is_ok() }",
            "result.is_ok_to_is_err",
            "v.is_ok()",
            "v.is_err()",
            Shape::Expr,
        ),
        (
            "fn f(v: Result<u8,u8>) { v.is_err() }",
            "result.is_err_to_is_ok",
            "v.is_err()",
            "v.is_ok()",
            Shape::Expr,
        ),
        (
            "fn f(v: Option<u8>) { v.unwrap_or_else(|| 9) }",
            "fallback.unwrap_or_else_to_default",
            "v.unwrap_or_else(|| 9)",
            "v.unwrap_or_else(|| Default::default())",
            Shape::Expr,
        ),
        (
            "fn f(v: Option<u8>) { v.map_or(9, |x| x) }",
            "fallback.map_or_to_default",
            "v.map_or(9, |x| x)",
            "v.map_or(Default::default(), |x| x)",
            Shape::Expr,
        ),
        (
            "fn f(v: Option<u8>) { v.map_or_else(|| 9, |x| x) }",
            "fallback.map_or_else_to_default",
            "v.map_or_else(|| 9, |x| x)",
            "v.map_or_else(|| Default::default(), |x| x)",
            Shape::Expr,
        ),
        (
            "fn f(v: impl Iterator<Item=u8>) { v.rev() }",
            "iter.remove_rev",
            "v.rev()",
            "v",
            Shape::IterExpr,
        ),
        (
            "fn f(v: impl Iterator<Item=u8>) { v.take(7) }",
            "iter.take_to_skip",
            "v.take(7)",
            "v.skip(7)",
            Shape::IterExpr,
        ),
        (
            "fn f(v: impl Iterator<Item=u8>) { v.skip(7) }",
            "iter.skip_to_take",
            "v.skip(7)",
            "v.take(7)",
            Shape::IterExpr,
        ),
        (
            "fn f()->Option<u8> { None }",
            "option.none_to_some",
            "None",
            "Some(Default::default())",
            Shape::Expr,
        ),
        (
            "fn f()->Result<u8,u8> { Ok(7) }",
            "result.ok_to_err",
            "Ok(7)",
            "Err(Default::default())",
            Shape::Expr,
        ),
        (
            "fn f()->Result<u8,u8> { Err(7) }",
            "result.err_to_ok",
            "Err(7)",
            "Ok(Default::default())",
            Shape::Expr,
        ),
        (
            "fn f(){ let _ = vec![11, 22, 33]; }",
            "collection.reverse_vec",
            "vec![11, 22, 33]",
            "vec![33, 22, 11]",
            Shape::Expr,
        ),
        (
            "fn f(){ let _ = [11, 22, 33]; }",
            "collection.reverse_array",
            "[11, 22, 33]",
            "[33, 22, 11]",
            Shape::Expr,
        ),
    ];
    for &(source, mutator, original, replacement, shape) in cases {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse(mutator).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1, "{source}: {candidates:?}");
        let candidate = &candidates[0];
        assert_eq!(candidate.mutator, mutator);
        assert_eq!(file.slice(&candidate.span), original);
        assert_eq!(candidate.replacement, replacement);
        assert_eq!(candidate.shape, shape);
        assert_eq!(candidate.replacement_index, 0);
        assert_eq!(candidate.identity_occurrence, Some(0));
        assert_eq!(candidate.confidence, Confidence::Proven);
        assert_eq!(&*candidate.item_path, "f");
    }
    for (source, selection) in [
        ("fn f(v: V) { v.is_some(1) }", "option.is_some_to_is_none"),
        ("fn f(v: V) { v.take() }", "iter.take_to_skip"),
        ("fn f(){ let _ = [1]; }", "collection.reverse_array"),
        ("fn f(){ let _ = [vec![1].as_slice(), &[]]; }", "collection.reverse_array"),
        (
            "fn f(v: Option<u8>) { v.unwrap_or_else(|| 0) }",
            "fallback.unwrap_or_else_to_default",
        ),
        ("fn f(v: Option<u8>) { v.map_or(0, |x| x) }", "fallback.map_or_to_default"),
        (
            "fn f(v: Option<u8>) { v.map_or_else(|| 0, |x| x) }",
            "fallback.map_or_else_to_default",
        ),
        ("fn f(v: Option<u8>) { v.filter(|x| *x > 0) }", "iter.remove_filter"),
    ] {
        assert!(mutators(source, selection, &CfgSet::unconditional()).is_empty(), "{source}");
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the literal matrix keeps replacement ordering and confidence oracles together"
)]
fn literal_output_matrix_keeps_exact_replacements_ordinals_and_confidence() {
    use super::super::{Confidence, Shape};

    type ExpectedLiteral<'a> = (&'a str, &'a str, u32, Confidence);
    let cases: &[(&str, &str, &[ExpectedLiteral<'_>])] = &[
        (
            "7",
            "literal.int_increment,literal.int_decrement,literal.int_to_zero,literal.int_to_one",
            &[
                ("literal.int_increment", "8", 2, Confidence::Proven),
                ("literal.int_decrement", "6", 3, Confidence::Proven),
                ("literal.int_to_zero", "0", 0, Confidence::Proven),
                ("literal.int_to_one", "1", 1, Confidence::Proven),
            ],
        ),
        (
            "0",
            "literal.int_decrement",
            &[("literal.int_decrement", "-1", 3, Confidence::Optimistic)],
        ),
        (
            "0i32",
            "literal.int_decrement",
            &[("literal.int_decrement", "-1", 3, Confidence::Proven)],
        ),
        ("0u32", "literal.int_decrement", &[]),
        (
            "true",
            "literal.bool_flip",
            &[("literal.bool_flip", "false", 0, Confidence::Proven)],
        ),
        (
            "false",
            "literal.bool_flip",
            &[("literal.bool_flip", "true", 0, Confidence::Proven)],
        ),
        (
            "\"abc\"",
            "literal.str_to_empty,literal.str_to_xyzzy",
            &[
                ("literal.str_to_empty", "\"\"", 0, Confidence::Proven),
                ("literal.str_to_xyzzy", "\"xyzzy\"", 1, Confidence::Proven),
            ],
        ),
        (
            "\"\"",
            "literal.str_to_empty,literal.str_to_xyzzy",
            &[("literal.str_to_xyzzy", "\"xyzzy\"", 1, Confidence::Proven)],
        ),
        (
            "\"xyzzy\"",
            "literal.str_to_empty,literal.str_to_xyzzy",
            &[("literal.str_to_empty", "\"\"", 0, Confidence::Proven)],
        ),
        (
            "2.5f32",
            "literal.float_to_zero,literal.float_to_one,literal.float_negate",
            &[
                ("literal.float_to_zero", "0.0f32", 0, Confidence::Proven),
                ("literal.float_to_one", "1.0f32", 1, Confidence::Proven),
                ("literal.float_negate", "-2.5f32", 2, Confidence::Proven),
            ],
        ),
        ("0.0", "literal.float_to_zero", &[]),
        ("0f64", "literal.float_to_zero", &[]),
        ("1.0", "literal.float_to_one", &[]),
        ("1f64", "literal.float_to_one", &[]),
        (
            "'a'",
            "literal.char_to_nul,literal.char_to_distinct",
            &[
                ("literal.char_to_nul", "'\\0'", 0, Confidence::Proven),
                ("literal.char_to_distinct", "'x'", 1, Confidence::Proven),
            ],
        ),
        (
            "'x'",
            "literal.char_to_distinct",
            &[("literal.char_to_distinct", "'y'", 1, Confidence::Proven)],
        ),
        ("'\\0'", "literal.char_to_nul", &[]),
        (
            "b'a'",
            "literal.byte_to_nul,literal.byte_to_distinct",
            &[
                ("literal.byte_to_nul", "b'\\0'", 0, Confidence::Proven),
                ("literal.byte_to_distinct", "b'x'", 1, Confidence::Proven),
            ],
        ),
        (
            "b'x'",
            "literal.byte_to_distinct",
            &[("literal.byte_to_distinct", "b'y'", 1, Confidence::Proven)],
        ),
        ("b'\\0'", "literal.byte_to_nul", &[]),
    ];
    for &(literal, selector, expected) in cases {
        let source = format!("fn f() {{ let _ = {literal}; }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        let actual = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.mutator,
                    candidate.replacement.as_str(),
                    candidate.replacement_index,
                    candidate.confidence,
                )
            })
            .collect::<Vec<_>>();
        let mut expected = expected.to_vec();
        expected.sort_by_key(|(mutator, _, ordinal, _)| (*mutator, *ordinal));
        assert_eq!(actual, expected, "{literal}: {selector}");
        assert!(candidates.iter().all(|candidate| file.slice(&candidate.span) == literal
            && candidate.shape == Shape::Expr
            && candidate.identity_occurrence == Some(0)
            && &*candidate.item_path == "f"));
    }
}

#[test]
fn zero_decrement_context_matrix_follows_annotations_assignments_arguments_and_casts() {
    use super::super::Confidence;

    for (source, expected) in [
        ("fn f()->u8 { 0 }", None),
        ("fn f()->u8 { return 0; }", None),
        ("fn f()->i8 { 0 }", Some(Confidence::Proven)),
        ("fn f()->i8 { return 0; }", Some(Confidence::Proven)),
        ("fn f(){let value:u8=0;}", None),
        ("fn f(){let value:i8=0;}", Some(Confidence::Proven)),
        ("fn f(){let value=0;}", Some(Confidence::Optimistic)),
        ("fn f(){let mut value:u8=7;value=0;}", None),
        ("fn f(){let mut value:i8=7;value=0;}", Some(Confidence::Proven)),
        ("fn f(value:u8){if value==0 {}}", None),
        ("fn f(value:i8){if value==0 {}}", Some(Confidence::Proven)),
        ("fn f(value:u8){if 0==value {}}", None),
        ("fn f(value:i8){if 0==value {}}", Some(Confidence::Proven)),
        ("struct S{value:u8} fn f(){let _=S{value:0};}", None),
        ("struct S{value:i8} fn f(){let _=S{value:0};}", Some(Confidence::Proven)),
        ("fn helper(value:u8){} fn f(){helper(0)}", None),
        ("fn helper(value:i8){} fn f(){helper(0)}", Some(Confidence::Proven)),
        ("fn f(){0 as u8}", None),
        ("fn f(){0 as i8}", Some(Confidence::Optimistic)),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse("literal.int_decrement").unwrap(), &CfgSet::unconditional());
        let zeros = candidates
            .iter()
            .filter(|candidate| file.slice(&candidate.span) == "0")
            .collect::<Vec<_>>();
        assert_eq!(zeros.len(), usize::from(expected.is_some()), "{source}: {candidates:?}");
        if let Some(expected) = expected {
            assert_eq!(zeros[0].confidence, expected, "{source}");
            assert_eq!(zeros[0].replacement, "-1", "{source}");
            assert_eq!(zeros[0].replacement_index, 3);
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the control-flow matrix keeps exact source spans and replacement shapes together"
)]
fn control_flow_output_matrix_preserves_shapes_labels_and_statement_spans() {
    use super::super::Shape;

    for (source, selector, original, replacement, shape) in [
        ("fn f(){ work(); }", "stmt.delete_call", "work();", "", Shape::Stmt),
        ("fn f(){ value.work(); }", "stmt.delete_call", "value.work();", "", Shape::Stmt),
        (
            "fn f(){ let mut value=1; value=2; }",
            "stmt.delete_assign",
            "value=2;",
            "",
            Shape::Stmt,
        ),
        (
            "fn f(){ let mut value=1; value+=2; }",
            "stmt.delete_assign",
            "value+=2;",
            "",
            Shape::Stmt,
        ),
        (
            "fn f(v: &[u8]){ for _ in v { break; } }",
            "loop.delete_break",
            "break;",
            "",
            Shape::Stmt,
        ),
        ("fn f(){ loop { continue; } }", "loop.delete_continue", "continue;", "", Shape::Stmt),
        (
            "fn f(v: &[u8]){ for _ in v { break; } }",
            "loop.break_to_continue",
            "break",
            "continue",
            Shape::Break,
        ),
        (
            "fn f(){ loop { break 7; } }",
            "loop.break_value_default",
            "7",
            "Default::default()",
            Shape::Expr,
        ),
        (
            "fn f(){ loop { continue; } }",
            "loop.continue_to_break",
            "continue",
            "break",
            Shape::Continue,
        ),
        (
            "fn f(){ 'outer: loop { continue 'outer; } }",
            "loop.continue_to_break",
            "continue 'outer",
            "break 'outer",
            Shape::Continue,
        ),
        (
            "fn f(){ match x { 1=>a(), _=>b() } }",
            "match_arm.never_matches",
            "1",
            "",
            Shape::Arm,
        ),
        ("fn f(){ if ready {a()} }", "cond.always_true", "ready", "true", Shape::Expr),
        ("fn f(){ if ready {a()} }", "cond.always_false", "ready", "false", Shape::Expr),
        ("fn f(){ -value }", "unary.remove_neg", "-value", "value", Shape::Expr),
        ("fn f(value: bool){ !value }", "unary.remove_not", "!value", "value", Shape::Expr),
        (
            "fn f()->u8 { return 7; }",
            "return_value.default",
            "7",
            "Default::default()",
            Shape::Expr,
        ),
        (
            "fn f()->Result<u8,u8> { helper()?; Ok(1) }",
            "try.propagate_to_unwrap",
            "helper()?",
            "::gamma_rt::panic_on_failure(helper())?",
            Shape::Expr,
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1, "{source}: {candidates:?}");
        let candidate = &candidates[0];
        assert_eq!(file.slice(&candidate.span), original, "{source}");
        assert_eq!(candidate.replacement, replacement, "{source}");
        assert_eq!(candidate.shape, shape, "{source}");
        assert_eq!(
            candidate.replacement_index,
            match selector {
                "cond.always_true" => 1,
                "cond.always_false" => 2,
                _ => 0,
            },
            "{source}"
        );
        assert_eq!(candidate.identity_occurrence, Some(0), "{source}");
    }
    for (source, selector) in [
        ("fn f(){drop(value);}", "stmt.delete_call"),
        ("fn f(){let value; value=2;}", "stmt.delete_assign"),
        ("fn f(){ loop { break 7; } }", "loop.delete_break,loop.break_to_continue"),
        ("fn f(){ 'outer: loop { break 'outer; } }", "loop.break_to_continue"),
        ("fn f()->u8 { loop { continue; break 7; } }", "loop.continue_to_break"),
        ("fn f(){ if true {a()} }", "cond.always_true"),
        ("fn f(){ if false {a()} }", "cond.always_false"),
        ("fn f(){ if let Some(value)=input {a()} }", "cond"),
        ("fn f(){ match x { 1=>a(), 2=>b() } }", "match_arm.never_matches"),
    ] {
        assert!(mutators(source, selector, &CfgSet::unconditional()).is_empty(), "{source}");
    }
}

#[test]
fn labelled_continue_resolves_the_innermost_matching_loop_and_restores_stack() {
    for (source, count) in [
        ("fn f(){'outer: loop { break 7; 'outer: loop { continue 'outer; } }}", 1),
        ("fn f(){'outer: loop { 'outer: loop { break 7; continue 'outer; } }}", 0),
        ("fn f(){for item in items {continue;}}", 1),
        ("fn f(){while ready {continue;}}", 1),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let selection = Selection::parse("loop.continue_to_break").unwrap();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::of(&file.ast);
        let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        collector.visit_file(&file.ast);
        assert!(collector.loops.is_empty(), "{source}");
        let candidates = collector.finish();
        assert_eq!(candidates.len(), count, "{source}: {candidates:?}");
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.replacement == "break" || candidate.replacement == "break 'outer")
        );
    }
}

#[test]
fn logical_and_arithmetic_output_metadata_and_temporal_vetoes_are_exact() {
    use super::super::{Confidence, Shape};
    for (expression, selector, expected) in [
        (
            "left && right",
            "logical",
            vec![
                ("logical.and_to_or", "(left) || (right)"),
                ("logical.and_remove_left", "(right)"),
                ("logical.and_remove_right", "(left)"),
            ],
        ),
        (
            "left || right",
            "logical",
            vec![
                ("logical.or_to_and", "(left) && (right)"),
                ("logical.or_remove_left", "(right)"),
                ("logical.or_remove_right", "(left)"),
            ],
        ),
        (
            "left + right",
            "arith",
            vec![("arith.add_to_sub", "(left) - (right)"), ("arith.add_to_mul", "(left) * (right)")],
        ),
        (
            "left - right",
            "arith",
            vec![("arith.sub_to_add", "(left) + (right)"), ("arith.sub_to_div", "(left) / (right)")],
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", format!("fn f(left:u8,right:u8){{{expression}}}")).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        let mut expected = expected
            .into_iter()
            .enumerate()
            .map(|(index, (mutator, replacement))| {
                (
                    mutator,
                    replacement,
                    if selector == "logical" { 0 } else { u32::try_from(index).unwrap() },
                )
            })
            .collect::<Vec<_>>();
        expected.sort_by_key(|(mutator, _, ordinal)| (*mutator, *ordinal));
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.mutator, candidate.replacement.as_str(), candidate.replacement_index))
                .collect::<Vec<_>>(),
            expected
        );
        assert!(candidates.iter().all(|candidate| candidate.shape == Shape::Expr
            && candidate.identity_occurrence == Some(0)
            && candidate.confidence == Confidence::Proven
            && file.slice(&candidate.span) == expression));
    }
    for source in [
        "fn f(text:String,value:&str){text + value}",
        "fn f(text:String,value:&str){value + text}",
        "use core::time::Duration; fn f(left:Duration,right:Duration){left * right}",
        "fn f(value:u8){ if let Some(x)=input && value>0 {} }",
    ] {
        let selector = if source.contains("if let") { "logical" } else { "arith" };
        assert!(mutators(source, selector, &CfgSet::unconditional()).is_empty(), "{source}");
    }
    let file = SourceFile::parse(
        "fixture.rs",
        "use core::time::Duration; fn f(left:Duration,right:Duration){left + right}".to_owned(),
    )
    .unwrap();
    let candidates = collect_in(&file, &Selection::parse("arith").unwrap(), &CfgSet::unconditional());
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| (
                candidate.mutator,
                candidate.replacement.as_str(),
                candidate.replacement_index,
                candidate.confidence,
                candidate.shape,
            ))
            .collect::<Vec<_>>(),
        [("arith.add_to_sub", "(left) - (right)", 0, Confidence::Proven, Shape::Expr)]
    );
    assert_eq!(file.slice(&candidates[0].span), "left + right");
    let file = SourceFile::parse(
        "fixture.rs",
        "use core::time::Duration; fn f(left:Duration,right:Duration){left - right}".to_owned(),
    )
    .unwrap();
    let candidates = collect_in(&file, &Selection::parse("arith").unwrap(), &CfgSet::unconditional());
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].mutator, "arith.sub_to_add");
    assert_eq!(candidates[0].replacement, "(left) + (right)");
}

#[test]
fn inert_const_syntax_still_reserves_identity_and_runtime_children_are_visited() {
    let source = r"
        const ROOT: u8 = 7;
        static STATIC: u8 = 7;
        enum E { V = 7 }
        struct S([u8; 7]);
        trait T { const VALUE:u8=7; fn method()->u8 { 7 } }
        impl T for S { const VALUE:u8=7; fn method()->u8 { 7 } }
        fn f() {
            let _ = const { 7 };
            let _ = [7; 7];
            let _ = helper::<7>(7);
            let _ = 7;
        }
    ";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::unconditional());
    assert_eq!(candidates.len(), 6, "{candidates:?}");
    assert!(
        candidates
            .iter()
            .all(|candidate| file.slice(&candidate.span) == "7" && candidate.replacement == "0")
    );
    assert_eq!(
        candidates.iter().map(|candidate| &*candidate.item_path).collect::<Vec<_>>(),
        ["T::method", "<S as T>::method", "f", "f", "f", "f"]
    );
    let runtime: Vec<_> = candidates.iter().filter(|candidate| &*candidate.item_path == "f").collect();
    assert_eq!(
        runtime.iter().map(|candidate| candidate.span.start).collect::<Vec<_>>(),
        [
            source.find("const { 7 }").unwrap() + "const { ".len(),
            source.find("[7; 7]").unwrap() + 1,
            source.find("helper::<7>(7)").unwrap() + "helper::<7>(".len(),
            source.rfind("let _ = 7;").unwrap() + "let _ = ".len(),
        ]
    );
    assert_eq!(
        runtime.iter().map(|candidate| candidate.identity_occurrence).collect::<Vec<_>>(),
        [Some(0), Some(1), Some(2), Some(3)]
    );
}

#[test]
fn traversal_visits_each_runtime_child_once_without_skipping_after_gated_nodes() {
    let source = "struct S{value:u8} fn f() {
        #[cfg(not(unix))] let _=7;
        let _=wrapper(helper(7)); let _=[7,7]; let _=(7);
        let _=obj.method(7); let _=S{value:7,..base(7)}; return 7;
    }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("literal.int_to_zero").unwrap(), &CfgSet::parse("unix"));
    assert_eq!(candidates.len(), 8, "{candidates:?}");
    assert!(candidates.iter().all(|candidate| file.slice(&candidate.span) == "7"
        && candidate.replacement == "0"
        && candidate.replacement_index == 0
        && &*candidate.item_path == "f"));
    assert_eq!(
        candidates.iter().map(|candidate| candidate.identity_occurrence).collect::<Vec<_>>(),
        (0..8).map(Some).collect::<Vec<_>>()
    );
}

#[test]
fn generic_default_and_iterator_bounds_are_isolated_between_nested_functions() {
    let file = SourceFile::parse("fixture.rs", "fn f() {}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.defaulted.push("OuterDefault".into());
    collector.iterator_bounded.push("OuterIterator".into());
    let generic: syn::Generics = syn::parse_quote!(<T: Default, I: Iterator<Item = u8>, U>);
    collector.with_defaulted_parameters(&generic, |collector| {
        assert_eq!(collector.defaulted, ["OuterDefault", "T"]);
        assert_eq!(collector.iterator_bounded, ["OuterIterator", "I"]);
    });
    assert_eq!(collector.defaulted, ["OuterDefault"]);
    assert_eq!(collector.iterator_bounded, ["OuterIterator"]);
    collector.with_function_parameters(&generic, |collector| {
        assert_eq!(collector.function_bound_depth, Some((1, 0)));
        assert_eq!(collector.iterator_bounded, ["OuterIterator", "I"]);
        let nested: syn::Generics = syn::parse_quote!(<J: core::iter::Iterator<Item = bool>>);
        collector.with_function_parameters(&nested, |collector| {
            assert_eq!(collector.function_bound_depth, Some((1, 0)));
            assert_eq!(collector.iterator_bounded, ["OuterIterator", "J"]);
        });
        assert_eq!(collector.iterator_bounded, ["OuterIterator", "I"]);
        assert_eq!(collector.function_bound_depth, Some((1, 0)));
    });
    assert_eq!(collector.iterator_bounded, ["OuterIterator"]);
    assert_eq!(collector.defaulted, ["OuterDefault"]);
    assert_eq!(collector.function_bound_depth, None);
}

#[test]
fn expression_type_and_import_evidence_tracks_dereferences_not_unrelated_operators() {
    let file = SourceFile::parse("fixture.rs", "struct S{field:u32} fn helper()->i16{1} fn f(){}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind_type("number".into(), syn::parse_quote!(u32));
    collector.bind_type("signed".into(), syn::parse_quote!(i16));
    collector.bind_type("ptr".into(), syn::parse_quote!(&u32));
    let captured = crate::HashMap::from_iter([("Special".into(), Some(vec!["original".into(), "Special".into()]))]);
    collector.binding_types.get_mut("ptr").unwrap().imports = captured.clone();
    for (text, expected) in [
        ("number", Some(syn::parse_quote!(u32))),
        ("(number)", Some(syn::parse_quote!(u32))),
        ("number + 1", Some(syn::parse_quote!(u32))),
        ("1 + number", Some(syn::parse_quote!(u32))),
        ("number + unknown", None),
        ("unknown + number", None),
        ("number < 1", None),
        ("signed as u8", Some(syn::parse_quote!(u8))),
        ("helper()", Some(syn::parse_quote!(i16))),
        ("(helper)()", None),
        ("(*ptr)", Some(syn::parse_quote!(u32))),
        ("!ptr", None),
        ("-ptr", None),
        ("unknown", None),
    ] {
        assert_eq!(
            collector.type_of_expression(&syn::parse_str::<Expr>(text).unwrap()),
            expected.as_ref(),
            "{text}"
        );
    }
    for (text, inherited) in [
        ("ptr", true),
        ("(ptr)", true),
        ("*ptr", true),
        ("!ptr", false),
        ("-ptr", false),
        ("number", false),
    ] {
        let expression = syn::parse_str::<Expr>(text).unwrap();
        assert_eq!(
            collector.imports_of_expression(&expression),
            if inherited { &captured } else { &collector.root_imports },
            "{text}"
        );
    }
    collector.reusable_payloads.push((1..2, "outer".into()));
    collector.reusable_payloads.push((3..4, "other".into()));
    collector.reusable_payloads.push((1..2, "inner".into()));
    for name in ["inner", "other"] {
        let _previous = collector
            .parameter_types
            .insert(name.into(), syn::parse_quote!(core::num::NonZeroU8));
        collector.bind_type(name.into(), syn::parse_quote!(core::num::NonZeroU8));
    }
    assert_eq!(collector.reusable_payload(&(1..2)), Some("inner"));
    assert_eq!(collector.reusable_payload(&(3..4)), Some("other"));
    assert_eq!(collector.reusable_payload(&(5..6)), None);
    for (text, unsigned, signed) in [
        ("number", true, false),
        ("signed", false, true),
        ("number as u8", true, false),
        ("signed as i32", false, true),
        ("helper()", false, true),
        ("unknown()", false, false),
        ("(number)", true, false),
        ("(signed)", false, true),
    ] {
        let expression = syn::parse_str::<Expr>(text).unwrap();
        assert_eq!(collector.is_known_unsigned(&expression), unsigned, "{text}");
        assert_eq!(collector.is_known_signed(&expression), signed, "{text}");
    }
}

#[test]
fn current_scope_type_evidence_does_not_borrow_root_imports_or_drop_self_aliases() {
    use super::types::Alias;

    let file = SourceFile::parse("fixture.rs", "use other::Option; struct Missing; fn f(){}".to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let option: syn::Type = syn::parse_quote!(Option<u8>);
    assert!(!collector.can_directly_default(&option));
    let scope = crate::HashMap::from_iter([("Option".into(), Some(vec!["core".into(), "option".into(), "Option".into()]))]);
    collector.scope_path.push((1, 2));
    let _prior = collector.scope_imports.insert(collector.scope_path.clone(), scope.clone());
    assert_eq!(collector.current_imports(), &scope);
    assert!(collector.can_directly_default(&option));
    collector.impl_self_type = Some(syn::parse_quote!(u8));
    let _prior = collector.impl_self_associated.insert("Value".into(), syn::parse_quote!(bool));
    let _prior = collector.aliases.insert(
        "Alias".into(),
        Some(Alias {
            parameters: vec![],
            target: syn::parse_quote!(u32),
        }),
    );
    assert!(collector.can_directly_default(&syn::parse_quote!(Self)));
    assert!(collector.can_directly_default(&syn::parse_quote!(Self::Value)));
    assert!(collector.can_directly_default(&syn::parse_quote!(Alias)));
    assert!(!collector.can_directly_default(&syn::parse_quote!(Result<u8,u8>)));
    assert!(collector.expression_is_known_default(&syn::parse_quote!((0))));
    assert!(!collector.expression_is_known_default(&syn::parse_quote!((7))));
    assert!(collector.expression_is_known_default_for_type(&syn::parse_quote!(None), &option));
    collector.scope_path.clear();
    assert_eq!(collector.current_imports(), &collector.root_imports);
    assert!(!collector.can_directly_default(&option));
}

#[test]
fn iterator_range_and_result_evidence_requires_each_structural_component() {
    let source = "fn f(){}";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (name, ty) in [
        ("items", "Vec<u8>"),
        ("slice", "&[u8]"),
        ("array", "[u8; 3]"),
        ("count", "usize"),
        ("text", "String"),
        ("foreign", "other::Vec<u8>"),
        ("result", "Result<u8,u8>"),
        ("option", "Option<u8>"),
        ("iter", "I"),
    ] {
        collector.bind_type(name.into(), syn::parse_str(ty).unwrap());
    }
    collector.iterator_bounded.push("I".into());
    for (text, iterator) in [
        ("items", false),
        ("items.iter()", true),
        ("items.iter_mut()", true),
        ("items.into_iter()", true),
        ("slice.iter()", true),
        ("array.iter()", true),
        ("foreign.iter()", false),
        ("text.iter()", false),
        ("unknown.iter()", false),
        ("iter", true),
        ("(iter)", true),
        ("iter.map(|x| x).filter(|x| true).rev().take(2)", true),
        ("unknown.map(|x| x)", false),
        ("0..4", true),
        ("count..", true),
        ("count..unknown", false),
        ("unknown..count", false),
        ("..count", false),
        ("std::iter::once(1)", true),
        ("core::iter::empty()", true),
        ("other::iter::once(1)", false),
    ] {
        let expression = syn::parse_str::<Expr>(text).unwrap();
        assert_eq!(collector.is_iterator_receiver(&expression), iterator, "{text}");
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: syn::token::Group::default(),
            expr: Box::new(expression),
        });
        assert_eq!(collector.is_iterator_receiver(&grouped), iterator, "grouped {text}");
    }
    for (ty, expected) in [
        ("Vec<u8>", true),
        ("std::collections::HashMap<u8,u8>", true),
        ("other::Vec<u8>", false),
        ("[u8; 4]", true),
        ("[u8]", true),
        ("&[u8]", true),
        ("&&Vec<u8>", true),
        ("String", false),
        ("u8", false),
    ] {
        assert_eq!(
            collector.type_has_standard_iterator_methods(&syn::parse_str(ty).unwrap(), &collector.root_imports),
            expected,
            "{ty}"
        );
    }
    for (text, expected) in [
        ("result", true),
        ("Ok(1)", false),
        ("Err(1)", false),
        ("std::env::var(\"key\")", true),
        ("option", false),
        ("unknown", false),
    ] {
        assert_eq!(
            collector.is_result_expression(&syn::parse_str::<Expr>(text).unwrap()),
            expected,
            "{text}"
        );
    }
}

#[test]
fn constructor_path_allowlists_require_exact_names_roots_and_unsigned_kinds() {
    use super::{ident_is_atomic_unsigned, ident_is_nonzero_unsigned, path_ends_with, path_matches, unsigned_context};

    for name in ["NonZeroU8", "NonZeroU16", "NonZeroU32", "NonZeroU64", "NonZeroU128", "NonZeroUsize"] {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        assert!(ident_is_nonzero_unsigned(&ident), "{name}");
    }
    for name in ["AtomicU8", "AtomicU16", "AtomicU32", "AtomicU64", "AtomicUsize"] {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        assert!(ident_is_atomic_unsigned(&ident), "{name}");
    }
    for name in [
        "NonZeroI32",
        "AtomicI32",
        "NonZero",
        "Atomic",
        "NonZeroU8Other",
        "OtherNonZeroU8",
        "Other",
    ] {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        assert!(!ident_is_nonzero_unsigned(&ident), "{name}");
        assert!(!ident_is_atomic_unsigned(&ident), "{name}");
    }
    for (text, exact, suffix) in [
        ("std::env::var_os", true, true),
        ("::std::env::var_os", true, true),
        ("other::std::env::var_os", false, true),
        ("std::wrong::var_os", false, false),
        ("std::env::other", false, false),
        ("var_os", false, false),
    ] {
        let path = syn::parse_str(text).unwrap();
        assert_eq!(path_matches(&path, &["std", "env", "var_os"]), exact, "{text}");
        assert_eq!(path_ends_with(&path, &["std", "env", "var_os"]), suffix, "{text}");
    }
    for (text, expected) in [
        ("Range<u8>", true),
        ("RangeInclusive<u32>", true),
        ("Range<i32>", false),
        ("Other<u32>", false),
        ("u32", true),
        ("()", false),
    ] {
        assert_eq!(unsigned_context(&syn::parse_str(text).unwrap()), expected, "{text}");
    }
}

#[test]
fn recursive_default_admissibility_and_payload_reuse_match_exact_typed_sites() {
    use super::types::Types;
    use super::{admissible_default_spans, reusable_payload_spans};

    let defaults = Defaults::of(&syn::parse_file("struct Missing;").unwrap());
    let imports = crate::HashMap::default();
    let types = Types {
        abstracts: &[],
        defaulted: &[],
        imports: &imports,
        defaults: &defaults,
        aliases: None,
        self_type: None,
        self_associated: None,
    };
    for (expected, expression, admissible_count, reusable_count) in [
        ("Option<u8>", "None", 1, 0),
        ("Option<u8>", "(None)", 1, 0),
        ("Option<u8>", "{ work(); None }", 1, 0),
        ("Option<u8>", "if ready { None } else { None }", 2, 0),
        ("Option<u8>", "match value { true => None, false => None }", 2, 0),
        ("Option<u8>", "Other", 0, 0),
        ("u8", "None", 0, 0),
        ("Result<u8,u16>", "Ok(7)", 1, 0),
        ("Result<u8,u16>", "Err(7)", 1, 0),
        ("Result<u8,Missing>", "Ok(7)", 0, 0),
        ("Result<Missing,u16>", "Err(7)", 0, 0),
        ("Option<Missing>", "None", 0, 0),
        ("Option<String>", "None", 1, 0),
        ("Option<NonZeroU8>", "None", 0, 1),
        ("Option<&u8>", "(None)", 0, 0),
        ("Option<&'a u8>", "{ work(); None }", 0, 1),
        ("Option<&'a u8>", "if ready { None } else { None }", 0, 2),
        ("Option<&'a u8>", "match value { true => None, false => None }", 0, 2),
        ("Option<&mut u8>", "None", 0, 0),
        ("Result<u8,&'a u8>", "Ok(7)", 0, 1),
        ("Result<&'a u8,u8>", "Err(7)", 0, 1),
    ] {
        let source = format!("fn f() {{ {expression} }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
        let Stmt::Expr(expression, None) = &function.block.stmts[0] else {
            unreachable!()
        };
        let expected: syn::Type = syn::parse_str(expected).unwrap();
        let payload = super::predicates::payload(&expected, 0).unwrap_or(&expected).clone();
        let mut parameters = crate::HashMap::from_iter([("copy".into(), payload.clone())]);
        if matches!(&expected, syn::Type::Path(path) if path.path.segments.last().unwrap().ident == "Result")
            && file.slice(&expression.span().byte_range()).starts_with("Ok")
        {
            let _prior = parameters.insert("copy".into(), super::predicates::payload(&expected, 1).unwrap().clone());
        }
        let mut admissible = Vec::new();
        let mut reusable = Vec::new();
        admissible_default_spans(expression, &expected, &types, &mut admissible);
        reusable_payload_spans(expression, &expected, &types, &parameters, &mut reusable);
        assert_eq!(admissible.len(), admissible_count, "{expected:?}: {expression:?}");
        assert_eq!(reusable.len(), reusable_count, "{expected:?}: {expression:?}");
        assert!(reusable.iter().all(|(_, name)| name == "copy"));
        assert!(
            admissible
                .iter()
                .all(|span| file.slice(span) == "None" || file.slice(span).starts_with("Ok") || file.slice(span).starts_with("Err"))
        );
    }
}

#[test]
fn recursive_value_position_walks_record_only_type_preserving_sites() {
    use super::{unsigned_zero_spans, value_position_loop_spans};

    for (expression, zero_count, loop_count) in [
        ("0", 1, 0),
        ("((0))", 1, 0),
        ("{ call(0); 0 }", 1, 0),
        ("{ 0; }", 0, 0),
        ("if ready { 0 } else { 0 }", 2, 0),
        ("match value { 1 => 0, _ => 0 }", 2, 0),
        ("0..0", 2, 0),
        ("0..", 1, 0),
        ("..0", 1, 0),
        ("opt.map_or(0, |x| 0)", 2, 0),
        ("opt.map_or(0, f)", 1, 0),
        ("xs.map(|x| 0).rev().sum()", 1, 0),
        ("call(0)", 0, 0),
        ("[0, 0]", 0, 0),
        ("loop { break 0; }", 0, 1),
        ("if ready { loop {} } else { loop {} }", 0, 2),
        ("match value { 1 => loop {}, _ => loop {} }", 0, 2),
        ("{ loop {}; (loop {}) }", 0, 1),
    ] {
        let source = format!("fn f() {{ {expression} }}");
        let file = SourceFile::parse("fixture.rs", source).unwrap();
        let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
        let Stmt::Expr(expression, None) = function.block.stmts.last().unwrap() else {
            unreachable!()
        };
        let mut zeros = Vec::new();
        let mut loops = Vec::new();
        unsigned_zero_spans(expression, &mut zeros);
        value_position_loop_spans(expression, &mut loops);
        assert_eq!(zeros.len(), zero_count, "{expression:?}");
        assert!(zeros.iter().all(|span| file.slice(span) == "0"));
        assert_eq!(loops.len(), loop_count, "{expression:?}");
        assert!(loops.iter().all(|span| file.slice(span).starts_with("loop")));
    }
    let group = Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: syn::token::Group::default(),
        expr: Box::new(syn::parse_quote!(0)),
    });
    let mut zeros = Vec::new();
    unsigned_zero_spans(&group, &mut zeros);
    assert_eq!(zeros.len(), 1);
}

#[test]
fn standard_unsigned_arguments_require_exact_constructor_paths_and_positions() {
    use super::standard_unsigned_argument;
    use crate::HashMap;

    let imports = HashMap::from_iter([
        ("Duration".to_owned(), Some(vec!["std".into(), "time".into(), "Duration".into()])),
        (
            "NonZeroU32".to_owned(),
            Some(vec!["core".into(), "num".into(), "NonZeroU32".into()]),
        ),
        (
            "AtomicU32".to_owned(),
            Some(vec!["std".into(), "sync".into(), "atomic".into(), "AtomicU32".into()]),
        ),
    ]);
    for (callee, accepted) in [
        ("std::time::Duration::new", vec![0, 1]),
        ("core::time::Duration::new", vec![0, 1]),
        ("Duration::from_secs", vec![0]),
        ("Duration::from_millis", vec![0]),
        ("Duration::from_micros", vec![0]),
        ("Duration::from_nanos", vec![0]),
        ("Duration::unknown", vec![]),
        ("other::Duration::new", vec![]),
        ("NonZeroU32::new", vec![0]),
        ("AtomicU32::new", vec![0]),
        ("char::from_u32", vec![0]),
        ("core::num::NonZeroI32::new", vec![]),
        ("other::AtomicU32::new", vec![]),
        ("helper", vec![]),
        ("(|| {})", vec![]),
    ] {
        let callee = syn::parse_str::<Expr>(callee).unwrap();
        for index in 0..3 {
            assert_eq!(
                standard_unsigned_argument(&callee, index, &imports),
                accepted.contains(&index),
                "{callee:?}, {index}"
            );
        }
    }
    for name in ["NonZeroU8", "NonZeroU16", "NonZeroU32", "NonZeroU64", "NonZeroU128", "NonZeroUsize"] {
        let callee = syn::parse_str::<Expr>(&format!("core::num::{name}::new")).unwrap();
        assert!(standard_unsigned_argument(&callee, 0, &imports), "{name}");
        assert!(!standard_unsigned_argument(&callee, 1, &imports), "{name}");
    }
    for (module, names) in [
        (
            "num",
            &["NonZeroU8", "NonZeroU16", "NonZeroU32", "NonZeroU64", "NonZeroU128", "NonZeroUsize"][..],
        ),
        (
            "sync::atomic",
            &["AtomicU8", "AtomicU16", "AtomicU32", "AtomicU64", "AtomicUsize"][..],
        ),
    ] {
        for name in names {
            for root in ["std", "core"] {
                let source = format!("{root}::{module}::{name}::new");
                let callee = syn::parse_str::<Expr>(&source).unwrap();
                assert!(standard_unsigned_argument(&callee, 0, &imports), "{source}");
                assert!(!standard_unsigned_argument(&callee, 1, &imports), "{source}");
                let mut paths = vec![root.to_owned()];
                paths.extend(module.split("::").map(str::to_owned));
                paths.push((*name).to_owned());
                let renamed = crate::HashMap::from_iter([("Alias".into(), Some(paths))]);
                assert!(standard_unsigned_argument(&syn::parse_quote!(Alias::new), 0, &renamed), "{source}");
                assert!(
                    !standard_unsigned_argument(&syn::parse_quote!(Alias::other), 0, &renamed),
                    "{source}"
                );
            }
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "classification checks share one controlled binding and import environment"
)]
fn numeric_and_temporal_classification_preserves_exact_confidence() {
    use super::super::Confidence;
    use super::BindingType;
    use super::predicates::is_numeric_binding;

    let source = "use core::time::Duration; use std::time::{Instant,SystemTime}; use chrono::{DateTime,NaiveDateTime,Utc}; fn f() {}";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for (name, ty) in [
        ("number", "usize"),
        ("float", "f64"),
        ("duration", "Duration"),
        ("instant", "Instant"),
        ("system", "SystemTime"),
        ("datetime", "chrono::DateTime"),
        ("naive", "chrono::NaiveDateTime"),
        ("text", "String"),
        ("option", "Option<u8>"),
        ("vector", "Vec<u8>"),
    ] {
        let ty = syn::parse_str(ty).unwrap();
        let _old = collector.bindings.insert(name.to_owned(), is_numeric_binding(&ty));
        let _old = collector.binding_types.insert(
            name.to_owned(),
            BindingType {
                ty,
                imports: collector.root_imports.clone(),
                scope: collector.scope_path.clone(),
                resource_trusted: true,
            },
        );
    }
    for (text, numeric, confidence) in [
        ("number", true, Confidence::Proven),
        ("float", true, Confidence::Proven),
        ("unknown", false, Confidence::Optimistic),
        ("number as u32", true, Confidence::Proven),
        ("number + 1", true, Confidence::Proven),
        ("1 + number", true, Confidence::Proven),
        ("number - 1", true, Confidence::Proven),
        ("duration - duration", false, Confidence::Optimistic),
        ("instant - instant", false, Confidence::Optimistic),
        ("duration.as_secs_f32()", true, Confidence::Proven),
        ("duration.as_secs_f64()", true, Confidence::Proven),
        ("duration.as_nanos()", true, Confidence::Proven),
        ("duration.subsec_nanos()", true, Confidence::Proven),
        ("float.sqrt()", true, Confidence::Proven),
        ("vector.len()", true, Confidence::Proven),
        ("text.len()", true, Confidence::Proven),
        ("(0..4).count()", true, Confidence::Proven),
        ("unknown.len()", true, Confidence::Optimistic),
        ("unknown.as_secs_f32()", false, Confidence::Optimistic),
    ] {
        let expression = syn::parse_str::<Expr>(text).unwrap();
        assert_eq!(collector.is_known_numeric(&expression), numeric, "{text}");
        assert_eq!(collector.numeric_confidence(&expression), confidence, "{text}");
    }
    for name in ["duration", "instant", "system", "datetime", "naive"] {
        let expression = syn::parse_str::<Expr>(name).unwrap();
        assert!(collector.is_known_temporal(&expression), "{name}");
        assert!(collector.is_known_textual_or_temporal(&expression), "{name}");
    }
    for name in ["number", "text", "option", "unknown"] {
        assert!(!collector.is_known_temporal(&syn::parse_str::<Expr>(name).unwrap()), "{name}");
    }
    for text in [
        "std::option::Option<u8>",
        "core::option::Option<u8>",
        "std::string::String",
        "alloc::string::String",
        "std::ffi::OsString",
        "std::path::PathBuf",
        "core::time::Duration",
        "&str",
    ] {
        assert!(
            collector.is_known_textual_or_temporal_type(&syn::parse_str(text).unwrap()),
            "{text}"
        );
    }
    for text in ["other::Duration", "other::String", "other::Option<u8>", "&u8", "()", "u8"] {
        assert!(
            !collector.is_known_textual_or_temporal_type(&syn::parse_str(text).unwrap()),
            "{text}"
        );
    }
    for text in [
        "Instant::now()",
        "std::time::Instant::now()",
        "SystemTime::now()",
        "Duration::new(1,2)",
        "(DateTime::new)()",
        "NaiveDateTime::new()",
        "Utc::now()",
    ] {
        assert!(
            collector.inferred_expression_type(&syn::parse_str::<Expr>(text).unwrap()).is_some(),
            "{text}"
        );
    }
    for text in ["helper()", "unknown", "DateTime"] {
        assert!(
            collector.inferred_expression_type(&syn::parse_str::<Expr>(text).unwrap()).is_none(),
            "{text}"
        );
    }
}

#[test]
fn omission_and_function_candidates_keep_exact_ordinals_and_explicit_values() {
    use super::super::{Confidence, Shape};

    let file = SourceFile::parse("fixture.rs", "fn f() { let _ = vec![11, 22, 33]; }".to_owned()).unwrap();
    let candidates = collect_in(
        &file,
        &Selection::parse("collection.omit_element").unwrap(),
        &CfgSet::unconditional(),
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.replacement.as_str())
            .collect::<Vec<_>>(),
        ["vec![22, 33]", "vec![11, 33]", "vec![11, 22]"]
    );
    assert_eq!(
        candidates.iter().map(|candidate| candidate.replacement_index).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.shape == Shape::Expr && candidate.identity_occurrence == Some(0))
    );

    let file = SourceFile::parse("fixture.rs", "fn f(no: &T, mut yes: u8) -> u8 { yes }".to_owned()).unwrap();
    let candidates = collect_in(
        &file,
        &Selection::parse("parameter.default_shadow").unwrap(),
        &CfgSet::unconditional(),
    );
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].replacement, "{ let mut yes: u8 = Default::default(); yes }");
    assert_eq!(candidates[0].replacement_index, 1);
    assert_eq!(candidates[0].shape, Shape::Block);

    let file = SourceFile::parse("fixture.rs", "#[gamma::value(99)] fn f() -> u8 { 7 }".to_owned()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("fn_value").unwrap(), &CfgSet::unconditional());
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].mutator, "fn_value.stated");
    assert_eq!(candidates[0].replacement, "99");
    assert_eq!(candidates[0].replacement_index, 0);
    assert_eq!(candidates[0].confidence, Confidence::Explicit);

    let file = SourceFile::parse("fixture.rs", "fn f() -> Result<u8,u8> { Ok(7) }".to_owned()).unwrap();
    let mut selection = Selection::parse("fn_value.err_with").unwrap();
    selection.set_errors(vec!["7".into(), "9".into()]);
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.replacement.as_str())
            .collect::<Vec<_>>(),
        ["Err(7)", "Err(9)"]
    );
    assert_eq!(
        candidates.iter().map(|candidate| candidate.replacement_index).collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.confidence == Confidence::Explicit && candidate.shape == Shape::Block)
    );
}

#[test]
fn alias_and_associated_return_evidence_preserves_default_recursion_exclusions() {
    for (source, selector, expected, path) in [
        ("type Count=u8; fn f()->Count{7}", "fn_value.zero", "0", "f"),
        (
            "struct S; trait T{type Value;} impl T for S{type Value=u8;fn f()->Self::Value{7}}",
            "fn_value.zero",
            "0",
            "<S as T>::f",
        ),
        (
            "struct S; impl Default for S{fn default()->Self{S} fn other()->Self{S}}",
            "fn_value.default",
            "Default::default()",
            "<S as Default>::other",
        ),
        (
            "fn f((x,y):(u8,u8), no:&u8, mut yes:bool){yes}",
            "parameter.default_shadow",
            "{ let mut yes: bool = Default::default();yes}",
            "f",
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), 1, "{source}: {candidates:?}");
        assert_eq!(candidates[0].replacement, expected);
        assert_eq!(&*candidates[0].item_path, path);
        assert_eq!(
            candidates[0].replacement_index,
            if selector == "parameter.default_shadow" { 2 } else { 0 }
        );
    }
    for source in [
        "struct S;impl Default for S{fn default()->Self{S}}",
        "fn f<T>()->T{loop{}}",
        "fn f()->&u8{&1}",
    ] {
        assert!(
            mutators(source, "fn_value.default", &CfgSet::unconditional()).is_empty(),
            "{source}"
        );
    }
}

#[test]
fn struct_field_and_method_rewrites_preserve_nonzero_offsets_and_original_spelling() {
    use super::super::Shape;

    let source = "struct S { a:u8,b:u8 } fn f(base:S){ let _ = S { a: 11, b: 22, ..base }; }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("struct_field.omit").unwrap(), &CfgSet::unconditional());
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.replacement.as_str())
            .collect::<Vec<_>>(),
        ["S { b: 22, ..base }", "S { a: 11, ..base }"]
    );
    assert_eq!(
        candidates.iter().map(|candidate| candidate.replacement_index).collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| file.slice(&candidate.span) == "S { a: 11, b: 22, ..base }" && candidate.shape == Shape::Expr)
    );
    assert!(
        mutators(
            "struct S{a:u8} fn f(){let _=S{a:1};}",
            "struct_field.omit",
            &CfgSet::unconditional()
        )
        .is_empty()
    );
    for (source, selector, expected) in [
        ("fn f(items:Vec<u8>){items.last()}", "iter.last_to_first", Some("items.first()")),
        ("fn f(items:&[u8]){items.last()}", "iter.last_to_first", Some("items.first()")),
        ("fn f(items:[u8;3]){items.last()}", "iter.last_to_first", Some("items.first()")),
        ("fn f(items:impl Iterator<Item=u8>){items.last()}", "iter.last_to_first", None),
        ("fn f(items:Unknown){items.last()}", "iter.last_to_first", None),
        ("fn f(items:Vec<u8>){items.last(1)}", "iter.last_to_first", None),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.len(), usize::from(expected.is_some()), "{source}");
        if let Some(expected) = expected {
            assert_eq!(candidates[0].replacement, expected, "{source}");
            assert_eq!(candidates[0].replacement_index, 0, "{source}");
            assert_eq!(candidates[0].shape, Shape::Expr);
        }
    }
}

#[test]
fn subspan_rewrites_preserve_boundaries_and_reject_noncontained_ranges() {
    let source = "fn f() { value.map_or(9, |x| x) }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
    let Stmt::Expr(expression, None) = &function.block.stmts[0] else {
        unreachable!()
    };
    let Expr::MethodCall(call) = expression else { unreachable!() };
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    assert_eq!(
        collector.rewrite_part(call.span(), call.span(), "replacement").as_deref(),
        Some("replacement")
    );
    assert_eq!(
        collector.rewrite_part(call.span(), call.receiver.span(), "receiver").as_deref(),
        Some("receiver.map_or(9, |x| x)")
    );
    assert_eq!(
        collector.rewrite_part(call.span(), call.method.span(), "renamed").as_deref(),
        Some("value.renamed(9, |x| x)")
    );
    assert_eq!(collector.rewrite_part(call.method.span(), call.span(), "replacement"), None);
    assert_eq!(collector.rewrite_part(call.span(), function.sig.ident.span(), "replacement"), None);

    for keep in [0, call.receiver.span().byte_range().end] {
        let mut file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        file.text.truncate(keep);
        let Item::Fn(function) = &file.ast.items[0] else { unreachable!() };
        let Stmt::Expr(Expr::MethodCall(call), None) = &function.block.stmts[0] else {
            unreachable!()
        };
        let defaults = Defaults::of(&file.ast);
        let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        assert_eq!(
            collector.rewrite_part(call.span(), call.receiver.span(), "replacement"),
            None,
            "truncated at {keep}"
        );
    }
}

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
    let defaults = Defaults::of_in(&file.ast, cfg);

    collect_with(&file, &selection, cfg, &defaults)
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

#[test]
fn a_configured_out_arm_does_not_hide_later_mutatable_arms() {
    let source = "fn f(x: u8) -> u8 { match x { #[cfg(not(unix))] 0 => 0, 1 => 1, _ => 2 } }";
    let found = mutators(source, "match_arm.never_matches", &CfgSet::parse("unix"));

    assert_eq!(found, vec!["match_arm.never_matches"]);
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
fn a_reference_value_is_not_fabricated_with_a_generic_leak() {
    let source = "fn f<T: Default>() -> &'static T { Box::leak(Box::new(make())) }";
    let found = mutators(source, "fn_value", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
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
fn local_unit_call_results_are_omitted_but_call_deletion_remains() {
    for source in [
        "fn helper() {} fn f() { helper(); }",
        "fn helper() -> () {} fn f() { helper(); }",
        "fn helper<T>(value: T) {} fn f() { helper(7); }",
        "mod nested { fn helper() {} fn f() { helper(); } }",
        "fn f() { fn helper() {} helper(); }",
        "fn helper() {} fn f() { { helper(); } }",
    ] {
        for selector in ["@default", "all", "call_result.default"] {
            let found = mutators(source, selector, &CfgSet::unconditional());
            assert!(!found.contains(&"call_result.default"), "{source}: {selector}: {found:?}");
        }
        assert_eq!(
            mutators(source, "call.replace_with_default", &CfgSet::unconditional()),
            ["call.replace_with_default"],
            "{source}"
        );
    }
}

#[test]
fn ambiguous_unit_callees_and_nonunit_results_stay_in_the_population() {
    for source in [
        "mod first { fn helper() {} } mod second { fn helper() {} fn f() { helper(); } }",
        "fn helper() {} fn f() { inject_binding!(); helper(); }",
        "fn helper() {} fn f() { #[derive(Custom)] struct Local; helper(); }",
        "fn helper() {} fn f() { #[custom_attribute] other(); helper(); }",
        "#[custom_attribute] fn helper() {} fn f() { helper(); }",
        "fn helper() {} #[custom_attribute] fn f() { helper(); }",
        "#[cfg_attr(feature = \"transform\", custom_attribute)] fn helper() {} fn f() { helper(); }",
        "type Unit = (); fn helper() -> Unit {} fn f() { helper(); }",
        "fn helper() -> u8 { 1 } fn f() { helper(); }",
        "#[derive(Default)] struct Guard; impl Drop for Guard { fn drop(&mut self) {} } fn helper() -> Guard { Guard } fn f() { helper(); }",
        "#[derive(Default)] struct Unknown; fn helper() -> Unknown { Unknown } fn f() { helper(); }",
        "struct Default; impl Default { fn default() { observable(); } } fn helper() {} fn f() { helper(); }",
        "fn helper() {} fn f<Default: Custom>() { helper(); }",
        "#![no_implicit_prelude] fn helper() {} fn f() { helper(); }",
    ] {
        let found = mutators(source, "call_result.default", &CfgSet::unconditional());
        assert_eq!(found, ["call_result.default"], "{source}: {found:?}");
    }
    for source in [
        "fn helper() {} fn f(helper: fn() -> u8) { helper(); }",
        "fn helper() {} fn f() { let helper = || 1u8; helper(); }",
        "fn helper() {} fn f() { let (helper, _) = (|| 1u8, 0); helper(); }",
        "fn helper() {} fn f() { const helper: fn() -> u8 = other; helper(); }",
        "fn helper() {} fn f() { static helper: fn() -> u8 = other; helper(); }",
        "fn helper() {} fn f() { unsafe extern \"C\" { fn helper() -> u8; } helper(); }",
        "fn helper() {} fn f() { (|helper: fn() -> u8| helper())(other); }",
        "fn helper() {} mod nested { use dependency::helper; fn f() { helper(); } }",
        "fn helper() {} mod nested { use dependency::*; fn f() { helper(); } }",
        "mod nested { fn helper() {} } fn f() { helper(); }",
        "async fn helper() {} fn f() { helper(); }",
        "fn helper() {} fn f() { dependency::helper(); }",
        "fn helper() {} fn f() { <Thing as Trait>::helper(); }",
        "fn helper() {} fn f() { (helper)(); }",
        "fn f() { unknown(); }",
    ] {
        assert!(
            mutators(source, "call_result.default", &CfgSet::unconditional()).is_empty(),
            "{source}"
        );
    }
}

#[test]
fn standard_empty_constructor_returns_are_omitted_even_with_all_mutators() {
    for source in [
        "fn f() -> Vec<u8> { return Vec::new(); }",
        "fn f<T>() -> Vec<T> { return Vec::<T>::new(); }",
        "fn f() -> std::vec::Vec<u8> { return ::std::vec::Vec::new(); }",
        "use alloc::vec::Vec as Values; fn f() -> Values<u8> { return Values::new(); }",
        "use std::vec as vectors; fn f() -> vectors::Vec<u8> { return vectors::Vec::new(); }",
        "fn f() -> String { return (String::new()); }",
        "fn f() -> Vec<u8> { return Vec::default(); }",
        "fn f() -> std::collections::VecDeque<u8> { return std::collections::VecDeque::new(); }",
        "fn f() -> std::collections::LinkedList<u8> { return std::collections::LinkedList::new(); }",
        "fn f() -> std::collections::BinaryHeap<u8> { return std::collections::BinaryHeap::new(); }",
        "fn f() -> std::collections::BTreeSet<u8> { return std::collections::BTreeSet::new(); }",
        "fn f() -> std::collections::BTreeMap<u8,u8> { return std::collections::BTreeMap::new(); }",
        "fn f() -> bool { return bool::default(); }",
        "fn f() -> Option<u8> { return Option::default(); }",
        "use std::default::Default as StdDefault; fn f() -> Vec<u8> { return StdDefault::default(); }",
    ] {
        for selector in ["@default", "all", "return_value.default"] {
            let found = mutators(source, selector, &CfgSet::unconditional());
            assert!(!found.contains(&"return_value.default"), "{source}: {selector}: {found:?}");
        }
    }
}

#[test]
fn nonstandard_or_stateful_constructors_are_not_default_noops() {
    for source in [
        "#[derive(Default)] struct Vec<T>(T); impl<T: Default> Vec<T> { fn new() -> Self { effect(); Self(T::default()) } } fn f() -> Vec<u8> { return Vec::new(); }",
        "#[derive(Default)] struct String; impl String { fn default() -> Self { effect(); Self } } fn f() -> String { return String::default(); }",
        "use dependency::String; fn f() -> String { return String::new(); }",
        "use std::collections::HashMap; fn f() -> HashMap<u8,u8> { return HashMap::new(); }",
        "use std::collections::HashSet; fn f() -> HashSet<u8> { return HashSet::new(); }",
        "fn f() -> Vec<u8> { return Vec::with_capacity(0); }",
        "fn f() -> Vec<u8> { return Vec::with_capacity(1); }",
        "type Values = Vec<u8>; fn f() -> Values { return Values::new(); }",
        "fn f<Vec: Default>() -> Vec { return Vec::new(); }",
        "#[derive(Default)] struct Unknown; impl Unknown { fn new() -> Self { effect(); Self } } fn f() -> Unknown { return Unknown::new(); }",
        "fn f() -> Vec<u8> { return <Vec<u8> as Custom>::default(); }",
        "struct Default; impl Default { fn default() -> Vec<u8> { effect(); Vec::new() } } fn f() -> Vec<u8> { return Vec::new(); }",
        "#[derive(Default)] struct Option<T>(T); impl<T: Default> Option<T> { fn default() -> Self { effect(); Self(T::default()) } } fn f() -> Option<u8> { return Option::default(); }",
        "use custom::Default as CustomDefault; fn f() -> Vec<u8> { return CustomDefault::default(); }",
    ] {
        let found = mutators(source, "return_value.default", &CfgSet::unconditional());
        assert!(found.contains(&"return_value.default"), "{source}: {found:?}");
    }
}

#[test]
fn singleton_reversals_are_already_omitted_without_assuming_element_purity() {
    for source in [
        "fn f() { let _ = [effect()]; let _ = vec![effect()]; }",
        "fn f() { let _ = [Guard::new()]; let _ = vec![Guard::new()]; }",
        "fn f() { let _ = [unknown()]; let _ = vec![unknown()]; }",
        "fn f() { let _ = []; let _ = vec![]; }",
    ] {
        for selector in ["all", "collection.reverse_array,collection.reverse_vec"] {
            let found = mutators(source, selector, &CfgSet::unconditional());
            assert!(
                !found.contains(&"collection.reverse_array") && !found.contains(&"collection.reverse_vec"),
                "{source}: {found:?}"
            );
        }
    }
    let source = "fn f() { let _ = [effect(1), effect(2)]; let _ = vec![effect(1), effect(2)]; }";
    let found = mutators(source, "collection.reverse_array,collection.reverse_vec", &CfgSet::unconditional());
    assert_eq!(found, ["collection.reverse_array", "collection.reverse_vec"]);
}

#[test]
fn semantic_noops_reserve_occurrences_for_remaining_call_results() {
    let source = "fn helper() {} fn f() { helper(); { opaque!(); helper(); helper(); } }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    for selector in ["call_result.default", "all"] {
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        let calls = candidates
            .iter()
            .filter(|candidate| candidate.mutator == "call_result.default")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{selector}");
        assert_eq!(
            calls.iter().map(|candidate| candidate.identity_occurrence).collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert!(
            calls
                .iter()
                .all(|candidate| file.slice(&candidate.span) == "helper()" && &*candidate.item_path == "f")
        );
    }
}

#[test]
fn semantic_return_noops_reserve_occurrences_for_shadowed_defaults() {
    let source = "fn f() -> Vec<u8> { if condition { return Vec::new(); } { use custom::Default; return Vec::new(); } }";
    let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
    for selector in ["return_value.default", "all"] {
        let candidates = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        let returns = candidates
            .iter()
            .filter(|candidate| candidate.mutator == "return_value.default")
            .collect::<Vec<_>>();
        assert_eq!(returns.len(), 1, "{selector}");
        assert_eq!(returns[0].identity_occurrence, Some(1));
        assert_eq!(file.slice(&returns[0].span), "Vec::new()");
    }
}

#[test]
fn unknown_imports_and_custom_allocator_types_do_not_supply_noop_proof() {
    use super::super::defaults::DefaultPaths;
    use super::noop::is_standard_default_constructor;

    for (source, expected, expression) in [
        (
            "use dependency::Vec; fn f() {}",
            syn::parse_quote!(Vec<u8>),
            syn::parse_quote!(Vec::new()),
        ),
        (
            "mod std { pub mod vec { pub struct Vec<T>(T); } }",
            syn::parse_quote!(std::vec::Vec<u8>),
            syn::parse_quote!(std::vec::Vec::new()),
        ),
        (
            "extern crate dependency as std;",
            syn::parse_quote!(std::vec::Vec<u8>),
            syn::parse_quote!(std::vec::Vec::new()),
        ),
        (
            "extern crate dependency as std;",
            syn::parse_quote!(::std::vec::Vec<u8>),
            syn::parse_quote!(::std::vec::Vec::new()),
        ),
        (
            "extern crate dependency as core;",
            syn::parse_quote!(::core::option::Option<u8>),
            syn::parse_quote!(::core::option::Option::default()),
        ),
        ("fn f() {}", syn::parse_quote!(Unknown), syn::parse_quote!(Unknown::new())),
        ("fn f() {}", syn::parse_quote!(Vec<u8>), syn::parse_quote!(Vec::<u8, 0>::new())),
        (
            "fn f() {}",
            syn::parse_quote!(Vec<u8>),
            syn::parse_quote!(Vec::<u8, Item = u8>::new()),
        ),
        (
            "fn f() {}",
            syn::parse_quote!(Vec<u8>),
            syn::parse_quote!(Vec::<u8, Item: Copy>::new()),
        ),
        (
            "fn f() {}",
            syn::parse_quote!(Vec<u8>),
            syn::parse_quote!(Vec::<u8, 'static>::new()),
        ),
        (
            "fn f() {}",
            syn::parse_quote!(Option<u8>),
            syn::parse_quote!(Option::<Item = u8>::default()),
        ),
        (
            "fn f() {}",
            syn::parse_quote!(Vec<u8, CustomAllocator>),
            syn::parse_quote!(Vec::new()),
        ),
        (
            "use std::collections::BTreeMap; fn f() {}",
            syn::parse_quote!(BTreeMap<u8, u8, CustomAllocator>),
            syn::parse_quote!(BTreeMap::new()),
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.to_owned()).unwrap();
        let selection = Selection::everything();
        let cfg = CfgSet::unconditional();
        let defaults = Defaults::of(&file.ast);
        let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
        assert!(
            !is_standard_default_constructor(&expression, &expected, &collector.types(), &DefaultPaths::of(&file.ast)),
            "{source}"
        );
    }
}

#[test]
fn parenthesized_constructor_arguments_do_not_supply_noop_proof() {
    use super::types::constructor_argument_count;

    let mut path: syn::Path = syn::parse_quote!(std::vec::Vec);
    path.segments.last_mut().unwrap().arguments = syn::PathArguments::Parenthesized(syn::parse_quote!((u8)));
    assert_eq!(constructor_argument_count(&path), None);
    let mut path: syn::Path = syn::parse_quote!(std::vec::Vec);
    path.segments.first_mut().unwrap().arguments = syn::PathArguments::AngleBracketed(syn::parse_quote!(<u8>));
    assert_eq!(constructor_argument_count(&path), None);
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

        assert!(found.is_empty(), "{source}: {found:?}");
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

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn only_standard_default_bounds_make_generic_fallbacks_available() {
    let standard = "use core::default::Default as StdDefault; fn f<T: StdDefault>() -> &'static mut T { g() }";
    let custom = "mod custom { pub trait Default {} } fn f<T: custom::Default>() -> &'static mut T { g() }";
    let aliased_custom = "mod custom { pub trait Default {} } use custom::Default as Alias; fn f<T: Alias>() -> &'static mut T { g() }";

    assert!(mutators(standard, "fn_value", &CfgSet::unconditional()).is_empty());
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
        ("fn f(v: Vec<u8>) -> Option<&u8> { v.last() }", "iter.last_to_first", "v.first()"),
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
        // No automatic replacement may retain one fresh allocation per invocation.
        ("&'static mut str", &[]),
        ("&mut str", &[]),
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
        ("&'static [u8]", &["fn_value.empty_collection=&[]"]),
        ("&'static u8", &[]),
        ("&'static mut u8", &[]),
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
        ("Cow<'static, str>", &[]),
        (
            "Option<u8>",
            &["fn_value.none=None", "fn_value.some=Some(0)", "fn_value.some=Some(1)"],
        ),
        ("Result<u8, E>", &["fn_value.ok=Ok(0)", "fn_value.ok=Ok(1)"]),
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

#[test]
fn a_zero_width_site_offers_nothing() {
    let file = SourceFile::parse("test.rs", "fn f() {}".to_owned()).unwrap();
    let selection = Selection::parse("relational.eq_to_ne").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.emit_at(
        "relational.eq_to_ne",
        0..0,
        "!=",
        0,
        crate::ops::collect::Shape::Expr,
        crate::ops::collect::Confidence::Proven,
    );

    assert!(collector.finish().is_empty());
}

#[test]
fn parameter_shadowing_skips_const_functions_and_reference_patterns() {
    for source in [
        "const fn f(value: usize) -> usize { value }",
        "fn f(ref value: usize) -> usize { *value }",
    ] {
        let found = mutators(source, "parameter.default_shadow", &CfgSet::unconditional());

        assert!(found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn mutators_require_positive_type_and_source_shape_evidence() {
    let cfg = CfgSet::unconditional();
    // Each case omits one source-visible fact the collector requires before emitting a candidate:
    // expression position, direct defaultability, iterator shape, an unqualified local signature,
    // or an array whose guard preserves temporary lifetimes.
    let cases = [
        (
            "struct Flags { applied: bool } fn f(applied: bool) -> Flags { Flags { applied } }",
            "bool_expr.negate",
        ),
        (
            "enum Placement { Start } fn f(value: Option<Placement>, placement: Placement) -> Placement { value.unwrap_or(placement) }",
            "fallback.unwrap_or_to_default",
        ),
        (
            "fn f(value: Option<u8>) -> Option<u8> { value.and_then(Some).filter(|value| *value > 0) }",
            "iter.remove_filter",
        ),
        (
            "fn remove_file() -> usize { 1 } fn f(path: &std::path::Path) { let _ = std::fs::remove_file(path); }",
            "call.replace_with_default,call_result.default",
        ),
        (
            r#"fn f() { for _ in [("first", ["a", "b"].as_slice()), ("second", ["c", "d"].as_slice())] {} }"#,
            "collection.reverse_array",
        ),
    ];

    for (source, selection) in cases {
        let found = mutators(source, selection, &cfg);

        assert!(found.is_empty(), "{selection}: {source}: {found:?}");
    }
}

#[test]
fn mutators_remain_available_with_positive_evidence() {
    // The paired fixture supplies those facts explicitly, proving that the conservative gates
    // withhold only unresolved cases rather than disabling the mutator families.
    for (source, selection, expected) in [
        (
            "#[derive(Default)] struct Flags { applied: bool } fn fields(applied: bool) -> Flags { Flags { applied: applied } }",
            "bool_expr.negate",
            vec![("bool_expr.negate", "!(applied)", "applied")],
        ),
        (
            "fn fallback(value: Option<usize>, other: usize) -> usize { value.unwrap_or(other) }",
            "fallback.unwrap_or_to_default",
            vec![(
                "fallback.unwrap_or_to_default",
                "(value).unwrap_or_default()",
                "value.unwrap_or(other)",
            )],
        ),
        (
            "fn filtered(value: impl Iterator<Item = usize>) -> impl Iterator<Item = usize> { value.filter(|value| *value > 0) }",
            "iter.remove_filter",
            vec![("iter.remove_filter", "value", "value.filter(|value| *value > 0)")],
        ),
        (
            "fn local() -> usize { 1 } fn calls() -> usize { local() }",
            "call.replace_with_default,call_result.default",
            vec![
                ("call.replace_with_default", "Default::default()", "local()"),
                ("call_result.default", "::gamma_rt::default_after(local())", "local()"),
            ],
        ),
        (
            "fn arrays() { let _ = [1, 2]; }",
            "collection.reverse_array",
            vec![("collection.reverse_array", "[2, 1]", "[1, 2]")],
        ),
    ] {
        let found = collect_in(
            &SourceFile::parse("test.rs", source.to_owned()).unwrap(),
            &Selection::parse(selection).unwrap(),
            &CfgSet::unconditional(),
        );
        let found: Vec<_> = found
            .iter()
            .map(|candidate| (candidate.mutator, candidate.replacement.as_str(), &source[candidate.span.clone()]))
            .collect();

        assert_eq!(found, expected, "{selection}: {source}");
    }
}

#[test]
fn default_replacements_are_not_offered_for_values_already_at_their_default() {
    for source in [
        "fn f(value: Option<bool>) -> bool { value.unwrap_or(false) }",
        "fn f(value: Option<bool>) -> bool { value.unwrap_or_else(|| false) }",
        "fn f(value: Option<bool>) -> bool { value.map_or(false, |value| value) }",
        "fn f(value: Option<bool>) -> bool { value.map_or_else(|| false, |value| value) }",
        "fn f() -> bool { return false; }",
        "fn f() -> bool { return bool::default(); }",
        "fn f() -> Option<bool> { return None; }",
        "fn f() -> Option<bool> { return Option::default(); }",
        "fn f() -> String { return String::new(); }",
        "fn f() -> String { return String::default(); }",
    ] {
        let found = mutators(
            source,
            "fallback.unwrap_or_to_default,fallback.unwrap_or_else_to_default,fallback.map_or_to_default,fallback.map_or_else_to_default,return_value.default",
            &CfgSet::unconditional(),
        );

        assert!(found.is_empty(), "{source}: {found:?}");
    }

    let found = mutators(
        "fn f(value: Option<bool>, fallback: bool) -> bool { if fallback { return true; } value.unwrap_or(fallback) }",
        "fallback.unwrap_or_to_default,return_value.default",
        &CfgSet::unconditional(),
    );
    assert!(found.contains(&"fallback.unwrap_or_to_default"), "{found:?}");
    assert!(found.contains(&"return_value.default"), "{found:?}");
}

#[test]
fn negating_floating_point_zero_is_not_offered_as_a_mutant() {
    let found = mutators(
        "fn f() { let _ = 0.0; let _ = 0.00_f64; let _ = 1.5; }",
        "literal.float_negate",
        &CfgSet::unconditional(),
    );

    assert_eq!(found, vec!["literal.float_negate"]);
}

#[test]
fn call_and_regex_semantics_require_their_exact_source_shapes() {
    for source in [
        "fn f() { (|| 1)(); }",
        "fn f(pattern: &str) { Regex::new(pattern); }",
        r#"fn f() { Regex::new("["); }"#,
    ] {
        let found = mutators(
            source,
            "call.replace_with_default,call_result.default,regex",
            &CfgSet::unconditional(),
        );

        assert!(found.is_empty(), "{source}: {found:?}");
    }
}

#[test]
fn result_variant_mutations_reuse_copy_safe_parameter_payloads() {
    let ok = mutators(
        "fn f(error: &'static str) -> Result<u8, &'static str> { Ok(1) }",
        "result.ok_to_err",
        &CfgSet::unconditional(),
    );
    let err = mutators(
        "fn f(value: &'static str) -> Result<&'static str, u8> { Err(1) }",
        "result.err_to_ok",
        &CfgSet::unconditional(),
    );

    assert_eq!(ok, vec!["result.ok_to_err"]);
    assert_eq!(err, vec!["result.err_to_ok"]);
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

#[test]
fn standalone_collection_stays_optimistic_about_cross_file_default_impls() {
    let file = SourceFile::parse("test.rs", "struct Error; fn f() -> Result<u8, Error> { Ok(1) }".to_owned()).expect("source parses");
    let selection = Selection::parse("result.ok_to_err").expect("selection parses");
    let found: Vec<_> = collect_in(&file, &selection, &CfgSet::unconditional())
        .into_iter()
        .map(|candidate| candidate.mutator)
        .collect();

    assert_eq!(found, ["result.ok_to_err"]);
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

    // Local but undeclared here, so there is no positive `Default` evidence.
    let local = mutators(
        "mod b { use crate::Error; pub fn g() -> Result<u8, Error> { Ok(0) } }",
        "fn_value.err_default",
        &CfgSet::unconditional(),
    );

    assert!(
        local.is_empty(),
        "an unknown workspace error type has no positive `Default` evidence"
    );
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

#[test]
fn a_float_method_result_is_perturbed_as_a_number() {
    let source = "fn f(value: f64) { consume(value.sqrt()); }\n";
    let file = SourceFile::parse("test.rs", source.to_owned()).unwrap();
    let selection = Selection::parse("expr.increment,expr.decrement").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of_in(&file.ast, &cfg);
    let found = collect_with(&file, &selection, &cfg, &defaults);

    assert!(found.iter().any(|candidate| {
        candidate.mutator == "expr.increment"
            && candidate.replacement == "(value.sqrt()) + 1.0"
            && candidate.confidence == crate::ops::collect::Confidence::Proven
    }));
    assert!(found.iter().any(|candidate| {
        candidate.mutator == "expr.decrement"
            && candidate.replacement == "(value.sqrt()) - 1.0"
            && candidate.confidence == crate::ops::collect::Confidence::Proven
    }));
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

#[test]
fn subtraction_is_known_to_be_numeric() {
    let source = "fn f(a: i32, b: i32) { g(a - b); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert_eq!(found, vec!["expr.decrement", "expr.increment"], "{found:?}");
}

#[test]
fn an_operator_on_a_written_temporal_type_is_not_assumed_numeric() {
    let source = "use std::time::Duration; fn f(a: Duration, b: Duration) { g(a - b); }\n";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
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
fn omit_elements_skips_only_an_element_with_an_unreadable_span() {
    let file = SourceFile::parse("test.rs", "fn f() { vec![1, 2, 3]; }".to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Macro(statement) = &function.block.stmts[0] else {
        panic!("test source should contain a macro statement");
    };
    let node = statement.mac.clone();
    let mut elements = node.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated).unwrap();
    elements[1] = syn::parse_quote!(outside);
    let selection = Selection::parse("collection.omit_element").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.omit_elements(&node, &elements);

    assert_eq!(collector.finish().len(), 2);
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
fn rename_method_refuses_call_text_past_the_file() {
    let mut file = SourceFile::parse("test.rs", "fn f(values: &[u8]) { values.first(); }".to_owned()).unwrap();
    let Item::Fn(function) = &file.ast.items[0] else {
        panic!("test source should contain a function");
    };
    let Stmt::Expr(Expr::MethodCall(node), _) = &function.block.stmts[0] else {
        panic!("test source should contain a method call");
    };
    let node = node.clone();
    file.text.truncate(node.span().byte_range().end - 1);
    let selection = Selection::parse("iter.first_to_last").unwrap();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::default();
    let mut collector = Collector::new(&file, &selection, selection.errors(), &cfg, &defaults);

    collector.rename_method(&node, "first");

    assert!(collector.finish().is_empty());
}

#[test]
fn typed_and_inferred_let_else_divergence_repairs_are_each_visited() {
    for (source, expected) in [
        (
            "fn f(value: Option<usize>) { loop { let Some(index): Option<usize> = value else { continue; }; consume(index); } }",
            "loop.continue_to_break",
        ),
        (
            "fn f(value: usize, values: &[u8]) { loop { let index = value else { continue; }; consume(values[index]); } }",
            "expr.increment",
        ),
    ] {
        let found = mutators(source, "loop.continue_to_break,expr.increment", &CfgSet::unconditional());
        assert!(found.contains(&expected), "{source}: expected {expected}: {found:?}");
    }
}

#[test]
fn comparison_traversal_respects_unsigned_evidence_on_each_side() {
    let source = "fn f(left: usize, right: i32) { let _ = left < right; let _ = right < left; }";
    let found = mutators(source, "expr.increment,expr.decrement", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn value_required_if_branches_keep_their_trailing_continue() {
    let source = "fn f(flag: bool) { loop { let _ = if flag { 1 } else { continue; }; } }";
    let found = mutators(source, "loop.delete_continue", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn tuple_struct_fields_and_as_slice_arguments_are_visited() {
    let source = "struct Pair(usize); fn f(value: usize) { let _ = Pair { 0: value }; let _ = [value].as_slice(value + 1); }";
    let found = mutators(source, "expr.increment,arith.add_to_sub", &CfgSet::unconditional());

    assert!(!found.is_empty(), "{found:?}");
}

#[test]
fn calls_with_unknown_or_undefaultable_returns_are_not_replaced() {
    let source = "fn borrowed() -> &'static str { \"value\" } fn f() { <usize>::default(); borrowed(); }";
    let found = mutators(source, "call.replace_with_default,call_result.default", &CfgSet::unconditional());

    assert!(found.is_empty(), "{found:?}");
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
