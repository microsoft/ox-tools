// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZeroU8;
use core::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use syn::parse::Parser as _;
use syn::spanned::Spanned as _;
use syn::token::Group;
use syn::{Expr, Item, ReturnType, Type};

use super::calls::{Calls, OperatorResult};
use super::indexes::{ScopePath, block_id, indexes_in, module_id};
use super::{Collector, lexical, temporaries};
use crate::HashMap;
use crate::cfg::CfgSet;
use crate::ops::collect::{Confidence, Defaults, collect_with};
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

const ROOT: ScopePath = Vec::new();

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
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
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
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
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
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
    assert_eq!(
        evidence.numeric,
        source.match_indices("value").map(|(start, _)| start..start + 5).collect()
    );
    assert!(evidence.unsigned.is_empty());
}

#[test]
fn followup_lexical_let_else_divergence_reaches_uses_of_outer_bindings() {
    let source = "fn f(value: Unknown) { let Some(other) = input else { value - 1; return; }; }";
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
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
        let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
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
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
    assert!(evidence.reinitializations.is_empty());
    assert!(evidence.invalid_ownership.is_empty());
}

#[test]
fn followup_lexical_reinitialization_is_load_bearing_when_the_following_statement_is_the_whole_place() {
    let source = "fn f(mut value: String) -> String { consume(value); value = make(); value }";
    let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::parse(""));
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
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::parse(""));
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
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::parse(""));
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
                (syn::parse_quote!(Wrapper<u8>), "+", syn::parse_quote!(u16)),
                (syn::parse_quote!(crate::Wrapper<u8>), "+", syn::parse_quote!(u16)),
                (syn::parse_quote!((u8, bool)), "-", syn::parse_quote!(u32)),
            ] {
                let OperatorResult::Output(actual) = calls.operator_result(&syn::parse_quote!(Left), &rhs, operation, &ROOT, imports)
                else {
                    panic!("the fixture declares this exact operator signature");
                };
                assert_eq!(actual, &expected, "{rhs:?} {operation}");
            }
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(Left), &syn::parse_quote!(Wrapper<bool>), "+", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(Left), &syn::parse_quote!((bool, u8)), "-", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(Left), "+", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(Left), "-", &ROOT, imports));
            assert!(calls.unit_operator_absent(&syn::parse_quote!(Left), "*", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(Unknown), "*", &ROOT, imports));
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
            for expression in [syn::parse_quote!(shared), syn::parse_quote!(crate::shared)] {
                assert_eq!(
                    calls.signature(&expression, &ROOT, imports, None, |_| false).map(output),
                    Some(syn::parse_quote!(bool))
                );
            }
            let signature = calls.method_signature(&syn::parse_quote!(Owner), "shared", &ROOT, imports).unwrap();
            assert_eq!(output(signature), syn::parse_quote!(u8));
            assert_eq!(Calls::parameters(signature, &CfgSet::parse("")), [syn::parse_quote!(&crate::Owner)]);
            assert!(
                calls
                    .method_signature(&syn::parse_quote!(Other), "shared", &ROOT, imports)
                    .is_none()
            );
            assert!(
                calls
                    .signature(&syn::parse_quote!(crate::trait_only), &ROOT, imports, None, |_| false)
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
            let signature = calls
                .method_signature(&syn::parse_quote!(Outer<u8>), "after", &ROOT, imports)
                .unwrap();
            assert_eq!(
                Calls::parameters(signature, &CfgSet::parse("")),
                [syn::parse_quote!(&crate::Outer), syn::parse_quote!(_)]
            );
            assert_eq!(output(signature), syn::parse_quote!(_));
            assert!(
                calls
                    .method_signature(&syn::parse_quote!(Inner<u8>), "after", &ROOT, imports)
                    .is_none()
            );
            assert!(
                calls
                    .method_signature(&syn::parse_quote!(Inner<u8>), "nested", &ROOT, imports)
                    .is_none()
            );
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
            for (item, expected) in implementation.items.iter().zip([Some(syn::parse_quote!(u8)), None]) {
                let syn::ImplItem::Fn(method) = item else { unreachable!() };
                let scope = vec![block_id(&method.block)];
                assert_eq!(
                    calls
                        .signature(&syn::parse_quote!(helper), &scope, imports, None, |_| false)
                        .map(output),
                    expected,
                    "{}",
                    method.sig.ident
                );
            }
            let Item::Trait(contract) = &file.items[2] else { unreachable!() };
            for (item, expected) in contract.items.iter().zip([Some(syn::parse_quote!(u32)), None]) {
                let syn::TraitItem::Fn(method) = item else { unreachable!() };
                let scope = vec![block_id(method.default.as_ref().unwrap())];
                assert_eq!(
                    calls
                        .signature(&syn::parse_quote!(helper), &scope, imports, None, |_| false)
                        .map(output),
                    expected,
                    "{}",
                    method.sig.ident
                );
            }
            assert!(
                calls
                    .signature(&syn::parse_quote!(crate::helper), &ROOT, imports, None, |_| false)
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
            let syn::Stmt::Local(local) = &function.block.stmts[0] else {
                unreachable!()
            };
            let Expr::Block(local_block) = &*local.init.as_ref().unwrap().expr else {
                unreachable!()
            };
            let syn::Stmt::Expr(Expr::Call(call), _) = &function.block.stmts[1] else {
                unreachable!()
            };
            let Expr::Block(argument_block) = &call.args[0] else {
                unreachable!()
            };
            let syn::Stmt::Expr(Expr::Block(visible_block), _) = &function.block.stmts[2] else {
                unreachable!()
            };
            for (block, name, expected) in [
                (local_block, "hidden_statement", None),
                (argument_block, "hidden_expression", None),
                (visible_block, "visible", Some(syn::parse_quote!(u32))),
            ] {
                let scope = vec![block_id(&function.block), block_id(&block.block)];
                assert_eq!(
                    calls
                        .signature(&syn::parse_str(name).unwrap(), &scope, imports, None, |_| false)
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
            let syn::Stmt::Expr(Expr::Block(block), _) = &function.block.stmts[1] else {
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
            assert!(calls.structure(&syn::parse_quote!(Item), &module_scope, module_imports));
            assert!(!calls.structure(&syn::parse_quote!(Item), &scope, block_imports));
            assert!(!calls.is_enum(&syn::parse_quote!(Item), &scope, block_imports));
            assert!(
                calls
                    .field(&syn::parse_quote!(Item), &syn::parse_quote!(value), &scope, block_imports)
                    .is_none()
            );
            assert!(
                calls
                    .method_signature(&syn::parse_quote!(Item), "read", &scope, block_imports)
                    .is_none()
            );
            assert_eq!(
                calls.field(
                    &syn::parse_quote!(crate::outer::Item),
                    &syn::parse_quote!(value),
                    &scope,
                    block_imports
                ),
                Some(syn::parse_quote!(u8))
            );
            assert_eq!(
                calls
                    .method_signature(&syn::parse_quote!(crate::outer::Item), "read", &scope, block_imports)
                    .map(output),
                Some(syn::parse_quote!(u8))
            );
            assert!(calls.is_enum(&syn::parse_quote!(Choice), &scope, block_imports));
            assert!(!calls.is_enum(&syn::parse_quote!(Opaque), &scope, block_imports));
        },
    );
}

#[test]
fn campaign_receiver_bindings_preserve_reference_modes_and_later_destructured_parameters() {
    for (signature, receiver) in [
        ("fn method(self, (count, flag): (u16, bool))", syn::parse_quote!(Owner)),
        ("fn method(&self, (count, flag): (u16, bool))", syn::parse_quote!(&Owner)),
        ("fn method(&mut self, (count, flag): (u16, bool))", syn::parse_quote!(&mut Owner)),
        (
            "fn method(self: Box<Self>, (count, flag): (u16, bool))",
            syn::parse_quote!(Box<Owner>),
        ),
    ] {
        with_collector("struct Owner;", |collector| {
            collector.impl_self_type = Some(syn::parse_quote!(Owner));
            let signature = syn::parse_str(signature).unwrap();
            collector.in_function(&signature, |collector| {
                assert_eq!(collector.binding_types.len(), 3);
                assert_eq!(collector.binding_types["self"].ty, receiver);
                assert!(collector.binding_types["self"].resource_trusted);
                assert_eq!(collector.type_of_place(&syn::parse_quote!(count)), Some(&syn::parse_quote!(u16)));
                assert_eq!(collector.type_of_place(&syn::parse_quote!(flag)), Some(&syn::parse_quote!(bool)));
                assert!(!collector.binding_types["count"].resource_trusted);
                assert!(!collector.binding_types["flag"].resource_trusted);
            });
            assert!(collector.binding_types.is_empty());
            assert_eq!(collector.impl_self_type, Some(syn::parse_quote!(Owner)));
        });
    }
}

#[test]
fn campaign_nested_functions_restore_floating_evidence_without_inheriting_it() {
    with_collector("", |collector| {
        let _inserted = collector.inferred_floating.insert("outer".into());
        collector.bind_type("outer".into(), syn::parse_quote!(f64));
        let signature = syn::parse_quote!(fn inner(value: u8));
        collector.in_function(&signature, |collector| {
            assert!(!collector.is_known_float(&syn::parse_quote!(outer)));
            assert!(!collector.is_known_float(&syn::parse_quote!(value)));
            let _inserted = collector.inferred_floating.insert("inner".into());
            assert!(collector.is_known_float(&syn::parse_quote!(inner)));
        });
        assert!(collector.is_known_float(&syn::parse_quote!(outer)));
        assert!(!collector.is_known_float(&syn::parse_quote!(inner)));
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
            collector.bind_type("value".into(), syn::parse_str(ty).unwrap());
            assert_eq!(collector.character_receiver(&syn::parse_quote!(value)), expected, "{ty}");
        }
        assert!(!collector.character_receiver(&syn::parse_quote!(missing)));
    });
    with_collector("struct char;", |collector| {
        collector.bind_type("value".into(), syn::parse_quote!(char));
        assert!(!collector.character_receiver(&syn::parse_quote!(value)));
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
                ("Packet(1)", Some(syn::parse_quote!(Packet))),
                ("payload::Packet(2)", Some(syn::parse_quote!(payload::Packet))),
                ("factory()", None),
                ("Utc::now()", Some(syn::parse_quote!(chrono::DateTime))),
                ("Utc::other()", None),
                ("DateTime::now()", Some(syn::parse_quote!(chrono::DateTime))),
                ("NaiveDateTime::now()", Some(syn::parse_quote!(chrono::NaiveDateTime))),
                ("Duration::from_secs(2)", Some(syn::parse_quote!(std::time::Duration))),
                ("other::now()", None),
                ("value", None),
            ] {
                assert_eq!(collector.inferred_call_type(&syn::parse_str(call).unwrap()), expected, "{call}");
            }
        },
    );
    with_collector("use chrono::Other;", |collector| {
        assert_eq!(collector.inferred_call_type(&syn::parse_quote!(Other::now())), None);
    });
}

