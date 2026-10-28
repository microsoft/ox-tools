// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZeroU8;
use core::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use syn::parse::Parser as _;
use syn::spanned::Spanned as _;
use syn::token::Group;
use syn::{
    Arm, Expr, ExprBinary, ExprBlock, ExprGroup, File, GenericArgument, Generics, ImplItem, Item, Pat, Path, PathArguments, ReturnType,
    Signature, Stmt, TraitItem, Type, TypeGroup, TypeTuple, parse_file, parse_quote, parse_str,
};

use super::calls::{Calls, OperatorResult};
use super::indexes::{ScopePath, block_id, indexes_in, module_id};
use super::resource::{Binding, Primitive};
use super::{Collector, lexical, temporaries};
use crate::HashMap;
use crate::cfg::CfgSet;
use crate::ops::collect::{Confidence, Defaults, collect_with};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

const ROOT: ScopePath = Vec::new();

#[test]
fn inferred_open_options_and_slice_iterators_follow_reference_chains_and_reject_wrong_arity() {
    with_collector("", |collector| {
        for (name, ty) in [
            ("options", parse_quote!(&mut &mut std::fs::OpenOptions)),
            ("foreign", parse_quote!(dependency::OpenOptions)),
            ("vector", parse_quote!(&Vec<char>)),
            ("slice", parse_quote!(&[bool])),
            ("array", parse_quote!([u8; 3])),
            ("incomplete", parse_quote!(std::slice::Iter)),
        ] {
            collector.bind_type(name.into(), ty);
        }
        for method in ["read", "write", "append", "truncate", "create", "create_new"] {
            let expression = parse_str(&format!("options.{method}(true)")).unwrap();
            assert_eq!(
                collector.inferred_expression_type(&expression),
                Some(parse_quote!(&mut std::fs::OpenOptions))
            );
            for arguments in ["", "true, false"] {
                let expression = parse_str(&format!("options.{method}({arguments})")).unwrap();
                assert_eq!(collector.inferred_expression_type(&expression), None);
            }
        }
        for (expression, expected) in [
            (
                "options.read(true).append(false).open(\"input\")",
                Some(parse_quote!(std::result::Result<std::fs::File, std::io::Error>)),
            ),
            ("options.open()", None),
            ("options.unknown(true)", None),
            ("foreign.read(true)", None),
            ("vector.iter()", Some(parse_quote!(std::slice::Iter<'_, char>))),
            ("slice.iter()", Some(parse_quote!(std::slice::Iter<'_, bool>))),
            ("array.iter()", Some(parse_quote!(std::slice::Iter<'_, u8>))),
            ("vector.iter(true)", None),
            ("vector.iter().next()", Some(parse_quote!(std::option::Option<&char>))),
            ("incomplete.next()", None),
        ] {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                expected,
                "{expression}"
            );
        }
    });
}

#[test]
fn constructor_and_pattern_payload_failures_remain_unknown_through_reference_wrappers() {
    with_collector("", |collector| {
        for expression in [
            "Box::new()",
            "Box::new(unknown)",
            "std::boxed::Box::new()",
            "std::sync::Arc::new()",
            "std::sync::Mutex::new()",
            "std::boxed::Box::new(unknown)",
        ] {
            assert_eq!(
                collector.standard_constructor_type(&parse_str(expression).unwrap()),
                None,
                "{expression}"
            );
        }
        for (constructor, expected) in [
            ("Some", "&Result<u8, bool>"),
            ("Ok", "&Option<u8>"),
            ("Err", "&mut Option<u8>"),
            ("std::option::Option::Some", "&&Result<u8, bool>"),
            ("std::result::Result::Ok", "&&Option<u8>"),
            ("std::result::Result::Err", "&mut &mut Option<u8>"),
        ] {
            assert_eq!(
                collector.standard_pattern_payload(&parse_str(constructor).unwrap(), &parse_str(expected).unwrap()),
                None,
                "{constructor}: {expected}"
            );
        }
    });
}

#[test]
fn scalar_literals_reject_invalid_time_operators_and_unknown_right_types_do_not_panic() {
    with_collector("", |collector| {
        collector.bind_type("duration".into(), parse_quote!(std::time::Duration));
        collector.bind_type("scalar".into(), parse_quote!(u32));
        collector.bind_type("unknown_type".into(), parse_quote!(_));
        for expression in ["duration * 2", "duration * 2.0"] {
            assert!(collector.unsupported_operator(&parse_str(expression).unwrap(), "+"), "{expression}");
        }
        for (expression, replacement) in [
            ("duration -= duration", "+="),
            ("duration *= scalar", "/="),
            ("duration /= scalar", "*="),
            ("scalar % scalar", "+"),
            ("scalar %= scalar", "+="),
        ] {
            assert!(
                !collector.unsupported_operator(&parse_str(expression).unwrap(), replacement),
                "{expression}"
            );
        }
        assert_eq!(collector.binary_expression_type(&parse_quote!(scalar + unknown_type)), None);
        assert!(collector.unsupported_operator(&parse_quote!(duration *= scalar), "+"));
    });
    with_collector(
        "struct Value; impl core::ops::Rem for Value { type Output = Self; } impl core::ops::RemAssign for Value {}",
        |collector| {
            collector.bind_type("value".into(), parse_quote!(Value));
            assert!(collector.unsupported_operator(&parse_quote!(value % value), "+"));
            assert!(collector.unsupported_operator(&parse_quote!(value %= value), "+="));
        },
    );
}

#[test]
fn binary_operator_outputs_preserve_the_right_operands_original_duration_path() {
    with_collector("", |collector| {
        collector.bind_type("duration".into(), parse_quote!(core::time::Duration));
        collector.bind_type("scalar".into(), parse_quote!(u32));
        assert_eq!(
            collector.binary_expression_type(&parse_quote!(scalar * duration)),
            Some(parse_quote!(core::time::Duration))
        );
    });
}

#[test]
fn local_unit_operator_absence_is_not_negative_evidence_after_merging_another_source() {
    let file = SourceFile::parse("fixture.rs", "struct Value;".into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::parse("");
    let mut defaults = Defaults::of(&file.ast);
    defaults.absorb(Defaults::of(&parse_file("struct Other;").unwrap()));
    assert!(!defaults.has_single_source_evidence());
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let ty = parse_quote!(Value);
    collector.bind_type("value".into(), ty);
    assert!(
        collector
            .calls
            .unit_operator_absent(&parse_quote!(Value), "+", &collector.scope_path, collector.current_imports())
    );
    assert!(!collector.unsupported_range_unit(&parse_quote!(value), "+"));
}

#[test]
fn followup_lexical_method_and_trait_bodies_respect_cfg_without_losing_active_parameters() {
    let source = "
        struct Owner;
        impl Owner {
            fn visible(live_impl: Unknown) { live_impl - 1; }
            #[cfg(any())] fn hidden(dead_impl: Unknown) { dead_impl - 1; }
        }
        trait Contract {
            fn visible(live_trait: Unknown) { live_trait - 1; }
            #[cfg(any())] fn hidden(dead_trait: Unknown) { dead_trait - 1; }
            fn declaration(declaration_only: Unknown);
        }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    let expected = ["live_impl", "live_trait"]
        .into_iter()
        .flat_map(|name| source.match_indices(name).map(move |(start, _)| start..start + name.len()))
        .collect();
    assert_eq!(evidence.numeric, expected);
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn followup_lexical_cfg_gates_skip_nested_expressions_and_resume_at_later_statements() {
    let source = "
        fn f(dead_stmt: Unknown, dead_expr: Unknown, live: Unknown) {
            #[cfg(any())] let unused = dead_stmt - 1;
            consume(#[cfg(any())] { dead_expr - 1; });
            live - 1;
        }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    assert_eq!(
        evidence.numeric,
        source.match_indices("live").map(|(start, _)| start..start + 4).collect()
    );
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn followup_lexical_match_guards_are_numeric_uses_but_gated_arms_are_not() {
    let source = "
        fn f(input: Unknown) {
            match input {
                value if value - 1 > 0 => value,
                #[cfg(any())] hidden if hidden - 1 > 0 => hidden,
                _ => 0,
            }
        }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    assert_eq!(
        evidence.numeric,
        source.match_indices("value").map(|(start, _)| start..start + 5).collect()
    );
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn followup_lexical_let_else_divergence_reaches_uses_of_outer_bindings() {
    let source = "fn f(value: Unknown) { let Some(other) = input else { value - 1; return; }; }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    assert_eq!(
        evidence.numeric,
        source.match_indices("value").map(|(start, _)| start..start + 5).collect()
    );
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn followup_lexical_nested_field_restoration_uses_place_prefixes_not_matching_leaf_names() {
    for (source, assignment, moved, invalid) in [
        (
            "fn f(mut record: Record) { let saved = record.left.value; record.left = make(); use_value(record.left.value); }",
            "record.left = make()",
            "record.left.value",
            true,
        ),
        (
            "fn f(mut record: Record) { let saved = (record.left.value); record.left = make(); use_value(record.right.value); }",
            "record.left = make()",
            "(record.left.value)",
            false,
        ),
    ] {
        let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
        let site = span(source, assignment);
        assert_eq!(
            evidence.reinitializations[&site]
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>(),
            [moved],
            "{source}"
        );
        assert_eq!(evidence.invalid_ownership.contains_key(&site), invalid, "{source}");
    }
    let source = "fn f(mut record: Record) { let saved = record.left.value; record.right = make(); use_value(record.left.value); }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    assert!(evidence.reinitializations.is_empty());
    assert!(evidence.invalid_ownership.is_empty());
}

#[test]
fn followup_lexical_reinitialization_is_load_bearing_when_the_following_statement_is_the_whole_place() {
    let source = "fn f(mut value: String) -> String { consume(value); value = make(); value }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    let site = span(source, "value = make()");
    assert_eq!(
        evidence.reinitializations[&site]
            .iter()
            .map(|expression| &source[expression.span().byte_range()])
            .collect::<Vec<_>>(),
        ["value"]
    );
    assert_eq!(
        evidence.invalid_ownership[&site]
            .iter()
            .map(|expression| &source[expression.span().byte_range()])
            .collect::<Vec<_>>(),
        ["value"]
    );
    assert_eq!(
        evidence.ownership_regions.get(&site),
        Some(&span(source, "{ consume(value); value = make(); value }"))
    );
}

#[test]
fn followup_lexical_nested_functions_and_closures_restore_outer_loop_and_move_context() {
    for nested in ["fn nested() {}", "let callback = || {};"] {
        let body = format!("loop {{ consume(value); {nested} value = make(); }}");
        let source = format!("fn f(mut value: String) {{ {body} }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::parse(""));
        let site = span(&source, "value = make()");
        assert_eq!(
            evidence.reinitializations[&site]
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>(),
            ["value"],
            "{nested}"
        );
        assert_eq!(evidence.ownership_regions.get(&site), Some(&span(&source, &body)), "{nested}");
        assert_eq!(
            evidence.invalid_ownership[&site]
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>(),
            ["value"],
            "{nested}"
        );
    }
}

#[test]
fn followup_lexical_nested_callable_reinitializations_do_not_inherit_outer_loop_regions() {
    for nested in [
        "fn nested(mut value: String) { consume(value); value = make(); }",
        "let callback = |mut value: String| { consume(value); value = make(); };",
    ] {
        let source = format!("fn f() {{ loop {{ {nested} break; }} }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::parse(""));
        let site = span(&source, "value = make()");
        assert_eq!(
            evidence.ownership_regions.get(&site),
            Some(&span(&source, "{ consume(value); value = make(); }")),
            "{nested}"
        );
        assert_eq!(
            evidence.reinitializations[&site]
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>(),
            ["value"],
            "{nested}"
        );
        assert!(evidence.invalid_ownership.is_empty(), "{nested}");
        assert!(evidence.invalid_transitions.is_empty(), "{nested}");
    }
}

#[test]
fn followup_call_operator_proofs_distinguish_rhs_arguments_and_unrelated_operations() {
    with_calls(
        "struct Left; struct Wrapper<T>(T);
         impl core::ops::Add<Wrapper<u8>> for Left { type Output = u16; }
         impl core::ops::Sub<(u8, bool)> for Left { type Output = u32; }",
        |_, calls, imports| {
            for (rhs, operation, expected) in [
                (parse_quote!(Wrapper<u8>), "+", parse_quote!(u16)),
                (parse_quote!(crate::Wrapper<u8>), "+", parse_quote!(u16)),
                (parse_quote!((u8, bool)), "-", parse_quote!(u32)),
            ] {
                let OperatorResult::Output(actual) = calls.operator_result(&parse_quote!(Left), &rhs, operation, &ROOT, imports) else {
                    panic!("the fixture declares this exact operator signature");
                };
                assert_eq!(actual, &expected, "{rhs:?} {operation}");
            }
            assert!(matches!(
                calls.operator_result(&parse_quote!(Left), &parse_quote!(Wrapper<bool>), "+", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(matches!(
                calls.operator_result(&parse_quote!(Left), &parse_quote!((bool, u8)), "-", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(!calls.unit_operator_absent(&parse_quote!(Left), "+", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&parse_quote!(Left), "-", &ROOT, imports));
            assert!(calls.unit_operator_absent(&parse_quote!(Left), "*", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&parse_quote!(Unknown), "*", &ROOT, imports));
        },
    );
}

#[test]
fn followup_inherent_and_trait_methods_cannot_replace_free_function_signatures() {
    with_calls(
        "fn shared() -> bool { true }
         struct Owner; impl Owner { fn shared(&self) -> u8 { 1 } }
         struct Other; trait Contract { fn shared(&self) -> u16; fn trait_only(&self); }
         impl Contract for Other {
             fn shared(&self) -> u16 { 2 }
             fn trait_only(&self) {}
         }",
        |_, calls, imports| {
            for expression in [parse_quote!(shared), parse_quote!(crate::shared)] {
                assert_eq!(
                    calls.signature(&expression, &ROOT, imports, None, |_| false).map(output),
                    Some(parse_quote!(bool))
                );
            }
            let signature = calls.method_signature(&parse_quote!(Owner), "shared", &ROOT, imports).unwrap();
            assert_eq!(output(signature), parse_quote!(u8));
            assert_eq!(Calls::parameters(signature, &CfgSet::parse("")), [parse_quote!(&crate::Owner)]);
            assert!(calls.method_signature(&parse_quote!(Other), "shared", &ROOT, imports).is_none());
            assert!(
                calls
                    .signature(&parse_quote!(crate::trait_only), &ROOT, imports, None, |_| false)
                    .is_none()
            );
            assert!(calls.has_method_named("trait_only"));
        },
    );
}

#[test]
fn followup_nested_impls_restore_outer_method_owner_and_generic_parameters() {
    with_calls(
        "struct Outer<T>(T); struct Inner<U>(U);
         impl<T> Outer<T> {
             fn before(&self) {
                 impl<U> Inner<U> { fn nested(&self, value: U) -> U { value } }
             }
             fn after(&self, value: T) -> T { value }
         }",
        |_, calls, imports| {
            let signature = calls.method_signature(&parse_quote!(Outer<u8>), "after", &ROOT, imports).unwrap();
            assert_eq!(
                Calls::parameters(signature, &CfgSet::parse("")),
                [parse_quote!(&crate::Outer), parse_quote!(_)]
            );
            assert_eq!(output(signature), parse_quote!(_));
            assert!(calls.method_signature(&parse_quote!(Inner<u8>), "after", &ROOT, imports).is_none());
            assert!(calls.method_signature(&parse_quote!(Inner<u8>), "nested", &ROOT, imports).is_none());
            assert!(calls.has_method_named("nested"));
        },
    );
}

#[test]
fn followup_call_walk_reaches_only_active_impl_and_trait_default_bodies() {
    with_calls(
        "struct Owner;
         impl Owner {
             fn active() { fn helper() -> u8 { 1 } }
             #[cfg(any())] fn hidden() { fn helper() -> u16 { 2 } }
         }
         trait Contract {
             fn active() { fn helper() -> u32 { 3 } }
             #[cfg(any())] fn hidden() { fn helper() -> u64 { 4 } }
         }",
        |file, calls, imports| {
            let Item::Impl(implementation) = &file.items[1] else {
                unreachable!()
            };
            for (item, expected) in implementation.items.iter().zip([Some(parse_quote!(u8)), None]) {
                let ImplItem::Fn(method) = item else { unreachable!() };
                let scope = vec![block_id(&method.block)];
                assert_eq!(
                    calls.signature(&parse_quote!(helper), &scope, imports, None, |_| false).map(output),
                    expected,
                    "{}",
                    method.sig.ident
                );
            }
            let Item::Trait(contract) = &file.items[2] else { unreachable!() };
            for (item, expected) in contract.items.iter().zip([Some(parse_quote!(u32)), None]) {
                let TraitItem::Fn(method) = item else { unreachable!() };
                let scope = vec![block_id(method.default.as_ref().unwrap())];
                assert_eq!(
                    calls.signature(&parse_quote!(helper), &scope, imports, None, |_| false).map(output),
                    expected,
                    "{}",
                    method.sig.ident
                );
            }
            assert!(
                calls
                    .signature(&parse_quote!(crate::helper), &ROOT, imports, None, |_| false)
                    .is_none()
            );
        },
    );
}

#[test]
fn followup_call_walk_cfg_gates_local_initializers_and_nested_expression_arguments() {
    with_calls(
        "fn outer() {
             #[cfg(any())] let discarded = { fn hidden_statement() -> u8 { 1 } };
             consume(#[cfg(any())] { fn hidden_expression() -> u16 { 2 } });
             { fn visible() -> u32 { 3 } }
         }",
        |file, calls, imports| {
            let Item::Fn(function) = &file.items[0] else { unreachable!() };
            let Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            let Expr::Block(local_block) = &*local.init.as_ref().unwrap().expr else {
                unreachable!()
            };
            let Stmt::Expr(Expr::Call(call), _) = &function.block.stmts[1] else {
                unreachable!()
            };
            let Expr::Block(argument_block) = &call.args[0] else {
                unreachable!()
            };
            let Stmt::Expr(Expr::Block(visible_block), _) = &function.block.stmts[2] else {
                unreachable!()
            };
            for (block, name, expected) in [
                (local_block, "hidden_statement", None),
                (argument_block, "hidden_expression", None),
                (visible_block, "visible", Some(parse_quote!(u32))),
            ] {
                let scope = vec![block_id(&function.block), block_id(&block.block)];
                assert_eq!(
                    calls
                        .signature(&parse_str(name).unwrap(), &scope, imports, None, |_| false)
                        .map(output),
                    expected,
                    "{name}"
                );
            }
        },
    );
}

#[test]
fn followup_call_scope_annotations_withhold_block_enum_shadows_without_hiding_explicit_root_types() {
    with_calls(
        "mod outer {
             pub struct Item { pub value: u8 }
             impl Item { fn read(&self) -> u8 { self.value } }
             pub enum Choice { A }
             #[transform] pub enum Opaque { A }
             fn f() { enum Item { Local } { inspect(); } }
         }",
        |file, calls, _| {
            let Item::Mod(module) = &file.items[0] else { unreachable!() };
            let Item::Fn(function) = &module.content.as_ref().unwrap().1[4] else {
                unreachable!()
            };
            let Stmt::Expr(Expr::Block(block), _) = &function.block.stmts[1] else {
                unreachable!()
            };
            let module_scope = vec![module_id(&module.ident)];
            let scope = vec![module_id(&module.ident), block_id(&function.block), block_id(&block.block)];
            let mut environments = HashMap::from_iter([(module_scope.clone(), HashMap::default()), (scope.clone(), HashMap::default())]);
            calls.annotate_imports(&mut environments);
            let module_imports = &environments[&module_scope];
            let block_imports = &environments[&scope];
            assert_eq!(
                block_imports,
                &HashMap::from_iter([
                    (super::types::LOCAL_MODULE.into(), Some(vec!["outer".into()])),
                    (super::types::BLOCK_TYPES.into(), Some(vec!["Item".into()])),
                ])
            );
            assert!(calls.structure(&parse_quote!(Item), &module_scope, module_imports));
            assert!(!calls.structure(&parse_quote!(Item), &scope, block_imports));
            assert!(!calls.is_enum(&parse_quote!(Item), &scope, block_imports));
            assert!(
                calls
                    .field(&parse_quote!(Item), &parse_quote!(value), &scope, block_imports)
                    .is_none()
            );
            assert!(calls.method_signature(&parse_quote!(Item), "read", &scope, block_imports).is_none());
            assert_eq!(
                calls.field(&parse_quote!(crate::outer::Item), &parse_quote!(value), &scope, block_imports),
                Some(parse_quote!(u8))
            );
            assert_eq!(
                calls
                    .method_signature(&parse_quote!(crate::outer::Item), "read", &scope, block_imports)
                    .map(output),
                Some(parse_quote!(u8))
            );
            assert!(calls.is_enum(&parse_quote!(Choice), &scope, block_imports));
            assert!(!calls.is_enum(&parse_quote!(Opaque), &scope, block_imports));
        },
    );
}

#[test]
fn campaign_receiver_bindings_preserve_reference_modes_and_later_destructured_parameters() {
    for (signature, receiver) in [
        ("fn method(self, (count, flag): (u16, bool))", parse_quote!(Owner)),
        ("fn method(&self, (count, flag): (u16, bool))", parse_quote!(&Owner)),
        ("fn method(&mut self, (count, flag): (u16, bool))", parse_quote!(&mut Owner)),
        ("fn method(self: Box<Self>, (count, flag): (u16, bool))", parse_quote!(Box<Owner>)),
    ] {
        with_collector("struct Owner;", |collector| {
            collector.impl_self_type = Some(parse_quote!(Owner));
            let signature = parse_str(signature).unwrap();
            collector.in_function(&signature, |collector| {
                assert_eq!(collector.binding_types.len(), 3);
                assert_eq!(collector.binding_types["self"].ty, receiver);
                assert!(collector.binding_types["self"].resource_trusted);
                assert_eq!(collector.type_of_place(&parse_quote!(count)), Some(&parse_quote!(u16)));
                assert_eq!(collector.type_of_place(&parse_quote!(flag)), Some(&parse_quote!(bool)));
                assert!(!collector.binding_types["count"].resource_trusted);
                assert!(!collector.binding_types["flag"].resource_trusted);
            });
            assert!(collector.binding_types.is_empty());
            assert_eq!(collector.impl_self_type, Some(parse_quote!(Owner)));
        });
    }
}

#[test]
fn campaign_nested_functions_restore_floating_evidence_without_inheriting_it() {
    with_collector("", |collector| {
        let _inserted = collector.inferred_floating.insert("outer".into());
        collector.bind_type("outer".into(), parse_quote!(f64));
        let signature = parse_quote!(fn inner(value: u8));
        collector.in_function(&signature, |collector| {
            assert!(!collector.is_known_float(&parse_quote!(outer)));
            assert!(!collector.is_known_float(&parse_quote!(value)));
            let _inserted = collector.inferred_floating.insert("inner".into());
            assert!(collector.is_known_float(&parse_quote!(inner)));
        });
        assert!(collector.is_known_float(&parse_quote!(outer)));
        assert!(!collector.is_known_float(&parse_quote!(inner)));
        assert_eq!(
            collector.inferred_floating.iter().map(String::as_str).collect::<Vec<_>>(),
            ["outer"]
        );
    });
}

#[test]
fn campaign_character_receiver_proofs_resolve_aliases_and_all_reference_layers() {
    with_collector("type Letter = char; type Borrowed = &'static Letter;", |collector| {
        for (ty, expected) in [
            ("char", true),
            ("core::primitive::char", true),
            ("std::primitive::char", true),
            ("&mut &char", true),
            ("Letter", true),
            ("Borrowed", true),
            ("bool", false),
            ("&String", false),
            ("(char,)", false),
            ("unknown::char", false),
        ] {
            collector.bind_type("value".into(), parse_str(ty).unwrap());
            assert_eq!(collector.character_receiver(&parse_quote!(value)), expected, "{ty}");
        }
        assert!(!collector.character_receiver(&parse_quote!(missing)));
    });
    with_collector("struct char;", |collector| {
        collector.bind_type("value".into(), parse_quote!(char));
        assert!(!collector.character_receiver(&parse_quote!(value)));
    });
}

#[test]
fn campaign_direct_call_inference_keeps_nominal_constructors_and_rejects_unqualified_factories() {
    with_collector(
        "struct Packet(u8); mod payload { pub struct Packet(u16); }
         use chrono::{Utc, DateTime, NaiveDateTime};
         use std::time::Duration;",
        |collector| {
            for (call, expected) in [
                ("Packet(1)", Some(parse_quote!(Packet))),
                ("payload::Packet(2)", Some(parse_quote!(payload::Packet))),
                ("factory()", None),
                ("Utc::now()", Some(parse_quote!(chrono::DateTime))),
                ("Utc::other()", None),
                ("DateTime::now()", Some(parse_quote!(chrono::DateTime))),
                ("NaiveDateTime::now()", Some(parse_quote!(chrono::NaiveDateTime))),
                ("Duration::from_secs(2)", Some(parse_quote!(std::time::Duration))),
                ("other::now()", None),
                ("value", None),
            ] {
                assert_eq!(collector.inferred_call_type(&parse_str(call).unwrap()), expected, "{call}");
            }
        },
    );
    with_collector("use chrono::Other;", |collector| {
        assert_eq!(collector.inferred_call_type(&parse_quote!(Other::now())), None);
    });
}

#[test]
fn campaign_lexical_numeric_operators_do_not_turn_signed_evidence_into_unsigned_evidence() {
    for operator in ["-", "*", "/", "%", "-=", "*=", "/=", "%="] {
        let source = format!("fn f(left: Unknown, right: Unknown) {{ left {operator} right; }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::unconditional());
        let expected = ["left", "right"]
            .into_iter()
            .flat_map(|name| source.match_indices(name).map(move |(start, _)| start..start + name.len()))
            .collect::<crate::HashSet<_>>();
        assert_eq!(evidence.numeric, expected, "{operator}");
        assert!(evidence.unsigned.is_empty(), "{operator}");
    }
    for expression in [
        "left + right",
        "left += right",
        "left == right",
        "left != right",
        "left & right",
        "left << right",
    ] {
        let source = format!("fn f(left: Unknown, right: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::unconditional());
        assert!(evidence.numeric.is_empty(), "{expression}");
        assert!(evidence.unsigned.is_empty(), "{expression}");
    }
}

#[test]
fn campaign_lexical_integer_comparisons_prove_either_operand_but_not_equality() {
    for expression in [
        "value + 1",
        "1 + value",
        "value += 1",
        "value < 1",
        "1 < value",
        "value <= 1",
        "1 <= value",
        "value > 1",
        "1 > value",
        "value >= 1",
        "1 >= value",
        "1 + (&(value))",
    ] {
        let source = format!("fn f(value: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::unconditional());
        assert_eq!(
            evidence.numeric,
            source.match_indices("value").map(|(start, _)| start..start + 5).collect(),
            "{expression}"
        );
        assert!(evidence.unsigned.is_empty(), "{expression}");
    }
    for expression in ["value == 1", "1 != value", "value + \"text\"", "value < other"] {
        let source = format!("fn f(value: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::unconditional());
        assert!(evidence.numeric.is_empty(), "{expression}");
        assert!(evidence.unsigned.is_empty(), "{expression}");
    }
}

#[test]
fn campaign_lexical_index_ranges_prove_both_endpoints_without_marking_the_container() {
    for expression in ["values[start..end]", "values[..end]", "values[start..]", "values[(start)..=(end)]"] {
        let source = format!("fn f(values: Unknown, start: Unknown, end: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::unconditional());
        let endpoints: &[&str] = match expression {
            "values[..end]" => &["end"],
            "values[start..]" => &["start"],
            _ => &["start", "end"],
        };
        let expected = endpoints
            .iter()
            .flat_map(|name| source.match_indices(name).map(move |(start, _)| start..start + name.len()))
            .collect::<crate::HashSet<_>>();
        assert_eq!(evidence.numeric, expected, "{expression}");
        assert_eq!(evidence.unsigned, evidence.numeric, "{expression}");
    }
}

#[test]
fn campaign_call_signatures_keep_imported_module_suffixes_and_absolute_provenance() {
    with_calls(
        "use dependency::types as imported;
         use ::dependency::other as absolute;
         fn make(value: &imported::Wrapper<u16>) -> (imported::Item, absolute::Item) { todo!() }
         fn associated() -> <imported::Owner as Trait>::Item { todo!() }",
        |_, calls, imports| {
            let signature = calls.signature(&parse_quote!(make), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(
                Calls::parameters(signature, &CfgSet::unconditional()),
                [parse_quote!(&dependency::types::Wrapper<u16>)]
            );
            assert_eq!(
                output(signature),
                parse_quote!((dependency::types::Item, ::dependency::other::Item))
            );
            let signature = calls.signature(&parse_quote!(associated), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(output(signature), parse_quote!(<imported::Owner as Trait>::Item));
        },
    );
}

#[test]
fn campaign_call_signatures_erase_ambiguous_relative_roots_but_not_absolute_paths() {
    with_calls(
        "use first::std; use second::std;
         fn relative() -> std::primitive::u8 { todo!() }
         fn absolute() -> ::std::primitive::u8 { todo!() }",
        |_, calls, imports| {
            for (name, expected) in [("relative", parse_quote!(_)), ("absolute", parse_quote!(::std::primitive::u8))] {
                let signature = calls.signature(&parse_str(name).unwrap(), &ROOT, imports, None, |_| false).unwrap();
                assert_eq!(output(signature), expected, "{name}");
            }
        },
    );
}

#[test]
fn campaign_character_casing_candidates_keep_exact_iterator_shape_and_identity() {
    for (source, shape) in [
        (
            "fn f(value: &core::primitive::char) { value.to_lowercase(); }",
            super::Shape::IterExpr,
        ),
        (
            "fn f(value: &std::primitive::char) { value.to_lowercase(); }",
            super::Shape::IterExpr,
        ),
        (
            "type Letter = char; fn f(value: &Letter) { value.to_lowercase(); }",
            super::Shape::IterExpr,
        ),
        ("fn f(value: String) { value.to_lowercase(); }", super::Shape::Expr),
        ("struct char; fn f(value: char) { value.to_lowercase(); }", super::Shape::Expr),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let candidates = collect_with(
            &file,
            &Selection::parse("string.lower_to_upper").unwrap(),
            &CfgSet::unconditional(),
            &Defaults::of(&file.ast),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (
                    candidate.mutator,
                    file.slice(&candidate.span),
                    candidate.replacement.as_str(),
                    candidate.replacement_index,
                    candidate.identity_occurrence,
                    candidate.item_path.as_ref(),
                    candidate.shape,
                    candidate.confidence,
                ))
                .collect::<Vec<_>>(),
            [(
                "string.lower_to_upper",
                "value.to_lowercase()",
                "value.to_uppercase()",
                0,
                Some(0),
                "f",
                shape,
                Confidence::Proven,
            )],
            "{source}"
        );
    }
}

#[test]
fn campaign_regex_candidates_keep_nonzero_replacement_indices_under_narrow_selection() {
    let source = "fn f() { Regex::new(\"^a+[bc]?$\"); }";
    for (selector, expected) in [
        (
            "regex",
            vec![
                ("regex.negate_character_class", "\"^a+[^bc]?$\"", 3),
                ("regex.optional_to_required", "\"^a+[bc]$\"", 4),
                ("regex.plus_to_star", "\"^a*[bc]?$\"", 2),
                ("regex.remove_end_anchor", "\"^a+[bc]?\"", 1),
                ("regex.remove_start_anchor", "\"a+[bc]?$\"", 0),
            ],
        ),
        (
            "regex.optional_to_required",
            vec![("regex.optional_to_required", "\"^a+[bc]$\"", 4)],
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let candidates = collect_with(
            &file,
            &Selection::parse(selector).unwrap(),
            &CfgSet::unconditional(),
            &Defaults::of(&file.ast),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.mutator, candidate.replacement.as_str(), candidate.replacement_index))
                .collect::<Vec<_>>(),
            expected,
            "{selector}"
        );
        for candidate in candidates {
            assert_eq!(file.slice(&candidate.span), "\"^a+[bc]?$\"");
            assert_eq!(candidate.shape, super::Shape::Expr);
            assert_eq!(candidate.confidence, Confidence::Proven);
            assert_eq!(candidate.item_path.as_ref(), "f");
            assert_eq!(candidate.identity_occurrence, Some(0));
        }
    }
}

#[test]
fn not_operands_choose_one_replacement_from_available_type_evidence() {
    for (source, replacement, index, confidence) in [
        (
            "fn f(values: &[(u8, bool)]) -> bool { values.iter().any(|(_, flag)| !flag) }",
            "*(flag)",
            1,
            Confidence::Proven,
        ),
        ("fn f() { !value.clone() }", "value.clone()", 0, Confidence::Optimistic),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let candidates = collect_with(
            &file,
            &Selection::parse("unary.remove_not").unwrap(),
            &CfgSet::unconditional(),
            &Defaults::of(&file.ast),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (candidate.replacement.as_str(), candidate.replacement_index, candidate.confidence))
                .collect::<Vec<_>>(),
            [(replacement, index, confidence)],
            "{source}"
        );
    }
}

#[test]
fn standard_unsized_string_wrapper_defaults_remain_break_value_candidates() {
    assert_eq!(Box::<str>::default().as_ref(), "");
    assert_eq!(Rc::<str>::default().as_ref(), "");
    assert_eq!(Arc::<str>::default().as_ref(), "");
    for (source, count) in [
        ("fn f(value: Box<str>) -> Box<str> { loop { break value; } }", 1),
        ("fn f(value: std::rc::Rc<str>) -> std::rc::Rc<str> { loop { break value; } }", 1),
        (
            "fn f(value: std::sync::Arc<str>) -> std::sync::Arc<str> { loop { break value; } }",
            1,
        ),
        (
            "fn f(value: Box<core::primitive::str>) -> Box<core::primitive::str> { loop { break value; } }",
            1,
        ),
        (
            "struct Missing; fn f(value: Box<Missing>) -> Box<Missing> { loop { break value; } }",
            0,
        ),
        (
            "struct Missing; fn f(value: std::rc::Rc<Missing>) -> std::rc::Rc<Missing> { loop { break value; } }",
            0,
        ),
        (
            "struct Missing; fn f(value: std::sync::Arc<Missing>) -> std::sync::Arc<Missing> { loop { break value; } }",
            0,
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let candidates = collect_with(
            &file,
            &Selection::parse("loop.break_value_default").unwrap(),
            &CfgSet::unconditional(),
            &Defaults::of(&file.ast),
        );
        assert_eq!(candidates.len(), count, "{source}: {candidates:?}");
        for candidate in candidates {
            assert_eq!(candidate.replacement, "Default::default()");
            assert_eq!(file.slice(&candidate.span), "value");
        }
    }
}

#[test]
fn review_glob_imports_cannot_prove_aliases_unsigned() {
    let source = "mod aliases { pub type u32 = i32; }
                  use aliases::*;
                  type Number = u32;
                  fn f() -> Number { 0 }";
    with_collector(source, |collector| {
        assert!(
            !collector
                .types()
                .supports_primitive_kind(&parse_quote!(Number), super::values::Kind::Unsigned)
        );
    });
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let candidates = collect_with(&file, &Selection::everything(), &CfgSet::unconditional(), &Defaults::of(&file.ast));
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.mutator == "literal.int_decrement" && candidate.replacement == "-1"),
        "{candidates:?}"
    );
}

#[test]
fn review_forwarded_io_aliases_keep_vector_return_candidates() {
    let source = "type Wrapped<T> = std::io::Result<Vec<T>>;
                  type Forward<T> = Wrapped<T>;
                  fn f() -> Forward<u8> { Ok(vec![1]) }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let candidates = collect_with(
        &file,
        &Selection::parse("fn_value").unwrap(),
        &CfgSet::unconditional(),
        &Defaults::of(&file.ast),
    );
    assert!(
        candidates.iter().any(|candidate| candidate.replacement.contains("Ok(Vec::new())")),
        "{candidates:?}"
    );
    assert!(
        !candidates.iter().any(|candidate| candidate.replacement == "Ok(0)"),
        "{candidates:?}"
    );
}

#[test]
fn review_operator_outputs_compare_primitive_identities_not_spellings() {
    with_collector(
        "struct A;
         type Byte = u8;
         impl core::ops::Add for A { type Output = Byte; }
         impl core::ops::Sub for A { type Output = u8; }
         impl core::ops::Mul for A { type Output = std::primitive::u8; }
         impl core::ops::Div for A { type Output = core::primitive::u8; }
         impl core::ops::Rem for A { type Output = i8; }",
        |collector| {
            collector.bind_type("a".into(), parse_quote!(A));
            collector.bind_type("b".into(), parse_quote!(A));
            for replacement in ["-", "*", "/"] {
                assert!(!collector.unsupported_operator(&parse_quote!(a + b), replacement), "{replacement}");
            }
            assert!(collector.unsupported_operator(&parse_quote!(a + b), "%"));
        },
    );
}

#[test]
fn review_nested_unknown_operator_outputs_are_not_negative_evidence() {
    with_collector(
        "type Wide0 = (u8, u8);
         type Wide1 = (Wide0, Wide0);
         type Wide2 = (Wide1, Wide1);
         type Wide3 = (Wide2, Wide2);
         type Wide4 = (Wide3, Wide3);
         type Wide5 = (Wide4, Wide4);
         type Wide6 = (Wide5, Wide5);
         type Wide7 = (Wide6, Wide6);
         type Wide8 = (Wide7, Wide7);
         type Wide9 = (Wide8, Wide8);
         type Wide10 = (Wide9, Wide9);
         type Wide11 = (Wide10, Wide10);
         type Wide12 = (Wide11, Wide11);
         struct A;
         impl core::ops::Add for A { type Output = Box<Wide12>; }
         impl core::ops::Sub for A { type Output = Box<u8>; }",
        |collector| {
            collector.bind_type("a".into(), parse_quote!(A));
            collector.bind_type("b".into(), parse_quote!(A));
            let output = collector.binary_expression_type(&parse_quote!(a + b)).unwrap();
            assert!(quote::quote!(#output).to_string().contains('_'));
            assert!(!collector.unsupported_operator(&parse_quote!(a + b), "-"));
            assert!(!collector.unsupported_operator(&parse_quote!(a - b), "+"));
        },
    );
}

#[test]
fn ownership_sensitive_continue_does_not_emit_a_break_that_skips_reinitialization() {
    with_collector("fn f() { loop { continue; } }", |collector| {
        collector.bind_type("value".into(), parse_quote!(String));
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Loop(loop_), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Continue(node), _) = &loop_.body.stmts[0] else {
            unreachable!()
        };
        let node = node.clone();
        collector
            .numeric_evidence
            .invalid_transitions
            .insert(node.span().byte_range(), vec![parse_quote!(value)]);
        syn::visit::Visit::visit_expr_continue(collector, &node);
        assert!(collector.candidates.is_empty());
    });
}

#[test]
fn operator_output_canonicalization_handles_invisible_groups_and_rejects_oversized_tuples() {
    for oversized in [false, true] {
        let mut file = parse_file("struct A; impl core::ops::Add for A { type Output = u8; }").unwrap();
        let Item::Impl(implementation) = &mut file.items[1] else {
            unreachable!()
        };
        let ImplItem::Type(output) = &mut implementation.items[0] else {
            unreachable!()
        };
        let expected = if oversized {
            let mut tuple: TypeTuple = parse_quote!(());
            tuple
                .elems
                .extend(core::iter::repeat_n::<Type>(parse_quote!(u8), super::types::TYPE_EXPANSION_LIMIT));
            output.ty = Type::Tuple(tuple);
            parse_quote!(_)
        } else {
            let group = Type::Group(TypeGroup {
                attrs: Vec::new(),
                group_token: Group::default(),
                elem: Box::new(parse_quote!(u8)),
            });
            output.ty = group.clone();
            group
        };
        let cfg = CfgSet::unconditional();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let OperatorResult::Output(actual) = calls.operator_result(&parse_quote!(A), &parse_quote!(A), "+", &ROOT, &indexes.root_imports)
        else {
            panic!("the local addition implementation must remain indexed");
        };
        assert_eq!(actual, &expected);
    }
}

#[test]
fn resource_local_helper_return_provenance_requires_sync_nongeneric_primitives() {
    let source = "
        fn primitive() -> u8 { 0 }
        fn reference() -> &'static u8 { &0 }
        fn opaque() -> Other { make() }
        fn unit() {}
        async fn deferred() -> u8 { 0 }
        fn generic<T>() -> u8 { 0 }
        fn f() {
            let a = primitive();
            let b = reference();
            let c = opaque();
            let d = unit();
            let e = deferred();
            let f = generic();
            while true {}
        }
    ";
    with_collector(source, |collector| {
        let Item::Fn(function) = collector.file.ast.items.last().unwrap() else {
            unreachable!()
        };
        let Stmt::Expr(expression, _) = function.block.stmts.last().unwrap() else {
            unreachable!()
        };
        let mut bindings = HashMap::default();
        collector.resource_source_bindings(expression, &mut bindings);
        assert_eq!(
            bindings.get("a"),
            Some(&super::resource::Binding::Primitive(super::resource::Primitive::Unsigned(8)))
        );
        for unknown in ["b", "c", "d", "e", "f"] {
            assert!(!bindings.contains_key(unknown), "{unknown}");
        }
    });
}

#[test]
fn extending_labeled_resource_evidence_preserves_nominal_type_isolation() {
    let cfg = CfgSet::unconditional();
    let left_file = parse_file("struct Left;").unwrap();
    let right_file = parse_file("struct Right;").unwrap();
    let mut left = Defaults::of_source_in(&left_file, &cfg, "left.rs");
    let right = Defaults::of_source_in(&right_file, &cfg, "right.rs");
    left.extend_resource_evidence(&right);
    left.bind_sources(&[
        ("left.rs".into(), "left.rs".into(), Vec::new()),
        ("right.rs".into(), "right.rs".into(), Vec::new()),
    ]);
    assert!(left.source_defines("left.rs", &[], &["Left".into()]));
    assert!(!left.source_defines("left.rs", &[], &["Right".into()]));
    assert!(!left.source_defines("right.rs", &[], &["Right".into()]));
}

#[test]
fn operator_literal_and_reference_classification_preserves_unknown_trait_worlds() {
    with_collector("", |collector| {
        collector.bind_type("text".into(), parse_quote!(String));
        collector.bind_type("number".into(), parse_quote!(&u32));
        assert!(collector.unsupported_operator(&parse_quote!(text + "suffix"), "-"));
        assert!(!collector.unsupported_operator(&parse_quote!(true + false), "-"));
        assert_eq!(
            collector.binary_expression_type(&parse_quote!(number + number)),
            Some(parse_quote!(&u32))
        );
    });
    with_collector(
        "struct A; impl core::ops::Add for A { type Output = A; } impl core::ops::Mul for A { type Output = A; } impl core::ops::Mul for A { type Output = A; }",
        |collector| {
            collector.bind_type("a".into(), parse_quote!(A));
            assert!(!collector.unsupported_operator(&parse_quote!(a + a), "*"));
        },
    );
}

#[test]
fn network_readers_and_qualified_result_constructors_keep_standard_api_identity() {
    with_collector("use std::io::Read;", |collector| {
        for (name, ty) in [
            ("tcp", parse_quote!(std::net::TcpStream)),
            ("unix", parse_quote!(std::os::unix::net::UnixStream)),
        ] {
            collector.bind_type(name.into(), ty);
            assert!(collector.is_read_receiver(&parse_str(name).unwrap()));
        }
        assert!(collector.some_constructor(&grouped(parse_quote!(Some))));
        assert_eq!(
            collector.standard_constructor_type(&parse_quote!(core::result::Result::Ok(true))),
            Some(parse_quote!(std::result::Result<_, _>))
        );
        assert!(
            collector
                .standard_constructor_type(&parse_quote!(<Owner as Trait>::make()))
                .is_none()
        );
    });
    with_collector("#![no_implicit_prelude]", |collector| {
        assert_eq!(
            collector.standard_constructor_type(&parse_quote!(std::result::Result::Err(false))),
            Some(parse_quote!(std::result::Result<_, _>))
        );
    });
}

#[test]
fn call_identity_resolution_rejects_absolute_and_block_shadowed_owners() {
    with_calls(
        "struct A; mod child { struct B; } impl core::ops::Add<Self> for A { type Output = A; }",
        |file, calls, imports| {
            let mut shadowed = imports.clone();
            shadowed.insert(super::types::BLOCK_TYPES.into(), Some(vec!["A".into()]));
            assert!(!calls.has_noncopy_field(&parse_quote!(A), &ROOT, &shadowed, |_| true));
            assert!(!calls.unit_operator_absent(&parse_quote!(A), "+", &ROOT, &shadowed));
            assert!(matches!(
                calls.operator_result(&parse_quote!(A), &parse_quote!(A), "+", &ROOT, &shadowed),
                OperatorResult::Unknown
            ));
            assert!(matches!(
                calls.operator_result(&parse_quote!(A), &parse_quote!(&A), "+", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(matches!(
                calls.operator_result(&parse_quote!(A), &parse_quote!(A), "+", &ROOT, imports),
                OperatorResult::Output(_)
            ));
            let Item::Mod(child) = &file.items[1] else { unreachable!() };
            let child = vec![module_id(&child.ident)];
            assert!(calls.structure(&parse_quote!(super::A), &child, imports));
            let mut unknown_scopes = HashMap::from_iter([(vec![(usize::MAX, usize::MAX)], HashMap::default())]);
            calls.annotate_imports(&mut unknown_scopes);
            assert!(
                unknown_scopes
                    .values()
                    .all(|imports| imports.contains_key(super::types::LOCAL_MODULE))
            );
        },
    );
    for source in [
        "struct A; impl core::ops::Add for &A { type Output = A; }",
        "struct A; impl core::ops::Add for ::external::A { type Output = A; }",
    ] {
        with_calls(source, |_, calls, imports| {
            assert!(!calls.structure(&parse_quote!(::A), &ROOT, imports));
        });
    }
}

#[test]
fn lexical_consuming_methods_are_moves_but_their_receiver_remains_reborrowable() {
    for method in [
        "into_iter",
        "into_bytes",
        "into_boxed_str",
        "into_string",
        "into_os_string",
        "into_vec",
    ] {
        let source = format!("fn f(value: String) {{ consume(value.{method}()); }}");
        let file = parse_file(&source).unwrap();
        let evidence = lexical::evidence(&file, &CfgSet::unconditional());
        let start = source.find("value.").unwrap();
        assert!(evidence.reborrowable.contains(&(start..start + "value".len())), "{method}");
    }
}

#[test]
fn temporal_constructors_and_reference_receivers_preserve_local_return_types() {
    with_collector(
        "use std::time::Duration; struct Owner; impl Owner { fn value(&self) -> Self { Owner } }",
        |collector| {
            collector.bind_type("owner".into(), parse_quote!(&Owner));
            assert_eq!(
                collector.inferred_expression_type(&parse_quote!(owner.value())),
                Some(parse_quote!(crate::Owner))
            );
            assert_eq!(
                collector.inferred_call_type(&parse_quote!(Duration::from_secs(2))),
                Some(parse_quote!(std::time::Duration))
            );
        },
    );
}

#[test]
fn method_inference_advances_each_reference_before_resolving_the_result() {
    with_collector("struct Owner; impl Owner { fn value(&self) -> Self { Owner } }", |collector| {
        collector.bind_type("owner".into(), parse_quote!(&Owner));
        assert_eq!(
            collector.inferred_method_type_with(&parse_quote!(owner.value()), || {
                panic!("recorded local signatures already have canonical owner types");
            }),
            Some(parse_quote!(crate::Owner))
        );
    });
    with_collector("", |collector| {
        collector.bind_type("options".into(), parse_quote!(&mut &mut std::fs::OpenOptions));
        let mut visits = 0;
        assert_eq!(
            collector.inferred_method_type_with(&parse_quote!(options.read(true)), || {
                visits += 1;
                assert!(visits <= 2, "the receiver has exactly two reference layers");
            }),
            Some(parse_quote!(&mut std::fs::OpenOptions))
        );
        assert_eq!(visits, 2);
    });
}

#[test]
fn ownership_places_follow_fields_tuples_and_parentheses_but_not_generic_or_imported_records() {
    with_collector("struct Record { value: String } struct Tuple(String);", |collector| {
        collector.bind_type("record".into(), parse_quote!(Record));
        collector.bind_type("tuple".into(), parse_quote!(Tuple));
        collector.bind_type("pair".into(), parse_quote!((u8, String)));
        for (expression, expected) in [
            ("record.value", Some(parse_quote!(String))),
            ("(record.value)", Some(parse_quote!(String))),
            ("tuple.0", Some(parse_quote!(String))),
            ("(pair).1", Some(parse_quote!(String))),
            ("pair.5", None),
            ("pair.value", None),
        ] {
            assert_eq!(
                collector.ownership_type(&parse_str(expression).unwrap()),
                expected.as_ref(),
                "{expression}"
            );
        }
        for name in [
            "std::fs::File",
            "std::path::PathBuf",
            "std::ffi::OsString",
            "std::process::Child",
            "std::process::Command",
            "std::process::Stdio",
            "std::process::Output",
            "std::thread::JoinHandle<()>",
            "std::sync::Mutex<u8>",
            "std::sync::RwLock<u8>",
            "std::sync::MutexGuard<'_, u8>",
            "std::sync::RwLockReadGuard<'_, u8>",
            "std::sync::RwLockWriteGuard<'_, u8>",
        ] {
            assert!(collector.noncopy_type(&parse_str(name).unwrap(), 8), "{name}");
        }
        collector.generics.push("Record".into());
        assert!(collector.ownership_type(&parse_quote!(record.value)).is_none());
    });
    with_collector("use other::Record;", |collector| {
        collector.bind_type("record".into(), parse_quote!(Record));
        assert!(collector.ownership_type(&parse_quote!(record.value)).is_none());
    });
}

#[test]
fn pattern_bindings_preserve_reference_modes_and_subpatterns() {
    with_collector("", |collector| {
        for (pattern, expected, names) in [
            ("ref x", "u8", vec![("x", "&u8")]),
            ("whole @ Some(x)", "Option<u8>", vec![("whole", "Option<u8>"), ("x", "u8")]),
            ("(x, y)", "&'a mut (u8, bool)", vec![("x", "&'a mut u8"), ("y", "&'a mut bool")]),
            ("&x", "&u16", vec![("x", "u16")]),
            ("(x, y)", "&u16", vec![]),
        ] {
            collector.bind_pattern(&Pat::parse_single.parse_str(pattern).unwrap(), Some(&parse_str(expected).unwrap()));
            for (name, ty) in names {
                assert_eq!(
                    collector.type_of_place(&parse_str(name).unwrap()),
                    Some(&parse_str(ty).unwrap()),
                    "{pattern}"
                );
            }
        }
    });
}

#[test]
fn iterator_absence_and_mandatory_tail_classification_do_not_confuse_present_values() {
    for (value, absent) in [
        ("None", true),
        ("Err(problem)", true),
        ("Some(None)", true),
        ("Some(None, 1)", true),
        ("Some(1, None)", false),
        ("Ok(Some((None)))", true),
        ("Some(1)", false),
        ("Ok(1..3)", false),
        ("unknown(None)", false),
        ("1", false),
        ("invalid !", false),
    ] {
        assert_eq!(super::absent_iterator_value(value), absent, "{value}");
    }
    for source in [
        "{ if ready { return; } else { return; } }",
        "{ let x = 1; }",
        "{ match x { _ => { return; } } }",
    ] {
        let expression = parse_str(source).unwrap();
        let mut tails = Vec::new();
        super::mandatory_tail_statements(&expression, &mut tails);
        if source.contains("return") {
            assert!(!tails.is_empty(), "{source}");
        } else {
            assert!(tails.is_empty());
        }
    }
}

#[test]
fn collector_uses_closed_operator_outputs_and_retains_unknown_replacement_worlds() {
    let source = "
        struct A; struct B;
        impl core::ops::Add<B> for A { type Output = B; }
        impl core::ops::Sub<B> for A { type Output = A; }
        impl core::ops::Mul<B> for A { type Output = B; }
    ";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::unconditional();
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    collector.bind_type("left".into(), parse_quote!(A));
    collector.bind_type("right".into(), parse_quote!(B));
    let binary = parse_quote!(left + right);
    assert_eq!(collector.binary_expression_type(&binary), Some(parse_quote!(crate::B)));
    assert!(collector.unsupported_operator(&binary, "-"));
    assert!(!collector.unsupported_operator(&binary, "*"));
    assert!(collector.unsupported_operator(&binary, "/"));
    assert!(collector.unsupported_range_unit(&parse_quote!(right), "+"));
    assert_eq!(collector.inferred_expression_type(&parse_quote!(A)), Some(parse_quote!(A)));
}

fn with_collector(source: &str, check: impl FnOnce(&mut Collector<'_>)) {
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::parse("");
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    check(&mut collector);
}

#[test]
fn recorded_local_method_outputs_are_canonical_for_borrowed_receivers() {
    with_collector("struct Owner; impl Owner { fn duplicate(&self) -> Self { Owner } }", |collector| {
        let signature = collector
            .calls
            .method_signature(&parse_quote!(Owner), "duplicate", &ROOT, collector.current_imports())
            .unwrap();
        assert_eq!(signature.output, parse_quote!(-> crate::Owner));
        for receiver in ["Owner", "&Owner", "&mut Owner"] {
            collector.bind_type("owner".into(), parse_str(receiver).unwrap());
            assert_eq!(
                collector.inferred_expression_type(&parse_quote!(owner.duplicate())),
                Some(parse_quote!(crate::Owner)),
                "{receiver}"
            );
        }
    });
}

#[test]
fn constructor_inference_preserves_wrapper_payloads_and_distinguishes_factory_methods() {
    with_collector("", |collector| {
        for (expression, expected) in [
            ("Box::new(true)", "Box<bool>"),
            ("Box::new(true, 'q')", "Box<bool>"),
            ("std::boxed::Box::new('q')", "std::boxed::Box<char>"),
            ("alloc::boxed::Box::new(true)", "alloc::boxed::Box<bool>"),
            ("std::rc::Rc::new(true)", "std::rc::Rc<bool>"),
            ("std::rc::Rc::new(true, 'q')", "std::rc::Rc<bool>"),
            ("alloc::rc::Rc::new(true)", "alloc::rc::Rc<bool>"),
            ("std::sync::Arc::new(true)", "std::sync::Arc<bool>"),
            ("alloc::sync::Arc::new(true)", "alloc::sync::Arc<bool>"),
            ("std::cell::Cell::new(true)", "std::cell::Cell<bool>"),
            ("core::cell::RefCell::new(true)", "core::cell::RefCell<bool>"),
            ("std::sync::Mutex::new(true)", "std::sync::Mutex<bool>"),
            ("std::sync::RwLock::new(true)", "std::sync::RwLock<bool>"),
            ("String::new()", "String"),
            ("String::from(\"text\")", "String"),
            ("std::string::String::from(\"text\")", "String"),
            ("alloc::string::String::new()", "String"),
            (
                "std::fs::File::open(\"input\")",
                "std::result::Result<std::fs::File, std::io::Error>",
            ),
            (
                "std::fs::File::create(\"output\")",
                "std::result::Result<std::fs::File, std::io::Error>",
            ),
            (
                "std::fs::File::create_new(\"output\")",
                "std::result::Result<std::fs::File, std::io::Error>",
            ),
            ("std::fs::File::options()", "std::fs::OpenOptions"),
            ("std::fs::OpenOptions::new()", "std::fs::OpenOptions"),
            ("std::io::Cursor::new(bytes)", "std::io::Cursor"),
            ("std::io::BufReader::new(reader)", "std::io::BufReader"),
            ("core::time::Duration::from_secs(1)", "std::time::Duration"),
            ("std::time::Duration::from_millis(1)", "std::time::Duration"),
            ("std::time::Duration::from_micros(1)", "std::time::Duration"),
            ("std::time::Duration::from_nanos(1)", "std::time::Duration"),
            ("std::time::Duration::new(1, 2)", "std::time::Duration"),
        ] {
            assert_eq!(
                collector.standard_constructor_type(&parse_str(expression).unwrap()),
                Some(parse_str::<Type>(expected).unwrap()),
                "{expression}"
            );
        }
        for expression in [
            "Box::new(unknown)",
            "std::boxed::Box::new(3u8)",
            "Box::new()",
            "Box::from(true)",
            "other::sync::Arc::new(true)",
            "std::rc::Arc::new(true)",
            "std::cell::Mutex::new(true)",
            "std::fs::File::new()",
            "std::fs::OpenOptions::open(\"input\")",
            "std::io::Cursor::from(bytes)",
            "std::time::Duration::from_weeks(1)",
            "other::time::Duration::from_secs(1)",
            "std::other::Duration::from_secs(1)",
            "std::string::Other::new()",
            "std::time::Other::new()",
            "std::fs::Other::options()",
            "std::io::Other::new(reader)",
            "make(true)",
        ] {
            assert_eq!(
                collector.standard_constructor_type(&parse_str(expression).unwrap()),
                None,
                "{expression}"
            );
        }
    });
}

#[test]
fn qualified_factory_identity_requires_each_namespace_component_independently() {
    with_collector("", |collector| {
        for (module, name, method, arguments) in [
            ("string", "String", "new", ""),
            ("fs", "File", "open", "\"input\""),
            ("fs", "File", "options", ""),
            ("fs", "OpenOptions", "new", ""),
            ("io", "BufReader", "new", "reader"),
            ("io", "Cursor", "new", "bytes"),
            ("time", "Duration", "from_secs", "1"),
        ] {
            for components in [["foreign", module, name], ["std", "other", name], ["std", module, "Other"]] {
                let expression = format!("{}::{method}({arguments})", components.join("::"));
                assert_eq!(
                    collector.standard_constructor_type(&parse_str(&expression).unwrap()),
                    None,
                    "{expression}"
                );
            }
        }
        for expression in [
            "<Owner>::new(true)",
            "<Owner as Factory>::new(true)",
            "<Owner as std::fs::File>::open(\"input\")",
        ] {
            assert_eq!(
                collector.standard_constructor_type(&parse_str(expression).unwrap()),
                None,
                "{expression}"
            );
        }
    });
}

#[test]
fn reader_identity_requires_lexical_standard_traits_and_rejects_lookalike_types() {
    for import in ["use std::io::Read;", "use ::std::io::BufRead as Reader;", "use std::io::Read as _;"] {
        with_collector(import, |collector| {
            for ty in [
                "std::fs::File",
                "std::io::BufReader<File>",
                "std::io::Cursor<Vec<u8>>",
                "std::io::Stdin",
                "std::io::StdinLock<'static>",
                "std::io::Take<File>",
                "std::io::Chain<File, File>",
                "std::io::Empty",
                "std::io::Repeat",
                "std::net::TcpStream",
                "std::os::unix::net::UnixStream",
                "&mut std::io::Cursor<Vec<u8>>",
            ] {
                collector.bind_type("reader".into(), parse_str(ty).unwrap());
                for expression in ["reader", "reader.take(5)", "reader.by_ref()", "reader.by_ref().take(5)"] {
                    assert!(
                        collector.is_read_receiver(&parse_str(expression).unwrap()),
                        "{import} {ty}: {expression}"
                    );
                }
            }
            for ty in [
                "other::io::Cursor",
                "std::net::File",
                "std::io::File",
                "std::io::Other",
                "String",
                "Unknown",
                "()",
                "[u8; 2]",
                "fn() -> u8",
            ] {
                collector.bind_type("reader".into(), parse_str(ty).unwrap());
                assert!(!collector.is_read_receiver(&parse_quote!(reader)), "{ty}");
            }
        });
    }
    for import in ["", "use other::io::Read;", "use std::io::Write;", "use std::io::Read::Nested;"] {
        with_collector(import, |collector| {
            collector.bind_type("reader".into(), parse_quote!(std::fs::File));
            assert!(!collector.is_read_receiver(&parse_quote!(reader)), "{import}");
        });
    }
    with_collector(
        "use std::io::Read; struct Custom; impl Custom { fn by_ref(&self) {} }",
        |collector| {
            collector.bind_type("reader".into(), parse_quote!(std::fs::File));
            assert!(collector.is_read_receiver(&parse_quote!(reader)));
            assert!(collector.is_read_receiver(&parse_quote!(reader.take(5))));
            assert!(!collector.is_read_receiver(&parse_quote!(reader.by_ref())));
            assert!(!collector.is_read_receiver(&parse_quote!(reader.by_ref().take(5))));
        },
    );
}

#[test]
fn bounded_reader_types_do_not_require_a_concrete_reader_import() {
    with_collector("", |collector| {
        for ty in [
            "impl std::io::Read",
            "dyn std::io::BufRead",
            "&mut dyn std::io::Read",
            "impl std::io::Read + Send",
            "dyn Send + std::io::BufRead",
            "dyn std::io::Read + 'static",
        ] {
            collector.bind_type("reader".into(), parse_str(ty).unwrap());
            assert!(collector.is_read_receiver(&parse_quote!(reader)), "{ty}");
        }
        for ty in ["impl other::io::Read", "dyn std::io::Write", "impl Send"] {
            collector.bind_type("reader".into(), parse_str(ty).unwrap());
            assert!(!collector.is_read_receiver(&parse_quote!(reader)), "{ty}");
        }
    });
}

#[test]
fn reader_adapters_do_not_propagate_identity_through_unrecognized_method_shapes() {
    with_collector("use std::io::Read;", |collector| {
        collector.bind_type("reader".into(), parse_quote!(std::fs::File));
        for expression in ["reader.unknown()", "reader.by_ref(true)", "reader.unknown().take(5)"] {
            assert!(!collector.is_read_receiver(&parse_str(expression).unwrap()), "{expression}");
        }
    });
}

fn with_calls(source: &str, check: impl FnOnce(&File, &Calls, &HashMap<String, Option<Vec<String>>>)) {
    let file = parse_file(source).unwrap();
    let cfg = CfgSet::parse("");
    let indexes = indexes_in(&file, &Selection::everything(), &cfg);
    let calls = Calls::of(&file, &cfg, &indexes);
    check(&file, &calls, &indexes.root_imports);
}

fn grouped(expression: Expr) -> Expr {
    Expr::Group(ExprGroup {
        attrs: Vec::new(),
        group_token: Group::default(),
        expr: Box::new(expression),
    })
}

#[test]
fn invisible_expression_groups_preserve_inference_and_tail_analysis() {
    with_collector("", |collector| {
        assert!(collector.is_known_float(&grouped(parse_quote!(1.5))));
        assert_eq!(
            collector.inferred_expression_type(&grouped(parse_quote!(true))),
            Some(parse_quote!(bool))
        );
        collector.bind_type("number".into(), parse_quote!(f64));
        let value = grouped(parse_quote!(number));
        assert_eq!(collector.ownership_type(&value), Some(&parse_quote!(f64)));
        assert_eq!(collector.inferred_expression_type(&value), Some(parse_quote!(f64)));
        assert!(collector.is_known_float(&value));
        let mut spans = Vec::new();
        super::mandatory_tail_statements(
            &grouped(parse_quote!({
                return;
            })),
            &mut spans,
        );
        assert_eq!(spans.len(), 1);
        let value = grouped(parse_quote!(values.iter().sum()));
        assert!(super::needs_expected_numeric_type(&value));
        assert_eq!(
            collector.numeric_confidence(&parse_quote!((unknown))),
            super::Confidence::Optimistic
        );
        assert!(collector.type_of_place(&parse_quote!(*number)).is_none());
    });
}

#[test]
fn expression_attributes_stay_outside_lifted_ownership_guard_replacements() {
    let source = "fn f() { #[allow(unused)] value(); }";
    with_collector(source, |collector| {
        let whole = span(source, "#[allow(unused)] value()");
        let content = span(source, "value()");
        collector.expression_contents.insert(whole.clone(), content.clone());
        collector.emit_at(
            "call.replace_with_default",
            whole.clone(),
            "#[allow(unused)] false",
            0,
            super::Shape::Expr,
            super::Confidence::Proven,
        );
        assert_eq!(collector.candidates.len(), 1);
        assert_eq!(collector.candidates[0].replacement, "false");
        assert_eq!(collector.candidates[0].span, content);
        collector.emit_at(
            "call.replace_with_default",
            whole,
            "false",
            0,
            super::Shape::Expr,
            super::Confidence::Proven,
        );
        assert_eq!(collector.candidates.len(), 1);
    });
    let source = "fn f() { { value(); after(); } }";
    with_collector(source, |collector| {
        let edit = span(source, "value()");
        let region = span(source, "{ value(); after(); }");
        collector.ownership_lifts.insert(edit.clone(), region.clone());
        collector.emit_at(
            "call.replace_with_default",
            edit,
            "false",
            0,
            super::Shape::Expr,
            super::Confidence::Proven,
        );
        assert_eq!(collector.candidates.len(), 1);
        assert_eq!(collector.candidates[0].span, region);
        assert_eq!(collector.candidates[0].shape, super::Shape::Block);
        assert_eq!(collector.candidates[0].replacement, "{ false; after(); }");
    });
}

#[test]
fn ownership_admission_rejects_invalid_moves_and_lifts_complete_reinitialization_regions() {
    let source = "fn f() { value = make(); use_value(value); }";
    with_collector(source, |collector| {
        collector.bind_type("value".into(), parse_quote!(String));
        let assignment = span(source, "value = make()");
        let region = span(source, "{ value = make(); use_value(value); }");
        collector
            .numeric_evidence
            .invalid_ownership
            .insert(assignment.clone(), vec![parse_quote!(value)]);
        assert!(!collector.admit_ownership_edit(assignment.clone(), &assignment, false));
        collector.numeric_evidence.invalid_ownership.clear();
        collector
            .numeric_evidence
            .reinitializations
            .insert(assignment.clone(), vec![parse_quote!(value)]);
        collector
            .numeric_evidence
            .ownership_regions
            .insert(assignment.clone(), region.clone());
        assert!(collector.admit_ownership_edit(assignment.clone(), &assignment, false));
        assert_eq!(collector.ownership_lifts.get(&assignment), Some(&region));
    });
}

#[test]
fn wrapped_iterator_body_and_numeric_reduction_mutants_use_type_preserving_shapes() {
    let source = "fn f() -> Option<impl Iterator<Item = u8>> { Some(1..3) }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let candidates = collect_with(&file, &Selection::everything(), &CfgSet::unconditional(), &Defaults::of(&file.ast));
    assert!(candidates.iter().any(|candidate| candidate.shape
        == super::Shape::IterWrappedBlock {
            depth: NonZeroU8::new(1).unwrap()
        }));
    let source = "fn f(values: &[u32]) -> u32 { values.iter().copied().sum() }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let candidates = collect_with(&file, &Selection::everything(), &CfgSet::unconditional(), &Defaults::of(&file.ast));
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.replacement.starts_with("::gamma_rt::increment("))
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.replacement.starts_with("::gamma_rt::decrement("))
    );
}

#[test]
fn generic_arm_visitor_and_explicit_receiver_patterns_restore_bindings() {
    with_collector("", |collector| {
        let arm: Arm = parse_quote!(Some(value) => value + 1,);
        syn::visit::Visit::visit_arm(collector, &arm);
        assert!(!collector.bindings.contains_key("value"));
        let arm: Arm = parse_quote!(#[gamma::skip] _ => 1,);
        syn::visit::Visit::visit_arm(collector, &arm);
        collector.bind_pattern(&parse_quote!(&value), Some(&parse_quote!(u8)));
        assert!(collector.type_of_place(&parse_quote!(value)).is_none());
    });
    with_collector("struct Owner; impl Owner { fn f(self: Box<Self>) {} }", |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        assert!(collector.binding_types.is_empty());
    });
}

fn output(signature: &Signature) -> Type {
    match &signature.output {
        ReturnType::Default => parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    }
}

#[test]
fn call_resolution_respects_module_boundaries_imports_and_value_shadows() {
    with_calls(
        "
        fn target() -> u8 { 1 }
        mod outer {
            pub fn target() -> u16 { 2 }
            pub struct Owner;
            impl Owner { pub fn make() -> Self { Self } }
            mod inner { fn target() -> u32 { 3 } }
        }
        use outer::target as imported;
        use outer::Owner as Renamed;
        ",
        |file, calls, imports| {
            let Item::Mod(outer) = &file.items[1] else {
                panic!("module fixture")
            };
            let outer_scope = vec![module_id(&outer.ident)];
            let Item::Mod(inner) = &outer.content.as_ref().unwrap().1[3] else {
                panic!("nested module fixture")
            };
            let mut inner_scope = outer_scope.clone();
            inner_scope.push(module_id(&inner.ident));
            let empty = HashMap::default();
            for (callee, scope, imports, expected) in [
                ("target", Vec::new(), imports, Some(parse_quote!(u8))),
                ("imported", Vec::new(), imports, Some(parse_quote!(u16))),
                ("Renamed::make", Vec::new(), imports, Some(parse_quote!(crate::outer::Owner))),
                ("target", outer_scope.clone(), &empty, Some(parse_quote!(u16))),
                ("Owner::make", outer_scope.clone(), &empty, Some(parse_quote!(crate::outer::Owner))),
                ("target", inner_scope.clone(), &empty, Some(parse_quote!(u32))),
                ("self::target", inner_scope.clone(), &empty, Some(parse_quote!(u32))),
                ("super::target", inner_scope.clone(), &empty, Some(parse_quote!(u16))),
                ("super::super::target", inner_scope.clone(), &empty, Some(parse_quote!(u8))),
                ("crate::target", inner_scope.clone(), &empty, Some(parse_quote!(u8))),
                ("::target", inner_scope.clone(), &empty, Some(parse_quote!(u8))),
                ("super::super::super::target", inner_scope, &empty, None),
                ("missing", outer_scope, &empty, None),
                ("(target)", Vec::new(), imports, None),
                ("<Owner as Trait>::target", Vec::new(), imports, None),
            ] {
                let expression = parse_str(callee).unwrap();
                assert_eq!(
                    calls.signature(&expression, &scope, imports, None, |_| false).map(output),
                    expected,
                    "{callee} in {scope:?}"
                );
            }
            assert!(
                calls
                    .signature(&parse_quote!(imported), &ROOT, imports, None, |name| name == "imported")
                    .is_none()
            );
            let ambiguous = HashMap::from_iter([("imported".into(), None)]);
            assert!(
                calls
                    .signature(&parse_quote!(imported), &ROOT, &ambiguous, None, |_| false)
                    .is_none()
            );
            assert_eq!(
                calls
                    .signature(&parse_quote!(Self::make), &ROOT, imports, Some(&parse_quote!(outer::Owner)), |_| {
                        false
                    },)
                    .map(output),
                Some(parse_quote!(crate::outer::Owner))
            );
            assert!(
                calls
                    .signature(&parse_quote!(Self::make), &ROOT, imports, None, |_| false)
                    .is_none()
            );
        },
    );
}

#[test]
fn local_call_signatures_walk_blocks_but_stop_at_modules_and_ambiguous_declarations() {
    with_calls(
        "
        fn known() -> u8 { 1 }
        fn duplicate() -> u8 { 1 }
        fn duplicate() -> u16 { 2 }
        fn f() {
            fn known() -> u16 { 2 }
            { known(); }
            { const known: u8 = 1; known(); }
            { unsafe extern \"C\" { fn known(); } known(); }
            { static known: u8 = 1; known(); }
        }
        mod child { fn f() { known(); } }
        ",
        |file, calls, imports| {
            let Item::Fn(function) = &file.items[3] else {
                panic!("function fixture")
            };
            let outer = vec![block_id(&function.block)];
            for (statement, expected) in function.block.stmts[1..].iter().zip([Some(parse_quote!(u16)), None, None, None]) {
                let Stmt::Expr(Expr::Block(block), _) = statement else {
                    panic!("block fixture")
                };
                let mut scope = outer.clone();
                scope.push(block_id(&block.block));
                assert_eq!(
                    calls.signature(&parse_quote!(known), &scope, imports, None, |_| false).map(output),
                    expected
                );
            }
            assert!(calls.signature(&parse_quote!(duplicate), &ROOT, imports, None, |_| false).is_none());
            let Item::Mod(module) = &file.items[4] else {
                panic!("child fixture")
            };
            let Item::Fn(function) = &module.content.as_ref().unwrap().1[0] else {
                panic!("child function fixture")
            };
            let scope = vec![module_id(&module.ident), block_id(&function.block)];
            assert!(calls.signature(&parse_quote!(known), &scope, imports, None, |_| false).is_none());
        },
    );
}

#[test]
fn call_signatures_canonicalize_aliases_and_erase_unbound_generic_parameters() {
    with_calls(
        "
        use std::string::String as Text;
        type Pair<T> = (T, Text);
        type Chained<T> = Pair<T>;
        type CycleA = CycleB;
        type CycleB = CycleA;
        fn aliased(input: &[Chained<bool>; 2]) -> Chained<u8> { todo!() }
        fn generic<T>(input: T) -> Option<T> { todo!() }
        fn malformed(input: Pair) -> CycleA { todo!() }
        struct Owner<T>(T);
        impl<T> Owner<T> {
            fn borrowed(&self, value: T) -> Self { todo!() }
            fn mutable(&mut self) {}
            fn owned(self) {}
            fn boxed(self: Box<Self>) {}
            async fn deferred(&self) -> u32 { 1 }
        }
        ",
        |_, calls, imports| {
            let cfg = CfgSet::unconditional();
            let signature = calls.signature(&parse_quote!(aliased), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(
                Calls::parameters(signature, &cfg),
                [parse_quote!(&[(bool, std::string::String); 2])]
            );
            assert_eq!(output(signature), parse_quote!((u8, std::string::String)));
            let signature = calls.signature(&parse_quote!(generic), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(_)]);
            assert_eq!(output(signature), parse_quote!(Option<_>));
            let signature = calls.signature(&parse_quote!(malformed), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(_)]);
            assert_eq!(output(signature), parse_quote!(_));
            for (name, expected) in [
                ("borrowed", vec![parse_quote!(&crate::Owner), parse_quote!(_)]),
                ("mutable", vec![parse_quote!(&mut crate::Owner)]),
                ("owned", vec![parse_quote!(crate::Owner)]),
                ("boxed", vec![parse_quote!(Box<crate::Owner>)]),
            ] {
                let signature = calls.method_signature(&parse_quote!(&Owner<u8>), name, &ROOT, imports).unwrap();
                assert_eq!(Calls::parameters(signature, &cfg), expected, "{name}");
            }
            assert!(
                calls
                    .method_signature(&parse_quote!(Owner<u8>), "deferred", &ROOT, imports)
                    .unwrap()
                    .asyncness
                    .is_some()
            );
            assert!(calls.has_method_named("borrowed"));
            assert!(!calls.has_method_named("absent"));
            assert!(
                calls
                    .method_signature(&parse_quote!((Owner<u8>,)), "borrowed", &ROOT, imports)
                    .is_none()
            );
        },
    );
}

#[test]
fn call_signatures_bound_generic_and_branching_alias_expansion() {
    with_calls(include_str!("fixtures/expanding_aliases.rs"), |_, calls, imports| {
        let cfg = CfgSet::unconditional();
        let signature = calls.signature(&parse_quote!(generic), &ROOT, imports, None, |_| false).unwrap();
        assert_eq!(Calls::parameters(signature, &cfg), [parse_quote!(_)]);
        assert_eq!(output(signature), parse_quote!(_));

        let signature = calls.signature(&parse_quote!(branching), &ROOT, imports, None, |_| false).unwrap();
        let mut parameters = Calls::parameters(signature, &cfg);
        parameters.push(output(signature));
        for ty in parameters {
            let mut budget = super::types::TYPE_EXPANSION_LIMIT;
            assert!(super::types::reserve_type(&ty, &mut budget));
            assert!(quote::quote!(#ty).to_string().contains('_'));
        }
    });
}

#[test]
fn source_operator_signatures_distinguish_rhs_output_and_missing_implementations() {
    with_calls(
        "
        struct Left;
        struct Right;
        struct Other;
        use core::ops::Add as Plus;
        impl Plus<Right> for Left { type Output = Right; fn add(self, _: Right) -> Right { Right } }
        impl std::ops::Sub for Left { type Output = Self; fn sub(self, _: Self) -> Self { self } }
        impl core::ops::Mul<Right> for Left { type Output = Right; fn mul(self, _: Right) -> Right { Right } }
        impl core::ops::Div<Right> for Left { type Output = Right; fn div(self, _: Right) -> Right { Right } }
        impl core::ops::Rem<Right> for Left { type Output = Right; fn rem(self, _: Right) -> Right { Right } }
        impl core::ops::AddAssign<Right> for Left { fn add_assign(&mut self, _: Right) {} }
        impl core::ops::SubAssign<Right> for Left { fn sub_assign(&mut self, _: Right) {} }
        impl core::ops::MulAssign<Right> for Left { fn mul_assign(&mut self, _: Right) {} }
        impl core::ops::DivAssign<Right> for Left { fn div_assign(&mut self, _: Right) {} }
        impl core::ops::RemAssign<Right> for Left { fn rem_assign(&mut self, _: Right) {} }
        impl dependency::ops::Add<Right> for Other { type Output = Right; }
        impl core::other::Sub<Right> for Other { type Output = Right; }
        ",
        |_, calls, imports| {
            for (operator, rhs, expected) in [
                ("+", parse_quote!(Right), parse_quote!(crate::Right)),
                ("-", parse_quote!(Left), parse_quote!(crate::Left)),
                ("*", parse_quote!(Right), parse_quote!(crate::Right)),
                ("/", parse_quote!(Right), parse_quote!(crate::Right)),
                ("%", parse_quote!(Right), parse_quote!(crate::Right)),
                ("+=", parse_quote!(Right), parse_quote!(())),
                ("-=", parse_quote!(Right), parse_quote!(())),
                ("*=", parse_quote!(Right), parse_quote!(())),
                ("/=", parse_quote!(Right), parse_quote!(())),
                ("%=", parse_quote!(Right), parse_quote!(())),
            ] {
                let OperatorResult::Output(actual) = calls.operator_result(&parse_quote!(Left), &rhs, operator, &ROOT, imports) else {
                    panic!("missing {operator} signature");
                };
                assert_eq!(actual, &expected, "{operator}");
            }

            for (left, right, operator) in [
                (parse_quote!(Left), parse_quote!(Other), "+"),
                (parse_quote!(Right), parse_quote!(Left), "+"),
                (parse_quote!(Other), parse_quote!(Right), "+"),
                (parse_quote!(Other), parse_quote!(Right), "-"),
            ] {
                assert!(matches!(
                    calls.operator_result(&left, &right, operator, &ROOT, imports),
                    OperatorResult::Missing
                ));
            }
            assert!(!calls.unit_operator_absent(&parse_quote!(Left), "+", &ROOT, imports));
            assert!(calls.unit_operator_absent(&parse_quote!(Right), "+", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&parse_quote!(dependency::Unknown), "+", &ROOT, imports));
            assert!(matches!(
                calls.operator_result(&parse_quote!(&Left), &parse_quote!(Right), "+", &ROOT, imports),
                OperatorResult::Unknown
            ));
        },
    );
}

#[test]
fn open_or_ambiguous_operator_worlds_never_prove_an_operator_absent() {
    for source in [
        "struct A; mod external;",
        "struct A; generate!();",
        "struct A; #[transform] fn altered() {}",
        "struct A; impl<T> core::ops::Add<T> for A { type Output = A; }",
        "struct A; impl core::ops::Add for A {}",
        "struct A; impl core::ops::Add for &A { type Output = A; }",
        "struct A; type Borrowed = &'static A; impl core::ops::Add for Borrowed { type Output = A; }",
    ] {
        with_calls(source, |_, calls, imports| {
            assert!(
                matches!(
                    calls.operator_result(&parse_quote!(A), &parse_quote!(A), "+", &ROOT, imports),
                    OperatorResult::Unknown
                ),
                "{source}"
            );
            assert!(!calls.unit_operator_absent(&parse_quote!(A), "-", &ROOT, imports), "{source}");
        });
    }
    with_calls(
        "struct A;
         impl core::ops::Add for A { type Output = A; }
         impl core::ops::Add for A { type Output = A; }",
        |_, calls, imports| {
            assert!(matches!(
                calls.operator_result(&parse_quote!(A), &parse_quote!(A), "+", &ROOT, imports),
                OperatorResult::Unknown
            ));
        },
    );
}

#[test]
fn field_ownership_proof_requires_unconditional_untransformed_structure_and_field() {
    with_collector(
        r"
        struct Owned { value: String }
        struct Tuple(u8, String);
        struct Generic<T> { value: T }
        struct CopyOnly { value: u8 }
        struct RefOnly { value: &'static str }
        struct MutableRef<'a> { value: &'a mut u8 }
        #[cfg(all())] struct Gated { value: String }
        #[cfg_attr(all(), derive(Debug))] struct Conditional { value: String }
        struct GatedField { #[cfg(all())] value: String }
        struct ConditionalField { #[cfg_attr(all(), allow(dead_code))] value: String }
        #[transform] struct Altered { value: String }
        struct AlteredField { #[transform] value: String }
        struct Recursive { next: Recursive }
        struct Mixed { next: Mixed, value: String }
        ",
        |collector| {
            for (ty, expected) in [
                ("Owned", true),
                ("Tuple", true),
                ("Generic<String>", false),
                ("CopyOnly", false),
                ("RefOnly", false),
                ("MutableRef<'_>", true),
                ("Gated", false),
                ("Conditional", false),
                ("GatedField", false),
                ("ConditionalField", false),
                ("Altered", false),
                ("AlteredField", false),
                ("Recursive", false),
                ("Mixed", true),
                ("(u8, [Owned; 2])", true),
                ("(u8, [CopyOnly; 2])", false),
                ("&Owned", false),
                ("&mut CopyOnly", true),
            ] {
                let ty = parse_str(ty).unwrap();
                assert_eq!(collector.noncopy_type(&ty, 8), expected, "{ty:?}");
                assert!(!collector.noncopy_type(&ty, 0), "exhausted proof budget: {ty:?}");
            }
            assert!(!collector.noncopy_type(&parse_quote!((String,)), 1));
            assert!(collector.noncopy_type(&parse_quote!((String,)), 2));
            assert!(!collector.noncopy_type(&parse_quote!([String; 2]), 1));
            assert!(collector.noncopy_type(&parse_quote!([String; 2]), 2));
        },
    );
}

#[test]
fn documented_fields_retain_their_move_sensitive_ownership_evidence() {
    with_calls(
        r#"struct Documented { #[doc = "owned payload"] value: String }"#,
        |_, calls, imports| {
            assert!(calls.has_noncopy_field(&parse_quote!(Documented), &ROOT, imports, |ty| {
                assert_eq!(ty, &parse_quote!(String));
                true
            }));
        },
    );
}

#[test]
fn field_lookup_preserves_owner_identity_and_cfg_without_guessing_generic_arguments() {
    with_calls(
        "
        mod a { pub struct Same { pub value: u8 } }
        mod b { pub struct Same { pub value: bool } }
        use a::Same as First;
        struct Pair(u16, String);
        struct Generic<T> { value: T }
        struct Hidden { #[cfg(any())] value: u8, kept: bool }
        struct Duplicate { value: u8 }
        struct Duplicate { value: bool }
        ",
        |_, calls, imports| {
            for (ty, member, expected) in [
                ("First", "value", Some(parse_quote!(u8))),
                ("&First", "value", Some(parse_quote!(u8))),
                ("b::Same", "value", Some(parse_quote!(bool))),
                ("Pair", "0", Some(parse_quote!(u16))),
                ("Pair", "1", Some(parse_quote!(String))),
                ("Pair", "2", None),
                ("Generic<u8>", "value", Some(parse_quote!(_))),
                ("Hidden", "value", None),
                ("Hidden", "kept", Some(parse_quote!(bool))),
                ("Duplicate", "value", None),
                ("(u8, bool)", "0", None),
            ] {
                assert_eq!(
                    calls.field(&parse_str(ty).unwrap(), &parse_str(member).unwrap(), &ROOT, imports),
                    expected,
                    "{ty}.{member}"
                );
            }
            assert!(calls.same_type(&parse_quote!(First), &parse_quote!(crate::a::Same), &ROOT, imports));
            assert!(!calls.same_type(&parse_quote!(First), &parse_quote!(b::Same), &ROOT, imports));
        },
    );
}

fn span(source: &str, text: &str) -> Range<usize> {
    let start = source.find(text).unwrap();
    start..start + text.len()
}

#[test]
fn temporary_borrows_are_guarded_at_the_consumer_including_adapter_chains() {
    for (source, borrowed, consumer) in [
        ("fn f() { consume(&make()); }", "&make()", "consume(&make())"),
        ("fn f() { consume(make().trim()); }", "make().trim()", "consume(make().trim())"),
        (
            "fn f() { consume(make().get(0).unwrap()); }",
            "make().get(0).unwrap()",
            "consume(make().get(0).unwrap())",
        ),
        (
            "fn f() { consume(Some(make().as_str())); }",
            "Some(make().as_str())",
            "consume(Some(make().as_str()))",
        ),
        (
            "fn f() { consume(Ok(make().as_slice())); }",
            "Ok(make().as_slice())",
            "consume(Ok(make().as_slice()))",
        ),
        (
            "fn f() { consume(Err(make().as_path())); }",
            "Err(make().as_path())",
            "consume(Err(make().as_path()))",
        ),
        (
            "fn f() { consume((make().get(0))?); }",
            "(make().get(0))?",
            "consume((make().get(0))?)",
        ),
        ("fn f() { consume(&make()[0]); }", "&make()[0]", "consume(&make()[0])"),
        ("fn f() { consume(&make().field); }", "&make().field", "consume(&make().field)"),
        ("fn f() { consume(&[1, 2]); }", "&[1, 2]", "consume(&[1, 2])"),
        ("fn f() { consume(&(1, 2)); }", "&(1, 2)", "consume(&(1, 2))"),
        (
            "fn f() { consume(&Owner { value: 1 }); }",
            "&Owner { value: 1 }",
            "consume(&Owner { value: 1 })",
        ),
        ("fn f() { consume(&make()?); }", "&make()?", "consume(&make()?)"),
    ] {
        let ast = parse_file(source).unwrap();
        let sites = temporaries::consuming_sites(&ast);
        assert_eq!(
            sites.get(&span(source, borrowed)),
            Some(&span(source, consumer)),
            "{source}: {sites:?}"
        );
    }
    for source in [
        "fn f(value: String) { consume(&value); }",
        "fn f(value: String) { consume(value.as_str()); }",
        "fn f(value: String) { consume(value.get(0).unwrap()); }",
        "fn f() { consume(make().clone()); }",
        "fn f() { consume(Custom(make().clone())); }",
    ] {
        assert!(temporaries::consuming_sites(&parse_file(source).unwrap()).is_empty(), "{source}");
    }
}

#[test]
fn consuming_guard_regions_stop_at_control_flow_but_include_match_scrutinees() {
    for (source, borrowed, consumer) in [
        (
            "fn f() { outer({ inner(make().trim()) }); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(|| inner(make().trim())); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(async { inner(make().trim()) }); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(if ready { inner(make().trim()) } else { other() }); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(match value { _ => inner(make().trim()) }); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(match make().trim() { _ => true }); }",
            "make().trim()",
            "outer(match make().trim() { _ => true })",
        ),
        (
            "fn f() { outer(ready && inner(make().trim())); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { outer(ready || inner(make().trim())); }",
            "make().trim()",
            "inner(make().trim())",
        ),
        ("fn f() { return inner(make().trim()); }", "make().trim()", "inner(make().trim())"),
        ("fn f() { loop { inner(make().trim()); } }", "make().trim()", "inner(make().trim())"),
        (
            "fn f() { while ready { inner(make().trim()); } }",
            "make().trim()",
            "inner(make().trim())",
        ),
        (
            "fn f() { for item in values { inner(make().trim()); } }",
            "make().trim()",
            "inner(make().trim())",
        ),
    ] {
        let sites = temporaries::consuming_sites(&parse_file(source).unwrap());
        assert_eq!(sites.get(&span(source, borrowed)), Some(&span(source, consumer)), "{source}");
    }
}

#[test]
fn lexical_reinitialization_proof_distinguishes_reads_writes_and_loop_backedges() {
    for (source, assignment, invalid) in [
        (
            "fn f(mut value: String) { consume(value); value = make(); use_value(value); }",
            "value = make()",
            true,
        ),
        (
            "fn f(mut value: String) { consume(value); value = make(); value = other(); }",
            "value = make()",
            false,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); value = make(); } }",
            "value = make()",
            true,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); value = make(); value = other(); } }",
            "value = make()",
            false,
        ),
        (
            "fn f() { loop { let mut value = make(); consume(value); value = other(); } }",
            "value = other()",
            false,
        ),
        (
            "fn f(mut value: String) { consume(value); { value = make(); use_value(value); } }",
            "value = make()",
            false,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); value = make(); break; } use_value(value); }",
            "value = make()",
            true,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); value = make(); break; } }",
            "value = make()",
            false,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); break; value = make(); use_value(value); } }",
            "value = make()",
            false,
        ),
    ] {
        let file = parse_file(source).unwrap();
        let evidence = lexical::evidence(&file, &CfgSet::unconditional());
        let site = span(source, assignment);
        assert_eq!(
            evidence.reinitializations.get(&site).map(|moved| moved
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>()),
            Some(vec!["value"]),
            "{source}"
        );
        assert_eq!(evidence.invalid_ownership.contains_key(&site), invalid, "{source}");
        assert!(evidence.ownership_regions.contains_key(&site), "{source}");
    }
}

#[test]
fn lexical_exit_proof_tracks_restoration_order_and_transition_direction() {
    for (source, exit, invalid, transition) in [
        (
            "fn f(value: String) { loop { consume(value); break; } use_value(value); }",
            "break",
            true,
            true,
        ),
        ("fn f(value: String) { loop { consume(value); break; } }", "break", true, true),
        (
            "fn f(mut value: String) { loop { consume(value); value = make(); break; } use_value(value); }",
            "break",
            false,
            false,
        ),
        (
            "fn f(mut value: String) { loop { consume(value); continue; value = make(); } }",
            "continue",
            false,
            false,
        ),
        (
            "fn f(value: String) { loop { consume(value); continue; } use_value(value); }",
            "continue",
            true,
            true,
        ),
        (
            "fn f() { loop { let value = make(); consume(value); break; } }",
            "break",
            false,
            false,
        ),
        (
            "fn f(value: String) { 'outer: loop { consume(value); break 'outer; } use_value(value); }",
            "break 'outer",
            false,
            false,
        ),
        (
            "fn f(value: String) { loop { { consume(value); } break; } use_value(value); }",
            "break",
            false,
            false,
        ),
    ] {
        let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::unconditional());
        let site = span(source, exit);
        assert_eq!(evidence.invalid_ownership.contains_key(&site), invalid, "{source}");
        assert_eq!(evidence.invalid_transitions.contains_key(&site), transition, "{source}");
        if invalid || transition {
            let names = evidence.exits[&site]
                .iter()
                .map(|expression| &source[expression.span().byte_range()])
                .collect::<Vec<_>>();
            assert_eq!(names, ["value"], "{source}");
        }
    }
}

#[test]
fn lexical_field_moves_require_overlap_and_the_same_binding_identity() {
    for (source, invalid) in [
        (
            "fn f(mut pair: Pair) { consume(pair.left); pair.left = make(); use_value(pair.left); }",
            true,
        ),
        (
            "fn f(mut pair: Pair) { consume(pair.left); pair.left = make(); use_value(pair.right); }",
            false,
        ),
        (
            "fn f(mut pair: Pair) { consume(pair.left); pair.left = make(); use_value(pair); }",
            true,
        ),
        (
            "fn f(mut pair: Pair) { consume(pair.left); pair.left = make(); let pair = other(); use_value(pair.left); }",
            false,
        ),
        (
            "fn f(mut pair: Pair) { consume(pair.left); pair.left = make(); pair = other(); use_value(pair.left); }",
            false,
        ),
    ] {
        let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::unconditional());
        assert_eq!(
            evidence.invalid_ownership.contains_key(&span(source, "pair.left = make()")),
            invalid,
            "{source}"
        );
    }
}

#[test]
fn inferred_match_types_require_branch_agreement_except_for_result_shape_evidence() {
    with_collector(
        "
            struct Owner;
            impl Owner {
                fn owned(self) -> Self { self }
                fn borrowed(&self) -> bool { true }
                async fn deferred(&self) -> u32 { 1 }
            }
            fn sync_value() -> u32 { 1 }
            async fn async_value() -> u32 { 1 }
            ",
        |collector| {
            collector.bind_type("owner".into(), parse_quote!(Owner));
            collector.bind_type("optional".into(), parse_quote!(Option<u8>));
            collector.bind_type("result".into(), parse_quote!(Result<u16, bool>));
            for (expression, expected) in [
                ("match value { A => true, B => false }", Some(parse_quote!(bool))),
                ("match value { A => true, B => 'x' }", None),
                ("match value { A => true, B => unknown }", None),
                ("match value {}", None),
                (
                    "match value { A => Ok(true), B => Err(false) }",
                    Some(parse_quote!(std::result::Result<_, _>)),
                ),
                (
                    "match value { A => Ok(true), B => unknown }",
                    Some(parse_quote!(std::result::Result<_, _>)),
                ),
                (
                    "match value { A => Ok(true), B => false }",
                    Some(parse_quote!(std::result::Result<_, _>)),
                ),
                ("{ true }", Some(parse_quote!(bool))),
                ("{ true; }", None),
                ("{ let value = true; }", None),
                ("{ }", None),
                ("(true, unknown)", Some(parse_quote!((bool, _,)))),
                ("optional?", Some(parse_quote!(u8))),
                ("result?", Some(parse_quote!(u16))),
                ("owner?", None),
                ("owner.owned()", Some(parse_quote!(crate::Owner))),
                ("owner.borrowed()", Some(parse_quote!(bool))),
                ("owner.deferred()", None),
                ("sync_value()", Some(parse_quote!(u32))),
                ("async_value()", None),
            ] {
                assert_eq!(
                    collector.inferred_expression_type(&parse_str(expression).unwrap()),
                    expected,
                    "{expression}"
                );
            }
        },
    );
}

#[test]
fn standard_method_inference_requires_receiver_family_and_argument_shape() {
    with_collector("", |collector| {
        for (name, ty) in [
            ("text", parse_quote!(&str)),
            ("owned", parse_quote!(String)),
            ("chars", parse_quote!(std::str::Chars<'_>)),
            ("result", parse_quote!(Result<u8, bool>)),
            ("optional", parse_quote!(Option<u16>)),
            ("foreign", parse_quote!(dependency::Result<u8, bool>)),
            ("number", parse_quote!(u32)),
            ("instant", parse_quote!(std::time::Instant)),
        ] {
            collector.bind_type(name.into(), ty);
        }
        for (expression, expected) in [
            ("text.to_owned()", Some(parse_quote!(std::string::String))),
            ("text.to_string()", Some(parse_quote!(std::string::String))),
            ("owned.to_owned()", Some(parse_quote!(std::string::String))),
            ("owned.to_string()", Some(parse_quote!(std::string::String))),
            ("text.to_owned(1)", None),
            ("owned.to_string(1)", None),
            ("number.to_owned()", None),
            ("text.unknown()", None),
            ("text.chars()", Some(parse_quote!(std::str::Chars<'_>))),
            ("owned.chars()", Some(parse_quote!(std::str::Chars<'_>))),
            ("text.chars(1)", None),
            ("number.chars()", None),
            ("chars.next()", Some(parse_quote!(std::option::Option<char>))),
            ("chars.next(1)", None),
            ("number.next()", None),
            ("result.map(transform)", Some(parse_quote!(std::result::Result<_, _>))),
            ("result.map_err(transform)", Some(parse_quote!(std::result::Result<_, _>))),
            ("result.and_then(transform)", Some(parse_quote!(std::result::Result<_, _>))),
            ("result.or_else(transform)", Some(parse_quote!(std::result::Result<_, _>))),
            ("result.map()", None),
            ("result.map(a, b)", None),
            ("foreign.map(transform)", None),
            ("optional.map(transform)", None),
            ("result.unwrap()", Some(parse_quote!(u8))),
            ("result.expect(message)", Some(parse_quote!(u8))),
            ("optional.unwrap_or(1)", Some(parse_quote!(u16))),
            ("optional.unwrap_or_else(make)", Some(parse_quote!(u16))),
            ("foreign.unwrap()", None),
            ("instant.elapsed()", Some(parse_quote!(std::time::Duration))),
            ("instant.duration_since(other)", Some(parse_quote!(std::time::Duration))),
            ("instant.saturating_duration_since(other)", Some(parse_quote!(std::time::Duration))),
            ("instant.unknown()", None),
            ("number.elapsed()", None),
        ] {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                expected,
                "{expression}"
            );
        }
    });
}

#[test]
fn standard_constructor_inference_does_not_borrow_local_string_identity() {
    for (source, expression, expected) in [
        ("", "String::new()", Some(parse_quote!(String))),
        ("", "String::from(value)", Some(parse_quote!(String))),
        ("", "String::other()", None),
        ("", "Other::new()", None),
        ("struct String;", "String::new()", None),
        ("use dependency::String;", "String::new()", None),
        ("use std::string::String;", "String::new()", Some(parse_quote!(String))),
        ("use std::string::String as Text;", "Text::new()", Some(parse_quote!(String))),
        ("use dependency::String as Text;", "Text::new()", None),
        ("", "std::string::String::new()", Some(parse_quote!(String))),
        ("", "alloc::string::String::from(value)", Some(parse_quote!(String))),
        ("", "Box::new(true)", Some(parse_quote!(Box<bool>))),
        ("", "Box::new()", None),
        ("", "std::sync::Arc::new('x')", Some(parse_quote!(std::sync::Arc<char>))),
        ("", "std::cell::RefCell::new(true)", Some(parse_quote!(std::cell::RefCell<bool>))),
        ("", "std::sync::Mutex::new(true)", Some(parse_quote!(std::sync::Mutex<bool>))),
        ("", "dependency::Box::new(true)", None),
    ] {
        with_collector(source, |collector| {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                expected,
                "{source} / {expression}"
            );
        });
    }
}

#[test]
fn arithmetic_replacement_filter_preserves_the_operator_output_type() {
    with_collector("", |collector| {
        for (name, ty) in [
            ("duration", parse_quote!(std::time::Duration)),
            ("instant", parse_quote!(std::time::Instant)),
            ("system", parse_quote!(std::time::SystemTime)),
            ("date", parse_quote!(chrono::DateTime)),
            ("naive", parse_quote!(chrono::NaiveDateTime)),
            ("delta", parse_quote!(chrono::TimeDelta)),
            ("text", parse_quote!(String)),
            ("suffix", parse_quote!(&str)),
            ("scalar", parse_quote!(u32)),
            ("qualified", parse_quote!(::core::primitive::u32)),
            ("boolean", parse_quote!(::core::primitive::bool)),
            ("foreign", parse_quote!(dependency::Value)),
        ] {
            collector.bind_type(name.into(), ty);
        }
        for (expression, replacement, unsupported, expected_type) in [
            ("duration + duration", "-", false, Some(parse_quote!(std::time::Duration))),
            ("duration - duration", "+", false, Some(parse_quote!(std::time::Duration))),
            ("duration + duration", "*", true, Some(parse_quote!(std::time::Duration))),
            ("duration * scalar", "/", false, Some(parse_quote!(std::time::Duration))),
            ("duration * qualified", "/", false, Some(parse_quote!(std::time::Duration))),
            ("duration / qualified", "%", true, Some(parse_quote!(std::time::Duration))),
            ("duration * boolean", "/", false, None),
            ("duration / scalar", "%", true, Some(parse_quote!(std::time::Duration))),
            ("scalar * duration", "/", true, Some(parse_quote!(std::time::Duration))),
            ("instant + duration", "-", false, Some(parse_quote!(std::time::Instant))),
            ("instant - instant", "+", true, Some(parse_quote!(std::time::Duration))),
            ("system - system", "+", true, Some(parse_quote!(std::time::Duration))),
            ("system + duration", "-", false, Some(parse_quote!(std::time::SystemTime))),
            ("date - date", "+", true, Some(parse_quote!(chrono::TimeDelta))),
            ("naive - naive", "+", true, Some(parse_quote!(chrono::TimeDelta))),
            ("date + delta", "-", false, Some(parse_quote!(chrono::DateTime))),
            ("naive - duration", "+", false, Some(parse_quote!(chrono::NaiveDateTime))),
            ("delta * scalar", "/", false, Some(parse_quote!(chrono::TimeDelta))),
            ("delta + delta", "*", true, Some(parse_quote!(chrono::TimeDelta))),
            ("text + suffix", "-", true, Some(parse_quote!(String))),
            ("scalar % scalar", "+", false, Some(parse_quote!(u32))),
            ("scalar + foreign", "-", false, None),
            ("foreign + scalar", "-", false, None),
            ("unknown + scalar", "-", false, None),
        ] {
            let binary: ExprBinary = parse_str(expression).unwrap();
            assert_eq!(
                collector.unsupported_operator(&binary, replacement),
                unsupported,
                "{expression} -> {replacement}"
            );
            assert_eq!(collector.binary_expression_type(&binary), expected_type, "{expression}");
        }
        for (expression, replacement, unsupported) in [
            ("duration += duration", "-=", false),
            ("duration -= duration", "*=", true),
            ("duration *= scalar", "/=", false),
            ("duration /= scalar", "%=", true),
            ("scalar %= scalar", "+=", false),
            ("text += suffix", "-=", true),
            ("scalar < scalar", "-", false),
        ] {
            let binary = parse_str(expression).unwrap();
            assert_eq!(collector.unsupported_operator(&binary, replacement), unsupported, "{expression}");
        }
        for expression in ["duration", "instant", "system", "date", "naive", "delta", "text", "suffix"] {
            assert!(
                collector.unsupported_range_unit(&parse_str(expression).unwrap(), "+"),
                "{expression}"
            );
        }
        for expression in ["scalar", "foreign", "unknown", "1", "1.0"] {
            assert!(
                !collector.unsupported_range_unit(&parse_str(expression).unwrap(), "+"),
                "{expression}"
            );
        }
    });
}

#[test]
fn standard_pattern_payloads_keep_variant_family_import_identity_and_reference_qualifiers() {
    with_collector(
        "use core::option::Option::Some as Present;
             use core::result::Result::Err as Failure;
             use dependency::Option::Some as Foreign;",
        |collector| {
            for (path, expected, payload) in [
                ("Some", "Option<u8>", Some(parse_quote!(u8))),
                ("Present", "Option<bool>", Some(parse_quote!(bool))),
                ("core::option::Option::Some", "Option<u8>", Some(parse_quote!(u8))),
                ("std::option::Option::Some", "&'a mut Option<u8>", Some(parse_quote!(&'a mut u8))),
                ("Ok", "Result<u8, bool>", Some(parse_quote!(u8))),
                ("Err", "Result<u8, bool>", Some(parse_quote!(bool))),
                ("Failure", "Result<u8, bool>", Some(parse_quote!(bool))),
                ("core::result::Result::Ok", "Result<u8, bool>", Some(parse_quote!(u8))),
                ("std::result::Result::Err", "&Result<u8, bool>", Some(parse_quote!(&bool))),
                ("Some", "Result<u8, bool>", None),
                ("Ok", "Option<u8>", None),
                ("Err", "Option<u8>", None),
                ("Foreign", "Option<u8>", None),
                ("Some", "dependency::Option<u8>", None),
                ("std::result::Result::Other", "Result<u8, bool>", None),
            ] {
                assert_eq!(
                    collector.standard_pattern_payload(&parse_str(path).unwrap(), &parse_str(expected).unwrap()),
                    payload,
                    "{path} / {expected}"
                );
            }
        },
    );
    with_collector("#![no_implicit_prelude]", |collector| {
        for constructor in ["Some", "Ok", "Err"] {
            assert!(
                collector
                    .standard_pattern_payload(&parse_str(constructor).unwrap(), &parse_quote!(Option<u8>))
                    .is_none(),
                "{constructor}"
            );
        }
        assert_eq!(
            collector.standard_pattern_payload(&parse_quote!(core::option::Option::Some), &parse_quote!(core::option::Option<u8>)),
            Some(parse_quote!(u8))
        );
    });
}

#[test]
fn standard_pattern_identity_checks_each_qualifier_and_the_expected_variant_family() {
    with_collector("", |collector| {
        for (module, name, variant, expected) in [
            ("option", "Option", "Some", "Option<u8>"),
            ("result", "Result", "Ok", "Result<u8, bool>"),
            ("result", "Result", "Err", "Result<u8, bool>"),
        ] {
            for components in [
                ["foreign", module, name, variant],
                ["std", "other", name, variant],
                ["std", module, "Other", variant],
                ["std", module, name, "Other"],
            ] {
                let path = components.join("::");
                assert_eq!(
                    collector.standard_pattern_payload(&parse_str(&path).unwrap(), &parse_str(expected).unwrap()),
                    None,
                    "{path} / {expected}"
                );
            }
            let path = format!("std::{module}::{name}::{variant}");
            assert_eq!(
                collector.standard_pattern_payload(&parse_str(&path).unwrap(), &parse_quote!(foreign::Pair<u8, bool>)),
                None,
                "{path}"
            );
        }
        assert_eq!(
            collector.standard_pattern_payload(&parse_quote!(Err), &parse_quote!(foreign::Pair<u8, bool>)),
            None
        );
    });
    with_collector("#![no_implicit_prelude]", |collector| {
        for (path, expected) in [
            ("Some", "core::option::Option<u8>"),
            ("Ok", "core::result::Result<u8, bool>"),
            ("Err", "core::result::Result<u8, bool>"),
        ] {
            assert_eq!(
                collector.standard_pattern_payload(&parse_str(path).unwrap(), &parse_str(expected).unwrap()),
                None,
                "{path} / {expected}"
            );
        }
    });
}

#[test]
fn custom_some_constructors_require_the_same_enum_as_the_visible_none_variant() {
    for (source, expression, expected) in [
        ("enum Custom { Some(u8), Missing }", "Custom::Some", false),
        ("enum Custom { Some(u8), None } use Custom::None;", "Custom::Some", true),
        (
            "enum Custom { Some(u8), None } enum Other { None } use Other::None;",
            "Custom::Some",
            false,
        ),
        (
            "enum Custom { Some(u8), None } use Custom::Some as Present; use Custom::None;",
            "Present",
            true,
        ),
        ("enum Custom { Some(u8), None } use Custom::None as Absent;", "Custom::Some", false),
        ("", "Some", true),
        ("", "(Some)", true),
        ("", "std::option::Option::Some", true),
        ("", "std::option::Option::None", false),
        ("", "Some(value)", false),
        ("", "<Custom as Trait>::Some", false),
    ] {
        with_collector(source, |collector| {
            assert_eq!(
                collector.some_constructor(&parse_str(expression).unwrap()),
                expected,
                "{source} / {expression}"
            );
        });
    }
}

#[test]
fn custom_constructor_pairs_resolve_external_source_enums_without_file_local_declarations() {
    let file = SourceFile::parse("fixture.rs", "mod choices; use choices::First::None;".into()).unwrap();
    let choices = parse_file(
        "pub enum First { Some(u8), None }
         pub enum Second { Some(u8), None }
         pub type Alias = First;",
    )
    .unwrap();
    let cfg = CfgSet::parse("");
    let selection = Selection::everything();
    let mut defaults = Defaults::of_source_in(&file.ast, &cfg, "fixture.rs");
    defaults.absorb(Defaults::of_source_in(&choices, &cfg, "choices.rs"));
    defaults.bind_sources(&[
        ("fixture.rs".into(), "fixture.rs".into(), Vec::new()),
        ("choices.rs".into(), "fixture.rs".into(), vec!["choices".into()]),
    ]);
    let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    for name in ["First", "Alias", "Second"] {
        let path: Path = parse_str(&format!("choices::{name}")).unwrap();
        assert!(!collector.calls.is_enum(&path, &ROOT, collector.current_imports()));
        assert!(collector.types().source_is_enum(&parse_quote!(#path)));
        assert_eq!(
            collector.some_constructor(&parse_str(&format!("choices::{name}::Some")).unwrap()),
            name != "Second",
            "{name}"
        );
    }
}

#[test]
fn reader_take_detection_requires_standard_traits_and_does_not_cross_custom_by_ref() {
    for (source, suppressed) in [
        ("use std::io::Read as Input; fn f(value: std::fs::File) { value.take(3); }", true),
        ("use std::io::Read as _; fn f(value: std::fs::File) { value.take(3); }", true),
        (
            "use ::std::io::BufRead; fn f(value: &mut std::io::Cursor<Vec<u8>>) { value.take(3); }",
            true,
        ),
        ("fn f(value: &mut dyn std::io::Read) { value.take(3); }", true),
        ("fn f(value: impl std::io::BufRead) { value.take(3); }", true),
        ("fn f(value: std::fs::File) { value.take(3); }", false),
        ("use dependency::Read; fn f(value: std::fs::File) { value.take(3); }", false),
        ("use std::io::Read; fn f(value: dependency::File) { value.take(3); }", false),
        ("use std::io::Read; fn f(value: impl dependency::Read) { value.take(3); }", false),
        ("use std::io::Read; fn f(value: std::fs::File) { value.take(2).take(3); }", true),
        ("use std::io::Read; fn f(value: std::fs::File) { value.by_ref().take(3); }", true),
        (
            "use std::io::Read; struct Custom; impl Custom { fn by_ref(&self) {} }
                 fn f(value: std::fs::File) { value.by_ref().take(3); }",
            false,
        ),
    ] {
        let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
        let found = collect_with(
            &file,
            &Selection::parse("iter.take_to_skip").unwrap(),
            &CfgSet::parse(""),
            &Defaults::of(&file.ast),
        );
        assert_eq!(found.is_empty(), suppressed, "{source}: {found:?}");
        if !suppressed {
            assert_eq!(found.len(), 1, "{source}: {found:?}");
            assert_eq!(found[0].mutator, "iter.take_to_skip");
            assert!(found[0].replacement.contains(".skip("), "{source}: {found:?}");
        }
    }
}

#[test]
fn nested_numeric_bindings_restore_inferred_float_evidence() {
    with_collector("", |collector| {
        collector.inferred_floating.insert("value".into());
        collector.in_scope(|collector| {
            collector.bind("value".into(), false);
            assert!(!collector.inferred_floating.contains("value"));
        });
        assert!(collector.inferred_floating.contains("value"));
    });
}

#[test]
fn temporal_range_ends_do_not_emit_integer_unit_edits() {
    let file = SourceFile::parse(
        "fixture.rs",
        "fn f(start: u32, end: std::time::Duration) { consume(start..end); }".into(),
    )
    .unwrap();
    let found = collect_with(
        &file,
        &Selection::parse("range").unwrap(),
        &CfgSet::unconditional(),
        &Defaults::of(&file.ast),
    );
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn stable_call_provenance_rejects_qualified_generic_and_exhausted_call_searches() {
    with_collector("fn safe() -> u8 { 0 } fn f() { safe(); }", |collector| {
        let Item::Fn(function) = &collector.file.ast.items[1] else {
            unreachable!()
        };
        let Stmt::Expr(original, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Expr::Call(original_call) = original else { unreachable!() };
        for callee in ["::safe", "safe::<u8>", "<u8 as Trait>::safe"] {
            let mut call = original_call.clone();
            *call.func = parse_str(callee).unwrap();
            assert!(collector.resource_stable_calls(&Expr::Call(call)).is_empty(), "{callee}");
        }
        let mut call = original_call.clone();
        call.args.extend(core::iter::repeat_n::<Expr>(parse_quote!(0), 256));
        assert!(collector.resource_stable_calls(&Expr::Call(call)).is_empty());
        assert!(collector.resource_stable_calls(&parse_quote!(safe())).is_empty());
    });
}

#[test]
fn stable_call_provenance_distinguishes_local_and_absolute_names_with_bound_sources() {
    let file = SourceFile::parse(
        "fixture.rs",
        "fn safe(value: usize) -> usize { value } fn f() { safe(1); ::safe(1); }".into(),
    )
    .unwrap();
    let cfg = CfgSet::unconditional();
    let mut defaults = Defaults::of_source_in(&file.ast, &cfg, file.path.as_ref());
    defaults.bind_sources(&[("fixture.rs".into(), "fixture.rs".into(), Vec::new())]);
    let selection = Selection::everything();
    let collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let Item::Fn(function) = &file.ast.items[1] else { unreachable!() };
    let Stmt::Expr(local, _) = &function.block.stmts[0] else {
        unreachable!()
    };
    let Stmt::Expr(absolute, _) = &function.block.stmts[1] else {
        unreachable!()
    };
    let local_calls = collector.resource_stable_calls(local);
    assert!(
        local_calls.contains_key("safe"),
        "the local declaration must establish a positive stable-call proof"
    );
    assert!(
        collector.resource_stable_calls(absolute).is_empty(),
        "an absolute name cannot inherit the local declaration's stable-call proof"
    );
}

#[test]
fn resource_initializers_recognize_vectors_without_guessing_element_contents() {
    with_collector(
        "fn f() {
                let chars = Vec::<char>::with_capacity(8);
                let numbers = std::vec::Vec::with_capacity(8);
                let other = String::new();
                while true {}
            }",
        |collector| {
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = function.block.stmts.last().unwrap() else {
                unreachable!()
            };
            let mut bindings = HashMap::default();
            collector.resource_source_bindings(expression, &mut bindings);
            assert_eq!(bindings.get("chars"), Some(&super::resource::Binding::Vec));
            assert_eq!(bindings.get("numbers"), Some(&super::resource::Binding::Vec));
            assert!(!bindings.contains_key("other"));
        },
    );
}

#[test]
fn resource_binding_kinds_preserve_primitive_collection_and_reference_distinctions() {
    with_collector("", |collector| {
        for (ty, expected) in [
            ("bool", Some(Binding::Bool)),
            ("u16", Some(Binding::Integer)),
            ("f32", Some(Binding::Primitive(Primitive::named("f32").unwrap()))),
            ("Vec<char>", Some(Binding::VecChar)),
            ("Vec<String>", Some(Binding::VecString)),
            ("Vec<u8>", Some(Binding::Vec)),
            ("dependency::Vec<u8>", None),
            ("&Vec<char>", Some(Binding::VecChar)),
            ("&mut Vec<String>", Some(Binding::VecString)),
            ("&str", Some(Binding::StrReference)),
            ("String", Some(Binding::String)),
            ("std::collections::HashMap<u32, bool>", Some(Binding::BoolCache)),
            ("std::collections::HashMap<char, bool>", None),
            ("std::collections::BTreeMap<char, bool>", None),
            ("std::collections::HashSet<u8>", None),
            ("std::collections::BTreeSet<u8>", None),
            ("&bool", None),
            ("&String", None),
            ("std::iter::Peekable<std::str::Chars<'static>>", Some(Binding::PeekableChars)),
            ("&mut std::iter::Peekable<std::str::Chars<'static>>", Some(Binding::PeekableChars)),
            ("&std::iter::Peekable<std::str::Chars<'static>>", None),
            ("String<u8>", None),
        ] {
            assert_eq!(collector.resource_binding(&parse_str(ty).unwrap(), &ROOT), expected, "{ty}");
        }
        collector.defaulted.push("std".into());
        assert_eq!(collector.resource_binding(&parse_quote!(std::primitive::u8), &ROOT), None);
    });
    for (source, ty) in [
        ("struct Vec<T>(T);", "Vec<u8>"),
        ("struct HashMap<K, V>(K, V);", "HashMap<char, bool>"),
        ("mod std {}", "std::primitive::u8"),
        ("mod std {}", "std::collections::HashMap<u32, bool>"),
        ("mod std {}", "::std::vec::Vec<u8>"),
    ] {
        with_collector(source, |collector| {
            assert_eq!(collector.resource_binding(&parse_str(ty).unwrap(), &ROOT), None, "{source} / {ty}");
        });
    }
    with_collector("struct char;", |collector| {
        assert_eq!(collector.resource_binding(&parse_quote!(Vec<char>), &ROOT), Some(Binding::Vec));
    });
}

#[test]
fn resource_aliases_are_lexical_nongeneric_unimported_and_cycle_bounded() {
    with_collector(
        "type Flag = bool;
         type Text<'a> = &'a str;
         type Generic<T = u8> = Vec<T>;
         type First = Second;
         type Second = First;
         fn f() { { let marker = 0; } }",
        |collector| {
            for (ty, expected) in [
                ("Flag", Some(Binding::Bool)),
                ("Flag<>", Some(Binding::Bool)),
                ("Text<'static>", Some(Binding::StrReference)),
                ("Flag<bool>", None),
                ("Flag::Associated", None),
                ("<Owner>::Flag", None),
                ("Generic", None),
                ("Generic<bool>", None),
                ("First", None),
            ] {
                assert_eq!(collector.resource_binding(&parse_str(ty).unwrap(), &ROOT), expected, "{ty}");
            }

            let nested = collector.block_scopes.iter().max_by_key(|scope| scope.len()).unwrap();
            assert!(!nested.is_empty());
            assert_eq!(collector.resource_binding(&parse_quote!(Flag), nested), Some(Binding::Bool));
        },
    );
    with_collector("type Shared = bool; fn helper() {} use helper as Shared;", |collector| {
        assert_eq!(collector.resource_binding(&parse_quote!(Shared), &ROOT), None);
    });
    with_collector("type Flag = bool; fn f() { type Flag = &'static str; }", |collector| {
        let Item::Fn(function) = &collector.file.ast.items[1] else {
            unreachable!()
        };
        collector.scope_path = vec![block_id(&function.block)];
        assert_eq!(collector.resource_binding(&parse_quote!(Flag), &ROOT), Some(Binding::Bool));
    });
}

#[test]
fn resource_alias_search_visits_a_missing_root_alias_once_before_using_the_primitive() {
    with_collector("", |collector| {
        let mut visits = 0;
        assert_eq!(
            collector.resource_binding_with(&parse_quote!(u8), &ROOT, &mut |scope| {
                assert_eq!(scope, &ROOT);
                visits += 1;
                assert_eq!(visits, 1, "the root scope has no parent to search");
            },),
            Some(super::resource::Binding::Integer)
        );
        assert_eq!(visits, 1);
    });
}

#[test]
fn resource_iterator_initializers_check_attributes_on_each_receiver_node() {
    with_collector("", |collector| {
        collector.bind_type("text".into(), parse_quote!(&str));
        assert!(collector.resource_iterator_initializer(&parse_quote!(text.chars().peekable())));
        for expression in [
            "text.bytes().peekable()",
            "text.chars(true).peekable()",
            "text.chars::<u8>().peekable()",
            "text.chars().peekable(true)",
            "text.chars().peekable::<u8>()",
            "<Owner>::text.chars().peekable()",
        ] {
            assert!(
                !collector.resource_iterator_initializer(&parse_str(expression).unwrap()),
                "{expression}"
            );
        }
        for node in 0..3 {
            let mut expression: Expr = parse_quote!(text.chars().peekable());
            let Expr::MethodCall(peekable) = &mut expression else {
                unreachable!()
            };
            let Expr::MethodCall(chars) = &mut *peekable.receiver else {
                unreachable!()
            };
            let Expr::Path(source) = &mut *chars.receiver else { unreachable!() };
            match node {
                0 => source.attrs.push(parse_quote!(#[allow(unused)])),
                1 => chars.attrs.push(parse_quote!(#[allow(unused)])),
                _ => peekable.attrs.push(parse_quote!(#[allow(unused)])),
            }
            assert!(!collector.resource_iterator_initializer(&expression), "{node}");
        }
    });
}

#[test]
fn noncopy_place_proofs_obey_the_exact_depth_limit_and_reference_reborrow_state() {
    with_collector(
        "struct S0 { value: String }
         struct S1 { value: S0 }
         struct S2 { value: S1 }
         struct S3 { value: S2 }
         struct S4 { value: S3 }
         struct S5 { value: S4 }
         struct S6 { value: S5 }
         struct S7 { value: S6 }",
        |collector| {
            let mut ty: Type = parse_quote!(String);
            for depth in 0..=8 {
                collector.bind_type("value".into(), ty.clone());
                assert_eq!(collector.noncopy_place(&parse_quote!(value)), depth < 8, "{depth}");
                ty = parse_quote!((#ty,));
            }
            for depth in 0..=7 {
                let ty = parse_str(&format!("S{depth}")).unwrap();
                assert_eq!(collector.noncopy_type(&ty, 8), depth < 7, "S{depth}");
            }
            for (ty, noncopy) in [("&String", false), ("&mut String", true), ("Vec<u8>", true), ("Box<u8>", true)] {
                collector.bind_type("value".into(), parse_str(ty).unwrap());
                let place = parse_quote!(value);
                assert_eq!(collector.noncopy_place(&place), noncopy, "{ty}");
            }
            let place: Expr = parse_quote!(value);
            collector.bind_type("value".into(), parse_quote!(&mut String));
            let _inserted = collector.numeric_evidence.reborrowable.insert(place.span().byte_range());
            assert!(!collector.noncopy_place(&place));
        },
    );
}

#[test]
fn unsigned_zero_propagation_requires_the_exact_adapter_name_and_argument_shape() {
    for (source, count) in [
        ("value.unknown(0, |_| 0)", 0),
        ("value.map_or()", 0),
        ("value.map_or(0, |_| 0, extra)", 0),
        ("value.map_or(0, |_| 0)", 2),
        ("values.map(|_| 0).sum()", 1),
        ("values.map(|_| 0).sum(extra)", 0),
        ("values.unknown(|_| 0).sum()", 0),
        ("values.map().sum()", 0),
        ("values.map(|_| 0, extra).sum()", 0),
        ("values.map(|_| 0).filter(predicate).sum()", 1),
        ("values.map(|_| 0).map(transform).sum()", 0),
    ] {
        let expression = parse_str(source).unwrap();
        let mut spans = Vec::new();
        super::unsigned_zero_spans(&expression, &mut spans);
        assert_eq!(spans.len(), count, "{source}: {spans:?}");
        for range in spans {
            assert_eq!(&source[range], "0", "{source}");
        }
    }
}

#[test]
fn expected_numeric_reductions_are_detected_in_each_value_branch_but_not_discarded_statements() {
    for (source, expected) in [
        ("(values.sum())", true),
        ("{ values.product() }", true),
        ("if flag { values.sum() } else { 0 }", true),
        ("if flag { 0 } else { values.product() }", true),
        ("if flag { values.sum(); } else { 0 }", false),
        ("match flag { true => values.sum(), false => 0 }", true),
        ("match flag { true => 0, false => 1 }", false),
        ("{ values.sum(); }", false),
    ] {
        assert_eq!(
            super::needs_expected_numeric_type(&parse_str(source).unwrap()),
            expected,
            "{source}"
        );
    }
}

#[test]
fn mandatory_tail_statements_keep_complete_semicolon_statements_in_every_branch() {
    for (source, expected) in [
        ("if flag { call(); } else { other(); }", vec!["call();", "other();"]),
        ("if flag { { nested(); } } else { { other(); } }", vec!["nested();", "other();"]),
        ("({ final_call(); })", vec!["final_call();"]),
        ("match flag { true => { one(); }, false => { two(); } }", vec!["one();", "two();"]),
        ("{ discarded(); value }", vec![]),
    ] {
        let expression = parse_str(source).unwrap();
        let mut spans = Vec::new();
        super::mandatory_tail_statements(&expression, &mut spans);
        let actual = spans.iter().map(|range| &source[range.clone()]).collect::<Vec<_>>();
        assert_eq!(actual, expected, "{source}");
    }
}

#[test]
fn reusable_payloads_choose_the_first_compatible_name_and_reach_wrapped_value_branches() {
    with_collector("", |collector| {
        let expected: Type = parse_quote!(Option<core::num::NonZeroU8>);
        let parameters = [
            ("z".to_owned(), parse_quote!(core::num::NonZeroU8)),
            ("a".to_owned(), parse_quote!(core::num::NonZeroU8)),
            ("unrelated".to_owned(), parse_quote!(bool)),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        for (source, count) in [
            ("None", 1),
            ("(None)", 1),
            ("{ None }", 1),
            ("if flag { None } else { None }", 2),
            ("match flag { true => None, false => None }", 2),
            ("Other", 0),
        ] {
            let expression = parse_str(source).unwrap();
            let mut spans = Vec::new();
            super::reusable_payload_spans(&expression, &expected, &collector.types(), &parameters, &mut spans);
            assert_eq!(spans.len(), count, "{source}: {spans:?}");
            for (range, name) in spans {
                assert_eq!(&source[range], "None", "{source}");
                assert_eq!(name, "a", "{source}");
            }
        }
        let mut spans = Vec::new();
        super::admissible_default_spans(&parse_quote!(None), &parse_quote!(Vec<u8>), &collector.types(), &mut spans);
        assert!(spans.is_empty());
    });
}

#[test]
fn nested_expression_content_ranges_end_at_the_expression_not_after_its_closing_token() {
    let source = "fn f() { #[allow(unused)] { #[allow(dead_code)] value(); } }";
    let file = parse_file(source).unwrap();
    let outer = span(source, "#[allow(unused)] { #[allow(dead_code)] value(); }");
    let inner = span(source, "#[allow(dead_code)] value()");
    assert_eq!(
        super::expression_contents(&file),
        [
            (outer.clone(), span(source, "#[allow(unused)]").end..outer.end),
            (inner.clone(), span(source, "#[allow(dead_code)]").end..inner.end),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>()
    );
}

#[test]
fn iterator_item_inference_checks_every_namespace_component_before_extracting_payloads() {
    with_collector("", |collector| {
        assert_eq!(
            collector.inferred_iterator_item(&parse_quote!(core::str::Chars<'a>)),
            Some(parse_quote!(char))
        );
        assert_eq!(
            collector.inferred_iterator_item(&parse_quote!(std::slice::Iter<'a, bool>)),
            Some(parse_quote!(&bool))
        );
        for (module, name) in [("str", "Chars"), ("slice", "Iter")] {
            for parts in [["foreign", module, name], ["std", "other", name], ["std", module, "Other"]] {
                let ty = format!("{}<'a, bool>", parts.join("::"));
                assert_eq!(collector.inferred_iterator_item(&parse_str(&ty).unwrap()), None, "{ty}");
            }
        }
    });
}

#[test]
fn inferred_structs_references_and_parentheses_keep_exact_types_and_qualified_self_boundaries() {
    with_collector("struct Record { value: bool } mod items { pub struct Unit; }", |collector| {
        let Expr::Path(qualified) = parse_quote!(<Owner as items>::Unit) else {
            unreachable!()
        };
        assert!(qualified.qself.is_some());
        assert!(qualified.path.leading_colon.is_none());
        assert!(collector.calls.structure(&qualified.path, &ROOT, collector.current_imports()));
        for (expression, expected) in [
            ("Record { value: true }", Some(parse_quote!(Record))),
            ("<Owner as Trait>::Record { value: true }", None),
            ("<Owner>::Record", None),
            ("<Owner as items>::Unit", None),
            ("(true)", Some(parse_quote!(bool))),
            ("&true", Some(parse_quote!(&(bool)))),
            ("&mut true", Some(parse_quote!(&mut (bool)))),
        ] {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                expected,
                "{expression}"
            );
        }
        collector.bind("items".into(), false);
        assert_eq!(collector.inferred_expression_type(&parse_quote!(items::Unit)), None);
    });
}

#[test]
fn iterator_method_inference_needs_an_iter_call_on_a_standard_vector_or_slice() {
    with_collector("struct Vec<T>(T);", |collector| {
        collector.bind_type("custom".into(), parse_quote!(Vec<bool>));
        assert_eq!(collector.inferred_expression_type(&parse_quote!(custom.iter())), None);
    });
    with_collector("", |collector| {
        collector.bind_type("vector".into(), parse_quote!(Vec<bool>));
        collector.bind_type("queue".into(), parse_quote!(std::collections::VecDeque<bool>));
        collector.bind_type("unknown".into(), parse_quote!(Foreign<bool>));
        for expression in ["vector.unknown()", "vector.iter(true)", "queue.iter()", "unknown.iter()"] {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                None,
                "{expression}"
            );
        }
    });
}

#[test]
fn parenthesized_string_factories_and_imported_duration_defaults_reach_fallback_inference() {
    with_collector("use std::time::Duration as Span;", |collector| {
        for (expression, expected) in [
            ("(String::new)()", parse_quote!(String)),
            ("Span::default()", parse_quote!(std::time::Duration)),
        ] {
            assert_eq!(
                collector.inferred_call_type(&parse_str(expression).unwrap()),
                Some(expected),
                "{expression}"
            );
        }
    });
}

#[test]
fn implicit_unit_method_returns_do_not_supply_a_nonexistent_explicit_output_type() {
    with_collector("struct Owner; impl Owner { fn unit(&self) {} }", |collector| {
        collector.bind_type("owner".into(), parse_quote!(Owner));
        assert_eq!(collector.inferred_expression_type(&parse_quote!(owner.unit())), None);
    });
}

#[test]
fn source_call_queries_reject_empty_module_aliases_and_never_unify_distinct_external_types() {
    with_calls("use self as root;", |_, calls, imports| {
        assert!(!calls.is_enum(&parse_quote!(root), &ROOT, imports));
        assert!(calls.signature(&parse_quote!(root), &ROOT, imports, None, |_| false).is_none());
        assert!(!calls.same_type(&parse_quote!(::std::fs::File), &parse_quote!(::std::path::PathBuf), &ROOT, imports));
        assert!(matches!(
            calls.operator_result(&parse_quote!(dependency::Unknown), &parse_quote!(bool), "+", &ROOT, imports),
            OperatorResult::Unknown
        ));
    });
}

#[test]
fn closed_operator_inventory_cannot_prove_absence_for_a_nonpath_owner() {
    with_calls("struct Owner;", |_, calls, imports| {
        assert!(calls.unit_operator_absent(&parse_quote!(Owner), "+", &ROOT, imports));
        for ty in [parse_quote!(&Owner), parse_quote!((Owner,)), parse_quote!([Owner; 1])] {
            assert!(!calls.unit_operator_absent(&ty, "+", &ROOT, imports), "{ty:?}");
        }
    });
}

#[test]
fn source_fields_canonicalize_imported_primitive_aliases_before_reporting_their_types() {
    with_calls(
        "use core::primitive::u8 as Byte; struct Record { value: Byte }",
        |_, calls, imports| {
            assert_eq!(
                calls.field(&parse_quote!(Record), &parse_quote!(value), &ROOT, imports),
                Some(parse_quote!(core::primitive::u8))
            );
        },
    );
}

#[test]
fn lexical_numeric_evidence_reaches_each_expression_continuation_without_inventing_unsignedness() {
    for source in [
        "fn f(value: Unknown) { if ready {} else { value - 1; } }",
        "fn f(value: Unknown) { if let Some(item) = make(value - 1) {} }",
        "fn f(value: Unknown) { for item in inputs[value..] {} }",
        "fn f(value: Unknown) { make(value - 1).field; }",
        "fn f(value: Unknown) { value.wrapping_add(1); }",
        "fn f(value: Unknown) { let mut output; output = value - 1; }",
        "fn f(value: Unknown) { let result = loop { break value - 1; }; }",
        "#[cfg(any())] fn hidden(dead: Unknown) { dead - 1; }
         fn f(value: Unknown) { value - 1; }",
    ] {
        let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
        assert_eq!(
            evidence.numeric,
            source.match_indices("value").map(|(start, _)| start..start + 5).collect(),
            "{source}"
        );
        assert_eq!(evidence.unsigned.is_empty(), !source.contains("inputs[value..]"), "{source}");
    }
    let source = "fn f(value: Unknown) { value.custom(); }";
    let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
    assert!(evidence.numeric.is_empty());
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn lexical_method_argument_and_consuming_receiver_moves_are_independent() {
    for (source, moved) in [
        (
            "fn f(mut value: String, target: Unknown) { target.consume(value); value = make(); use_value(value); }",
            true,
        ),
        (
            "fn f(mut value: Vec<u8>) { value.into_iter(); value = make(); use_value(value); }",
            true,
        ),
        (
            "fn f(mut value: String) { value.is_empty(); value = make(); use_value(value); }",
            false,
        ),
    ] {
        let evidence = lexical::evidence(&parse_file(source).unwrap(), &CfgSet::parse(""));
        let site = span(source, "value = make()");
        assert_eq!(evidence.reinitializations.contains_key(&site), moved, "{source}");
        if moved {
            assert_eq!(
                evidence.reinitializations[&site]
                    .iter()
                    .map(|expression| &source[expression.span().byte_range()])
                    .collect::<Vec<_>>(),
                ["value"],
                "{source}"
            );
        }
    }
}

#[test]
fn lexical_for_and_while_restorations_retain_their_carried_binding_backedge() {
    for body in [
        "for item in 0..3 { consume(value); value = make(); }",
        "while ready { consume(value); value = make(); }",
    ] {
        let source = format!("fn f(mut value: String) {{ {body} }}");
        let evidence = lexical::evidence(&parse_file(&source).unwrap(), &CfgSet::parse(""));
        let site = span(&source, "value = make()");
        assert!(evidence.invalid_ownership.contains_key(&site), "{body}");
        assert_eq!(evidence.ownership_regions.get(&site), Some(&span(&source, body)), "{body}");
    }
}

#[test]
fn configured_out_free_function_signatures_are_not_recorded() {
    with_calls("#[cfg(any())] fn hidden() -> bool { true }", |_, calls, imports| {
        assert!(calls.signature(&parse_quote!(hidden), &ROOT, imports, None, |_| false).is_none());
    });
}

#[test]
fn ownership_field_queries_reject_unknown_qualified_and_ambiguous_record_layouts() {
    with_collector(
        "mod first { struct Record { value: String } }
         mod second { struct Record { value: bool } }",
        |collector| {
            for (name, ty) in [
                ("unknown", parse_quote!(Missing)),
                ("qualified", parse_quote!(first::Record)),
                ("ambiguous", parse_quote!(Record)),
            ] {
                collector.bind_type(name.into(), ty);
            }
            for expression in ["unknown.value", "qualified.value", "ambiguous.value", "unbound.value"] {
                assert!(collector.ownership_type(&parse_str(expression).unwrap()).is_none(), "{expression}");
            }
        },
    );
    with_collector("struct Record { value: String }", |collector| {
        collector.bind_type("record".into(), parse_quote!(Record));
        let group = Expr::Group(ExprGroup {
            attrs: Vec::new(),
            group_token: Group::default(),
            expr: Box::new(parse_quote!(record.value)),
        });
        assert_eq!(collector.ownership_type(&group), Some(&parse_quote!(String)));
    });
}

#[test]
fn ownership_fields_do_not_reuse_same_named_layouts_across_import_or_generic_shadows() {
    for source in [
        "mod other { struct Record { value: String } } use dependency::Record;",
        "mod other { struct Record { value: String } } use dependency::*;",
    ] {
        with_collector(source, |collector| {
            collector.bind_type("record".into(), parse_quote!(Record));
            assert!(collector.ownership_type(&parse_quote!(record.value)).is_none(), "{source}");
        });
    }
    with_collector("struct Record { value: String }", |collector| {
        collector.bind_type("record".into(), parse_quote!(Record));
        collector.generics.push("Record".into());
        assert!(collector.ownership_type(&parse_quote!(record.value)).is_none());
    });
}

#[test]
fn ownership_admission_uses_any_noncopy_place_and_statement_deletion_checks_moves_not_transitions() {
    let source = "fn f(value: String) { consume(value); }";
    with_collector(source, |collector| {
        collector.bind_type("value".into(), parse_quote!(String));
        collector.bind_type("copy".into(), parse_quote!(bool));
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let statement = function.block.stmts[0].clone();
        let region = function.block.span().byte_range();
        let Stmt::Expr(expression, Some(_)) = &statement else {
            unreachable!()
        };
        let site = expression.span().byte_range();
        let places = vec![parse_quote!(copy), parse_quote!(value)];
        collector.numeric_evidence.invalid_ownership.insert(site.clone(), places.clone());
        assert!(!collector.admit_ownership_edit(site.clone(), &site, false));
        collector.statement(&statement);
        assert!(collector.candidates.is_empty());
        collector.numeric_evidence.invalid_ownership.clear();
        collector.numeric_evidence.reinitializations.insert(site.clone(), places);
        collector.numeric_evidence.ownership_regions.insert(site.clone(), region.clone());
        assert!(collector.admit_ownership_edit(site.clone(), &site, false));
        assert_eq!(collector.ownership_lifts.get(&site), Some(&region));
    });
}

#[test]
fn wrapped_iterator_absent_function_values_use_plain_blocks_and_present_values_keep_wrapping() {
    let source = "fn f() -> Option<impl Iterator<Item = u8>> { Some(1..3) }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let candidates = collect_with(&file, &Selection::everything(), &CfgSet::unconditional(), &Defaults::of(&file.ast));
    let values = candidates
        .iter()
        .filter(|candidate| candidate.mutator.starts_with("fn_value."))
        .collect::<Vec<_>>();
    assert!(!values.is_empty());
    assert!(values.iter().any(|candidate| candidate.replacement == "None"));
    assert!(values.iter().any(|candidate| !super::absent_iterator_value(&candidate.replacement)));
    for candidate in values {
        assert_eq!(
            candidate.shape,
            if super::absent_iterator_value(&candidate.replacement) {
                super::Shape::Block
            } else {
                super::Shape::IterWrappedBlock {
                    depth: NonZeroU8::new(1).unwrap(),
                }
            },
            "{}",
            candidate.replacement
        );
    }

    let file = SourceFile::parse("fixture.rs", "fn f() -> Option<u8> { Some(2) }".into()).unwrap();
    let candidates = collect_with(&file, &Selection::everything(), &CfgSet::unconditional(), &Defaults::of(&file.ast));
    assert!(
        candidates
            .iter()
            .filter(|candidate| candidate.mutator.starts_with("fn_value."))
            .all(|candidate| candidate.shape == super::Shape::Block)
    );
}

#[test]
fn character_receiver_resolves_reference_aliases_and_stops_generic_parameter_name_cycles() {
    with_collector(
        "type Character = char;
         type Borrowed<'a> = &'a Character;
         type Alias<'a, Alias> = &'a Alias;",
        |collector| {
            collector.bind_type("character".into(), parse_quote!(Character));
            collector.bind_type("borrowed".into(), parse_quote!(Borrowed<'static>));
            collector.bind_type("noncharacter".into(), parse_quote!(Alias<'static, u8>));
            assert!(collector.character_receiver(&parse_quote!(character)));
            assert!(collector.character_receiver(&parse_quote!(borrowed)));
            assert!(!collector.character_receiver(&parse_quote!(noncharacter)));
        },
    );
}

#[test]
fn character_literals_do_not_bypass_visible_aliases_of_their_inferred_type_name() {
    with_collector("type char = u8;", |collector| {
        let literal = parse_quote!('x');
        let inferred = collector.inferred_expression_type(&literal).unwrap();
        assert_eq!(inferred, parse_quote!(char));
        assert!(collector.types().supports_char_api());
        assert_eq!(collector.types().primitive_kind(&inferred), super::values::Kind::Unsigned);
        assert!(!collector.character_receiver(&literal));
    });
}

#[test]
fn character_literal_alias_inference_stops_when_a_generic_parameter_reenters_its_alias_name() {
    with_collector("type char<char: 'static> = &'static char;", |collector| {
        let mut visits = Vec::new();
        assert!(!collector.character_receiver_with(&parse_quote!('x'), |ty| {
            visits.push(ty);
            assert!(visits.len() <= 2, "the same reference target cannot be followed a third time");
        }));
        assert_eq!(visits.len(), 2);
        assert_eq!(visits[0], visits[1], "both lookups must reach the same real published alias target");
    });
}

#[test]
fn floating_evidence_requires_numeric_factories_and_arithmetic_or_negation_continuations() {
    with_collector("", |collector| {
        for (expression, floating) in [
            ("f64::from(1)", true),
            ("f32::unknown()", false),
            ("u32::from(1)", false),
            ("(1.0)", true),
            ("-1.0", true),
            ("!1.0", false),
            ("1.0 + unknown", true),
            ("unknown + 1.0", true),
            ("1.0 - unknown", true),
            ("1.0 * unknown", true),
            ("1.0 / unknown", true),
            ("1.0 % unknown", true),
            ("1.0 == unknown", false),
            ("1.0 < unknown", false),
        ] {
            assert_eq!(collector.is_known_float(&parse_str(expression).unwrap()), floating, "{expression}");
        }
    });
}

#[test]
fn reusable_payload_queries_prefer_the_latest_still_unshadowed_parameter() {
    with_collector("", |collector| {
        let site = 10..20;
        for name in ["first", "last"] {
            collector.bind_type(name.into(), parse_quote!(bool));
            collector.parameter_types.insert(name.into(), parse_quote!(bool));
            collector.reusable_payloads.push((site.clone(), name.into()));
        }
        assert_eq!(collector.reusable_payload(&site), Some("last"));
        collector.bind_type("last".into(), parse_quote!(char));
        assert_eq!(collector.reusable_payload(&site), Some("first"));
        assert_eq!(collector.reusable_payload(&(11..20)), None);
    });
}

#[test]
fn pattern_binding_discards_shadowed_types_and_keeps_unsigned_evidence_and_type_annotations_separate() {
    for unsigned in [false, true] {
        with_collector("", |collector| {
            let pattern = Pat::parse_single.parse_str("value").unwrap();
            collector.bind_type("value".into(), parse_quote!(bool));
            collector.numeric_evidence.numeric.insert(pattern.span().byte_range());
            if unsigned {
                collector.numeric_evidence.unsigned.insert(pattern.span().byte_range());
            }
            collector.bind_pattern(&pattern, None);
            assert_eq!(collector.bindings.get("value"), Some(&true));
            assert_eq!(collector.inferred_unsigned.contains("value"), unsigned);
            assert!(collector.type_of_place(&parse_quote!(value)).is_none());
        });
    }
    with_collector("", |collector| {
        let statement: Stmt = parse_quote!(let value: u16 = 1;);
        let Stmt::Local(local) = statement else { unreachable!() };
        collector.bind_pattern(&local.pat, None);
        assert_eq!(collector.bindings.get("value"), Some(&true));
        assert_eq!(collector.type_of_place(&parse_quote!(value)), Some(&parse_quote!(u16)));
    });
}

#[test]
fn unsupported_patterns_clear_every_nested_binding_without_claiming_numeric_types() {
    for source in ["[first, outer @ Some(inner)]", "Owner { field: outer @ Some(inner), .. }"] {
        with_collector("", |collector| {
            let pattern = Pat::parse_single.parse_str(source).unwrap();
            let names = if source.starts_with('[') {
                &["first", "outer", "inner"][..]
            } else {
                &["outer", "inner"][..]
            };
            for name in names {
                collector.bind((*name).into(), true);
                collector.bind_type((*name).into(), parse_quote!(u16));
            }
            collector.bind_pattern(&pattern, None);
            for name in names {
                assert_eq!(collector.bindings.get(*name), Some(&false), "{source}: {name}");
                assert!(collector.type_of_place(&parse_str(name).unwrap()).is_none(), "{source}: {name}");
            }
        });
    }
}

#[test]
fn tuple_rest_alignment_preserves_prefix_types_and_missing_payloads_remain_unknown() {
    with_collector("", |collector| {
        let pattern = Pat::parse_single.parse_str("(first, .., last)").unwrap();
        collector.bind_pattern(&pattern, Some(&parse_quote!((u8, bool, String, char))));
        assert_eq!(collector.type_of_place(&parse_quote!(first)), Some(&parse_quote!(u8)));
        assert_eq!(collector.type_of_place(&parse_quote!(last)), Some(&parse_quote!(char)));
    });
    with_collector("", |collector| {
        let pattern = Pat::parse_single.parse_str("(first, .., missing, last)").unwrap();
        collector.bind_pattern(&pattern, Some(&parse_quote!((u8,))));
        assert!(collector.type_of_place(&parse_quote!(missing)).is_none());
        assert_eq!(collector.type_of_place(&parse_quote!(last)), Some(&parse_quote!(u8)));
    });
}

#[test]
fn nested_function_reader_bounds_hide_then_restore_outer_generics_without_leaking_inner_bounds() {
    with_collector("", |collector| {
        let outer: Generics = parse_quote!(<Outer: std::io::Read>);
        let inner: Generics = parse_quote!(<Inner: std::io::BufRead>);
        collector.with_function_parameters(&outer, |collector| {
            assert_eq!(collector.reader_bounded, ["Outer"]);
            collector.with_function_parameters(&inner, |collector| {
                assert_eq!(collector.reader_bounded, ["Inner"]);
            });
            assert_eq!(collector.reader_bounded, ["Outer"]);
        });
        assert!(collector.reader_bounded.is_empty());
        assert_eq!(collector.function_bound_depth, None);
    });
}

#[test]
fn method_function_context_keeps_parameters_after_self_and_initializes_unsigned_return_state() {
    with_collector("struct Owner;", |collector| {
        collector.impl_self_type = Some(parse_quote!(Owner));
        let signature: Signature = parse_quote!(fn method(&self, value: u16) -> u8);
        collector.in_function(&signature, |collector| {
            assert_eq!(collector.type_of_place(&parse_quote!(self)), Some(&parse_quote!(&Owner)));
            assert_eq!(collector.type_of_place(&parse_quote!(value)), Some(&parse_quote!(u16)));
            assert_eq!(collector.parameter_types.get("value"), Some(&parse_quote!(u16)));
            assert!(matches!(collector.unsigned_return, super::UnsignedReturn::BodyPending));
        });
        assert!(collector.binding_types.is_empty());
        assert!(collector.parameter_types.is_empty());
    });
}

#[test]
fn parameter_shadowing_keeps_iterator_shapes_skips_ref_bindings_and_withholds_other_opaque_returns() {
    let source = "fn f(ref skipped: u8, value: bool) -> impl Iterator<Item = u8> { 0..3 }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::parse("parameter.default_shadow").unwrap();
    let candidates = collect_with(&file, &selection, &CfgSet::unconditional(), &Defaults::of(&file.ast));
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].replacement_index, 1);
    assert_eq!(candidates[0].shape, super::Shape::IterBlock);
    assert_eq!(candidates[0].replacement, "{ let value: bool = Default::default(); 0..3 }");

    let file = SourceFile::parse("fixture.rs", "fn f(value: u8) -> impl core::fmt::Display { value }".into()).unwrap();
    assert!(collect_with(&file, &selection, &CfgSet::unconditional(), &Defaults::of(&file.ast)).is_empty());
}

#[test]
fn unsigned_constructor_arguments_require_each_namespace_component_type_family_and_argument_position() {
    let imports = HashMap::default();
    for (callee, index, unsigned) in [
        ("std::time::Duration::new", 0, true),
        ("std::time::Duration::new", 1, true),
        ("std::time::Duration::new", 2, false),
        ("std::time::Duration::from_secs", 0, true),
        ("std::time::Duration::from_secs", 1, false),
        ("core::time::Duration::from_nanos", 0, true),
        ("foreign::time::Duration::new", 0, false),
        ("std::other::Duration::new", 0, false),
        ("std::time::Other::new", 0, false),
        ("foreign::NonZeroU8::new", 0, false),
        ("std::num::NonZeroU8::new", 0, true),
        ("std::num::Other::new", 0, false),
        ("std::sync::atomic::AtomicU8::new", 0, true),
        ("std::sync::atomic::Other::new", 0, false),
        ("char::from_u32", 0, true),
        ("other::from_u32", 0, false),
        ("new", 0, false),
        ("(factory)", 0, false),
    ] {
        assert_eq!(
            super::standard_unsigned_argument(&parse_str(callee).unwrap(), index, &imports),
            unsigned,
            "{callee}, {index}"
        );
    }
}

#[test]
fn regex_end_anchors_distinguish_escaped_dollars_and_class_negation_does_not_reenter_nested_classes() {
    assert!(super::regex_mutations(r"ab\$").is_empty());
    assert!(super::regex_mutations(r"\*").is_empty());
    assert_eq!(super::regex_mutations(r"ab\\$"), [("regex.remove_end_anchor", r"ab\\".to_owned())]);
    for (pattern, replacement) in [(r"\d$", r"\d"), ("x$", "x"), ("\0$", "\0")] {
        assert_eq!(
            super::regex_mutations(pattern),
            [("regex.remove_end_anchor", replacement.to_owned())],
            "{pattern:?}"
        );
    }
    assert_eq!(
        super::regex_mutations("[a[b]]"),
        [("regex.negate_character_class", "[^a[b]]".to_owned())]
    );
}

#[test]
fn item_impl_and_trait_item_visitors_restore_resource_transparency_after_attributed_declarations() {
    with_collector(
        "#[custom] fn decorated() {}
         struct Owner;
         impl Owner { #[custom] fn decorated() {} }
         trait Contract { #[custom] fn decorated() {} }",
        |collector| {
            assert!(collector.resource_transparent);
            let item = collector.file.ast.items[0].clone();
            syn::visit::Visit::visit_item(collector, &item);
            assert!(collector.resource_transparent);
            let Item::Impl(implementation) = &collector.file.ast.items[2] else {
                unreachable!()
            };
            let item = implementation.items[0].clone();
            syn::visit::Visit::visit_impl_item(collector, &item);
            assert!(collector.resource_transparent);
            let Item::Trait(contract) = &collector.file.ast.items[3] else {
                unreachable!()
            };
            let item = contract.items[0].clone();
            syn::visit::Visit::visit_trait_item(collector, &item);
            assert!(collector.resource_transparent);
        },
    );
}

#[test]
fn parenthesized_call_results_keep_signature_types_that_places_cannot_supply() {
    with_collector("fn number() -> u8 { 0 }", |collector| {
        for source in ["number()", "(number())"] {
            let expression = parse_str(source).unwrap();
            assert_eq!(collector.type_of_place(&expression), None);
            assert_eq!(collector.type_of_expression(&expression), Some(&parse_quote!(u8)));
        }
    });
}

#[test]
fn opaque_outer_blocks_disable_resource_proofs_for_nested_unattributed_loops() {
    for (attributes, retained) in [("", false), ("#[custom]", true)] {
        let source = format!("fn f() {{ {attributes} {{ loop {{ break; }} }} }}");
        with_collector(&source, |collector| {
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(outer, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            let Expr::Block(block) = outer else { unreachable!() };
            let Stmt::Expr(inner, _) = &block.block.stmts[0] else {
                unreachable!()
            };
            let Expr::Loop(looping) = inner else { unreachable!() };
            let Stmt::Expr(exit, _) = &looping.body.stmts[0] else {
                unreachable!()
            };
            let proof = collector.resource_proof(inner).unwrap();
            assert!(proof.omits(&source, &exit.span().byte_range(), "continue"));
            let outer = outer.clone();
            syn::visit::Visit::visit_expr(collector, &outer);
            assert_eq!(
                collector
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.mutator == "loop.break_to_continue")
                    .count(),
                usize::from(retained),
                "{source}"
            );
        });
    }
}

#[test]
fn loop_resource_proofs_require_the_implicit_standard_prelude() {
    for (source, expected) in [
        ("fn f() { loop { break; } }", true),
        ("#![no_implicit_prelude] fn f() { loop { break; } }", false),
    ] {
        with_collector(source, |collector| {
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            assert_eq!(collector.resource_proof(expression).is_some(), expected, "{source}");
        });
    }
}

#[test]
fn closure_payload_inference_selects_method_argument_and_result_branch_without_guessing_other_receivers() {
    for (expression, numeric) in [
        ("option.map(|value| consume(value))", true),
        ("option.and_then(|value| consume(value))", true),
        ("option.is_some_and(|value| consume(value))", true),
        ("option.map_or(0, |value| consume(value))", true),
        ("option.map_or_else(|| 0, |value| consume(value))", true),
        ("result.map(|value| consume(value))", false),
        ("result.map_err(|value| consume(value))", true),
        ("result.or_else(|value| consume(value))", true),
        ("option.or_else(|value| consume(value))", false),
        ("other.map(|value| consume(value))", false),
        ("iterator.any(|&value| consume(value))", true),
        ("iterator.all(|&value| consume(value))", true),
        ("iterator.other(|&value| consume(value))", false),
        ("iterator.any(|&value| consume(value), unknown)", false),
        ("option.map()", false),
        ("option.map_or(0)", false),
        ("incomplete.map(|value| consume(value))", false),
        ("wrong_arity.or_else(|value| consume(value))", false),
    ] {
        let source = format!("fn f() {{ {expression}; }}");
        with_collector(&source, |collector| {
            for (name, ty) in [
                ("option", parse_quote!(Option<u8>)),
                ("result", parse_quote!(Result<bool, u8>)),
                ("other", parse_quote!(Other<u8>)),
                ("iterator", parse_quote!(std::slice::Iter<'static, u8>)),
                ("incomplete", parse_quote!(Option)),
                ("wrong_arity", parse_quote!(Option<bool, u8>)),
            ] {
                collector.bind_type(name.into(), ty);
            }
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            let expression = expression.clone();
            syn::visit::Visit::visit_expr(collector, &expression);
            let found = collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator == "expr.increment" && &source[candidate.span.clone()] == "value")
                .count();
            assert_eq!(found, usize::from(numeric), "{source}");
            assert!(collector.closure_inputs.is_empty(), "{source}");
        });
    }
}

#[test]
fn range_endpoint_replacements_keep_float_units_when_only_one_endpoint_has_known_type() {
    for (range, replacement) in [
        ("1..5", "(1)..((5) + 1)"),
        ("1..=5", "(1)..=((5) - 1)"),
        ("..5", "..((5) + 1)"),
        ("..=5", "..=((5) - 1)"),
        ("dependency::left()..2.0", "(dependency::left())..((2.0) + 1.0)"),
        ("0.0..dependency::right()", "(0.0)..((dependency::right()) + 1.0)"),
        ("0.0..=dependency::right()", "(0.0)..=((dependency::right()) - 1.0)"),
    ] {
        let source = format!("fn f() {{ let _ = {range}; }}");
        with_collector(&source, |collector| {
            syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
            let found = collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator.starts_with("range."))
                .map(|candidate| (&source[candidate.span.clone()], candidate.replacement.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(found, [(range, replacement)], "{source}");
            if range == "1..5" {
                assert!(
                    collector
                        .candidates
                        .iter()
                        .any(|candidate| candidate.mutator == "literal.int_increment" && &source[candidate.span.clone()] == "5")
                );
            }
        });
    }
}

#[test]
fn range_endpoint_admission_uses_add_only_for_half_open_and_sub_only_for_closed_ranges() {
    for (operation, method, admitted, rejected) in [
        ("Add", "add", "start..end", "start..=endpoint(7)"),
        ("Sub", "sub", "start..=end", "start..endpoint(7)"),
    ] {
        let source = format!(
            "struct Endpoint;
             impl core::ops::{operation}<i32> for Endpoint {{
                 type Output = Self;
                 fn {method}(self, _: i32) -> Self {{ self }}
             }}
             fn endpoint(_: i32) -> Endpoint {{ Endpoint }}
             fn f(start: Endpoint, end: Endpoint) {{ let _ = {admitted}; let _ = {rejected}; }}"
        );
        with_collector(&source, |collector| {
            syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
            let ranges = collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator.starts_with("range."))
                .map(|candidate| &source[candidate.span.clone()])
                .collect::<Vec<_>>();
            assert_eq!(ranges, [admitted], "{source}");
            assert!(
                collector
                    .candidates
                    .iter()
                    .any(|candidate| candidate.mutator == "literal.int_increment" && &source[candidate.span.clone()] == "7"),
                "{source}"
            );
        });
    }
}

#[test]
fn direct_arm_visits_traverse_active_bodies_shadow_bindings_and_leave_inactive_bodies_unvisited() {
    let source = "fn f() { match true { value => consume(value, true), #[cfg(any())] _ => false } }";
    with_collector(source, |collector| {
        collector.bind("value".into(), true);
        collector.bind_type("value".into(), parse_quote!(u32));
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Match(expression), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let arms = expression.arms.clone();
        for arm in &arms {
            syn::visit::Visit::visit_arm(collector, arm);
        }
        let booleans = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "literal.bool_flip")
            .map(|candidate| (&source[candidate.span.clone()], candidate.replacement.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(booleans, [("true", "false")]);
        assert!(
            collector
                .candidates
                .iter()
                .all(|candidate| candidate.mutator != "expr.increment" || &source[candidate.span.clone()] != "value")
        );
        assert_eq!(collector.type_of_place(&parse_quote!(value)), Some(&parse_quote!(u32)));
    });
}

#[test]
fn match_tail_protection_combines_explicit_and_typed_nonunit_evidence_and_restores_outer_state() {
    for (first, protected) in [("1", true), ("value", true), ("unit", false), ("unknown", false)] {
        let source = format!("fn f() {{ let _ = match choose(0) {{ true => {first}, false => {{ dependency::abort(); }} }}; }}");
        with_collector(&source, |collector| {
            collector.bind_type("value".into(), parse_quote!(u8));
            collector.bind_type("unit".into(), parse_quote!(()));
            collector.bind_type("unknown".into(), parse_quote!(dependency::Unknown));
            collector.undeletable_statements.push(0..1);
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            let expression = local.init.as_ref().unwrap().expr.clone();
            syn::visit::Visit::visit_expr(collector, &expression);
            assert_eq!(collector.undeletable_statements.len(), 1, "{source}");
            assert_eq!(collector.undeletable_statements[0], 0..1, "{source}");
            let deleted = collector
                .candidates
                .iter()
                .any(|candidate| candidate.mutator == "stmt.delete_call" && &source[candidate.span.clone()] == "dependency::abort();");
            assert_eq!(deleted, !protected, "{source}");
            assert!(
                collector
                    .candidates
                    .iter()
                    .any(|candidate| candidate.mutator == "literal.int_increment" && &source[candidate.span.clone()] == "0"),
                "{source}"
            );
        });
    }
}

#[test]
fn numeric_confidence_uses_bare_constant_and_binding_evidence_without_capturing_qualified_self_names() {
    with_collector("const LIMIT: u32 = 7;", |collector| {
        collector.bind_type("count".into(), parse_quote!(u32));
        collector.bind("count".into(), true);
        assert_eq!(collector.type_of_expression(&parse_quote!((count))), Some(&parse_quote!(u32)));
        for (expression, confidence) in [
            ("LIMIT", Confidence::Proven),
            ("count", Confidence::Proven),
            ("(count)", Confidence::Proven),
            ("<T>::LIMIT", Confidence::Optimistic),
            ("unknown", Confidence::Optimistic),
        ] {
            assert_eq!(
                collector.numeric_confidence(&parse_str(expression).unwrap()),
                confidence,
                "{expression}"
            );
        }
        assert!(collector.is_known_numeric(&parse_quote!((count))));
        assert!(!collector.is_known_numeric(&parse_quote!(<T>::LIMIT)));
        collector.bind_type("text".into(), parse_quote!(String));
        collector.bind_type("time".into(), parse_quote!(std::time::Duration));
        for expression in ["text - unknown", "unknown - time"] {
            assert!(!collector.is_known_numeric(&parse_str(expression).unwrap()), "{expression}");
        }
        assert!(collector.is_known_numeric(&parse_quote!(left - right)));
    });
}

#[test]
fn numeric_confidence_keeps_uniform_constant_kinds_when_exact_types_conflict() {
    with_collector("mod one { const LIMIT: u8 = 1; } mod two { const LIMIT: u16 = 2; }", |collector| {
        assert_eq!(collector.constants.get("LIMIT"), Some(&true));
        assert_eq!(collector.declared_types.get("LIMIT"), Some(&None));
        for expression in ["LIMIT", "(LIMIT)"] {
            let expression = parse_str(expression).unwrap();
            assert!(collector.type_of_expression(&expression).is_none());
            assert_eq!(collector.numeric_confidence(&expression), Confidence::Proven);
        }
        let expression: Expr = parse_quote!(<T>::u32::MAX);
        let Expr::Path(path) = &expression else { unreachable!() };
        assert!(collector.is_primitive_constant(&path.path));
        assert!(collector.type_of_expression(&expression).is_none());
        assert_eq!(collector.numeric_confidence(&expression), Confidence::Optimistic);
        assert!(!collector.is_known_numeric(&expression));
    });
}

#[test]
fn source_visible_textual_aliases_veto_syntactic_numeric_name_evidence() {
    let source = "
        type u32 = String;
        fn f(value: u32) { consume(value); }
    ";
    with_collector(source, |collector| {
        collector.bind("value".into(), true);
        collector.bind_type("value".into(), parse_quote!(u32));
        let Item::Fn(function) = &collector.file.ast.items[1] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Call(call), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let expression = call.args[0].clone();
        assert!(collector.is_known_numeric(&expression));
        assert!(collector.is_known_textual_or_temporal(&expression));
        assert_eq!(collector.numeric_confidence(&expression), Confidence::Proven);
        collector.perturb(&expression);
        assert!(collector.candidates.is_empty());
    });
}

#[test]
fn function_and_nested_return_contexts_retain_negative_error_evidence_through_aliases() {
    with_collector(
        "type ErrorAlias = std::io::Error; fn f() -> Result<u8, ErrorAlias> { todo!() }",
        |collector| {
            let Item::Fn(function) = &collector.file.ast.items[1] else {
                unreachable!()
            };
            let signature = function.sig.clone();
            assert!(!collector.foreign_error_return);
            collector.in_function(&signature, |collector| {
                assert!(collector.foreign_error_return);
            });
            assert!(!collector.foreign_error_return);
            collector.in_nested_return_context(Some(&signature.output), |collector| {
                assert!(collector.foreign_error_return);
            });
            assert!(!collector.foreign_error_return);
        },
    );
}

#[test]
fn trait_default_bodies_keep_their_explicit_expected_payload_type() {
    let source = "trait Factory { fn make() -> Option<u8> { None } }";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let values = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "option.none_to_some")
            .map(|candidate| (&source[candidate.span.clone()], candidate.replacement.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(values, [("None", "Some(Default::default())")]);
    });
}

#[test]
fn impl_reader_bounds_protect_take_calls_without_leaking_to_the_outer_context() {
    let source = "
        struct Owner<T>(T);
        impl<T: std::io::Read> Owner<T> {
            fn f(mut reader: T) { let _ = reader.take(1); }
        }
    ";
    with_collector(source, |collector| {
        collector.reader_bounded.push("Outer".into());
        let implementation = collector.file.ast.items[1].clone();
        syn::visit::Visit::visit_item(collector, &implementation);
        assert_eq!(collector.reader_bounded, ["Outer"]);
        assert!(
            collector
                .candidates
                .iter()
                .all(|candidate| candidate.mutator != "iter.take_to_skip")
        );
    });
}

#[test]
fn typed_nonunit_expressions_register_their_mandatory_tail_only_during_the_context() {
    with_collector("fn f() { dependency::abort(); }", |collector| {
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let statement = function.block.stmts[0].span().byte_range();
        let expression = Expr::Block(ExprBlock {
            attrs: Vec::new(),
            label: None,
            block: (*function.block).clone(),
        });
        collector.undeletable_statements.push(0..1);
        collector.in_typed_expression_inner(&expression, &parse_quote!(u8), |collector| {
            assert_eq!(collector.undeletable_statements.len(), 2);
            assert_eq!(collector.undeletable_statements[0], 0..1);
            assert_eq!(collector.undeletable_statements[1], statement);
        });
        assert_eq!(collector.undeletable_statements.len(), 1);
        assert_eq!(collector.undeletable_statements[0], 0..1);
    });
}

#[test]
fn nonprimitive_numeric_name_call_defaults_retain_optimistic_confidence() {
    let source = "fn acquire() -> dependency::u32 { todo!() } fn f() { consume(acquire()); }";
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let defaults = Defaults::of(&file.ast);
    let selection = Selection::everything();
    let cfg = CfgSet::parse("");
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    let Item::Fn(function) = &file.ast.items[1] else { unreachable!() };
    let Stmt::Expr(Expr::Call(consume), _) = &function.block.stmts[0] else {
        unreachable!()
    };
    let Expr::Call(call) = &consume.args[0] else { unreachable!() };
    collector.call_semantics(call);
    assert_eq!(collector.candidates.len(), 2);
    for candidate in &collector.candidates {
        assert_eq!(candidate.confidence, Confidence::Optimistic);
        assert_eq!(&source[candidate.span.clone()], "acquire()");
    }
}

#[test]
fn inferred_binary_results_and_temporal_type_aliases_retain_their_declared_identities() {
    with_collector("type Text = String;", |collector| {
        collector.bind_type("instant".into(), parse_quote!(std::time::Instant));
        collector.bind_type("duration".into(), parse_quote!(std::time::Duration));
        collector.bind_type("text".into(), parse_quote!(String));
        collector.bind_type("suffix".into(), parse_quote!(&str));
        for (expression, expected) in [
            ("instant + duration", parse_quote!(std::time::Instant)),
            ("instant - instant", parse_quote!(std::time::Duration)),
            ("text + suffix", parse_quote!(String)),
        ] {
            assert_eq!(
                collector.inferred_expression_type(&parse_str(expression).unwrap()),
                Some(expected),
                "{expression}"
            );
        }
        assert!(collector.is_known_textual_or_temporal_type(&parse_quote!(Text)));
    });
}

#[test]
fn typed_local_destructuring_supplies_each_binding_with_its_annotated_payload() {
    let source = "fn f(input: (u8, bool)) { let (value, flag): (u8, bool) = input; consume(value, flag); }";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let increments = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "expr.increment")
            .map(|candidate| &source[candidate.span.clone()])
            .collect::<Vec<_>>();
        assert_eq!(increments, ["value"]);
    });
}

#[test]
fn untyped_local_initializers_retain_float_evidence_even_without_a_complete_inferred_type() {
    let source = "fn f() { let value = 1.0 + dependency::number(); }";
    with_collector(source, |collector| {
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Local(local) = &function.block.stmts[0] else {
            unreachable!()
        };
        let local = local.clone();
        syn::visit::Visit::visit_local(collector, &local);
        assert!(collector.inferred_floating.contains("value"));
        assert!(collector.type_of_place(&parse_quote!(value)).is_none());
    });
}

#[test]
fn unsigned_body_entry_marks_only_the_final_value_and_is_consumed_before_nested_blocks() {
    let source = "fn f() -> u8 { { consume(0); } 0 }";
    with_collector(source, |collector| {
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let body = (*function.block).clone();
        collector.unsigned_return = super::UnsignedReturn::BodyPending;
        syn::visit::Visit::visit_block(collector, &body);
        assert!(matches!(collector.unsigned_return, super::UnsignedReturn::BodyEntered));
        let decrements = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "literal.int_decrement")
            .map(|candidate| candidate.span.start)
            .collect::<Vec<_>>();
        assert_eq!(decrements, [source.find('0').unwrap()]);
        assert!(collector.unsigned_zeros.is_empty());
    });
}

#[test]
fn iterator_shape_withholds_a_syntactic_option_whose_alias_has_no_wrapper_payload() {
    with_collector("type Option<T> = T;", |collector| {
        let ty = parse_quote!(Option<impl Iterator<Item = u8>>);
        assert_eq!(super::iterator_body_shape(Some(&ty), &collector.types()), None);
    });
}

#[test]
fn character_method_shapes_distinguish_iterating_unicode_case_from_ascii_case_even_under_type_shadows() {
    for (declarations, method, expected) in [
        ("", "to_ascii_lowercase", super::Shape::Expr),
        ("", "to_lowercase", super::Shape::IterExpr),
        ("struct char;", "to_lowercase", super::Shape::IterExpr),
    ] {
        let source = format!("{declarations} fn f() {{ consume('A'.{method}()); }}");
        with_collector(&source, |collector| {
            syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
            let shapes = collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator == "string.lower_to_upper")
                .map(|candidate| candidate.shape)
                .collect::<Vec<_>>();
            assert_eq!(shapes, [expected], "{source}");
        });
    }
}

#[test]
fn unsigned_comparison_operands_are_both_visited_without_negative_zero_replacements() {
    let source = "fn f(value: u32) { consume(0 < value, value > 0); }";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let increments = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "literal.int_increment" && &source[candidate.span.clone()] == "0")
            .count();
        assert_eq!(increments, 2);
        assert!(
            collector
                .candidates
                .iter()
                .all(|candidate| candidate.mutator != "literal.int_decrement")
        );
    });
}

#[test]
fn iterator_body_shape_rejects_depth_beyond_u8_without_wrapping_or_panicking() {
    // Building, inspecting, cloning, and dropping this deeply nested syn tree recurse. Give this
    // boundary fixture conservative stack headroom, not a measured minimum or production limit.
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            with_collector("", |collector| {
                let types = super::types::Types {
                    aliases: None,
                    ..collector.types()
                };
                let mut ty: Type = parse_quote!(impl Iterator<Item = u8>);
                for depth in 1..=256 {
                    let mut wrapper: Type = parse_quote!(Option<()>);
                    let Type::Path(path) = &mut wrapper else { unreachable!() };
                    let PathArguments::AngleBracketed(arguments) = &mut path.path.segments[0].arguments else {
                        unreachable!()
                    };
                    arguments.args[0] = GenericArgument::Type(ty);
                    ty = wrapper;
                    if depth == 255 {
                        assert_eq!(
                            super::iterator_body_shape(Some(&ty), &types),
                            Some(super::Shape::IterWrappedBlock {
                                depth: NonZeroU8::new(255).unwrap()
                            })
                        );
                    } else if depth == 256 {
                        assert_eq!(super::iterator_body_shape(Some(&ty), &types), None);
                    }
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn operator_replacement_rejection_does_not_skip_a_later_supported_operator() {
    let source = "
        struct Left; struct Right;
        impl core::ops::Add<Right> for Left {
            type Output = Right;
            fn add(self, right: Right) -> Right { right }
        }
        impl core::ops::Mul<Right> for Left {
            type Output = Right;
            fn mul(self, right: Right) -> Right { right }
        }
        fn f(left: Left, right: Right) -> Right { left + right }
    ";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let replacements = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator.starts_with("arith.add_to_"))
            .map(|candidate| (candidate.mutator, candidate.replacement.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(replacements, [("arith.add_to_mul", "(left) * (right)")]);
    });
}

#[test]
fn arithmetic_operands_do_not_inherit_unsigned_comparison_zero_contexts() {
    let source = "fn f(value: u32) { consume(0 + value, value + 0); }";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let decrements = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "literal.int_decrement")
            .map(|candidate| candidate.span.clone())
            .collect::<Vec<_>>();
        let first = source.find('0').unwrap();
        let second = source.rfind('0').unwrap();
        assert_eq!(decrements, [first..first + 1, second..second + 1]);
    });
}

#[test]
fn if_tail_protection_restores_the_exact_enclosing_undeletable_statement_set() {
    let source = "fn f(ready: bool) { loop { if ready { continue; } else { 1 }; } }";
    with_collector(source, |collector| {
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Loop(loop_), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::If(branch), _) = &loop_.body.stmts[0] else {
            unreachable!()
        };
        let branch = branch.clone();
        let sentinel = span(source, "fn");
        collector.undeletable_statements.push(sentinel.clone());
        syn::visit::Visit::visit_expr_if(collector, &branch);
        assert_eq!(collector.undeletable_statements.len(), 1);
        assert_eq!(collector.undeletable_statements[0], sentinel);
        assert!(
            collector
                .candidates
                .iter()
                .all(|candidate| { candidate.mutator != "stmt.delete_call" || &source[candidate.span.clone()] != "continue;" })
        );
    });
}

#[test]
fn let_expression_pattern_shadows_prior_numeric_type_and_restores_it_at_scope_exit() {
    let source = "fn f(value: Option<bool>) { if let Some(number) = value { consume(number); } }";
    with_collector(source, |collector| {
        collector.bind_type("number".into(), parse_quote!(u32));
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::If(branch), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Expr::Let(binding) = &*branch.cond else { unreachable!() };
        let binding = binding.clone();
        collector.in_scope(|collector| {
            syn::visit::Visit::visit_expr_let(collector, &binding);
            assert!(collector.type_of_place(&parse_quote!(number)).is_none());
            assert!(!collector.is_known_unsigned(&parse_quote!(number)));
        });
        assert_eq!(collector.type_of_place(&parse_quote!(number)), Some(&parse_quote!(u32)));
    });
}

#[test]
fn struct_fields_and_call_arguments_preserve_unsigned_range_endpoint_contexts() {
    let source = "
        struct Window { span: core::ops::Range<u8> }
        fn consume(_: core::ops::Range<u8>) {}
        fn f() { let _ = Window { span: 0..1 }; consume(0..1); }
    ";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        assert_eq!(
            collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator == "literal.int_increment" && &source[candidate.span.clone()] == "0")
                .count(),
            2
        );
        assert!(
            collector
                .candidates
                .iter()
                .all(|candidate| { candidate.mutator != "literal.int_decrement" || &source[candidate.span.clone()] != "0" })
        );
    });
}

#[test]
fn struct_fields_without_a_unique_declared_type_still_visit_their_initializers() {
    let source = "
        struct First { value: u8 }
        struct Second { value: bool }
        fn f() { let _ = First { value: 7 }; }
    ";
    with_collector(source, |collector| {
        assert_eq!(collector.declared_types.get("value"), Some(&None));
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        let increments = collector
            .candidates
            .iter()
            .filter(|candidate| candidate.mutator == "literal.int_increment")
            .map(|candidate| &source[candidate.span.clone()])
            .collect::<Vec<_>>();
        assert_eq!(increments, ["7"]);
    });
}

#[test]
fn break_to_continue_checks_transition_ownership_and_lifts_the_required_region() {
    let source = "fn f() { loop { break; } }";
    with_collector(source, |collector| {
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Loop(loop_), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Stmt::Expr(Expr::Break(node), _) = &loop_.body.stmts[0] else {
            unreachable!()
        };
        let node = node.clone();
        let site = node.span().byte_range();
        collector.bind_type("owned".into(), parse_quote!(String));
        collector
            .numeric_evidence
            .invalid_transitions
            .insert(site.clone(), vec![parse_quote!(owned)]);
        syn::visit::Visit::visit_expr_break(collector, &node);
        assert!(collector.candidates.is_empty());

        collector.numeric_evidence.invalid_transitions.clear();
        collector.numeric_evidence.exits.insert(site.clone(), vec![parse_quote!(owned)]);
        let region = span(source, "{ break; }");
        collector.numeric_evidence.ownership_regions.insert(site.clone(), region.clone());
        syn::visit::Visit::visit_expr_break(collector, &node);
        assert_eq!(collector.candidates.len(), 1);
        assert_eq!(collector.candidates[0].mutator, "loop.break_to_continue");
        assert_eq!(collector.candidates[0].span, region);
        assert_eq!(collector.ownership_lifts.get(&site), Some(&region));
    });
}

#[test]
fn opaque_expression_attributes_disable_loop_resource_proofs_only_inside_the_expression() {
    for (attributes, retained) in [("", false), ("#[custom]", true)] {
        let source = format!("fn f() {{ {attributes} loop {{ break; }} }}");
        with_collector(&source, |collector| {
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            let expression = expression.clone();
            assert!(collector.resource_transparent);
            syn::visit::Visit::visit_expr(collector, &expression);
            assert!(collector.resource_transparent);
            assert_eq!(
                collector
                    .candidates
                    .iter()
                    .any(|candidate| candidate.mutator == "loop.break_to_continue"),
                retained,
                "{source}"
            );
        });
    }
}

#[test]
fn compound_arithmetic_replacements_are_left_for_compiler_validation() {
    let source = "
        struct Left; struct Right;
        impl core::ops::AddAssign<Right> for Left {
            fn add_assign(&mut self, _: Right) {}
        }
        fn custom(mut left: Left, right: Right) { left += right; }
        fn text(mut text: String) { text += \"suffix\"; }
        fn number(mut value: u32) { value += 2; }
    ";
    with_collector(source, |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        for mutator in ["assign.add_to_sub", "assign.add_to_mul"] {
            let replacements = collector
                .candidates
                .iter()
                .filter(|candidate| candidate.mutator == mutator)
                .map(|candidate| &source[candidate.span.clone()])
                .collect::<Vec<_>>();
            assert_eq!(replacements, ["left += right", "text += \"suffix\"", "value += 2"], "{mutator}");
        }
    });
}

#[test]
fn if_let_expression_scopes_withhold_resource_proofs_and_restore_the_outer_context() {
    for source in [
        "fn f() { if let _ = true { loop { break; } } }",
        "fn f(input: Option<bool>) { if let Some(binding) = input { loop { break; } } }",
    ] {
        with_collector(source, |collector| {
            let Item::Fn(function) = &collector.file.ast.items[0] else {
                unreachable!()
            };
            let Stmt::Expr(expression, _) = &function.block.stmts[0] else {
                unreachable!()
            };
            let expression = expression.clone();
            syn::visit::Visit::visit_expr(collector, &expression);
            assert!(collector.resource_transparent);
            assert_eq!(
                collector
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.mutator == "loop.break_to_continue")
                    .count(),
                1,
                "{source}"
            );
        });
    }
}

#[test]
fn published_resource_alias_maps_freeze_inherited_targets_and_poison_cycles() {
    with_collector(
        "type First = Second; type Second = First;
         type Flag = bool;
         fn f() { type Local = Flag; { type Flag = &'static str; let marker = 0; } }",
        |collector| {
            let nested = collector.block_scopes.iter().max_by_key(|scope| scope.len()).unwrap();
            let aliases = &collector.scope_aliases[nested];
            assert!(matches!(aliases["First"].as_ref().unwrap().target, Type::Infer(_)));
            assert!(matches!(aliases["Second"].as_ref().unwrap().target, Type::Infer(_)));
            let mut scopes = Vec::new();
            assert_eq!(
                collector.resource_binding_with(&parse_quote!(Local), nested, &mut |scope| {
                    scopes.push(scope.clone());
                    assert!(scopes.len() <= 2, "the published alias has no unresolved alias chain");
                }),
                Some(super::resource::Binding::Bool)
            );
            assert_eq!(scopes.as_slice(), core::slice::from_ref(nested));
            assert_eq!(
                collector.resource_binding(&parse_quote!(Flag), nested),
                Some(super::resource::Binding::StrReference)
            );
        },
    );
}
