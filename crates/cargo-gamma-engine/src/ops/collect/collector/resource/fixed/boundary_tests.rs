// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::token::{Group, Paren};

use super::*;

#[test]
fn branch_assumptions_require_boolean_values_and_replay_consistency() {
    let mut evaluator = machine();
    evaluator.values.insert("flag".into(), Value::Symbol("flag".into(), Binding::Bool));
    evaluator.paths.insert("flag".into(), true);
    assert_eq!(evaluator.guard(&syn::parse_quote!(flag)), Some(Value::Bool(true)));
    assert_eq!(evaluator.assumptions.len(), 1);
    assert_eq!(evaluator.eval(&syn::parse_quote!(flag), None), Some(Value::Bool(true)));
    evaluator.values.insert("flag".into(), Value::Bool(false));
    assert!(evaluator.guard(&syn::parse_quote!(flag)).is_none());
    evaluator.values.insert("flag".into(), Value::Int(1, Primitive::Unsigned(8)));
    assert!(evaluator.guard(&syn::parse_quote!(flag)).is_none());
}

#[test]
fn stable_calls_check_arity_parameter_kinds_and_character_options() {
    let stable_calls = HashMap::from_iter([(
        "helper".into(),
        super::super::StableCall {
            parameters: vec![Binding::CharReference],
            output: Binding::Opaque,
            char_option: true,
        },
    )]);
    let mut evaluator = Machine {
        stable_calls: Some(&stable_calls),
        ..machine()
    };
    assert_eq!(evaluator.eval(&syn::parse_quote!(helper(&'x')), None), Some(Value::MaybeChar('x')));
    assert!(evaluator.eval(&syn::parse_quote!(helper()), None).is_none());
    assert!(evaluator.eval(&syn::parse_quote!(helper('x')), None).is_none());
    let stable_calls = HashMap::from_iter([
        (
            "helper".into(),
            super::super::StableCall {
                parameters: vec![Binding::Bool],
                output: Binding::Opaque,
                char_option: true,
            },
        ),
        (
            super::super::CHAR_WIDTH.into(),
            super::super::StableCall {
                parameters: Vec::new(),
                output: Binding::Integer,
                char_option: false,
            },
        ),
    ]);
    let mut evaluator = Machine {
        stable_calls: Some(&stable_calls),
        ..machine()
    };
    assert!(evaluator.eval(&syn::parse_quote!(helper(true)), None).is_none());
    assert!(evaluator.eval(&syn::parse_quote!('x'.width().unwrap_or(0)), None).is_some());
    for expression in [
        "true.width().unwrap_or(0)",
        "'x'.width().unwrap_or(1)",
        "'x'.other().unwrap_or(0)",
        "true.unwrap_or(0)",
    ] {
        assert!(evaluator.eval(&syn::parse_str(expression).unwrap(), None).is_none(), "{expression}");
    }
}