#[test]
fn campaign_lexical_numeric_operators_do_not_turn_signed_evidence_into_unsigned_evidence() {
    for operator in ["-", "*", "/", "%", "-=", "*=", "/=", "%="] {
        let source = format!("fn f(left: Unknown, right: Unknown) {{ left {operator} right; }}");
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::unconditional());
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
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::unconditional());
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
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::unconditional());
        assert_eq!(
            evidence.numeric,
            source.match_indices("value").map(|(start, _)| start..start + 5).collect(),
            "{expression}"
        );
        assert!(evidence.unsigned.is_empty(), "{expression}");
    }
    for expression in ["value == 1", "1 != value", "value + \"text\"", "value < other"] {
        let source = format!("fn f(value: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::unconditional());
        assert!(evidence.numeric.is_empty(), "{expression}");
        assert!(evidence.unsigned.is_empty(), "{expression}");
    }
}

#[test]
fn campaign_lexical_index_ranges_prove_both_endpoints_without_marking_the_container() {
    for expression in ["values[start..end]", "values[..end]", "values[start..]", "values[(start)..=(end)]"] {
        let source = format!("fn f(values: Unknown, start: Unknown, end: Unknown) {{ {expression}; }}");
        let evidence = lexical::evidence(&syn::parse_file(&source).unwrap(), &CfgSet::unconditional());
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
            let signature = calls.signature(&syn::parse_quote!(make), &ROOT, imports, None, |_| false).unwrap();
            assert_eq!(
                Calls::parameters(signature, &CfgSet::unconditional()),
                [syn::parse_quote!(&dependency::types::Wrapper<u16>)]
            );
            assert_eq!(
                output(signature),
                syn::parse_quote!((dependency::types::Item, ::dependency::other::Item))
            );
            let signature = calls
                .signature(&syn::parse_quote!(associated), &ROOT, imports, None, |_| false)
                .unwrap();
            assert_eq!(output(signature), syn::parse_quote!(<imported::Owner as Trait>::Item));
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
            for (name, expected) in [
                ("relative", syn::parse_quote!(_)),
                ("absolute", syn::parse_quote!(::std::primitive::u8)),
            ] {
                let signature = calls
                    .signature(&syn::parse_str(name).unwrap(), &ROOT, imports, None, |_| false)
                    .unwrap();
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
                .supports_primitive_kind(&syn::parse_quote!(Number), super::values::Kind::Unsigned)
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
            collector.bind_type("a".into(), syn::parse_quote!(A));
            collector.bind_type("b".into(), syn::parse_quote!(A));
            for replacement in ["-", "*", "/"] {
                assert!(
                    !collector.unsupported_operator(&syn::parse_quote!(a + b), replacement),
                    "{replacement}"
                );
            }
            assert!(collector.unsupported_operator(&syn::parse_quote!(a + b), "%"));
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
            collector.bind_type("a".into(), syn::parse_quote!(A));
            collector.bind_type("b".into(), syn::parse_quote!(A));
            let output = collector.binary_expression_type(&syn::parse_quote!(a + b)).unwrap();
            assert!(quote::quote!(#output).to_string().contains('_'));
            assert!(!collector.unsupported_operator(&syn::parse_quote!(a + b), "-"));
            assert!(!collector.unsupported_operator(&syn::parse_quote!(a - b), "+"));
        },
    );
}

#[test]
fn ownership_sensitive_continue_does_not_emit_a_break_that_skips_reinitialization() {
    with_collector("fn f() { loop { continue; } }", |collector| {
        collector.bind_type("value".into(), syn::parse_quote!(String));
        let Item::Fn(function) = &collector.file.ast.items[0] else {
            unreachable!()
        };
        let syn::Stmt::Expr(Expr::Loop(loop_), _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let syn::Stmt::Expr(Expr::Continue(node), _) = &loop_.body.stmts[0] else {
            unreachable!()
        };
        let node = node.clone();
        collector
            .numeric_evidence
            .invalid_transitions
            .insert(node.span().byte_range(), vec![syn::parse_quote!(value)]);
        syn::visit::Visit::visit_expr_continue(collector, &node);
        assert!(collector.candidates.is_empty());
    });
}

#[test]
fn operator_output_canonicalization_handles_invisible_groups_and_rejects_oversized_tuples() {
    for oversized in [false, true] {
        let mut file = syn::parse_file("struct A; impl core::ops::Add for A { type Output = u8; }").unwrap();
        let Item::Impl(implementation) = &mut file.items[1] else {
            unreachable!()
        };
        let syn::ImplItem::Type(output) = &mut implementation.items[0] else {
            unreachable!()
        };
        let expected = if oversized {
            let mut tuple: syn::TypeTuple = syn::parse_quote!(());
            tuple.elems.extend(core::iter::repeat_n::<Type>(
                syn::parse_quote!(u8),
                super::types::TYPE_EXPANSION_LIMIT,
            ));
            output.ty = Type::Tuple(tuple);
            syn::parse_quote!(_)
        } else {
            let group = Type::Group(syn::TypeGroup {
                attrs: Vec::new(),
                group_token: Group::default(),
                elem: Box::new(syn::parse_quote!(u8)),
            });
            output.ty = group.clone();
            group
        };
        let cfg = CfgSet::unconditional();
        let indexes = indexes_in(&file, &Selection::everything(), &cfg);
        let calls = Calls::of(&file, &cfg, &indexes);
        let OperatorResult::Output(actual) =
            calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(A), "+", &ROOT, &indexes.root_imports)
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
        let syn::Item::Fn(function) = collector.file.ast.items.last().unwrap() else {
            unreachable!()
        };
        let syn::Stmt::Expr(expression, _) = function.block.stmts.last().unwrap() else {
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
    let left_file = syn::parse_file("struct Left;").unwrap();
    let right_file = syn::parse_file("struct Right;").unwrap();
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
        collector.bind_type("text".into(), syn::parse_quote!(String));
        collector.bind_type("number".into(), syn::parse_quote!(&u32));
        assert!(collector.unsupported_operator(&syn::parse_quote!(text + "suffix"), "-"));
        assert!(!collector.unsupported_operator(&syn::parse_quote!(true + false), "-"));
        assert_eq!(
            collector.binary_expression_type(&syn::parse_quote!(number + number)),
            Some(syn::parse_quote!(&u32))
        );
    });
    with_collector(
        "struct A; impl core::ops::Add for A { type Output = A; } impl core::ops::Mul for A { type Output = A; } impl core::ops::Mul for A { type Output = A; }",
        |collector| {
            collector.bind_type("a".into(), syn::parse_quote!(A));
            assert!(!collector.unsupported_operator(&syn::parse_quote!(a + a), "*"));
        },
    );
}

#[test]
fn network_readers_and_qualified_result_constructors_keep_standard_api_identity() {
    with_collector("use std::io::Read;", |collector| {
        for (name, ty) in [
            ("tcp", syn::parse_quote!(std::net::TcpStream)),
            ("unix", syn::parse_quote!(std::os::unix::net::UnixStream)),
        ] {
            collector.bind_type(name.into(), ty);
            assert!(collector.is_read_receiver(&syn::parse_str(name).unwrap()));
        }
        assert!(collector.some_constructor(&grouped(syn::parse_quote!(Some))));
        assert_eq!(
            collector.standard_constructor_type(&syn::parse_quote!(core::result::Result::Ok(true))),
            Some(syn::parse_quote!(std::result::Result<_, _>))
        );
        assert!(
            collector
                .standard_constructor_type(&syn::parse_quote!(<Owner as Trait>::make()))
                .is_none()
        );
    });
    with_collector("#![no_implicit_prelude]", |collector| {
        assert_eq!(
            collector.standard_constructor_type(&syn::parse_quote!(std::result::Result::Err(false))),
            Some(syn::parse_quote!(std::result::Result<_, _>))
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
            assert!(!calls.has_noncopy_field(&syn::parse_quote!(A), &ROOT, &shadowed, |_| true));
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(A), "+", &ROOT, &shadowed));
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(A), "+", &ROOT, &shadowed),
                OperatorResult::Unknown
            ));
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(&A), "+", &ROOT, imports),
                OperatorResult::Missing
            ));
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(A), "+", &ROOT, imports),
                OperatorResult::Output(_)
            ));
            let syn::Item::Mod(child) = &file.items[1] else { unreachable!() };
            let child = vec![module_id(&child.ident)];
            assert!(calls.structure(&syn::parse_quote!(super::A), &child, imports));
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
            assert!(!calls.structure(&syn::parse_quote!(::A), &ROOT, imports));
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
        let file = syn::parse_file(&source).unwrap();
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
            collector.bind_type("owner".into(), syn::parse_quote!(&Owner));
            assert_eq!(
                collector.inferred_expression_type(&syn::parse_quote!(owner.value())),
                Some(syn::parse_quote!(crate::Owner))
            );
            assert_eq!(
                collector.inferred_call_type(&syn::parse_quote!(Duration::from_secs(2))),
                Some(syn::parse_quote!(std::time::Duration))
            );
        },
    );
}

