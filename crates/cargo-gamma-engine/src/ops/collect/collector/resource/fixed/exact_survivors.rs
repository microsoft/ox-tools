// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::token::Group;
use syn::{ExprGroup, Item, parse_file, parse_quote, parse_str};

use super::*;

/// Supplies known scalars and symbolic collections so guard regressions reach their intended
/// evaluator path rather than fail on unknown receivers. The budget matches a fresh production
/// proof's allowance; these short direct checks do not deliberately exhaust it.
fn machine() -> Machine<'static> {
    Machine {
        values: HashMap::from_iter([
            ("count".into(), Value::Int(7, Primitive::Unsigned(8))),
            ("ready".into(), Value::Bool(true)),
            ("rows".into(), Value::Symbol("rows".into(), Binding::Vec)),
            ("chars".into(), Value::Symbol("chars".into(), Binding::VecChar)),
        ]),
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
fn exact_effect_metadata_is_attached_to_the_interpreted_node() {
    let mut assignment: Expr = parse_quote!(count = 0);
    let Expr::Assign(node) = &mut assignment else { unreachable!() };
    node.attrs.push(parse_quote!(#[custom]));
    let mut compound: Expr = parse_quote!(count += 0);
    let Expr::Binary(node) = &mut compound else { unreachable!() };
    node.attrs.push(parse_quote!(#[custom]));
    for expression in [assignment, compound] {
        let mut evaluator = machine();
        assert_eq!(evaluator.effect(&expression), None);
        assert_eq!(evaluator.values["count"], Value::Int(7, Primitive::Unsigned(8)));
        assert_eq!(evaluator.writes, HashSet::default());
    }
    let mut block: Block = parse_quote!({
        continue;
    });
    let Stmt::Expr(Expr::Continue(node), _) = &mut block.stmts[0] else {
        unreachable!()
    };
    node.attrs.push(parse_quote!(#[custom]));
    let mut evaluator = machine();
    assert_eq!(evaluator.block(&block), None);
    assert!(!evaluator.restarts);
}

#[test]
fn exact_invisible_group_and_literal_metadata_withholds_values() {
    let group = Expr::Group(ExprGroup {
        attrs: vec![parse_quote!(#[custom])],
        group_token: Group::default(),
        expr: Box::new(parse_quote!(true)),
    });
    assert_eq!(machine().eval(&group, None), None);
    let mut literal: Expr = parse_quote!(true);
    let Expr::Lit(node) = &mut literal else { unreachable!() };
    node.attrs.push(parse_quote!(#[custom]));
    assert_eq!(machine().eval(&literal, None), None);
}

#[test]
fn exact_vector_push_metadata_is_not_masked_by_an_unbound_receiver() {
    assert_eq!(machine().effect(&parse_quote!(rows.push('x'))), Some(()));
    let mut attributed: Expr = parse_quote!(rows.push('x'));
    let Expr::MethodCall(call) = &mut attributed else { unreachable!() };
    call.attrs.push(parse_quote!(#[custom]));
    for expression in [attributed, parse_quote!(rows.push::<char>('x'))] {
        let mut evaluator = machine();
        assert_eq!(evaluator.effect(&expression), None);
        assert_eq!(evaluator.values["rows"], Value::Symbol("rows".into(), Binding::Vec));
        assert_eq!(evaluator.writes, HashSet::default());
    }
}

#[test]
fn exact_character_get_guards_are_checked_with_an_available_character_fact() {
    let mut evaluator = machine();
    let character = evaluator.eval(&parse_quote!(chars[0]), None).unwrap();
    let fact = identity(format_args!("{character:?}")).unwrap();
    let facts = HashMap::from_iter([(fact, Value::Char('x'))]);
    for (source, expected) in [
        ("chars.get(0)", Some(Value::MaybeChar('x'))),
        ("chars.get::<u8>(0)", None),
        ("chars.get(0, 1)", None),
        ("chars.get()", None),
    ] {
        let mut evaluator = Machine {
            facts: facts.clone(),
            ..machine()
        };
        assert_eq!(evaluator.eval(&parse_str(source).unwrap(), None), expected, "{source}");
    }
    let mut attributed: Expr = parse_quote!(chars.get(0));
    let Expr::MethodCall(call) = &mut attributed else { unreachable!() };
    call.attrs.push(parse_quote!(#[custom]));
    assert_eq!(Machine { facts, ..machine() }.eval(&attributed, None), None);
}

#[test]
fn exact_width_guards_are_checked_after_certifying_the_width_contract() {
    let calls = HashMap::from_iter([(
        super::super::CHAR_WIDTH.into(),
        super::super::StableCall {
            parameters: vec![Binding::Char],
            output: Binding::Integer,
            char_option: false,
        },
    )]);
    for (source, accepted) in [
        ("'x'.width().unwrap_or(0)", true),
        ("'x'.width().unwrap_or::<usize>(0)", false),
        ("'x'.width::<char>().unwrap_or(0)", false),
        ("'x'.width(0).unwrap_or(0)", false),
        ("'x'.width().unwrap_or(unknown)", false),
        ("unknown.width().unwrap_or(0)", false),
    ] {
        let mut expression: Expr = parse_str(source).unwrap();
        let mut evaluator = Machine {
            stable_calls: Some(&calls),
            ..machine()
        };
        let value = evaluator.eval(&expression, None);
        assert_eq!(
            value,
            accepted.then(|| Value::Symbol("Char('x').width_or_zero".into(), Binding::Integer)),
            "{source}"
        );
        if accepted {
            let Expr::MethodCall(unwrap) = &mut expression else {
                unreachable!()
            };
            unwrap.attrs.push(parse_quote!(#[custom]));
            assert_eq!(
                Machine {
                    stable_calls: Some(&calls),
                    ..machine()
                }
                .eval(&expression, None),
                None
            );
            let Expr::MethodCall(unwrap) = &mut expression else {
                unreachable!()
            };
            unwrap.attrs.clear();
            let Expr::MethodCall(width) = &mut *unwrap.receiver else {
                unreachable!()
            };
            width.attrs.push(parse_quote!(#[custom]));
            assert_eq!(
                Machine {
                    stable_calls: Some(&calls),
                    ..machine()
                }
                .eval(&expression, None),
                None
            );
        }
    }
}

#[test]
fn exact_integer_parse_failure_reaches_the_requested_small_integer_domain() {
    assert_eq!(
        machine().eval(&parse_quote!(127), Some(Binding::Integer)),
        Some(Value::IntegerSmall(NonZeroU8::new(127).unwrap()))
    );
    assert_eq!(machine().eval(&parse_quote!(256), Some(Binding::Integer)), None);
}

#[test]
fn exact_f32_literal_parsing_does_not_double_round_through_f64() {
    let expression: Expr = parse_quote!(1.000000059604644775390625000001_f32);
    assert_eq!(
        machine().eval(&expression, None),
        Some(Value::Float(f64::from(f32::from_bits(0x3f80_0001)).to_bits(), Primitive::Float(32)))
    );
}

#[test]
fn exact_helper_contract_checks_are_not_masked_by_a_parameter_dependent_tail() {
    for (declaration, call) in [
        ("fn helper(value: u8) -> bool { true }", "helper()"),
        ("fn helper() -> bool { true }", "helper(7_u8)"),
        ("fn helper(value: u8) -> bool { true }", "helper(true)"),
        ("fn helper(value: u8) -> bool { true }", "helper(7_u16)"),
        ("fn helper() -> bool { true; true }", "helper()"),
    ] {
        let helpers = HashMap::from_iter([("helper".into(), parse_str(declaration).unwrap())]);
        let mut evaluator = Machine {
            helpers: Some(&helpers),
            ..machine()
        };
        assert_eq!(evaluator.eval(&parse_str(call).unwrap(), None), None, "{declaration}: {call}");
    }
    let helpers = HashMap::from_iter([(
        "helper".into(),
        parse_quote!(
            fn helper() -> bool {
                true
            }
        ),
    )]);
    let mut evaluator = Machine {
        helpers: Some(&helpers),
        ..machine()
    };
    let mut call: Expr = parse_quote!(helper());
    assert_eq!(evaluator.eval(&call, None), Some(Value::Bool(true)));
    let Expr::Call(node) = &mut call else { unreachable!() };
    node.attrs.push(parse_quote!(#[custom]));
    assert_eq!(evaluator.eval(&call, None), None);
}

#[test]
fn exact_primitive_call_names_do_not_ignore_extra_segments_or_a_different_method() {
    for call in ["u8::from::extra(7_u8)", "u16::other(7_u8)"] {
        assert_eq!(machine().eval(&parse_str(call).unwrap(), None), None, "{call}");
    }
}

#[test]
fn exact_helper_discovery_checks_signature_bindings() {
    let file = parse_file("fn helper()->bool {true} fn run(helper: bool) {while true {}}").unwrap();
    let Item::Fn(run) = &file.items[1] else { unreachable!() };
    let target = run.block.stmts.last().unwrap().span().byte_range();
    assert_eq!(helpers(&file, &target).into_keys().collect::<Vec<_>>(), vec!["run"]);
}

#[test]
fn exact_helper_pattern_budget_exhaustion_is_not_masked_by_a_later_expression_visit() {
    for (count, expected) in [(254, vec!["helper".to_owned()]), (255, vec![])] {
        let names = (0..count).map(|at| format!("binding_{at}")).collect::<Vec<_>>().join(",");
        let types = core::iter::repeat_n("u8", count).collect::<Vec<_>>().join(",");
        let file = parse_file(&format!(
            "fn helper()->bool{{true}} fn run(){{while true{{}} let ({names}):({types});}}"
        ))
        .unwrap();
        let Item::Fn(run) = &file.items[1] else { unreachable!() };
        let target = run.block.stmts[0].span().byte_range();
        assert_eq!(
            helpers(&file, &target).into_keys().collect::<Vec<_>>(),
            expected,
            "{count} trailing patterns"
        );
    }
}

#[test]
fn exact_containing_module_name_can_shadow_a_primitive() {
    let file = parse_file("mod u8 { fn run() {while true {}} }").unwrap();
    let Item::Mod(module) = &file.items[0] else { unreachable!() };
    let Item::Fn(run) = &module.content.as_ref().unwrap().1[0] else {
        unreachable!()
    };
    assert!(!numeric_names(&file, &run.block.stmts[0].span().byte_range()));
}

#[test]
fn exact_changed_guard_is_rejected_even_when_both_guards_are_true_on_entry() {
    let bindings = HashMap::from_iter([("ready".into(), Binding::Bool)]);
    let seeds = HashMap::from_iter([("ready".into(), parse_quote!(true))]);
    assert!(!proves(
        &parse_quote!(while ready {
            ready = false;
        }),
        &parse_quote!(while true {}),
        &bindings,
        &seeds,
        &HashMap::default(),
        true
    ));
}

#[test]
fn exact_warmup_rechecks_predicate_identity_not_only_interval_bounds() {
    let bindings = HashMap::from_iter([
        ("input".into(), Binding::Primitive(Primitive::Unsigned(8))),
        ("range".into(), Binding::Primitive(Primitive::Unsigned(8))),
        ("count".into(), Binding::Primitive(Primitive::Unsigned(8))),
    ]);
    let seeds = HashMap::from_iter([
        ("range".into(), parse_quote!(input.wrapping_add(0))),
        ("count".into(), parse_quote!(0)),
    ]);
    let expression: Expr = parse_quote!(while range < 5 {
        if count == 1 {
            range = range.wrapping_add(0);
        }
        count = 1;
    });
    assert!(!proves(&expression, &expression, &bindings, &seeds, &HashMap::default(), true));
}
