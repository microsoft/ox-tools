// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use proc_macro2::Span;
use syn::token::{Group, Paren};
use syn::{ExprBlock, ExprGroup, ExprTuple, File, Item, LitStr, parse_file, parse_quote, parse_str};

use super::*;

#[test]
fn scalar_local_loop_baseline_is_retained_by_the_quick_proof_but_detected_by_fixed_analysis() {
    let source = "while true { let local = true; }";
    let expression: Expr = parse_str(source).unwrap();
    let proof = super::super::LoopProof::new(&expression, HashMap::default(), true).unwrap();
    assert!(proof.proves(&expression).is_none());
    assert!(proves_with_effects(
        &expression,
        &expression,
        &HashMap::default(),
        &HashMap::default(),
        &HashMap::default(),
        true,
        &HashMap::default()
    ));
    let start = source.rfind("true").unwrap();
    let span = start..start + "true".len();
    let mut mutated = source.to_owned();
    mutated.replace_range(span.clone(), "false");
    let mutated: Expr = parse_str(&mutated).unwrap();
    assert!(proof.proves(&mutated).is_none());
    assert!(proves_with_effects(
        &expression,
        &mutated,
        &HashMap::default(),
        &HashMap::default(),
        &HashMap::default(),
        true,
        &HashMap::default()
    ));
    assert!(!proof.omits(source, &span, "false"));
}

#[test]
fn scalar_blocks_reject_each_unsupported_binding_shape_before_evaluating_later_statements() {
    for source in ["{let ref local=true;}", "{let local @ _=true;}"] {
        assert_eq!(machine().block(&parse_str(source).unwrap()), None, "{source}");
    }
    for source in [
        "3.25",
        "3.25_unknown",
        "256",
        "-(-170141183460469231731687303715884105728_i128)",
        "Default::default()",
    ] {
        assert_eq!(machine().eval(&parse_str(source).unwrap(), None), None, "{source}");
    }
}

#[test]
fn local_root_shadowing_cannot_reuse_an_outer_projection_as_its_own_field() {
    let mut evaluator = machine();
    evaluator
        .values
        .insert("tree.node.value".into(), Value::Int(7, Primitive::Unsigned(8)));
    evaluator
        .values
        .insert("treeish.node.value".into(), Value::Int(9, Primitive::Unsigned(8)));
    assert_eq!(
        evaluator.block(&parse_quote!({
            let tree = true;
            tree.node.value = 7;
        })),
        None
    );
}