#[test]
fn ownership_places_follow_fields_tuples_and_parentheses_but_not_generic_or_imported_records() {
    with_collector("struct Record { value: String } struct Tuple(String);", |collector| {
        collector.bind_type("record".into(), syn::parse_quote!(Record));
        collector.bind_type("tuple".into(), syn::parse_quote!(Tuple));
        collector.bind_type("pair".into(), syn::parse_quote!((u8, String)));
        for (expression, expected) in [
            ("record.value", Some(syn::parse_quote!(String))),
            ("tuple.0", Some(syn::parse_quote!(String))),
            ("(pair).1", Some(syn::parse_quote!(String))),
            ("pair.5", None),
            ("pair.value", None),
        ] {
            assert_eq!(
                collector.ownership_type(&syn::parse_str(expression).unwrap()),
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
            assert!(collector.noncopy_type(&syn::parse_str(name).unwrap(), 8), "{name}");
        }
        collector.generics.push("Record".into());
        assert!(collector.ownership_type(&syn::parse_quote!(record.value)).is_none());
    });
    with_collector("use other::Record;", |collector| {
        collector.bind_type("record".into(), syn::parse_quote!(Record));
        assert!(collector.ownership_type(&syn::parse_quote!(record.value)).is_none());
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
            collector.bind_pattern(
                &syn::Pat::parse_single.parse_str(pattern).unwrap(),
                Some(&syn::parse_str(expected).unwrap()),
            );
            for (name, ty) in names {
                assert_eq!(
                    collector.type_of_place(&syn::parse_str(name).unwrap()),
                    Some(&syn::parse_str(ty).unwrap()),
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
        let expression = syn::parse_str(source).unwrap();
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
    collector.bind_type("left".into(), syn::parse_quote!(A));
    collector.bind_type("right".into(), syn::parse_quote!(B));
    let binary = syn::parse_quote!(left + right);
    assert_eq!(collector.binary_expression_type(&binary), Some(syn::parse_quote!(crate::B)));
    assert!(collector.unsupported_operator(&binary, "-"));
    assert!(!collector.unsupported_operator(&binary, "*"));
    assert!(collector.unsupported_operator(&binary, "/"));
    assert!(collector.unsupported_range_unit(&syn::parse_quote!(right), "+"));
    assert_eq!(
        collector.inferred_expression_type(&syn::parse_quote!(A)),
        Some(syn::parse_quote!(A))
    );
}

fn with_collector(source: &str, check: impl FnOnce(&mut Collector<'_>)) {
    let file = SourceFile::parse("fixture.rs", source.into()).unwrap();
    let selection = Selection::everything();
    let cfg = CfgSet::parse("");
    let defaults = Defaults::of(&file.ast);
    let mut collector = Collector::new(&file, &selection, &[], &cfg, &defaults);
    check(&mut collector);
}

fn with_calls(source: &str, check: impl FnOnce(&syn::File, &Calls, &HashMap<String, Option<Vec<String>>>)) {
    let file = syn::parse_file(source).unwrap();
    let cfg = CfgSet::parse("");
    let indexes = indexes_in(&file, &Selection::everything(), &cfg);
    let calls = Calls::of(&file, &cfg, &indexes);
    check(&file, &calls, &indexes.root_imports);
}

fn grouped(expression: Expr) -> Expr {
    Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: Group::default(),
        expr: Box::new(expression),
    })
}

#[test]
fn invisible_expression_groups_preserve_inference_and_tail_analysis() {
    with_collector("", |collector| {
        assert!(collector.is_known_float(&grouped(syn::parse_quote!(1.5))));
        assert_eq!(
            collector.inferred_expression_type(&grouped(syn::parse_quote!(true))),
            Some(syn::parse_quote!(bool))
        );
        collector.bind_type("number".into(), syn::parse_quote!(f64));
        let value = grouped(syn::parse_quote!(number));
        assert_eq!(collector.ownership_type(&value), Some(&syn::parse_quote!(f64)));
        assert_eq!(collector.inferred_expression_type(&value), Some(syn::parse_quote!(f64)));
        assert!(collector.is_known_float(&value));
        let mut spans = Vec::new();
        super::mandatory_tail_statements(
            &grouped(syn::parse_quote!({
                return;
            })),
            &mut spans,
        );
        assert_eq!(spans.len(), 1);
        let value = grouped(syn::parse_quote!(values.iter().sum()));
        assert!(super::needs_expected_numeric_type(&value));
        assert_eq!(
            collector.numeric_confidence(&syn::parse_quote!((unknown))),
            super::Confidence::Optimistic
        );
        assert!(collector.type_of_place(&syn::parse_quote!(*number)).is_none());
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
            whole,
            "#[allow(unused)] false",
            0,
            super::Shape::Expr,
            super::Confidence::Proven,
        );
        assert_eq!(collector.candidates.len(), 1);
        assert_eq!(collector.candidates[0].replacement, "false");
        assert_eq!(collector.candidates[0].span, content);
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
        collector.bind_type("value".into(), syn::parse_quote!(String));
        let assignment = span(source, "value = make()");
        let region = span(source, "{ value = make(); use_value(value); }");
        collector
            .numeric_evidence
            .invalid_ownership
            .insert(assignment.clone(), vec![syn::parse_quote!(value)]);
        assert!(!collector.admit_ownership_edit(assignment.clone(), &assignment, false));
        collector.numeric_evidence.invalid_ownership.clear();
        collector
            .numeric_evidence
            .reinitializations
            .insert(assignment.clone(), vec![syn::parse_quote!(value)]);
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
        let arm: syn::Arm = syn::parse_quote!(Some(value) => value + 1,);
        syn::visit::Visit::visit_arm(collector, &arm);
        assert!(!collector.bindings.contains_key("value"));
        let arm: syn::Arm = syn::parse_quote!(#[gamma::skip] _ => 1,);
        syn::visit::Visit::visit_arm(collector, &arm);
        collector.bind_pattern(&syn::parse_quote!(&value), Some(&syn::parse_quote!(u8)));
        assert!(collector.type_of_place(&syn::parse_quote!(value)).is_none());
    });
    with_collector("struct Owner; impl Owner { fn f(self: Box<Self>) {} }", |collector| {
        syn::visit::Visit::visit_file(collector, &collector.file.ast.clone());
        assert!(collector.binding_types.is_empty());
    });
}

fn output(signature: &syn::Signature) -> Type {
    match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
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
                ("target", Vec::new(), imports, Some(syn::parse_quote!(u8))),
                ("imported", Vec::new(), imports, Some(syn::parse_quote!(u16))),
                ("Renamed::make", Vec::new(), imports, Some(syn::parse_quote!(crate::outer::Owner))),
                ("target", outer_scope.clone(), &empty, Some(syn::parse_quote!(u16))),
                ("target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u32))),
                ("self::target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u32))),
                ("super::target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u16))),
                ("super::super::target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u8))),
                ("crate::target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u8))),
                ("::target", inner_scope.clone(), &empty, Some(syn::parse_quote!(u8))),
                ("super::super::super::target", inner_scope, &empty, None),
                ("missing", outer_scope, &empty, None),
                ("(target)", Vec::new(), imports, None),
                ("<Owner as Trait>::target", Vec::new(), imports, None),
            ] {
                let expression = syn::parse_str(callee).unwrap();
                assert_eq!(
                    calls.signature(&expression, &scope, imports, None, |_| false).map(output),
                    expected,
                    "{callee} in {scope:?}"
                );
            }
            assert!(
                calls
                    .signature(&syn::parse_quote!(imported), &ROOT, imports, None, |name| name == "imported")
                    .is_none()
            );
            let ambiguous = HashMap::from_iter([("imported".into(), None)]);
            assert!(
                calls
                    .signature(&syn::parse_quote!(imported), &ROOT, &ambiguous, None, |_| false)
                    .is_none()
            );
            assert_eq!(
                calls
                    .signature(
                        &syn::parse_quote!(Self::make),
                        &ROOT,
                        imports,
                        Some(&syn::parse_quote!(outer::Owner)),
                        |_| false,
                    )
                    .map(output),
                Some(syn::parse_quote!(crate::outer::Owner))
            );
            assert!(
                calls
                    .signature(&syn::parse_quote!(Self::make), &ROOT, imports, None, |_| false)
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
            for (statement, expected) in function.block.stmts[1..]
                .iter()
                .zip([Some(syn::parse_quote!(u16)), None, None, None])
            {
                let syn::Stmt::Expr(Expr::Block(block), _) = statement else {
                    panic!("block fixture")
                };
                let mut scope = outer.clone();
                scope.push(block_id(&block.block));
                assert_eq!(
                    calls
                        .signature(&syn::parse_quote!(known), &scope, imports, None, |_| false)
                        .map(output),
                    expected
                );
            }
            assert!(
                calls
                    .signature(&syn::parse_quote!(duplicate), &ROOT, imports, None, |_| false)
                    .is_none()
            );
            let Item::Mod(module) = &file.items[4] else {
                panic!("child fixture")
            };
            let Item::Fn(function) = &module.content.as_ref().unwrap().1[0] else {
                panic!("child function fixture")
            };
            let scope = vec![module_id(&module.ident), block_id(&function.block)];
            assert!(
                calls
                    .signature(&syn::parse_quote!(known), &scope, imports, None, |_| false)
                    .is_none()
            );
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
            let signature = calls
                .signature(&syn::parse_quote!(aliased), &ROOT, imports, None, |_| false)
                .unwrap();
            assert_eq!(
                Calls::parameters(signature, &cfg),
                [syn::parse_quote!(&[(bool, std::string::String); 2])]
            );
            assert_eq!(output(signature), syn::parse_quote!((u8, std::string::String)));
            let signature = calls
                .signature(&syn::parse_quote!(generic), &ROOT, imports, None, |_| false)
                .unwrap();
            assert_eq!(Calls::parameters(signature, &cfg), [syn::parse_quote!(_)]);
            assert_eq!(output(signature), syn::parse_quote!(Option<_>));
            let signature = calls
                .signature(&syn::parse_quote!(malformed), &ROOT, imports, None, |_| false)
                .unwrap();
            assert_eq!(Calls::parameters(signature, &cfg), [syn::parse_quote!(_)]);
            assert_eq!(output(signature), syn::parse_quote!(_));
            for (name, expected) in [
                ("borrowed", vec![syn::parse_quote!(&crate::Owner), syn::parse_quote!(_)]),
                ("mutable", vec![syn::parse_quote!(&mut crate::Owner)]),
                ("owned", vec![syn::parse_quote!(crate::Owner)]),
                ("boxed", vec![syn::parse_quote!(Box<crate::Owner>)]),
            ] {
                let signature = calls
                    .method_signature(&syn::parse_quote!(&Owner<u8>), name, &ROOT, imports)
                    .unwrap();
                assert_eq!(Calls::parameters(signature, &cfg), expected, "{name}");
            }
            assert!(
                calls
                    .method_signature(&syn::parse_quote!(Owner<u8>), "deferred", &ROOT, imports)
                    .unwrap()
                    .asyncness
                    .is_some()
            );
            assert!(calls.has_method_named("borrowed"));
            assert!(!calls.has_method_named("absent"));
            assert!(
                calls
                    .method_signature(&syn::parse_quote!((Owner<u8>,)), "borrowed", &ROOT, imports)
                    .is_none()
            );
        },
    );
}

#[test]
fn call_signatures_bound_generic_and_branching_alias_expansion() {
    with_calls(include_str!("fixtures/expanding_aliases.rs"), |_, calls, imports| {
        let cfg = CfgSet::unconditional();
        let signature = calls
            .signature(&syn::parse_quote!(generic), &ROOT, imports, None, |_| false)
            .unwrap();
        assert_eq!(Calls::parameters(signature, &cfg), [syn::parse_quote!(_)]);
        assert_eq!(output(signature), syn::parse_quote!(_));

        let signature = calls
            .signature(&syn::parse_quote!(branching), &ROOT, imports, None, |_| false)
            .unwrap();
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
                ("+", syn::parse_quote!(Right), syn::parse_quote!(crate::Right)),
                ("-", syn::parse_quote!(Left), syn::parse_quote!(crate::Left)),
                ("*", syn::parse_quote!(Right), syn::parse_quote!(crate::Right)),
                ("/", syn::parse_quote!(Right), syn::parse_quote!(crate::Right)),
                ("%", syn::parse_quote!(Right), syn::parse_quote!(crate::Right)),
                ("+=", syn::parse_quote!(Right), syn::parse_quote!(())),
                ("-=", syn::parse_quote!(Right), syn::parse_quote!(())),
                ("*=", syn::parse_quote!(Right), syn::parse_quote!(())),
                ("/=", syn::parse_quote!(Right), syn::parse_quote!(())),
                ("%=", syn::parse_quote!(Right), syn::parse_quote!(())),
            ] {
                let OperatorResult::Output(actual) = calls.operator_result(&syn::parse_quote!(Left), &rhs, operator, &ROOT, imports) else {
                    panic!("missing {operator} signature");
                };
                assert_eq!(actual, &expected, "{operator}");
            }

            for (left, right, operator) in [
                (syn::parse_quote!(Left), syn::parse_quote!(Other), "+"),
                (syn::parse_quote!(Right), syn::parse_quote!(Left), "+"),
                (syn::parse_quote!(Other), syn::parse_quote!(Right), "+"),
                (syn::parse_quote!(Other), syn::parse_quote!(Right), "-"),
            ] {
                assert!(matches!(
                    calls.operator_result(&left, &right, operator, &ROOT, imports),
                    OperatorResult::Missing
                ));
            }
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(Left), "+", &ROOT, imports));
            assert!(calls.unit_operator_absent(&syn::parse_quote!(Right), "+", &ROOT, imports));
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(dependency::Unknown), "+", &ROOT, imports));
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(&Left), &syn::parse_quote!(Right), "+", &ROOT, imports),
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
    ] {
        with_calls(source, |_, calls, imports| {
            assert!(
                matches!(
                    calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(A), "+", &ROOT, imports),
                    OperatorResult::Unknown
                ),
                "{source}"
            );
            assert!(!calls.unit_operator_absent(&syn::parse_quote!(A), "-", &ROOT, imports), "{source}");
        });
    }
    with_calls(
        "struct A;
         impl core::ops::Add for A { type Output = A; }
         impl core::ops::Add for A { type Output = A; }",
        |_, calls, imports| {
            assert!(matches!(
                calls.operator_result(&syn::parse_quote!(A), &syn::parse_quote!(A), "+", &ROOT, imports),
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
                let ty = syn::parse_str(ty).unwrap();
                assert_eq!(collector.noncopy_type(&ty, 8), expected, "{ty:?}");
                assert!(!collector.noncopy_type(&ty, 0), "exhausted proof budget: {ty:?}");
            }
            assert!(!collector.noncopy_type(&syn::parse_quote!((String,)), 1));
            assert!(collector.noncopy_type(&syn::parse_quote!((String,)), 2));
            assert!(!collector.noncopy_type(&syn::parse_quote!([String; 2]), 1));
            assert!(collector.noncopy_type(&syn::parse_quote!([String; 2]), 2));
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
                ("First", "value", Some(syn::parse_quote!(u8))),
                ("&First", "value", Some(syn::parse_quote!(u8))),
                ("b::Same", "value", Some(syn::parse_quote!(bool))),
                ("Pair", "0", Some(syn::parse_quote!(u16))),
                ("Pair", "1", Some(syn::parse_quote!(String))),
                ("Pair", "2", None),
                ("Generic<u8>", "value", Some(syn::parse_quote!(_))),
                ("Hidden", "value", None),
                ("Hidden", "kept", Some(syn::parse_quote!(bool))),
                ("Duplicate", "value", None),
                ("(u8, bool)", "0", None),
            ] {
                assert_eq!(
                    calls.field(&syn::parse_str(ty).unwrap(), &syn::parse_str(member).unwrap(), &ROOT, imports),
                    expected,
                    "{ty}.{member}"
                );
            }
            assert!(calls.same_type(&syn::parse_quote!(First), &syn::parse_quote!(crate::a::Same), &ROOT, imports));
            assert!(!calls.same_type(&syn::parse_quote!(First), &syn::parse_quote!(b::Same), &ROOT, imports));
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
        let ast = syn::parse_file(source).unwrap();
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
        assert!(
            temporaries::consuming_sites(&syn::parse_file(source).unwrap()).is_empty(),
            "{source}"
        );
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
        let sites = temporaries::consuming_sites(&syn::parse_file(source).unwrap());
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
        let file = syn::parse_file(source).unwrap();
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
        let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());
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
        let evidence = lexical::evidence(&syn::parse_file(source).unwrap(), &CfgSet::unconditional());
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
            collector.bind_type("owner".into(), syn::parse_quote!(Owner));
            collector.bind_type("optional".into(), syn::parse_quote!(Option<u8>));
            collector.bind_type("result".into(), syn::parse_quote!(Result<u16, bool>));
            for (expression, expected) in [
                ("match value { A => true, B => false }", Some(syn::parse_quote!(bool))),
                ("match value { A => true, B => 'x' }", None),
                ("match value { A => true, B => unknown }", None),
                ("match value {}", None),
                (
                    "match value { A => Ok(true), B => Err(false) }",
                    Some(syn::parse_quote!(std::result::Result<_, _>)),
                ),
                (
                    "match value { A => Ok(true), B => unknown }",
                    Some(syn::parse_quote!(std::result::Result<_, _>)),
                ),
                ("{ true }", Some(syn::parse_quote!(bool))),
                ("{ true; }", None),
                ("{ let value = true; }", None),
                ("{ }", None),
                ("(true, unknown)", Some(syn::parse_quote!((bool, _,)))),
                ("optional?", Some(syn::parse_quote!(u8))),
                ("result?", Some(syn::parse_quote!(u16))),
                ("owner?", None),
                ("owner.owned()", Some(syn::parse_quote!(crate::Owner))),
                ("owner.borrowed()", Some(syn::parse_quote!(bool))),
                ("owner.deferred()", None),
                ("sync_value()", Some(syn::parse_quote!(u32))),
                ("async_value()", None),
            ] {
                assert_eq!(
                    collector.inferred_expression_type(&syn::parse_str(expression).unwrap()),
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
            ("text", syn::parse_quote!(&str)),
            ("owned", syn::parse_quote!(String)),
            ("chars", syn::parse_quote!(std::str::Chars<'_>)),
            ("result", syn::parse_quote!(Result<u8, bool>)),
            ("optional", syn::parse_quote!(Option<u16>)),
            ("foreign", syn::parse_quote!(dependency::Result<u8, bool>)),
            ("number", syn::parse_quote!(u32)),
            ("instant", syn::parse_quote!(std::time::Instant)),
        ] {
            collector.bind_type(name.into(), ty);
        }
        for (expression, expected) in [
            ("text.to_owned()", Some(syn::parse_quote!(std::string::String))),
            ("text.to_string()", Some(syn::parse_quote!(std::string::String))),
            ("owned.to_owned()", Some(syn::parse_quote!(std::string::String))),
            ("owned.to_string()", Some(syn::parse_quote!(std::string::String))),
            ("text.to_owned(1)", None),
            ("owned.to_string(1)", None),
            ("number.to_owned()", None),
            ("text.chars()", Some(syn::parse_quote!(std::str::Chars<'_>))),
            ("owned.chars()", Some(syn::parse_quote!(std::str::Chars<'_>))),
            ("text.chars(1)", None),
            ("number.chars()", None),
            ("chars.next()", Some(syn::parse_quote!(std::option::Option<char>))),
            ("chars.next(1)", None),
            ("number.next()", None),
            ("result.map(transform)", Some(syn::parse_quote!(std::result::Result<_, _>))),
            ("result.map_err(transform)", Some(syn::parse_quote!(std::result::Result<_, _>))),
            ("result.and_then(transform)", Some(syn::parse_quote!(std::result::Result<_, _>))),
            ("result.or_else(transform)", Some(syn::parse_quote!(std::result::Result<_, _>))),
            ("result.map()", None),
            ("result.map(a, b)", None),
            ("foreign.map(transform)", None),
            ("optional.map(transform)", None),
            ("result.unwrap()", Some(syn::parse_quote!(u8))),
            ("result.expect(message)", Some(syn::parse_quote!(u8))),
            ("optional.unwrap_or(1)", Some(syn::parse_quote!(u16))),
            ("optional.unwrap_or_else(make)", Some(syn::parse_quote!(u16))),
            ("foreign.unwrap()", None),
            ("instant.elapsed()", Some(syn::parse_quote!(std::time::Duration))),
            ("instant.duration_since(other)", Some(syn::parse_quote!(std::time::Duration))),
            (
                "instant.saturating_duration_since(other)",
                Some(syn::parse_quote!(std::time::Duration)),
            ),
            ("instant.unknown()", None),
        ] {
            assert_eq!(
                collector.inferred_expression_type(&syn::parse_str(expression).unwrap()),
                expected,
                "{expression}"
            );
        }
    });
}

#[test]
fn standard_constructor_inference_does_not_borrow_local_string_identity() {
    for (source, expression, expected) in [
        ("", "String::new()", Some(syn::parse_quote!(String))),
        ("", "String::from(value)", Some(syn::parse_quote!(String))),
        ("", "String::other()", None),
        ("", "Other::new()", None),
        ("struct String;", "String::new()", None),
        ("use dependency::String;", "String::new()", None),
        ("use std::string::String;", "String::new()", Some(syn::parse_quote!(String))),
        ("use std::string::String as Text;", "Text::new()", Some(syn::parse_quote!(String))),
        ("use dependency::String as Text;", "Text::new()", None),
        ("", "std::string::String::new()", Some(syn::parse_quote!(String))),
        ("", "alloc::string::String::from(value)", Some(syn::parse_quote!(String))),
        ("", "Box::new(true)", Some(syn::parse_quote!(Box<bool>))),
        ("", "Box::new()", None),
        ("", "std::sync::Arc::new('x')", Some(syn::parse_quote!(std::sync::Arc<char>))),
        (
            "",
            "std::cell::RefCell::new(true)",
            Some(syn::parse_quote!(std::cell::RefCell<bool>)),
        ),
        ("", "std::sync::Mutex::new(true)", Some(syn::parse_quote!(std::sync::Mutex<bool>))),
        ("", "dependency::Box::new(true)", None),
    ] {
        with_collector(source, |collector| {
            assert_eq!(
                collector.inferred_expression_type(&syn::parse_str(expression).unwrap()),
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
            ("duration", syn::parse_quote!(std::time::Duration)),
            ("instant", syn::parse_quote!(std::time::Instant)),
            ("system", syn::parse_quote!(std::time::SystemTime)),
            ("date", syn::parse_quote!(chrono::DateTime)),
            ("naive", syn::parse_quote!(chrono::NaiveDateTime)),
            ("delta", syn::parse_quote!(chrono::TimeDelta)),
            ("text", syn::parse_quote!(String)),
            ("suffix", syn::parse_quote!(&str)),
            ("scalar", syn::parse_quote!(u32)),
            ("qualified", syn::parse_quote!(::core::primitive::u32)),
            ("boolean", syn::parse_quote!(::core::primitive::bool)),
            ("foreign", syn::parse_quote!(dependency::Value)),
        ] {
            collector.bind_type(name.into(), ty);
        }
        for (expression, replacement, unsupported, expected_type) in [
            ("duration + duration", "-", false, Some(syn::parse_quote!(std::time::Duration))),
            ("duration - duration", "+", false, Some(syn::parse_quote!(std::time::Duration))),
            ("duration + duration", "*", true, Some(syn::parse_quote!(std::time::Duration))),
            ("duration * scalar", "/", false, Some(syn::parse_quote!(std::time::Duration))),
            ("duration * qualified", "/", false, Some(syn::parse_quote!(std::time::Duration))),
            ("duration / qualified", "%", true, Some(syn::parse_quote!(std::time::Duration))),
            ("duration * boolean", "/", false, None),
            ("duration / scalar", "%", true, Some(syn::parse_quote!(std::time::Duration))),
            ("scalar * duration", "/", true, Some(syn::parse_quote!(std::time::Duration))),
            ("instant + duration", "-", false, Some(syn::parse_quote!(std::time::Instant))),
            ("instant - instant", "+", true, Some(syn::parse_quote!(std::time::Duration))),
            ("system - system", "+", true, Some(syn::parse_quote!(std::time::Duration))),
            ("system + duration", "-", false, Some(syn::parse_quote!(std::time::SystemTime))),
            ("date - date", "+", true, Some(syn::parse_quote!(chrono::TimeDelta))),
            ("naive - naive", "+", true, Some(syn::parse_quote!(chrono::TimeDelta))),
            ("date + delta", "-", false, Some(syn::parse_quote!(chrono::DateTime))),
            ("naive - duration", "+", false, Some(syn::parse_quote!(chrono::NaiveDateTime))),
            ("delta * scalar", "/", false, Some(syn::parse_quote!(chrono::TimeDelta))),
            ("delta + delta", "*", true, Some(syn::parse_quote!(chrono::TimeDelta))),
            ("text + suffix", "-", true, Some(syn::parse_quote!(String))),
            ("scalar % scalar", "+", false, Some(syn::parse_quote!(u32))),
            ("scalar + foreign", "-", false, None),
            ("foreign + scalar", "-", false, None),
            ("unknown + scalar", "-", false, None),
        ] {
            let binary: syn::ExprBinary = syn::parse_str(expression).unwrap();
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
            let binary = syn::parse_str(expression).unwrap();
            assert_eq!(collector.unsupported_operator(&binary, replacement), unsupported, "{expression}");
        }
        for expression in ["duration", "instant", "system", "date", "naive", "delta", "text", "suffix"] {
            assert!(
                collector.unsupported_range_unit(&syn::parse_str(expression).unwrap(), "+"),
                "{expression}"
            );
        }
        for expression in ["scalar", "foreign", "unknown", "1", "1.0"] {
            assert!(
                !collector.unsupported_range_unit(&syn::parse_str(expression).unwrap(), "+"),
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
                ("Some", "Option<u8>", Some(syn::parse_quote!(u8))),
                ("Present", "Option<bool>", Some(syn::parse_quote!(bool))),
                ("core::option::Option::Some", "Option<u8>", Some(syn::parse_quote!(u8))),
                (
                    "std::option::Option::Some",
                    "&'a mut Option<u8>",
                    Some(syn::parse_quote!(&'a mut u8)),
                ),
                ("Ok", "Result<u8, bool>", Some(syn::parse_quote!(u8))),
                ("Err", "Result<u8, bool>", Some(syn::parse_quote!(bool))),
                ("Failure", "Result<u8, bool>", Some(syn::parse_quote!(bool))),
                ("core::result::Result::Ok", "Result<u8, bool>", Some(syn::parse_quote!(u8))),
                ("std::result::Result::Err", "&Result<u8, bool>", Some(syn::parse_quote!(&bool))),
                ("Some", "Result<u8, bool>", None),
                ("Ok", "Option<u8>", None),
                ("Err", "Option<u8>", None),
                ("Foreign", "Option<u8>", None),
                ("Some", "dependency::Option<u8>", None),
                ("std::result::Result::Other", "Result<u8, bool>", None),
            ] {
                assert_eq!(
                    collector.standard_pattern_payload(&syn::parse_str(path).unwrap(), &syn::parse_str(expected).unwrap()),
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
                    .standard_pattern_payload(&syn::parse_str(constructor).unwrap(), &syn::parse_quote!(Option<u8>))
                    .is_none(),
                "{constructor}"
            );
        }
        assert_eq!(
            collector.standard_pattern_payload(
                &syn::parse_quote!(core::option::Option::Some),
                &syn::parse_quote!(core::option::Option<u8>)
            ),
            Some(syn::parse_quote!(u8))
        );
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
                collector.some_constructor(&syn::parse_str(expression).unwrap()),
                expected,
                "{source} / {expression}"
            );
        });
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
        let syn::Stmt::Expr(original, _) = &function.block.stmts[0] else {
            unreachable!()
        };
        let Expr::Call(original_call) = original else { unreachable!() };
        for callee in ["::safe", "safe::<u8>", "<u8 as Trait>::safe"] {
            let mut call = original_call.clone();
            *call.func = syn::parse_str(callee).unwrap();
            assert!(collector.resource_stable_calls(&Expr::Call(call)).is_empty(), "{callee}");
        }
        let mut call = original_call.clone();
        call.args.extend(core::iter::repeat_n::<Expr>(syn::parse_quote!(0), 256));
        assert!(collector.resource_stable_calls(&Expr::Call(call)).is_empty());
        assert!(collector.resource_stable_calls(&syn::parse_quote!(safe())).is_empty());
    });
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
            let syn::Stmt::Expr(expression, _) = function.block.stmts.last().unwrap() else {
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