#[test]
fn focused_match_proofs_do_not_ignore_a_second_change_or_different_selector() {
    let bindings = HashMap::from_iter([("ch".into(), Binding::Char), ("ready".into(), Binding::Bool)]);
    let original: Expr = syn::parse_quote!(while ready {
        match ch {
            'a' => {
                ready = false;
            }
            other => {}
        }
    });
    for (mutated, accepted) in [
        ("while ready { match ch { 'a' => {}, other => {} } }", true),
        ("while ready { match ch { 'a' => { ready = false; }, other => {} } }", false),
        ("while ready { match ch { 'a' => {}, other => { ready = false; } } }", false),
        ("while ready { match ch { 'b' => {}, other => {} } }", false),
        ("while ready { match ready { 'a' => {}, other => {} } }", false),
        ("while ready { match ch { 'a' => true, other => {} } }", false),
    ] {
        assert_eq!(
            proves(
                &original,
                &syn::parse_str(mutated).unwrap(),
                &bindings,
                &HashMap::default(),
                &HashMap::default(),
                true
            ),
            accepted,
            "{mutated}"
        );
    }
    let before: Expr = syn::parse_quote!(while ready {
        match ch {
            other => {
                ready = false;
            }
        }
    });
    let after: Expr = syn::parse_quote!(while ready {
        match ch {
            other => {}
        }
    });
    assert!(proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    let mut bindings = bindings;
    bindings.insert("other".into(), Binding::Char);
    assert!(proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    assert!(!proves(
        &original,
        &syn::parse_quote!(true),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
}

#[test]
fn focused_branch_replay_establishes_only_stable_path_assumptions() {
    let bindings = HashMap::from_iter([("ready".into(), Binding::Bool), ("selected".into(), Binding::Bool)]);
    let original = syn::parse_quote!(while ready {
        if selected {
            ready = false;
        }
    });
    let mutant = syn::parse_quote!(while ready {
        if selected {}
    });
    assert!(proves(
        &original,
        &mutant,
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
    let mutant = syn::parse_quote!(while ready {
        if selected {
            selected = false;
        }
    });
    assert!(!proves(
        &original,
        &mutant,
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
    assert!(!proves(
        &syn::parse_quote!(while unknown {}),
        &syn::parse_quote!(while unknown {}),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
    assert_eq!(Primitive::Float(32).mask(), None);
    assert_eq!(Value::BoolCache(vec![(7, true)]).kind(), Binding::BoolCache);
}

#[test]
fn interpretation_budgets_bound_projection_collection_and_syntax_searches() {
    let projection: Expr = syn::parse_quote!(state.count);
    let tuple = Expr::Tuple(syn::ExprTuple {
        attrs: Vec::new(),
        paren_token: Paren::default(),
        elems: core::iter::repeat_n(projection, 65).collect(),
    });
    assert_eq!(projection_sites(&tuple).len(), 64);
    let mut file = syn::parse_file("fn helper() {} fn f() { while true {} }").unwrap();
    let target = match &file.items[1] {
        syn::Item::Fn(function) => function.block.stmts[0].span().byte_range(),
        _ => unreachable!(),
    };
    let syn::Item::Fn(function) = &mut file.items[1] else {
        unreachable!()
    };
    function
        .block
        .stmts
        .splice(0..0, core::iter::repeat_n(syn::parse_quote!(let x = 1;), 257));
    assert!(helpers(&file, &target).is_empty());
    assert!(!numeric_names(&file, &target));
    let mut oversized = syn::parse_file("fn f() { while true {} }").unwrap();
    let item = oversized.items[0].clone();
    oversized.items.extend(core::iter::repeat_n(item, 256));
    assert!(!numeric_names(&oversized, &target));
    assert!(initializers(&oversized, &target).is_empty());
}

#[test]
fn symbolic_condition_branches_must_agree_on_values_writes_and_control_flow() {
    for (block, accepted) in [
        ("{ if flag { count += 1; } else { count += 1; } }", true),
        ("{ if flag { count += 1; } }", false),
        ("{ if flag { continue; } else { continue; } }", true),
        ("{ if flag { continue; } else {} }", false),
        ("{ count == 1_u8; }", true),
        ("{ count = 1; }", true),
        ("{ text = 1; }", false),
        ("{ let x = true else { return; }; }", false),
    ] {
        let mut evaluator = machine();
        evaluator.values.insert("flag".into(), Value::Symbol("flag".into(), Binding::Bool));
        evaluator.values.insert("count".into(), Value::Int(0, Primitive::Unsigned(8)));
        evaluator
            .values
            .insert("text".into(), Value::Symbol("text".into(), Binding::String));
        assert_eq!(evaluator.block(&syn::parse_str(block).unwrap()).is_some(), accepted, "{block}");
    }
    for (source, expected) in [
        ("1.0_f64 + 2.0_f64", float(3.0, Primitive::Float(64))),
        ("1.0_f64 < 2.0_f64", Some(Value::Bool(true))),
        ("b'x'", None),
    ] {
        assert_eq!(machine().eval(&syn::parse_str(source).unwrap(), None), expected, "{source}");
    }
    for operator in [syn::parse_quote!(&&), syn::parse_quote!(||)] {
        let and = matches!(operator, BinOp::And(_));
        assert_eq!(binary(&operator, Value::Bool(true), Value::Bool(false)), Some(Value::Bool(!and)));
    }
    let unknown = Value::Symbol("n".into(), Binding::Integer);
    assert_eq!(binary(&syn::parse_quote!(*), unknown, Value::IntegerZero), Some(Value::IntegerZero));
    assert_eq!(float(1.0, Primitive::Unsigned(8)), None);
    let mut evaluator = machine();
    let group = Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: Group::default(),
        expr: Box::new(syn::parse_quote!(true)),
    });
    assert_eq!(evaluator.eval(&group, None), Some(Value::Bool(true)));
}

#[test]
fn invalid_character_facts_and_changed_match_boundaries_cannot_prove_recurrence() {
    let mut bindings = HashMap::from_iter([("ready".into(), Binding::Bool), ("ch".into(), Binding::Bool)]);
    let before = syn::parse_quote!(while ready {
        match ch {
            'a' => {
                ready = false;
            }
            _ => {}
        }
    });
    let after = syn::parse_quote!(while ready {
        match ch {
            'a' => {}
            _ => {}
        }
    });
    assert!(!proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    bindings.insert("ch".into(), Binding::Char);
    for source in [
        "while ready { match ch { #[cfg(test)] 'a' => {}, _ => {} } }",
        "while ready { match ch { 'a' => 'label: {}, _ => {} } }",
        "while ready { match ch { ref bound => {}, _ => {} } }",
    ] {
        assert!(
            changed_arm(
                match &before {
                    Expr::While(node) => &node.body,
                    _ => unreachable!(),
                },
                match &syn::parse_str::<Expr>(source).unwrap() {
                    Expr::While(node) => &node.body,
                    _ => unreachable!(),
                },
            )
            .is_none(),
            "{source}"
        );
    }
}

#[test]
fn replay_checks_owned_growth_and_focused_match_scalar_writes() {
    let bindings = HashMap::from_iter([
        ("ready".into(), Binding::Bool),
        ("selected".into(), Binding::Bool),
        ("tokens".into(), Binding::Vec),
        ("ch".into(), Binding::Char),
        ("count".into(), Binding::Primitive(Primitive::Unsigned(8))),
    ]);
    let before = syn::parse_quote!(while ready {
        if selected {
            tokens.push('a');
            ready = false;
        }
    });
    let after = syn::parse_quote!(while ready {
        if selected {
            tokens.push('a');
        }
    });
    assert!(proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    let before = syn::parse_quote!(while ready {
        match ch {
            _ => {
                count += 1;
            }
        }
    });
    let after = syn::parse_quote!(while ready {
        match ch {
            _ => {
                count = 0;
            }
        }
    });
    assert!(!proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    let before = syn::parse_quote!({
        match ch {
            ref character => {
                count += 1;
            }
        }
    });
    let after = syn::parse_quote!({
        match ch {
            ref character => {}
        }
    });
    assert!(changed_arm(&before, &after).is_none());
}

#[test]
fn numeric_names_are_unknown_beyond_module_and_item_search_budgets() {
    let mut file = syn::parse_file("mod child { fn f() { while true {} } }").unwrap();
    let syn::Item::Mod(module) = &mut file.items[0] else {
        unreachable!()
    };
    let items = &mut module.content.as_mut().unwrap().1;
    let item = items[0].clone();
    let target = item.span().byte_range();
    items.extend(core::iter::repeat_n(item, 256));
    assert!(!numeric_names(&file, &target));
    items_clear_for_budget(&mut file);
    assert!(!numeric_names(&file, &target));
}

fn items_clear_for_budget(file: &mut syn::File) {
    let syn::Item::Mod(module) = &mut file.items[0] else {
        unreachable!()
    };
    let items = &mut module.content.as_mut().unwrap().1;
    items.truncate(255);
    let nested = module.clone();
    file.items.push(syn::Item::Mod(nested));
}

fn machine() -> Machine<'static> {
    Machine {
        values: HashMap::default(),
        budget: 256,
        writes: HashSet::default(),
        locals: HashSet::default(),
        facts: HashMap::default(),
        helpers: None,
        restarts: false,
        numeric_names: true,
        stable_calls: None,
        paths: HashMap::default(),
        assumptions: Vec::new(),
    }
}

#[test]
fn floating_arithmetic_and_casts_preserve_declared_precision_at_rounding_boundaries() {
    for (source, bits, primitive) in [
        ("16777216.0_f32 + 1.0_f32", 16_777_216_f64.to_bits(), Primitive::Float(32)),
        ("16777216.0_f64 + 1.0_f64", 16_777_217_f64.to_bits(), Primitive::Float(64)),
        ("16777216.0_f32 - (-1.0_f32)", 16_777_216_f64.to_bits(), Primitive::Float(32)),
        ("16777216.0_f64 - (-1.0_f64)", 16_777_217_f64.to_bits(), Primitive::Float(64)),
        ("1.0_f32 / 3.0_f32", 0x3fd5_5555_6000_0000, Primitive::Float(32)),
        ("1.0_f64 / 3.0_f64", 0x3fd5_5555_5555_5555, Primitive::Float(64)),
        ("16777217_u32 as f32", 16_777_216_f64.to_bits(), Primitive::Float(32)),
        ("16777217_u32 as f64", 16_777_217_f64.to_bits(), Primitive::Float(64)),
        ("(-16777217_i32) as f32", (-16_777_216_f64).to_bits(), Primitive::Float(32)),
        ("(-16777217_i32) as f64", (-16_777_217_f64).to_bits(), Primitive::Float(64)),
        (
            "18014399583223809_u128 as f32",
            18_014_400_656_965_632_f64.to_bits(),
            Primitive::Float(32),
        ),
    ] {
        assert_eq!(
            machine().eval(
                &syn::parse_str(source).expect("the fixed primitive expression is valid Rust syntax"),
                None
            ),
            Some(Value::Float(bits, primitive)),
            "{source}"
        );
    }
    for source in [
        "1.0_f32 < 1.0_f32",
        "1.0_f64 < 1.0_f64",
        "(0.0_f32 / 0.0_f32) < 1.0_f32",
        "(0.0_f64 / 0.0_f64) == (0.0_f64 / 0.0_f64)",
    ] {
        assert_eq!(
            machine().eval(&syn::parse_str(source).expect("the fixed comparison is valid Rust syntax"), None),
            Some(Value::Bool(false)),
            "{source}"
        );
    }
}

#[test]
fn signed_arithmetic_keeps_negative_results_and_withholds_overflow_and_signed_shift_proofs() {
    for (source, expected) in [
        ("(-7_i8) + 2_i8", Some(Value::Int(251, Primitive::Signed(8)))),
        ("(-7_i8) - 2_i8", Some(Value::Int(247, Primitive::Signed(8)))),
        ("(-7_i8) * 2_i8", Some(Value::Int(242, Primitive::Signed(8)))),
        ("(-7_i8) * (-2_i8)", Some(Value::Int(14, Primitive::Signed(8)))),
        ("(-128_i8) + 0_i8", Some(Value::Int(128, Primitive::Signed(8)))),
        ("(-128_i8) - 1_i8", None),
        ("127_i8 + 1_i8", None),
        ("64_i8 * 2_i8", None),
        ("1_i8 >> 1_i8", None),
        ("(-2_i8) >> 1_i8", None),
        ("128_u8 >> 7_u8", Some(Value::Int(1, Primitive::Unsigned(8)))),
        ("128_u8 >> 8_u8", None),
        ("1_u128 << 4294967296_u128", None),
        ("1_u128 >> 4294967296_u128", None),
        ("340282366920938463463374607431768211455_u128 + 1_u128", None),
        ("340282366920938463463374607431768211455_u128 * 2_u128", None),
        ("170141183460469231731687303715884105727_i128 + 1_i128", None),
        ("(-170141183460469231731687303715884105728_i128) - 1_i128", None),
        ("170141183460469231731687303715884105727_i128 * 2_i128", None),
    ] {
        assert_eq!(
            machine().eval(
                &syn::parse_str(source).expect("the fixed arithmetic expression is valid Rust syntax"),
                None
            ),
            expected,
            "{source}"
        );
    }
}

#[test]
fn boolean_truth_tables_and_symbolic_zero_laws_do_not_certify_mixed_kinds() {
    for (left, right, and, or) in [
        (false, false, false, false),
        (false, true, false, true),
        (true, false, false, true),
        (true, true, true, true),
    ] {
        assert_eq!(
            binary(&syn::parse_quote!(&&), Value::Bool(left), Value::Bool(right)),
            Some(Value::Bool(and))
        );
        assert_eq!(
            binary(&syn::parse_quote!(||), Value::Bool(left), Value::Bool(right)),
            Some(Value::Bool(or))
        );
    }
    let unknown = Value::Symbol("length".into(), Binding::Integer);
    let flag = Value::Symbol("flag".into(), Binding::Bool);
    for (operator, left, right, expected) in [
        (syn::parse_quote!(-), unknown.clone(), unknown.clone(), Some(Value::IntegerZero)),
        (
            syn::parse_quote!(-),
            unknown.clone(),
            Value::Symbol("other".into(), Binding::Integer),
            None,
        ),
        (syn::parse_quote!(*), unknown.clone(), Value::Int(0, Primitive::Unsigned(8)), None),
        (syn::parse_quote!(+), Value::IntegerZero, flag.clone(), None),
        (syn::parse_quote!(*), Value::IntegerZero, flag.clone(), None),
        (syn::parse_quote!(+), Value::Bool(true), Value::IntegerZero, None),
        (syn::parse_quote!(&&), unknown.clone(), Value::Bool(false), None),
        (syn::parse_quote!(||), unknown, Value::Bool(true), None),
        (syn::parse_quote!(&&), flag.clone(), Value::Bool(false), Some(Value::Bool(false))),
        (syn::parse_quote!(||), flag.clone(), Value::Bool(true), Some(Value::Bool(true))),
        (syn::parse_quote!(+), flag.clone(), flag.clone(), None),
        (syn::parse_quote!(&&), flag, Value::IntegerZero, None),
    ] {
        assert_eq!(
            binary(&operator, left.clone(), right.clone()),
            expected,
            "{left:?} {operator:?} {right:?}"
        );
    }
}

#[test]
fn projection_shadows_restore_updated_outer_values_and_remove_local_descendants() {
    let mut evaluator = machine();
    evaluator.values = HashMap::from_iter([
        ("state".into(), Value::Symbol("outer".into(), Binding::Opaque)),
        ("state.count".into(), Value::Int(7, Primitive::Unsigned(8))),
        ("state.inner.count".into(), Value::Int(9, Primitive::Unsigned(8))),
        ("stateful.count".into(), Value::Int(11, Primitive::Unsigned(8))),
    ]);
    assert_eq!(
        evaluator.block(&syn::parse_quote!({
            state.count += 1;
            {
                let state = false;
                let state = true;
            }
            state.count += 2;
        })),
        Some(())
    );
    assert_eq!(
        evaluator.values,
        HashMap::from_iter([
            ("state".into(), Value::Symbol("outer".into(), Binding::Opaque)),
            ("state.count".into(), Value::Int(10, Primitive::Unsigned(8))),
            ("state.inner.count".into(), Value::Int(9, Primitive::Unsigned(8))),
            ("stateful.count".into(), Value::Int(11, Primitive::Unsigned(8))),
        ])
    );
    assert_eq!(evaluator.writes, HashSet::from_iter(["state.count".into()]));
    assert_eq!(evaluator.locals, HashSet::default());
}

#[test]
fn helper_evaluation_checks_parameter_and_result_types_and_rejects_effects() {
    for (source, call, expected) in [
        (
            "fn helper(x: u8) -> u16 { u16::from(x) }",
            "helper(7_u8)",
            Some(Value::Int(7, Primitive::Unsigned(16))),
        ),
        ("fn helper(x: bool) -> bool { !x }", "helper(false)", Some(Value::Bool(true))),
        ("fn helper() -> u8 { 9 }", "helper()", Some(Value::Int(9, Primitive::Unsigned(8)))),
        ("fn helper(x: u8) -> u8 { x }", "helper()", None),
        ("fn helper(ref x: u8) -> u8 { x }", "helper(7)", None),
        ("fn helper((x,): (u8,)) -> u8 { x }", "helper(7)", None),
        ("fn helper(x: bool) -> bool { x }", "helper(7_u8)", None),
        ("fn helper() { true }", "helper()", None),
        ("fn helper() -> bool {}", "helper()", None),
        ("fn helper() -> bool { true; }", "helper()", None),
        ("fn helper() -> bool { let x = false; x }", "helper()", None),
        ("fn helper() -> bool { 7_u8 }", "helper()", None),
    ] {
        let helpers = HashMap::from_iter([("helper".into(), syn::parse_str(source).unwrap())]);
        let mut evaluator = Machine {
            helpers: Some(&helpers),
            ..machine()
        };
        assert_eq!(evaluator.eval(&syn::parse_str(call).unwrap(), None), expected, "{source}");
    }
    let helpers = HashMap::from_iter([(
        "helper".into(),
        syn::parse_quote!(
            fn helper() -> bool {
                true
            }
        ),
    )]);
    let mut evaluator = Machine {
        helpers: Some(&helpers),
        ..machine()
    };
    evaluator.values.insert("helper".into(), Value::Bool(false));
    assert_eq!(evaluator.eval(&syn::parse_quote!(helper()), None), None);
}

#[test]
fn scalar_evaluation_distinguishes_known_values_unknowns_and_unsupported_operations() {
    for (source, expected_kind, expected) in [
        (
            "1.5",
            Some(Binding::Primitive(Primitive::Float(32))),
            float(1.5, Primitive::Float(32)),
        ),
        ("1.5", None, None),
        ("-1.5_f64", None, float(-1.5, Primitive::Float(64))),
        ("1.5_f32", None, float(1.5, Primitive::Float(32))),
        (
            "Default::default()",
            Some(Binding::Primitive(Primitive::Unsigned(16))),
            Some(Value::Int(0, Primitive::Unsigned(16))),
        ),
        (
            "Default::default()",
            Some(Binding::Primitive(Primitive::Float(64))),
            float(0.0, Primitive::Float(64)),
        ),
        ("1.0_f32 + 2.0_f32", None, float(3.0, Primitive::Float(32))),
        ("5.0_f32 - 2.0_f32", None, float(3.0, Primitive::Float(32))),
        ("1.5_f32 * 2.0_f32", None, float(3.0, Primitive::Float(32))),
        ("6.0_f32 / 2.0_f32", None, float(3.0, Primitive::Float(32))),
        ("1.0_f32 < 2.0_f32", None, Some(Value::Bool(true))),
        ("2.0_f32 == 2.0_f32", None, Some(Value::Bool(true))),
        ("1.0_f32 > 2.0_f32", None, None),
        ("5.0_f64 - 2.0_f64", None, float(3.0, Primitive::Float(64))),
        ("1.5_f64 * 2.0_f64", None, float(3.0, Primitive::Float(64))),
        ("2.0_f64 == 2.0_f64", None, Some(Value::Bool(true))),
        ("1.0_f64 > 2.0_f64", None, None),
        ("1_u8 / 1_u8", None, None),
        ("1 / 1", Some(Binding::Integer), None),
        ("'a' == 'a'", None, Some(Value::Bool(true))),
        ("'a' != 'b'", None, Some(Value::Bool(true))),
        ("'a' < 'b'", None, None),
        ("'a' == true", None, None),
        ("u32::from(1_u16)", None, Some(Value::Int(1, Primitive::Unsigned(32)))),
        ("1_u8 as f32", None, float(1.0, Primitive::Float(32))),
        ("(-1_i8) as f32", None, float(-1.0, Primitive::Float(32))),
        ("1.5_f64 as f32", None, float(1.5, Primitive::Float(32))),
        ("true as u8", None, None),
    ] {
        assert_eq!(
            machine().eval(&syn::parse_str(source).unwrap(), expected_kind),
            expected,
            "{source}"
        );
    }
    for source in [
        "value.len()",
        "value.push(0)",
        "value[0]",
        "value.get(0)",
        "value.wrapping_add(1_u8)",
        "&value",
    ] {
        let mut evaluator = machine();
        evaluator.values.insert("value".into(), Value::Bool(true));
        assert!(evaluator.effect(&syn::parse_str(source).unwrap()).is_none(), "{source}");
    }
}

#[test]
fn unknown_scalar_identities_preserve_zero_laws_without_inventing_other_arithmetic() {
    for (source, accepted) in [
        ("x + 0_u8", true),
        ("x - 0_u8", true),
        ("x * 0_u8", true),
        ("0_u8 * x", true),
        ("x / 0_u8", false),
        ("x.wrapping_add(0_u8)", true),
        ("x.wrapping_sub(0_u8)", true),
        ("x.wrapping_mul(0_u8)", true),
        ("x.unknown(0_u8)", false),
    ] {
        let mut evaluator = machine();
        evaluator
            .values
            .insert("x".into(), Value::Symbol("x".into(), Binding::Primitive(Primitive::Signed(8))));
        let expression: Expr = syn::parse_str(source.replace("u8", "i8").as_str()).unwrap();
        let value = evaluator.eval(&expression, None);
        assert_eq!(value.is_some(), accepted, "{source}");
        if source.contains('*') || source.contains("wrapping_mul") {
            assert_eq!(value, Some(Value::Int(0, Primitive::Signed(8))));
        }
    }
    let unknown = Value::Symbol("n".into(), Binding::Integer);
    for (operator, left, right, expected) in [
        (syn::parse_quote!(+), Value::IntegerZero, unknown.clone(), Some(unknown.clone())),
        (syn::parse_quote!(*), Value::IntegerZero, unknown.clone(), Some(Value::IntegerZero)),
        (syn::parse_quote!(-), Value::IntegerZero, unknown.clone(), None),
        (
            syn::parse_quote!(+),
            unknown.clone(),
            Value::Int(0, Primitive::Unsigned(8)),
            Some(unknown.clone()),
        ),
        (syn::parse_quote!(-), unknown.clone(), Value::IntegerZero, Some(unknown)),
        (
            syn::parse_quote!(==),
            Value::MaybeChar('a'),
            Value::MaybeChar('a'),
            Some(Value::Bool(true)),
        ),
        (
            syn::parse_quote!(!=),
            Value::MaybeChar('a'),
            Value::MaybeChar('b'),
            Some(Value::Bool(true)),
        ),
        (syn::parse_quote!(<), Value::MaybeChar('a'), Value::MaybeChar('b'), None),
    ] {
        assert_eq!(binary(&operator, left, right), expected);
    }
}

#[test]
fn scalar_blocks_reject_nonlocal_control_and_restore_nested_shadows() {
    for source in [
        "{ let (x, y) = (1, 2); }",
        "{ let ref x = 1; }",
        "{ let x; }",
        "{ let Some(x) = unknown else { return; }; }",
        "{ #[unknown] let x = true; }",
        "{ item!(); }",
        "{ let x: bool = true; }",
    ] {
        let mut evaluator = machine();
        evaluator.numeric_names = false;
        assert!(evaluator.block(&syn::parse_str(source).unwrap()).is_none(), "{source}");
    }
    let mut evaluator = machine();
    evaluator.values.insert("flag".into(), Value::Bool(true));
    evaluator.values.insert("count".into(), Value::Int(7, Primitive::Unsigned(8)));
    assert!(
        evaluator
            .block(&syn::parse_quote!({
                if false {
                    count = 99;
                } else {
                    count -= 2;
                }
                while false {
                    unknown();
                }
                flag = false;
            }))
            .is_some()
    );
    assert_eq!(evaluator.values["count"], Value::Int(5, Primitive::Unsigned(8)));
    assert_eq!(evaluator.values["flag"], Value::Bool(false));
}

#[test]
fn symbol_sources_require_matching_owned_receivers_and_known_character_facts() {
    let mut evaluator = machine();
    evaluator.values.extend([
        ("text".into(), Value::Symbol("text".into(), Binding::StrReference)),
        ("strings".into(), Value::Symbol("strings".into(), Binding::VecString)),
        ("chars".into(), Value::Symbol("chars".into(), Binding::VecChar)),
    ]);
    for expression in ["text[0..]", "strings[0]", "&strings[0]", "text.chars().next().expect(\"nonempty\")"] {
        assert!(evaluator.eval(&syn::parse_str(expression).unwrap(), None).is_some(), "{expression}");
    }
    for expression in [
        "text[0..1]",
        "text[..]",
        "text[0..=1]",
        "text.chars().next().expect(0)",
        "text.len().expect(\"x\")",
        "text.chars().len().expect(\"x\")",
        "text.chars(1).next().expect(\"x\")",
        "chars.get(0)",
        "strings.get(0)",
    ] {
        assert!(evaluator.eval(&syn::parse_str(expression).unwrap(), None).is_none(), "{expression}");
    }
    let character = evaluator.eval(&syn::parse_quote!(chars[0]), None).unwrap();
    evaluator
        .facts
        .insert(identity(format_args!("{character:?}")).unwrap(), Value::Char('a'));
    assert_eq!(evaluator.eval(&syn::parse_quote!(chars.get(0)), None), Some(Value::MaybeChar('a')));
    assert_eq!(evaluator.eval(&syn::parse_quote!(&chars[0]), None), Some(Value::CharReference('a')));
}

#[test]
fn initializer_and_numeric_scope_searches_fail_closed_on_ambiguous_syntax() {
    for source in [
        "mod child { fn f() { let i = -1; let x = (2_u8 as u32); while true {} } }",
        "struct S; impl S { fn f() { let i = 1; while true {} } }",
    ] {
        let file = syn::parse_file(source).unwrap();
        let start = source.find("while").unwrap();
        let values = initializers(&file, &(start..start + "while true {}".len()));
        assert!(values.contains_key("i"));
    }
    for source in [
        "extern crate custom as u8; fn f() { while true {} }",
        "union u8 { value: u8 } fn f() { while true {} }",
        "trait u8 {} fn f() { while true {} }",
        "mod u8 {} fn f() { while true {} }",
        "fn f<u8>() { while true {} }",
        "use thing::{self as u8}; fn f() { while true {} }",
        "use thing::*; fn f() { while true {} }",
        "fn f() { unknown!(); while true {} }",
    ] {
        let file = syn::parse_file(source).unwrap();
        let start = source.find("while").unwrap();
        assert!(!numeric_names(&file, &(start..start + 13)), "{source}");
    }
    let file = syn::parse_file("fn helper() {} fn f() { item!(); while true {} }").unwrap();
    assert!(helpers(&file, &(36..49)).is_empty());
    let file = syn::parse_file("fn helper() {} fn f() { struct S; while true {} }").unwrap();
    assert!(helpers(&file, &(33..46)).is_empty());
    assert!(constant_syntax(&syn::parse_quote!(([1, 2], -3, (4_u8 as u32)))));
    assert!(!constant_syntax(&syn::parse_quote!(unknown())));
}

#[test]
fn unsigned_arithmetic_and_comparisons_match_checked_native_boundaries() {
    for a in [0_u8, 1, 2, 7, 127, 128, 254, 255] {
        for b in [0_u8, 1, 2, 7, 8, 127, 128, 254, 255] {
            let ty = Primitive::Unsigned(8);
            let left = Value::Int(u128::from(a), ty);
            let right = Value::Int(u128::from(b), ty);
            for (op, expected) in [
                (syn::parse_quote!(+), a.checked_add(b)),
                (syn::parse_quote!(-), a.checked_sub(b)),
                (syn::parse_quote!(*), a.checked_mul(b)),
                (syn::parse_quote!(<<), a.checked_shl(u32::from(b))),
                (syn::parse_quote!(>>), a.checked_shr(u32::from(b))),
            ] {
                assert_eq!(
                    binary(&op, left.clone(), right.clone()),
                    expected.map(|value| Value::Int(u128::from(value), ty)),
                    "{a} {op:?} {b}"
                );
            }
            for (op, expected) in [
                (syn::parse_quote!(==), a == b),
                (syn::parse_quote!(!=), a != b),
                (syn::parse_quote!(<), a < b),
                (syn::parse_quote!(<=), a <= b),
                (syn::parse_quote!(>), a > b),
                (syn::parse_quote!(>=), a >= b),
            ] {
                assert_eq!(binary(&op, left.clone(), right.clone()), Some(Value::Bool(expected)));
            }
        }
    }
}

#[test]
fn signed_arithmetic_uses_signed_ordering_and_rejects_both_overflow_edges() {
    for a in [i8::MIN, -127, -2, -1, 0, 1, 2, 126, i8::MAX] {
        for b in [i8::MIN, -127, -2, -1, 0, 1, 2, 126, i8::MAX] {
            let ty = Primitive::Signed(8);
            let encode = |value: i8| Value::Int(u128::from(value.cast_unsigned()), ty);
            for (op, expected) in [
                (syn::parse_quote!(+), a.checked_add(b)),
                (syn::parse_quote!(-), a.checked_sub(b)),
                (syn::parse_quote!(*), a.checked_mul(b)),
            ] {
                assert_eq!(binary(&op, encode(a), encode(b)), expected.map(encode), "{a} {op:?} {b}");
            }
            for (op, expected) in [
                (syn::parse_quote!(==), a == b),
                (syn::parse_quote!(!=), a != b),
                (syn::parse_quote!(<), a < b),
                (syn::parse_quote!(<=), a <= b),
                (syn::parse_quote!(>), a > b),
                (syn::parse_quote!(>=), a >= b),
            ] {
                assert_eq!(binary(&op, encode(a), encode(b)), Some(Value::Bool(expected)), "{a} {op:?} {b}");
            }
        }
    }
}

#[test]
fn untyped_integer_construction_keeps_one_zero_and_the_shared_precision_bound() {
    for input in 0..=u8::MAX {
        match Value::integer(input) {
            Some(Value::IntegerZero) => assert_eq!(input, 0),
            Some(Value::IntegerSmall(value)) => {
                assert!((1..=127).contains(&input));
                assert_eq!(value.get(), input);
            }
            None => assert!(input > 127),
            Some(other) => panic!("unexpected integer representation for {input}: {other:?}"),
        }
    }
}

#[test]
fn small_untyped_integer_domain_keeps_its_precision_limit() {
    let encode = |value: u8| {
        if value == 0 {
            Value::IntegerZero
        } else {
            Value::IntegerSmall(NonZeroU8::new(value).unwrap())
        }
    };
    for a in [0_u8, 1, 2, 63, 64, 126, 127] {
        for b in [0_u8, 1, 2, 63, 64, 126, 127] {
            for (op, expected) in [
                (syn::parse_quote!(+), a.checked_add(b)),
                (syn::parse_quote!(-), a.checked_sub(b)),
                (syn::parse_quote!(*), a.checked_mul(b)),
            ] {
                assert_eq!(
                    binary(&op, encode(a), encode(b)),
                    expected.filter(|value| *value <= 127).map(encode),
                    "{a} {op:?} {b}"
                );
            }
            for (op, expected) in [
                (syn::parse_quote!(==), a == b),
                (syn::parse_quote!(!=), a != b),
                (syn::parse_quote!(<), a < b),
                (syn::parse_quote!(<=), a <= b),
                (syn::parse_quote!(>), a > b),
                (syn::parse_quote!(>=), a >= b),
            ] {
                assert_eq!(binary(&op, encode(a), encode(b)), Some(Value::Bool(expected)));
            }
        }
    }
}

#[test]
fn cast_saturation_and_sign_extension_match_native_operations() {
    for value in [i8::MIN, -1, 0, 1, i8::MAX] {
        let source = Value::Int(u128::from(value.cast_unsigned()), Primitive::Signed(8));
        assert_eq!(
            cast(source.clone(), Primitive::Signed(16)),
            Some(Value::Int(u128::from(i16::from(value).cast_unsigned()), Primitive::Signed(16)))
        );
        assert_eq!(
            cast(source, Primitive::Float(64)),
            Some(Value::Float(f64::from(value).to_bits(), Primitive::Float(64)))
        );
    }
    for value in [
        f64::NEG_INFINITY,
        -129.0,
        -128.0,
        -1.5,
        -0.0,
        0.0,
        1.5,
        127.0,
        128.0,
        255.0,
        256.0,
        f64::INFINITY,
        f64::NAN,
    ] {
        let source = Value::Float(value.to_bits(), Primitive::Float(64));
        // These native casts are the independent oracle for Rust's saturation semantics.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "native casts are the test oracle"
        )]
        let expected = (value as u8, value as i8);
        assert_eq!(
            cast(source.clone(), Primitive::Unsigned(8)),
            Some(Value::Int(u128::from(expected.0), Primitive::Unsigned(8)))
        );
        assert_eq!(
            cast(source, Primitive::Signed(8)),
            Some(Value::Int(u128::from(expected.1.cast_unsigned()), Primitive::Signed(8)))
        );
    }
}

#[test]
fn literal_evaluation_and_short_circuiting_preserve_unknown_operands() {
    for (expression, expected) in [
        ("false && unknown()", Some(Value::Bool(false))),
        ("true || unknown()", Some(Value::Bool(true))),
        ("true && unknown()", None),
        ("false || unknown()", None),
        ("!false", Some(Value::Bool(true))),
        ("!true", Some(Value::Bool(false))),
        ("-128_i8", Some(Value::Int(128, Primitive::Signed(8)))),
        ("-(-128_i8)", None),
        ("256_u8", None),
        ("127", Some(Value::IntegerSmall(NonZeroU8::new(127).unwrap()))),
        ("128", None),
        ("0", Some(Value::IntegerZero)),
        ("u32::from(255_u8)", Some(Value::Int(255, Primitive::Unsigned(32)))),
        ("u8::from(256_u32)", None),
        ("Default::default()", None),
    ] {
        assert_eq!(
            machine().eval(&syn::parse_str(expression).unwrap(), Some(Binding::Integer)),
            expected,
            "{expression}"
        );
    }
}

#[test]
fn concrete_wrapping_methods_match_native_underflow_overflow_and_zero() {
    for a in [0_u8, 1, 127, 128, 254, 255] {
        for b in [0_u8, 1, 2, 127, 128, 255] {
            let mut evaluator = machine();
            evaluator
                .values
                .insert("a".into(), Value::Int(u128::from(a), Primitive::Unsigned(8)));
            evaluator
                .values
                .insert("b".into(), Value::Int(u128::from(b), Primitive::Unsigned(8)));
            for (expression, expected) in [
                (syn::parse_quote!(a.wrapping_add(b)), a.wrapping_add(b)),
                (syn::parse_quote!(a.wrapping_sub(b)), a.wrapping_sub(b)),
                (syn::parse_quote!(a.wrapping_mul(b)), a.wrapping_mul(b)),
            ] {
                assert_eq!(
                    evaluator.eval(&expression, None),
                    Some(Value::Int(u128::from(expected), Primitive::Unsigned(8)))
                );
            }
        }
    }
}

#[test]
fn block_scope_restores_shadowed_projections_but_preserves_outer_writes() {
    let mut evaluator = machine();
    evaluator.values.insert("state".into(), Value::Int(7, Primitive::Unsigned(8)));
    evaluator.values.insert("state.count".into(), Value::Int(9, Primitive::Unsigned(8)));
    evaluator
        .values
        .insert("stateful.count".into(), Value::Int(11, Primitive::Unsigned(8)));
    evaluator.values.insert("outer".into(), Value::Int(1, Primitive::Unsigned(8)));
    assert!(
        evaluator
            .block(&syn::parse_quote!({
                let mut state: u8 = 0;
                state += 1;
                outer += state;
            }))
            .is_some()
    );
    assert_eq!(evaluator.values["state"], Value::Int(7, Primitive::Unsigned(8)));
    assert_eq!(evaluator.values["state.count"], Value::Int(9, Primitive::Unsigned(8)));
    assert_eq!(evaluator.values["stateful.count"], Value::Int(11, Primitive::Unsigned(8)));
    assert_eq!(evaluator.values["outer"], Value::Int(2, Primitive::Unsigned(8)));
    assert!(evaluator.locals.is_empty());
    assert!(!evaluator.values.contains_key("state.other"));
}