#[test]
fn focused_match_shape_checks_attach_metadata_to_the_exact_match_and_arm_block_nodes() {
    let original: Block = parse_quote!({
        match ch {
            'a' => {
                ready = false;
            }
            _ => {}
        }
    });
    for fault in 0..5 {
        let mut mutated: Block = parse_quote!({
            match ch {
                'a' => {}
                _ => {}
            }
        });
        let Stmt::Expr(Expr::Match(node), _) = &mut mutated.stmts[0] else {
            unreachable!()
        };
        match fault {
            0 => node.attrs.push(parse_quote!(#[custom])),
            1 => *node.expr = parse_quote!(other),
            2 => {
                node.attrs.push(parse_quote!(#[custom]));
                *node.expr = parse_quote!(other);
            }
            3 => {
                *node.arms[1].body = parse_quote!({
                    ready = false;
                });
            }
            4 => {
                let Expr::Block(body) = &mut *node.arms[0].body else {
                    unreachable!()
                };
                body.attrs.push(parse_quote!(#[custom]));
            }
            _ => unreachable!(),
        }
        assert!(changed_arm(&original, &mutated).is_none(), "unsupported focused shape {fault}");
    }
}

#[test]
fn replay_restores_character_facts_but_clears_path_facts_before_each_outer_guard_recheck() {
    let bindings = HashMap::from_iter([
        ("ch".into(), Binding::Char),
        ("ready".into(), Binding::Bool),
        ("selected".into(), Binding::Bool),
    ]);
    for (before, after) in [
        (
            parse_quote!(while selected {
                if selected {
                    ready = false;
                }
            }),
            parse_quote!(while selected {
                if selected {}
            }),
        ),
        (
            parse_quote!(while ready {
                match ch {
                    'a' => {
                        if ch == 'a' {
                            ready = false;
                        };
                    }
                    _ => {}
                }
            }),
            parse_quote!(while ready {
                match ch {
                    'a' => {
                        if ch == 'a' {};
                    }
                    _ => {}
                }
            }),
        ),
    ] {
        assert!(
            proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true),
            "guard replay for {before:?}"
        );
    }
}

#[test]
fn scalar_effect_and_expression_grammar_withholds_attributed_or_labeled_near_matches() {
    for expression in [
        "#[custom] count = 0",
        "#[custom] count += 0",
        "#[custom] { count = 0; }",
        "'outer: { count = 0; }",
        "#[custom] if ready {}",
        "#[custom] while false {}",
        "'outer: while false {}",
        "#[custom] rows.push(0)",
        "rows.push::<u8>(0)",
        "rows.push()",
        "rows.push(0, 1)",
        "rows.other('x')",
        "#[custom] text.len()",
        "text.len::<u8>()",
        "text.len(0)",
        "text.capacity()",
        "#[custom] text.chars().next().expect(\"nonempty\")",
        "text.chars().next().expect::<char>(\"nonempty\")",
        "text.chars().next().other(\"nonempty\")",
        "#[custom] chars.get(0)",
        "chars.get::<u8>(0)",
        "#[custom] count.wrapping_add(0)",
        "count.wrapping_add::<u8>(0)",
        "#[custom] chars[0]",
        "#[custom] &text",
        "&mut text",
        "#[custom] !ready",
        "#[custom] (count + 0)",
        "#[custom] (0_u8 as u16)",
        "0_u8 as module::u8",
        "#[custom] Default::default()",
        "{ continue 'outer; }",
    ] {
        let mut evaluator = machine();
        evaluator.values.extend([
            ("count".into(), Value::Int(7, Primitive::Unsigned(8))),
            ("ready".into(), Value::Bool(true)),
            ("rows".into(), Value::Symbol("rows".into(), Binding::Vec)),
            ("text".into(), Value::Symbol("text".into(), Binding::StrReference)),
            ("chars".into(), Value::Symbol("chars".into(), Binding::VecChar)),
        ]);
        assert_eq!(evaluator.effect(&parse_str(expression).unwrap()), None, "{expression}");
    }
}

#[test]
fn scalar_evaluation_work_budget_fails_closed_at_the_exact_required_visit() {
    for remaining in 0..=4 {
        let mut evaluator = Machine {
            budget: remaining,
            ..machine()
        };
        assert_eq!(
            evaluator.eval(&parse_quote!(true), None),
            (remaining >= 1).then_some(Value::Bool(true))
        );
        let mut evaluator = Machine {
            budget: remaining,
            ..machine()
        };
        assert_eq!(
            evaluator.block(&parse_quote!({
                true;
            })),
            (remaining >= 3).then_some(())
        );
    }
}

#[test]
fn constant_grammar_requires_every_array_tuple_and_cache_constructor_operand() {
    for (expression, accepted) in [
        ("[1, unknown]", false),
        ("[unknown, 1]", false),
        ("(1, unknown)", false),
        ("(unknown, 1)", false),
        ("std::collections::HashMap::from([])", true),
        ("std::collections::HashMap::from()", false),
        ("std::collections::HashMap::from([], [])", false),
        ("std::collections::HashMap::from(unknown)", false),
        ("custom([])", false),
    ] {
        assert_eq!(constant_syntax(&parse_str(expression).unwrap()), accepted, "{expression}");
    }
}

#[test]
fn initializer_and_helper_windows_preserve_the_last_allowed_declaration() {
    for (preceding, accepted) in [(62, true), (63, true), (64, false)] {
        let file = parse_file(&format!("fn run() {{ {} while true {{}} }}", "let value=7;".repeat(preceding))).unwrap();
        let Item::Fn(function) = &file.items[0] else { unreachable!() };
        let target = function.block.stmts.last().unwrap().span().byte_range();
        assert_eq!(
            initializers(&file, &target),
            if accepted {
                HashMap::from_iter([("value".into(), parse_quote!(7))])
            } else {
                HashMap::default()
            },
            "{preceding} preceding declarations"
        );
    }
    for (length, accepted) in [(1023, true), (1024, true), (1025, false)] {
        let literal = format!("\"{}\"", "x".repeat(length - 2));
        let file = parse_file(&format!("fn run() {{ let value={literal}; while true {{}} }}")).unwrap();
        let Item::Fn(function) = &file.items[0] else { unreachable!() };
        let target = function.block.stmts[1].span().byte_range();
        assert_eq!(
            initializers(&file, &target),
            if accepted {
                HashMap::from_iter([("value".into(), parse_str(&literal).unwrap())])
            } else {
                HashMap::default()
            },
            "{length}-byte initializer"
        );
    }
    let original = parse_file("fn helper() -> bool { true } fn run() { while true {} }").unwrap();
    let Item::Fn(function) = &original.items[1] else { unreachable!() };
    let target = function.block.stmts[0].span().byte_range();
    for (count, accepted) in [(63, true), (64, true), (65, false)] {
        let mut file = original.clone();
        file.items.resize(count, original.items[0].clone());
        let found = helpers(&file, &target);
        assert_eq!(found.contains_key("helper"), accepted, "{count} helper items");
        assert_eq!(found.contains_key("run"), accepted, "{count} helper items");
    }
}

#[test]
fn primitive_scope_shadow_names_include_bool_default_and_nested_declarations() {
    for prefix in [
        "struct bool;",
        "struct Default;",
        "struct u8;",
        "enum u8 {}",
        "mod container { type u8 = custom; }",
    ] {
        let source = format!("{prefix} fn run() {{ while true {{}} }}");
        let file = parse_file(&source).unwrap();
        let start = source.find("while").unwrap();
        let expected = prefix.starts_with("mod container");
        assert_eq!(numeric_names(&file, &(start..start + 13)), expected, "{prefix}");
    }
}

#[test]
fn initializer_scope_limits_count_only_containing_blocks_and_supported_item_windows() {
    let original = parse_file("fn run() { let value=7; while true {} } fn unrelated() {}").unwrap();
    let Item::Fn(run) = &original.items[0] else { unreachable!() };
    let target = run.block.stmts[1].span().byte_range();
    for (count, accepted) in [(255, true), (256, true), (257, false)] {
        let mut file = original.clone();
        file.items.resize(count, original.items[1].clone());
        assert_eq!(initializers(&file, &target).contains_key("value"), accepted, "{count} root items");
    }
    for (depth, accepted) in [(62, true), (63, true), (64, false)] {
        let mut file = original.clone();
        let Item::Fn(run) = &mut file.items[0] else { unreachable!() };
        for _ in 0..depth {
            let inner = (*run.block).clone();
            run.block.stmts = vec![Stmt::Expr(
                Expr::Block(ExprBlock {
                    attrs: Vec::new(),
                    label: None,
                    block: inner,
                }),
                None,
            )];
        }
        assert_eq!(
            initializers(&file, &target).contains_key("value"),
            accepted,
            "{depth} enclosing blocks"
        );
    }
    let mut file = original.clone();
    file.items.splice(0..0, core::iter::repeat_n(original.items[1].clone(), 64));
    assert_eq!(
        initializers(&file, &target),
        HashMap::from_iter([("value".into(), parse_quote!(7))])
    );
    for (count, accepted) in [(255, true), (256, true), (257, false)] {
        let mut file = parse_file("mod child { fn run() { let value=7; while true {} } fn unrelated() {} }").unwrap();
        let Item::Mod(module) = &mut file.items[0] else { unreachable!() };
        let items = &mut module.content.as_mut().unwrap().1;
        let Item::Fn(run) = &items[0] else { unreachable!() };
        let target = run.block.stmts[1].span().byte_range();
        items.resize(count, items[1].clone());
        assert_eq!(initializers(&file, &target).contains_key("value"), accepted, "{count} module items");
    }
}

#[test]
fn primitive_scope_search_distinguishes_256_complete_item_and_expression_visits() {
    let source = "mod child { struct Ordinary; fn run() { while true {} } }";
    let file = parse_file(source).unwrap();
    let start = source.find("while").unwrap();
    assert!(numeric_names(&file, &(start..start + 13)));
    let original = parse_file("fn unrelated() {} fn run() { while true {} }").unwrap();
    let Item::Fn(run) = &original.items[1] else { unreachable!() };
    let target = run.block.stmts[0].span().byte_range();
    for (count, accepted) in [(253, true), (254, true), (255, false), (256, false)] {
        let mut file = original.clone();
        file.items = core::iter::repeat_n(original.items[0].clone(), count - 1)
            .chain([original.items[1].clone()])
            .collect();
        assert_eq!(numeric_names(&file, &target), accepted, "{count} items with target last");
    }
}

#[test]
fn helper_search_limits_count_pattern_expression_and_declaration_bytes() {
    for declaration in ["struct External;", "const EXTERNAL: bool = true;"] {
        let file = parse_file(&format!("{declaration} fn helper()->bool{{true}} fn run(){{while true{{}}}}")).unwrap();
        let Item::Fn(run) = &file.items[2] else { unreachable!() };
        assert!(helpers(&file, &run.block.stmts[0].span().byte_range()).is_empty(), "{declaration}");
    }
    for (declarations, condition, accepted) in [(126, "true", true), (127, "true", true), (127, "(true)", false)] {
        let file = parse_file(&format!(
            "fn helper()->bool{{true}} fn run(){{ {} while {condition} {{}} }}",
            "let value=0;".repeat(declarations)
        ))
        .unwrap();
        let Item::Fn(run) = &file.items[1] else { unreachable!() };
        let target = run.block.stmts.last().unwrap().span().byte_range();
        assert_eq!(
            helpers(&file, &target).contains_key("helper"),
            accepted,
            "{declarations} declarations, {condition}"
        );
    }
    for (length, accepted) in [(1023, true), (1024, true), (1025, false)] {
        let base = "fn helper() -> bool { true }";
        let declaration = format!("fn helper() -> bool {{ {}true }}", " ".repeat(length - base.len()));
        assert_eq!(declaration.len(), length);
        let file = parse_file(&format!("{declaration} fn run() {{ while true {{}} }}")).unwrap();
        let Item::Fn(run) = &file.items[1] else { unreachable!() };
        let target = run.block.stmts[0].span().byte_range();
        assert_eq!(
            helpers(&file, &target).contains_key("helper"),
            accepted,
            "{length}-byte helper declaration"
        );
    }
    let file = parse_file("fn helper()->bool{true} fn run(){ let outer @ Some(helper)=input; while true{} }").unwrap();
    let Item::Fn(run) = &file.items[1] else { unreachable!() };
    assert!(!helpers(&file, &run.block.stmts[1].span().byte_range()).contains_key("helper"));
}

#[test]
fn symbolic_string_chains_reject_metadata_on_every_intermediate_call() {
    for source in [
        "text.chars().next().expect(\"nonempty\")",
        "text.chars().next::<char>().expect(\"nonempty\")",
        "text.chars::<char>().next().expect(\"nonempty\")",
        "(#[custom] text.chars()).next().expect(\"nonempty\")",
        "unknown.chars().next().expect(\"nonempty\")",
    ] {
        let mut evaluator = machine();
        evaluator
            .values
            .insert("text".into(), Value::Symbol("input".into(), Binding::StrReference));
        let expression = parse_str(source).unwrap();
        assert_eq!(
            evaluator.eval(&expression, None).is_some(),
            source == "text.chars().next().expect(\"nonempty\")",
            "{source}"
        );
    }
    for layer in ["next", "chars"] {
        let mut expression: Expr = parse_quote!(text.chars().next().expect("nonempty"));
        let Expr::MethodCall(expect) = &mut expression else {
            unreachable!()
        };
        let Expr::MethodCall(next) = &mut *expect.receiver else {
            unreachable!()
        };
        if layer == "next" {
            next.attrs.push(parse_quote!(#[custom]));
        } else {
            let Expr::MethodCall(chars) = &mut *next.receiver else {
                unreachable!()
            };
            chars.attrs.push(parse_quote!(#[custom]));
        }
        let mut evaluator = machine();
        evaluator
            .values
            .insert("text".into(), Value::Symbol("input".into(), Binding::StrReference));
        assert_eq!(evaluator.eval(&expression, None), None, "{layer} attributes");
    }
}

#[test]
fn bounded_symbol_identities_preserve_the_last_representable_byte() {
    for (length, accepted) in [(8191, true), (8192, true), (8193, false)] {
        let input = "x".repeat(length);
        assert_eq!(
            identity(format_args!("{input}")),
            accepted.then_some(input),
            "{length} identity bytes"
        );
    }
    let mut evaluator = machine();
    evaluator
        .values
        .insert("oversized".into(), Value::Symbol("x".repeat(8192), Binding::Vec));
    assert_eq!(evaluator.eval(&parse_quote!(oversized.len()), None), None);
    assert_eq!(evaluator.eval(&parse_quote!(oversized), None), None);
    let string_literal = LitStr::new(&"x".repeat(8192), Span::call_site());
    assert_eq!(evaluator.eval(&parse_quote!(#string_literal), None), None);
    assert_eq!(
        binary(&parse_quote!(&&), Value::Symbol("x".repeat(8192), Binding::Bool), Value::Bool(true)),
        None
    );
    assert_eq!(
        binary(
            &parse_quote!(==),
            Value::Symbol("x".repeat(8192), Binding::Integer),
            Value::IntegerZero
        ),
        None
    );
}

#[test]
fn large_but_bounded_intervals_do_not_need_symbol_fact_lookup() {
    let mut evaluator = machine();
    evaluator.values.insert(
        "n".into(),
        Value::Symbol("x".repeat(8100), Binding::Primitive(Primitive::Unsigned(8))),
    );
    let Some(Value::UnsignedRange(low, high, ty, _)) = evaluator.eval(&parse_quote!(n.wrapping_add(0)), None) else {
        panic!("the bounded interval result does not require a second symbolic fact identity");
    };
    assert_eq!((low, high, ty), (0, 255, Primitive::Unsigned(8)));
}

#[test]
fn derived_string_vector_and_call_identities_withhold_oversized_results() {
    let stable_calls = HashMap::from_iter([
        (
            super::super::CHAR_WIDTH.into(),
            super::super::StableCall {
                parameters: vec![],
                output: Binding::Integer,
                char_option: false,
            },
        ),
        (
            "helper".into(),
            super::super::StableCall {
                parameters: vec![Binding::Bool],
                output: Binding::Bool,
                char_option: false,
            },
        ),
    ]);
    for (source, kind, length) in [
        ("value.chars().next().expect(\"nonempty\")", Binding::StrReference, 8165),
        ("value.width().unwrap_or(0)", Binding::Char, 8170),
        ("value.get(0)", Binding::VecChar, 8170),
        ("value.get(0)", Binding::VecChar, 8140),
        ("value[0]", Binding::VecChar, 8170),
        ("value[0]", Binding::VecChar, 8140),
        ("value[0]", Binding::VecString, 8170),
        ("value[0..]", Binding::StrReference, 8165),
        ("&value", Binding::StrReference, 8168),
        ("helper(value)", Binding::Bool, 8170),
        ("value.push('x')", Binding::Vec, 8175),
    ] {
        let mut evaluator = Machine {
            stable_calls: Some(&stable_calls),
            ..machine()
        };
        evaluator.values.insert("value".into(), Value::Symbol("x".repeat(length), kind));
        assert_eq!(evaluator.effect(&parse_str(source).unwrap()), None, "{source}, {kind:?}, {length}");
    }
}

#[test]
fn signed_negation_accepts_the_positive_maximum_but_rejects_negated_minimum() {
    for (source, expected) in [
        ("-(-127_i8)", Some(Value::Int(127, Primitive::Signed(8)))),
        ("-(-128_i8)", None),
        ("-(-32767_i16)", Some(Value::Int(32767, Primitive::Signed(16)))),
        ("-(-32768_i16)", None),
    ] {
        assert_eq!(machine().eval(&parse_str(source).unwrap(), None), expected, "{source}");
    }
}

#[test]
fn recurrence_requires_an_unchanged_unattributed_unlabeled_boolean_guard() {
    let original: Expr = parse_quote!(while true {
        ready = false;
    });
    for (mutant, accepted) in [
        ("while true {}", true),
        ("#[custom] while true {}", false),
        ("'outer: while true {}", false),
        ("while false {}", false),
    ] {
        assert_eq!(
            proves(
                &original,
                &parse_str(mutant).unwrap(),
                &HashMap::default(),
                &HashMap::default(),
                &HashMap::default(),
                true
            ),
            accepted,
            "{mutant}"
        );
    }
    let bindings = HashMap::from_iter([("count".into(), Binding::Integer)]);
    let loop_: Expr = parse_quote!(while count {});
    assert!(!proves(&loop_, &loop_, &bindings, &HashMap::default(), &HashMap::default(), true));
    let loop_: Expr = parse_quote!(while false {});
    assert!(!proves(
        &loop_,
        &loop_,
        &HashMap::default(),
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
}

#[test]
fn recurrence_work_allowance_distinguishes_the_last_complete_recheck() {
    // Each literal statement costs statement + effect + eval; each tail parenthesis adds an eval.
    // The initial and final guards each cost one, leaving room for two wrappers, not three.
    for (wrappers, accepted) in [(1, true), (2, true), (3, false)] {
        let tail = format!("{}true{}", "(".repeat(wrappers), ")".repeat(wrappers));
        let loop_ = parse_str(&format!("while true{{ {} {tail}; }}", "true;".repeat(83))).unwrap();
        assert_eq!(
            proves(&loop_, &loop_, &HashMap::default(), &HashMap::default(), &HashMap::default(), true),
            accepted,
            "{wrappers} wrappers"
        );
    }
}

#[test]
fn primitive_scope_visits_macro_arguments_nested_in_type_parameter_bounds() {
    let source = "fn run<T: Trait<{ opaque!() }>>() { while true {} }";
    let file = parse_file(source).unwrap();
    let start = source.find("while").unwrap();
    assert!(!numeric_names(&file, &(start..start + 13)));
}

#[test]
fn focused_wildcard_arms_preserve_scalar_replay_but_not_owned_selector_changes() {
    let bindings = HashMap::from_iter([
        ("ch".into(), Binding::Char),
        ("ready".into(), Binding::Bool),
        ("selected".into(), Binding::Bool),
        ("count".into(), Binding::Primitive(Primitive::Unsigned(8))),
        ("chars".into(), Binding::VecChar),
    ]);
    assert!(proves(
        &parse_quote!(while true {
            match ch {
                _ => {
                    ready = false;
                }
            }
        }),
        &parse_quote!(while true {
            match ch {
                _ => {}
            }
        }),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true,
    ));
    assert!(!proves(
        &parse_quote!(while true {
            match chars[0] {
                _ => {
                    ready = false;
                }
            }
        }),
        &parse_quote!(while true {
            match chars[0] {
                _ => {
                    chars.push('x');
                }
            }
        }),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true,
    ));
    assert!(proves(
        &parse_quote!(while true {
            if selected {
                count += 1;
            }
        }),
        &parse_quote!(while true {
            if selected {
                count = count;
            }
        }),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true,
    ));
}

#[test]
fn replay_requires_both_a_complete_guard_recheck_and_fixed_scalar_writes() {
    let bindings = HashMap::from_iter([
        ("selected".into(), Binding::Bool),
        ("ready".into(), Binding::Bool),
        ("chars".into(), Binding::VecChar),
        ("count".into(), Binding::Integer),
    ]);
    // Each pass charges three visits per literal statement and three for the empty if
    // (statement + effect + guard). Two passes plus the three outer guard evaluations leave
    // one spare visit; a tail parenthesis costs one in each pass and prevents the final recheck.
    for (tail, accepted) in [("true", true), ("(true)", false)] {
        let suffix = format!("{} {tail};", "true;".repeat(40));
        let original = parse_str(&format!("while true{{if selected{{ready=false;}} {suffix}}}")).unwrap();
        let mutant = parse_str(&format!("while true{{if selected{{}} {suffix}}}")).unwrap();
        assert_eq!(
            proves(&original, &mutant, &bindings, &HashMap::default(), &HashMap::default(), true),
            accepted,
            "{tail}"
        );
    }
    assert!(!proves(
        &parse_quote!(while true {
            if selected {
                count = chars.len();
                chars.push('x');
                ready = false;
            }
        }),
        &parse_quote!(while true {
            if selected {
                count = chars.len();
                chars.push('x');
            }
        }),
        &bindings,
        &HashMap::from_iter([("count".into(), parse_quote!(chars.len()))]),
        &HashMap::default(),
        true,
    ));
}

#[test]
fn focused_pattern_binding_restoration_preserves_an_outer_guard_of_the_same_name() {
    let bindings = HashMap::from_iter([
        ("ch".into(), Binding::Char),
        ("other".into(), Binding::Char),
        ("ready".into(), Binding::Bool),
    ]);
    assert!(proves(
        &parse_quote!(while other == 'x' {
            match ch {
                other => {
                    ready = false;
                }
            }
        }),
        &parse_quote!(while other == 'x' {
            match ch {
                other => {}
            }
        }),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true,
    ));
}

#[test]
fn warmup_recurrence_allows_restored_temporary_writes_but_not_advancing_outer_state() {
    let bindings = HashMap::from_iter([
        ("ready".into(), Binding::Bool),
        ("count".into(), Binding::Primitive(Primitive::Unsigned(8))),
        ("other".into(), Binding::Primitive(Primitive::Unsigned(8))),
    ]);
    let seeds = HashMap::from_iter([("count".into(), parse_quote!(0)), ("other".into(), parse_quote!(0))]);
    let original = parse_quote!(while ready {
        count += 1;
    });
    for (mutated, accepted) in [
        ("while ready { count = 1; let mut temporary: u8 = 0; temporary += 1; }", true),
        ("while ready { count = 1; other += 1; }", false),
        ("while ready { count = 1; other += 1; continue; }", false),
    ] {
        assert_eq!(
            proves(
                &original,
                &parse_str(mutated).unwrap(),
                &bindings,
                &seeds,
                &HashMap::default(),
                true
            ),
            accepted,
            "{mutated}"
        );
    }
}

#[test]
fn helper_evaluation_propagates_work_spending_back_to_the_calling_machine() {
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
        budget: 2,
        ..machine()
    };
    assert_eq!(evaluator.eval(&parse_quote!(helper()), None), Some(Value::Bool(true)));
    assert_eq!(evaluator.eval(&parse_quote!(true), None), None);
}

#[test]
fn symbolic_boolean_predicates_remain_typed_repeatable_and_operand_sensitive() {
    let left = Value::Symbol("left".into(), Binding::Bool);
    let right = Value::Symbol("right".into(), Binding::Bool);
    let first = binary(&parse_quote!(&&), left.clone(), Value::Bool(true)).unwrap();
    assert_eq!(first.kind(), Binding::Bool);
    assert_eq!(binary(&parse_quote!(&&), left.clone(), Value::Bool(true)), Some(first.clone()));
    assert_ne!(binary(&parse_quote!(&&), right.clone(), Value::Bool(true)), Some(first));
    let first = binary(&parse_quote!(||), left, Value::Bool(false)).unwrap();
    assert_eq!(first.kind(), Binding::Bool);
    assert_ne!(binary(&parse_quote!(||), right, Value::Bool(false)), Some(first));
}

#[test]
fn unsupported_operands_return_no_evidence_without_panicking_or_inventing_effects() {
    for source in [
        "module::unknown",
        "factory().field",
        "array[0] = 0",
        "array[0] += 1",
        "unknown += 1",
        "text = true",
        "if ready {} else { unknown(); }",
        "rows.push(unknown())",
        "count.push('x')",
        "state.rows.push('x')",
        "unknown.push('x')",
        "unknown.get(0)",
        "chars.get(unknown)",
        "unknown[0]",
        "strings[unknown]",
        "chars[unknown]",
        "text[unknown..]",
        "340282366920938463463374607431768211456_u128",
    ] {
        let mut evaluator = machine();
        evaluator.values.extend([
            ("count".into(), Value::Int(7, Primitive::Unsigned(8))),
            ("ready".into(), Value::Symbol("ready".into(), Binding::Bool)),
            ("rows".into(), Value::Symbol("rows".into(), Binding::Vec)),
            ("text".into(), Value::Symbol("text".into(), Binding::StrReference)),
            ("chars".into(), Value::Symbol("chars".into(), Binding::VecChar)),
            ("strings".into(), Value::Symbol("strings".into(), Binding::VecString)),
        ]);
        assert_eq!(evaluator.effect(&parse_str(source).unwrap()), None, "{source}");
    }
    assert_eq!(
        machine().eval(&parse_quote!(1), Some(Binding::Primitive(Primitive::Float(32)))),
        None
    );
    let mut evaluator = machine();
    assert_eq!(
        evaluator.block(&parse_quote!({
            continue;
            unknown();
        })),
        Some(())
    );
    assert!(evaluator.restarts);
    let mut evaluator = machine();
    evaluator.values.extend([
        ("count".into(), Value::Int(7, Primitive::Unsigned(8))),
        ("ready".into(), Value::Symbol("ready".into(), Binding::Bool)),
    ]);
    assert_eq!(
        evaluator.block(&parse_quote!({
            if ready {
                count = 7;
            }
        })),
        Some(())
    );
    assert_eq!(evaluator.values["count"], Value::Int(7, Primitive::Unsigned(8)));
    assert_eq!(evaluator.writes, HashSet::from_iter(["count".into()]));
}

#[test]
fn branch_assumptions_require_boolean_values_and_replay_consistency() {
    let mut evaluator = machine();
    evaluator.values.insert("flag".into(), Value::Symbol("flag".into(), Binding::Bool));
    evaluator.paths.insert("flag".into(), true);
    assert_eq!(evaluator.guard(&parse_quote!(flag)), Some(Value::Bool(true)));
    assert_eq!(evaluator.assumptions.len(), 1);
    assert_eq!(evaluator.eval(&parse_quote!(flag), None), Some(Value::Bool(true)));
    evaluator.values.insert("flag".into(), Value::Bool(false));
    assert!(evaluator.guard(&parse_quote!(flag)).is_none());
    evaluator.values.insert("flag".into(), Value::Int(1, Primitive::Unsigned(8)));
    assert!(evaluator.guard(&parse_quote!(flag)).is_none());
}

#[test]
fn stable_calls_check_arity_parameter_kinds_and_character_options() {
    assert_eq!(machine().eval(&parse_quote!('x'.width().unwrap_or(0)), None), None);
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
    assert_eq!(evaluator.eval(&parse_quote!(helper(&'x')), None), Some(Value::MaybeChar('x')));
    assert!(evaluator.eval(&parse_quote!(helper()), None).is_none());
    assert!(evaluator.eval(&parse_quote!(helper('x')), None).is_none());
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
    assert!(evaluator.eval(&parse_quote!(helper(true)), None).is_none());
    assert!(evaluator.eval(&parse_quote!('x'.width().unwrap_or(0)), None).is_some());
    for expression in [
        "true.width().unwrap_or(0)",
        "'x'.width().unwrap_or(1)",
        "'x'.other().unwrap_or(0)",
        "true.unwrap_or(0)",
        "'x'.width().other(0)",
        "'x'.width().unwrap_or()",
        "'x'.width().unwrap_or(0, 1)",
    ] {
        assert!(evaluator.eval(&parse_str(expression).unwrap(), None).is_none(), "{expression}");
    }
}

#[test]
fn numeric_stable_calls_preserve_argument_context_arity_and_declared_types() {
    let calls = HashMap::from_iter([(
        "helper".into(),
        super::super::StableCall {
            parameters: vec![Binding::Primitive(Primitive::Unsigned(8))],
            output: Binding::Bool,
            char_option: false,
        },
    )]);
    for (source, accepted) in [
        ("helper(7)", true),
        ("helper()", false),
        ("helper(7, 7)", false),
        ("helper(unknown)", false),
        ("helper(true)", false),
        ("helper(7_u16)", false),
    ] {
        let mut evaluator = Machine {
            stable_calls: Some(&calls),
            ..machine()
        };
        assert_eq!(
            evaluator.eval(&parse_str(source).unwrap(), None).map(|value| value.kind()),
            accepted.then_some(Binding::Bool),
            "{source}"
        );
    }
}

#[test]
fn helper_scalar_execution_requires_exact_signature_arguments_and_tail_evidence() {
    for (declaration, argument, expected) in [
        ("fn helper(value:u8)->u8 {value}", "7", Some(Value::Int(7, Primitive::Unsigned(8)))),
        ("fn helper(value:u8)->u8 {value}", "unknown", None),
        ("fn helper(value:u8)->u8 {value}", "true", None),
        ("fn helper(value:u8)->u8 {value}", "", None),
        ("fn helper(value @ _:u8)->u8 {value}", "7", None),
        ("fn helper(value:u8)->Unknown {value}", "7", None),
        ("fn helper(value:u8)->u8 {unknown}", "7", None),
        ("fn helper(value:u8)->u8 {let prior=7;value}", "7", None),
    ] {
        let helpers = HashMap::from_iter([("helper".into(), parse_str(declaration).unwrap())]);
        let mut evaluator = Machine {
            helpers: Some(&helpers),
            ..machine()
        };
        assert_eq!(
            evaluator.eval(&parse_str(&format!("helper({argument})")).unwrap(), None),
            expected,
            "{declaration}, {argument}"
        );
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
        numeric_names: false,
        ..machine()
    };
    assert_eq!(evaluator.eval(&parse_quote!(helper()), None), None);
}

#[test]
fn directly_attributed_binary_cast_and_range_nodes_are_not_silently_accepted() {
    parse_str::<Expr>("\"value\"[0..=]").unwrap_err();
    for source in ["7_u8 + 0_u8", "7_u8 as u16", "\"value\"[0..]"] {
        let mut expression: Expr = parse_str(source).unwrap();
        match &mut expression {
            Expr::Binary(node) => node.attrs.push(parse_quote!(#[custom])),
            Expr::Cast(node) => node.attrs.push(parse_quote!(#[custom])),
            Expr::Index(node) => {
                let Expr::Range(range) = &mut *node.index else { unreachable!() };
                range.attrs.push(parse_quote!(#[custom]));
            }
            _ => unreachable!(),
        }
        assert_eq!(machine().eval(&expression, None), None, "{source}");
    }
}

#[test]
fn syntactically_valid_extreme_float_exponents_remain_values_not_parse_failures() {
    for (source, expected) in [
        (
            "1.0e9999999999999999999999999999999999999999_f32",
            float(f64::INFINITY, Primitive::Float(32)),
        ),
        (
            "1.0e9999999999999999999999999999999999999999_f64",
            float(f64::INFINITY, Primitive::Float(64)),
        ),
        (
            "1.0e-9999999999999999999999999999999999999999_f32",
            float(0.0, Primitive::Float(32)),
        ),
        (
            "1.0e-9999999999999999999999999999999999999999_f64",
            float(0.0, Primitive::Float(64)),
        ),
    ] {
        assert_eq!(machine().eval(&parse_str(source).unwrap(), None), expected, "{source}");
    }
}

#[test]
fn symbolic_operations_require_the_receiver_and_primitive_kinds_they_model() {
    let mut evaluator = machine();
    evaluator
        .values
        .insert("text".into(), Value::Symbol("input".into(), Binding::String));
    evaluator.values.insert(
        "count".into(),
        Value::Symbol("count".into(), Binding::Primitive(Primitive::Signed(8))),
    );
    for source in [
        "text.chars().next().expect(\"nonempty\")",
        "7_u8.wrapping_add(1_u16)",
        "count.wrapping_add(1_i8)",
        "count.wrapping_add(0_u16)",
        "1.5_f64 + 0_u8",
        "1_u8 as module::u16",
    ] {
        assert_eq!(evaluator.eval(&parse_str(source).unwrap(), None), None, "{source}");
    }
}

#[test]
fn focused_match_proofs_do_not_ignore_a_second_change_or_different_selector() {
    let bindings = HashMap::from_iter([("ch".into(), Binding::Char), ("ready".into(), Binding::Bool)]);
    let original: Expr = parse_quote!(while ready {
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
                &parse_str(mutated).unwrap(),
                &bindings,
                &HashMap::default(),
                &HashMap::default(),
                true
            ),
            accepted,
            "{mutated}"
        );
    }
    let before: Expr = parse_quote!(while ready {
        match ch {
            other => {
                ready = false;
            }
        }
    });
    let after: Expr = parse_quote!(while ready {
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
        &parse_quote!(true),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true
    ));
}

#[test]
fn focused_branch_replay_establishes_only_stable_path_assumptions() {
    let bindings = HashMap::from_iter([("ready".into(), Binding::Bool), ("selected".into(), Binding::Bool)]);
    let original = parse_quote!(while ready {
        if selected {
            ready = false;
        }
    });
    let mutant = parse_quote!(while ready {
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
    let mutant = parse_quote!(while ready {
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
        &parse_quote!(while unknown {}),
        &parse_quote!(while unknown {}),
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
    let projection: Expr = parse_quote!(state.count);
    let tuple = Expr::Tuple(ExprTuple {
        attrs: Vec::new(),
        paren_token: Paren::default(),
        elems: core::iter::repeat_n(projection, 65).collect(),
    });
    assert_eq!(projection_sites(&tuple).len(), 64);
    let mut file = parse_file("fn helper() {} fn f() { while true {} }").unwrap();
    let target = match &file.items[1] {
        Item::Fn(function) => function.block.stmts[0].span().byte_range(),
        _ => unreachable!(),
    };
    let Item::Fn(function) = &mut file.items[1] else { unreachable!() };
    function
        .block
        .stmts
        .splice(0..0, core::iter::repeat_n(parse_quote!(let x = 1;), 257));
    assert!(helpers(&file, &target).is_empty());
    assert!(!numeric_names(&file, &target));
    let mut oversized = parse_file("fn f() { while true {} }").unwrap();
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
        assert_eq!(evaluator.block(&parse_str(block).unwrap()).is_some(), accepted, "{block}");
    }
    for (source, expected) in [
        ("1.0_f64 + 2.0_f64", float(3.0, Primitive::Float(64))),
        ("1.0_f64 < 2.0_f64", Some(Value::Bool(true))),
        ("b'x'", None),
    ] {
        assert_eq!(machine().eval(&parse_str(source).unwrap(), None), expected, "{source}");
    }
    for operator in [parse_quote!(&&), parse_quote!(||)] {
        let and = matches!(operator, BinOp::And(_));
        assert_eq!(binary(&operator, Value::Bool(true), Value::Bool(false)), Some(Value::Bool(!and)));
    }
    let unknown = Value::Symbol("n".into(), Binding::Integer);
    assert_eq!(binary(&parse_quote!(*), unknown, Value::IntegerZero), Some(Value::IntegerZero));
    assert_eq!(float(1.0, Primitive::Unsigned(8)), None);
    let mut evaluator = machine();
    let group = Expr::Group(ExprGroup {
        attrs: Vec::new(),
        group_token: Group::default(),
        expr: Box::new(parse_quote!(true)),
    });
    assert_eq!(evaluator.eval(&group, None), Some(Value::Bool(true)));
}

#[test]
fn invalid_character_facts_and_changed_match_boundaries_cannot_prove_recurrence() {
    let mut bindings = HashMap::from_iter([("ready".into(), Binding::Bool), ("ch".into(), Binding::Bool)]);
    let before = parse_quote!(while ready {
        match ch {
            'a' => {
                ready = false;
            }
            _ => {}
        }
    });
    let after = parse_quote!(while ready {
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
                match &parse_str::<Expr>(source).unwrap() {
                    Expr::While(node) => &node.body,
                    _ => unreachable!(),
                },
            )
            .is_none(),
            "{source}"
        );
    }
    for body in [
        "{ #[custom] match ch { 'a' => {}, _ => {} } }",
        "{ match ch { 'a' => {} } }",
        "{ match ch { 'a' => {}, _ => {}, other => {} } }",
    ] {
        let Expr::While(before) = &before else { unreachable!() };
        assert!(changed_arm(&before.body, &parse_str(body).unwrap()).is_none(), "{body}");
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
    let before = parse_quote!(while ready {
        if selected {
            tokens.push('a');
            ready = false;
        }
    });
    let after = parse_quote!(while ready {
        if selected {
            tokens.push('a');
        }
    });
    assert!(proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    let before = parse_quote!(while ready {
        match ch {
            _ => {
                count += 1;
            }
        }
    });
    let after = parse_quote!(while ready {
        match ch {
            _ => {
                count = 0;
            }
        }
    });
    assert!(!proves(&before, &after, &bindings, &HashMap::default(), &HashMap::default(), true));
    let before = parse_quote!({
        match ch {
            ref character => {
                count += 1;
            }
        }
    });
    let after = parse_quote!({
        match ch {
            ref character => {}
        }
    });
    assert!(changed_arm(&before, &after).is_none());
}

#[test]
fn numeric_names_are_unknown_beyond_module_and_item_search_budgets() {
    let mut file = parse_file("mod child { fn f() { while true {} } }").unwrap();
    let Item::Mod(module) = &mut file.items[0] else { unreachable!() };
    let items = &mut module.content.as_mut().unwrap().1;
    let item = items[0].clone();
    let target = item.span().byte_range();
    items.extend(core::iter::repeat_n(item, 256));
    assert!(!numeric_names(&file, &target));
    items_clear_for_budget(&mut file);
    assert!(!numeric_names(&file, &target));
}

fn items_clear_for_budget(file: &mut File) {
    let Item::Mod(module) = &mut file.items[0] else { unreachable!() };
    let items = &mut module.content.as_mut().unwrap().1;
    items.truncate(255);
    let nested = module.clone();
    file.items.push(Item::Mod(nested));
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
fn helper_collection_filters_declaration_metadata_and_lexical_shadowing() {
    for (declaration, retained) in [
        ("fn helper(x: u8) -> u8 { x }", true),
        ("pub fn helper(x: u8) -> u8 { x }", true),
        ("const fn helper(x: u8) -> u8 { x }", true),
        ("async fn helper(x: u8) -> u8 { x }", false),
        ("unsafe fn helper(x: u8) -> u8 { x }", false),
        ("extern \"C\" fn helper(x: u8) -> u8 { x }", false),
        ("fn helper<T>(x: T) -> T { x }", false),
        ("#[inline] fn helper(x: u8) -> u8 { x }", false),
    ] {
        let file = parse_file(&format!("{declaration} fn target() {{ while true {{}} }}")).unwrap();
        let Item::Fn(target) = &file.items[1] else { unreachable!() };
        let found = helpers(&file, &target.block.stmts[0].span().byte_range());
        assert_eq!(found.contains_key("helper"), retained, "{declaration}");
        assert!(found.contains_key("target"), "{declaration}");
        assert_eq!(found.len(), if retained { 2 } else { 1 }, "{declaration}");
    }
    for (body, retained, opaque) in [
        ("let helper = false;", false, false),
        ("let other = false;", true, false),
        ("{ let helper = false; }", false, false),
        ("opaque!();", false, true),
        ("struct Local;", false, true),
    ] {
        let file = parse_file(&format!("fn helper() -> bool {{ true }} fn target() {{ {body} while true {{}} }}")).unwrap();
        let Item::Fn(target) = &file.items[1] else { unreachable!() };
        let found = helpers(&file, &target.block.stmts.last().unwrap().span().byte_range());
        assert_eq!(found.contains_key("helper"), retained, "{body}");
        assert_eq!(found.contains_key("target"), !opaque, "{body}");
    }
}

#[test]
fn initializer_evidence_keeps_only_consecutive_constant_declarations_before_the_target() {
    for (prefix, expected) in [
        ("let a = 1; let b: bool = true;", vec![("a", "1"), ("b", "true")]),
        ("let a = 1; let a = 2;", vec![("a", "2")]),
        ("let a = 1; effect(); let b = false;", vec![("b", "false")]),
        ("let a = 1; a = 2;", vec![]),
        ("let a = 1; let b = unknown;", vec![]),
        ("let a = 1; let (b, c) = (2, 3);", vec![]),
        ("let a = 1; let ref b = 2;", vec![]),
        ("let a = 1; #[custom] let b = 2;", vec![]),
        (
            "let a = [1, 2]; let b = (false, 'q');",
            vec![("a", "[1, 2]"), ("b", "(false, 'q')")],
        ),
        ("let a = -(1_i16 as i32);", vec![("a", "-(1_i16 as i32)")]),
    ] {
        let file = parse_file(&format!("fn target() {{ {prefix} while true {{}} let later = 9; }}")).unwrap();
        let Item::Fn(target) = &file.items[0] else { unreachable!() };
        let loop_ = &target.block.stmts[target.block.stmts.len() - 2];
        let expected = expected
            .into_iter()
            .map(|(name, expression)| (name.into(), parse_str(expression).unwrap()))
            .collect();
        assert_eq!(initializers(&file, &loop_.span().byte_range()), expected, "{prefix}");
    }
}

#[test]
fn projection_sites_keep_named_and_tuple_fields_in_visit_order_but_reject_computed_roots() {
    let expression: Expr = parse_quote!((state.inner.count, tuple.0.1, factory().value, array[0].field));
    let actual = projection_sites(&expression);
    assert_eq!(
        actual,
        vec![
            ("state.inner.count".into(), parse_quote!(state.inner.count)),
            ("state.inner".into(), parse_quote!(state.inner)),
            ("tuple.0.1".into(), parse_quote!(tuple.0.1)),
            ("tuple.0".into(), parse_quote!(tuple.0)),
        ]
    );
    assert_eq!(
        place_name(&parse_quote!(
            #[custom]
            state.field
        )),
        None
    );
}

#[test]
fn primitive_wrapping_obeys_each_declared_integer_width() {
    for (ty, width, signed) in [
        ("u8", 8, false),
        ("u16", 16, false),
        ("u32", 32, false),
        ("u64", 64, false),
        ("u128", 128, false),
        ("i8", 8, true),
        ("i16", 16, true),
        ("i32", 32, true),
        ("i64", 64, true),
        ("i128", 128, true),
    ] {
        let primitive = if signed {
            Primitive::Signed(width)
        } else {
            Primitive::Unsigned(width)
        };
        let mask = u128::MAX >> (128 - width);
        assert_eq!(Primitive::named(ty), Some(primitive));
        let mut evaluator = machine();
        evaluator.values.insert("maximum".into(), Value::Int(mask, primitive));
        evaluator.values.insert("zero".into(), Value::Int(0, primitive));
        for (expression, expected) in [
            ("maximum.wrapping_add(1)", 0),
            ("zero.wrapping_sub(1)", mask),
            ("maximum.wrapping_mul(2)", mask - 1),
            ("maximum.wrapping_mul(0)", 0),
        ] {
            assert_eq!(
                evaluator.eval(&parse_str(expression).unwrap(), None),
                Some(Value::Int(expected, primitive)),
                "{ty}: {expression}"
            );
        }
        evaluator
            .values
            .insert("unknown".into(), Value::Symbol("input".into(), Binding::Primitive(primitive)));
        let identity = evaluator.values["unknown"].clone();
        for expression in ["unknown.wrapping_add(0)", "unknown.wrapping_sub(0)"] {
            let actual = evaluator.eval(&parse_str(expression).unwrap(), None);
            if signed {
                assert_eq!(actual, Some(identity.clone()), "{ty}: {expression}");
            } else {
                let Some(Value::UnsignedRange(low, high, kind, _)) = actual else {
                    panic!("expected unsigned interval for {ty}: {expression}");
                };
                assert_eq!((low, high, kind), (0, mask, primitive));
            }
        }
        assert_eq!(
            evaluator.eval(&parse_quote!(unknown.wrapping_mul(0)), None),
            Some(Value::Int(0, primitive))
        );
        if signed {
            assert_eq!(evaluator.eval(&parse_quote!(unknown.wrapping_mul(1)), None), None);
        } else {
            let Some(Value::UnsignedRange(low, high, kind, _)) = evaluator.eval(&parse_quote!(unknown.wrapping_mul(1)), None) else {
                panic!("expected unsigned interval for {ty}");
            };
            assert_eq!((low, high, kind), (0, mask, primitive));
        }
        assert_eq!(evaluator.eval(&parse_quote!(maximum.saturating_add(0)), None), None);
    }
    assert_eq!(Primitive::named("f32"), Some(Primitive::Float(32)));
    assert_eq!(Primitive::named("f64"), Some(Primitive::Float(64)));
    for name in ["usize", "isize", "u256", "i256", "f16", "Unknown"] {
        assert_eq!(Primitive::named(name), None, "{name}");
    }
}

#[test]
fn primitive_constructors_use_expected_types_and_refuse_shadowed_or_generic_calls() {
    for primitive in [
        Primitive::Unsigned(8),
        Primitive::Signed(16),
        Primitive::Float(32),
        Primitive::Float(64),
    ] {
        let expected = match primitive {
            Primitive::Unsigned(_) | Primitive::Signed(_) => Some(Value::Int(0, primitive)),
            Primitive::Float(_) => float(0.0, primitive),
        };
        assert_eq!(
            machine().eval(&parse_quote!(Default::default()), Some(Binding::Primitive(primitive))),
            expected
        );
    }
    for expression in [
        "Default::default(true)",
        "u8::default()",
        "Unknown::default()",
        "Default::other()",
        "Default::default::<u8>()",
        "Default::<u8>::default()",
        "::Default::default()",
        "std::default::Default::default()",
        "<u8 as Default>::default()",
        "u16::from()",
        "u16::from(1, 2)",
        "u16::other(1)",
        "Unknown::from(1)",
    ] {
        assert_eq!(
            machine().eval(&parse_str(expression).unwrap(), Some(Binding::Primitive(Primitive::Unsigned(8)))),
            None,
            "{expression}"
        );
    }
    let mut opaque = Machine {
        numeric_names: false,
        ..machine()
    };
    assert_eq!(opaque.eval(&parse_quote!(u16::from(1_u8)), None), None);
    assert_eq!(opaque.eval(&parse_quote!(1_u8 as u16), None), None);
}

#[test]
fn boolean_short_circuiting_never_evaluates_an_unreachable_unknown_operand() {
    for (expression, expected) in [
        ("false && unknown()", Some(Value::Bool(false))),
        ("true || unknown()", Some(Value::Bool(true))),
        ("true && unknown()", None),
        ("false || unknown()", None),
        ("!false", Some(Value::Bool(true))),
        ("!true", Some(Value::Bool(false))),
    ] {
        assert_eq!(machine().eval(&parse_str(expression).unwrap(), None), expected, "{expression}");
    }
}

#[test]
fn symbolic_string_and_character_operations_preserve_identity_without_inventing_facts() {
    let mut evaluator = machine();
    evaluator
        .values
        .insert("text".into(), Value::Symbol("source".into(), Binding::StrReference));
    evaluator
        .values
        .insert("other".into(), Value::Symbol("other".into(), Binding::StrReference));
    let first = evaluator.eval(&parse_quote!(text.chars().next().expect("nonempty")), None).unwrap();
    assert_eq!(first.kind(), Binding::Char);
    assert_eq!(
        evaluator.eval(&parse_quote!(text.chars().next().expect("another message")), None),
        Some(first.clone())
    );
    assert_ne!(
        evaluator.eval(&parse_quote!(other.chars().next().expect("nonempty")), None),
        Some(first)
    );
    for expression in [
        "text.chars(1).next().expect(\"nonempty\")",
        "text.chars().next(1).expect(\"nonempty\")",
        "text.chars().next().expect(message)",
        "text.bytes().next().expect(\"nonempty\")",
        "text.chars().last().expect(\"nonempty\")",
        "text.chars().next().expect()",
        "text[..]",
        "text[0..1]",
        "text[0..=1]",
    ] {
        assert_eq!(evaluator.eval(&parse_str(expression).unwrap(), None), None, "{expression}");
    }
    let slice = evaluator.eval(&parse_quote!(text[1..]), None).unwrap();
    assert_eq!(slice.kind(), Binding::StrReference);
    assert_ne!(slice, evaluator.eval(&parse_quote!(text[2..]), None).unwrap());
    assert_eq!(evaluator.eval(&parse_quote!(&'q'), None), Some(Value::CharReference('q')));
    assert_eq!(evaluator.eval(&parse_quote!(&mut 'q'), None), None);
}

#[test]
fn floating_arithmetic_and_casts_preserve_declared_precision_at_rounding_boundaries() {
    for (source, bits, primitive) in [
        ("16777216.0_f32 + 1.0_f32", 16_777_216_f64.to_bits(), Primitive::Float(32)),
        ("16777216.0_f64 + 1.0_f64", 16_777_217_f64.to_bits(), Primitive::Float(64)),
        ("16777217.0_f64", 16_777_217_f64.to_bits(), Primitive::Float(64)),
        ("16777217.0_f32", 16_777_216_f64.to_bits(), Primitive::Float(32)),
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
                &parse_str(source).expect("the fixed primitive expression is valid Rust syntax"),
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
            machine().eval(&parse_str(source).expect("the fixed comparison is valid Rust syntax"), None),
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
                &parse_str(source).expect("the fixed arithmetic expression is valid Rust syntax"),
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
            binary(&parse_quote!(&&), Value::Bool(left), Value::Bool(right)),
            Some(Value::Bool(and))
        );
        assert_eq!(
            binary(&parse_quote!(||), Value::Bool(left), Value::Bool(right)),
            Some(Value::Bool(or))
        );
    }
    let unknown = Value::Symbol("length".into(), Binding::Integer);
    let flag = Value::Symbol("flag".into(), Binding::Bool);
    for (operator, left, right, expected) in [
        (parse_quote!(-), unknown.clone(), unknown.clone(), Some(Value::IntegerZero)),
        (
            parse_quote!(-),
            unknown.clone(),
            Value::Symbol("other".into(), Binding::Integer),
            None,
        ),
        (parse_quote!(*), unknown.clone(), Value::Int(0, Primitive::Unsigned(8)), None),
        (parse_quote!(+), Value::IntegerZero, flag.clone(), None),
        (parse_quote!(*), Value::IntegerZero, flag.clone(), None),
        (parse_quote!(+), Value::Bool(true), Value::IntegerZero, None),
        (parse_quote!(+), Value::Bool(true), Value::Int(0, Primitive::Unsigned(8)), None),
        (parse_quote!(&&), unknown.clone(), Value::Bool(false), None),
        (parse_quote!(||), unknown, Value::Bool(true), None),
        (parse_quote!(&&), flag.clone(), Value::Bool(false), Some(Value::Bool(false))),
        (parse_quote!(||), flag.clone(), Value::Bool(true), Some(Value::Bool(true))),
        (parse_quote!(+), flag.clone(), flag.clone(), None),
        (parse_quote!(&&), flag, Value::IntegerZero, None),
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
        evaluator.block(&parse_quote!({
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
        (
            "fn helper(x: u8) -> u16 { u16::from(x) }",
            "helper(7)",
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
        let helpers = HashMap::from_iter([("helper".into(), parse_str(source).unwrap())]);
        let mut evaluator = Machine {
            helpers: Some(&helpers),
            ..machine()
        };
        assert_eq!(evaluator.eval(&parse_str(call).unwrap(), None), expected, "{source}");
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
    evaluator.values.insert("helper".into(), Value::Bool(false));
    assert_eq!(evaluator.eval(&parse_quote!(helper()), None), None);
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
        assert_eq!(machine().eval(&parse_str(source).unwrap(), expected_kind), expected, "{source}");
    }
    for source in [
        "value.len()",
        "value.push(0)",
        "value[0]",
        "value.get(0)",
        "value.wrapping_add(1_u8)",
        "true.wrapping_add(0_u8)",
        "&value",
    ] {
        let mut evaluator = machine();
        evaluator.values.insert("value".into(), Value::Bool(true));
        assert!(evaluator.effect(&parse_str(source).unwrap()).is_none(), "{source}");
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
        let expression: Expr = parse_str(source.replace("u8", "i8").as_str()).unwrap();
        let value = evaluator.eval(&expression, None);
        assert_eq!(value.is_some(), accepted, "{source}");
        if source.contains('*') || source.contains("wrapping_mul") {
            assert_eq!(value, Some(Value::Int(0, Primitive::Signed(8))));
        }
    }
    let unknown = Value::Symbol("n".into(), Binding::Integer);
    for (operator, left, right, expected) in [
        (parse_quote!(+), Value::IntegerZero, unknown.clone(), Some(unknown.clone())),
        (parse_quote!(*), Value::IntegerZero, unknown.clone(), Some(Value::IntegerZero)),
        (parse_quote!(-), Value::IntegerZero, unknown.clone(), None),
        (
            parse_quote!(+),
            unknown.clone(),
            Value::Int(0, Primitive::Unsigned(8)),
            Some(unknown.clone()),
        ),
        (parse_quote!(-), unknown.clone(), Value::IntegerZero, Some(unknown)),
        (
            parse_quote!(==),
            Value::MaybeChar('a'),
            Value::MaybeChar('a'),
            Some(Value::Bool(true)),
        ),
        (
            parse_quote!(!=),
            Value::MaybeChar('a'),
            Value::MaybeChar('b'),
            Some(Value::Bool(true)),
        ),
        (parse_quote!(<), Value::MaybeChar('a'), Value::MaybeChar('b'), None),
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
        assert!(evaluator.block(&parse_str(source).unwrap()).is_none(), "{source}");
    }
    let mut evaluator = machine();
    evaluator.values.insert("flag".into(), Value::Bool(true));
    evaluator.values.insert("count".into(), Value::Int(7, Primitive::Unsigned(8)));
    assert!(
        evaluator
            .block(&parse_quote!({
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
        assert!(evaluator.eval(&parse_str(expression).unwrap(), None).is_some(), "{expression}");
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
        assert!(evaluator.eval(&parse_str(expression).unwrap(), None).is_none(), "{expression}");
    }
    let character = evaluator.eval(&parse_quote!(chars[0]), None).unwrap();
    evaluator
        .facts
        .insert(identity(format_args!("{character:?}")).unwrap(), Value::Char('a'));
    assert_eq!(evaluator.eval(&parse_quote!(chars.get(0)), None), Some(Value::MaybeChar('a')));
    assert_eq!(evaluator.eval(&parse_quote!(&chars[0]), None), Some(Value::CharReference('a')));
}

#[test]
fn initializer_and_numeric_scope_searches_fail_closed_on_ambiguous_syntax() {
    for source in [
        "mod child { fn f() { let i = -1; let x = (2_u8 as u32); while true {} } }",
        "struct S; impl S { fn f() { let i = 1; while true {} } }",
    ] {
        let file = parse_file(source).unwrap();
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
        let file = parse_file(source).unwrap();
        let start = source.find("while").unwrap();
        assert!(!numeric_names(&file, &(start..start + 13)), "{source}");
    }
    let file = parse_file("fn helper() {} fn f() { item!(); while true {} }").unwrap();
    assert!(helpers(&file, &(36..49)).is_empty());
    let file = parse_file("fn helper() {} fn f() { struct S; while true {} }").unwrap();
    assert!(helpers(&file, &(33..46)).is_empty());
    assert!(constant_syntax(&parse_quote!(([1, 2], -3, (4_u8 as u32)))));
    assert!(!constant_syntax(&parse_quote!(unknown())));
}

#[test]
fn unsigned_arithmetic_and_comparisons_match_checked_native_boundaries() {
    for a in [0_u8, 1, 2, 7, 127, 128, 254, 255] {
        for b in [0_u8, 1, 2, 7, 8, 127, 128, 254, 255] {
            let ty = Primitive::Unsigned(8);
            let left = Value::Int(u128::from(a), ty);
            let right = Value::Int(u128::from(b), ty);
            for (op, expected) in [
                (parse_quote!(+), a.checked_add(b)),
                (parse_quote!(-), a.checked_sub(b)),
                (parse_quote!(*), a.checked_mul(b)),
                (parse_quote!(<<), a.checked_shl(u32::from(b))),
                (parse_quote!(>>), a.checked_shr(u32::from(b))),
            ] {
                assert_eq!(
                    binary(&op, left.clone(), right.clone()),
                    expected.map(|value| Value::Int(u128::from(value), ty)),
                    "{a} {op:?} {b}"
                );
            }
            for (op, expected) in [
                (parse_quote!(==), a == b),
                (parse_quote!(!=), a != b),
                (parse_quote!(<), a < b),
                (parse_quote!(<=), a <= b),
                (parse_quote!(>), a > b),
                (parse_quote!(>=), a >= b),
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
                (parse_quote!(+), a.checked_add(b)),
                (parse_quote!(-), a.checked_sub(b)),
                (parse_quote!(*), a.checked_mul(b)),
            ] {
                assert_eq!(binary(&op, encode(a), encode(b)), expected.map(encode), "{a} {op:?} {b}");
            }
            for (op, expected) in [
                (parse_quote!(==), a == b),
                (parse_quote!(!=), a != b),
                (parse_quote!(<), a < b),
                (parse_quote!(<=), a <= b),
                (parse_quote!(>), a > b),
                (parse_quote!(>=), a >= b),
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
                (parse_quote!(+), a.checked_add(b)),
                (parse_quote!(-), a.checked_sub(b)),
                (parse_quote!(*), a.checked_mul(b)),
            ] {
                assert_eq!(
                    binary(&op, encode(a), encode(b)),
                    expected.filter(|value| *value <= 127).map(encode),
                    "{a} {op:?} {b}"
                );
            }
            for (op, expected) in [
                (parse_quote!(==), a == b),
                (parse_quote!(!=), a != b),
                (parse_quote!(<), a < b),
                (parse_quote!(<=), a <= b),
                (parse_quote!(>), a > b),
                (parse_quote!(>=), a >= b),
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
            machine().eval(&parse_str(expression).unwrap(), Some(Binding::Integer)),
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
                (parse_quote!(a.wrapping_add(b)), a.wrapping_add(b)),
                (parse_quote!(a.wrapping_sub(b)), a.wrapping_sub(b)),
                (parse_quote!(a.wrapping_mul(b)), a.wrapping_mul(b)),
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
            .block(&parse_quote!({
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
