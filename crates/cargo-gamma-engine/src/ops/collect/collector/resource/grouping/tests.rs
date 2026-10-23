// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::token::Paren;

use super::fixture::{self, Entry, Input};
use crate::cfg::CfgSet;
use crate::ops::collect::collect_in;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

#[test]
fn grouping_helpers_reject_ambiguous_records_and_nonbinding_locals() {
    let grouped = syn::Expr::Group(syn::ExprGroup {
        attrs: Vec::new(),
        group_token: syn::token::Group::default(),
        expr: Box::new(syn::parse_quote!(input)),
    });
    assert_eq!(super::name(&grouped).as_deref(), Some("input"));
    for statement in [
        "let (a, b) = pair;",
        "let ref value = input;",
        "#[cfg(test)] let value = input;",
        "let value @ _ = input;",
        "let value = input else { return; };",
    ] {
        assert!(super::local(&syn::parse_str(statement).unwrap()).is_none(), "{statement}");
    }
    let file = syn::parse_file("struct Record { value: u8 } struct Record { value: bool }").unwrap();
    assert!(super::record(&file, "Record").is_none());
    let mut record: syn::ItemStruct = syn::parse_quote!(
        struct Record {
            value: u8,
        }
    );
    let field = record.fields.iter().next().unwrap().clone();
    let syn::Fields::Named(fields) = &mut record.fields else {
        unreachable!()
    };
    fields.named.extend(core::iter::repeat_n(field, 64));
    assert!(super::field(&record, "value").is_none());
    let mut expression: syn::Expr = syn::parse_quote!(input);
    for _ in 0..17 {
        expression = syn::Expr::Paren(syn::ExprParen {
            attrs: Vec::new(),
            paren_token: Paren::default(),
            expr: Box::new(expression),
        });
    }
    assert!(matches!(super::bare(&expression), syn::Expr::Paren(_)));
    assert!(super::name(&expression).is_none());
}

#[test]
fn grouping_boundaries_require_identifier_closures_and_slice_shapes() {
    for source in [
        "input.entries[cursor..].iter().position(predicate).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|| true).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|(entry,)| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), || cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |(offset,)| cursor * offset)",
    ] {
        assert!(grouping().boundary(&syn::parse_str(source).unwrap(), true).is_none(), "{source}");
    }
    let original = group_loop();
    assert!(!grouping().proves(&syn::parse_quote!(true), &original));
    let mut before = original.clone();
    let syn::Expr::While(loop_) = &mut before else { unreachable!() };
    loop_.body.stmts[3] = syn::parse_quote!(let ids = group.iter().collect::<Vec<_>>(););
    assert!(!grouping().proves(&before, &multiplied(&before)));
    for statement in [
        "ids.dedup::<u8>();",
        "#[cfg(test)] ids.dedup();",
        "files.push(value);",
        "unknown();",
    ] {
        let mut before = original.clone();
        let syn::Expr::While(loop_) = &mut before else { unreachable!() };
        loop_.body.stmts[4] = syn::parse_str(statement).unwrap();
        assert!(!grouping().proves(&before, &multiplied(&before)), "{statement}");
    }
}

#[test]
fn grouping_setup_tolerates_nonbinding_setup_expressions_without_treating_them_as_outputs() {
    let mut file = syn::parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let syn::Item::Fn(function) = file
        .items
        .iter_mut()
        .find(|item| matches!(item, syn::Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    function.block.stmts.insert(0, syn::parse_quote!(setup();));
    let syn::Stmt::Expr(expression, _) = &function.block.stmts[3] else {
        unreachable!()
    };
    let expression = expression.clone();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    assert!(super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_some());
}

#[test]
fn captures_reject_unbound_closure_values_items_macros_attributes_and_budget_exhaustion() {
    let file = syn::parse_file("fn safe() {}").unwrap();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    for expression in [
        "|entry| unknown",
        "|(entry,)| entry",
        "|entry| { item!(); entry }",
        "|entry| { struct S; entry }",
        "|entry| { #[cfg(test)] let x = entry; x }",
    ] {
        let mut captures = super::Captures {
            sources: &sources,
            source: "",
            inline: &[],
            bound: crate::HashSet::default(),
            allowed: crate::HashSet::default(),
            remaining: 256,
            valid: true,
        };
        syn::visit::Visit::visit_expr(&mut captures, &syn::parse_str(expression).unwrap());
        assert!(!captures.valid, "{expression}");
    }
    let mut captures = super::Captures {
        sources: &sources,
        source: "",
        inline: &[],
        bound: crate::HashSet::default(),
        allowed: crate::HashSet::default(),
        remaining: 0,
        valid: true,
    };
    syn::visit::Visit::visit_expr(&mut captures, &syn::parse_quote!(true));
    assert!(!captures.valid);
}

#[test]
fn empty_group_proof_preserves_nonzero_mutable_and_effectful_cases() {
    let source = include_str!("../fixtures/grouping.rs");
    let file = SourceFile::parse("grouping.rs", source.into()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("arith.add_to_mul").unwrap(), &CfgSet::unconditional());
    for (function, retained) in [("groups", false), ("nonzero", true), ("mutable", true), ("unknown_factory", true)] {
        let found = candidates
            .iter()
            .filter(|candidate| &*candidate.item_path == function && file.slice(&candidate.span) == "cursor + offset")
            .count();
        assert_eq!(found, usize::from(retained), "{function}");
    }
}

#[test]
fn finite_grouping_controls_have_distinct_ordinary_results() {
    let input = Input {
        entries: ["a", "b", "c", "c", "c", "d"]
            .into_iter()
            .enumerate()
            .map(|(ordinal, path)| Entry {
                path: path.into(),
                ordinal: u32::try_from(ordinal).unwrap(),
            })
            .collect(),
    };
    let original = fixture::nonzero(&input);
    let mutated = fixture::finite_multiplication(&input);
    assert_eq!(original.iter().map(|group| group.path.as_str()).collect::<Vec<_>>(), ["c", "d"]);
    assert_eq!(mutated.len(), 1);
    assert_eq!(mutated[0].ids, [2, 3, 4, 5]);
    assert_ne!(original, mutated);
    let empty = Input { entries: Vec::new() };
    assert!(fixture::groups(&empty).is_empty());
    let mut singleton = Input {
        entries: vec![Entry {
            path: "same".into(),
            ordinal: 7,
        }],
    };
    assert_eq!(fixture::groups(&singleton)[0].ids, [7]);
    assert_eq!(fixture::mutable(&mut singleton)[0].ids, [7]);
    assert_eq!(fixture::unknown_factory(&singleton)[0].ids, [7]);
}

fn grouping() -> super::Grouping {
    super::Grouping {
        cursor: "cursor".into(),
        input: "input . entries".into(),
        field: "path".into(),
        copied: "path".into(),
        boundary: "end".into(),
        outputs: core::iter::once("files".into()).collect(),
        output_record: "Output".into(),
        vector_fields: core::iter::once("ids".into()).collect(),
    }
}

fn group_loop() -> syn::Expr {
    let file = syn::parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let syn::Item::Fn(function) = file
        .items
        .iter()
        .find(|item| matches!(item, syn::Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let syn::Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    expression.clone()
}

#[test]
fn grouping_setup_rejects_invalid_parameter_field_and_empty_output_provenance() {
    for fault in 0..20 {
        let mut file = syn::parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        let function_at = file
            .items
            .iter()
            .position(|item| matches!(item, syn::Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap();
        let syn::Item::Fn(function) = &mut file.items[function_at] else {
            unreachable!()
        };
        match fault {
            0 => function.sig.inputs[0] = syn::parse_quote!(input: Input),
            1 => function.sig.inputs[0] = syn::parse_quote!(input: &mut Input),
            2 => function.block.stmts[1] = syn::parse_quote!(let mut cursor = unknown();),
            3 => {
                function.block.stmts[0] = syn::parse_quote!(
                    struct Local;
                );
            }
            12 => function.block.stmts.extend(core::iter::repeat_n(syn::parse_quote!(after();), 64)),
            13 => function.sig.inputs[0] = syn::parse_quote!((input, other): (&Input, u8)),
            14 => function.sig.inputs[0] = syn::parse_quote!(input: &External),
            _ => {}
        }
        let syn::Stmt::Expr(syn::Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        match fault {
            4 => *loop_.cond = syn::parse_quote!(cursor < input.0.len()),
            5 => loop_.body.stmts[0] = syn::parse_quote!(let path = input.entries[cursor].0.clone();),
            6 => loop_.body.stmts[6] = syn::parse_quote!(files.push(Output { 0: path, ids });),
            7 => loop_.body.stmts[6] = syn::parse_quote!(files.push(external::Output { path, ids });),
            8 => loop_.body.stmts[0] = syn::parse_quote!(let path = input.entries[cursor].unknown.clone();),
            9 => *loop_.cond = syn::parse_quote!(cursor < input.entries.len()),
            10 => loop_.body.stmts[2] = syn::parse_quote!(let group = &input.entries[cursor..end];),
            17 => loop_.body.stmts[0] = syn::parse_quote!(let path = input.entries[other].path.clone();),
            18 => loop_.body.stmts[6] = syn::parse_quote!(files.push(External { path, ids });),
            19 => loop_.body.stmts[3] = syn::parse_quote!(let ids = group.iter().map(|entry| unknown).collect::<Vec<_>>();),
            _ => {}
        }
        let expression = syn::Expr::While(loop_.clone());
        if fault == 9 {
            let syn::Item::Struct(input) = &mut file.items[0] else {
                unreachable!()
            };
            input.fields.iter_mut().next().unwrap().ty = syn::parse_quote!(&[Entry]);
        }
        if fault == 10 {
            let syn::Item::Struct(input) = &mut file.items[0] else {
                unreachable!()
            };
            input.fields.iter_mut().next().unwrap().ty = syn::parse_quote!(Vec<(Entry,)>);
        }
        if fault == 11 {
            let syn::Item::Struct(output) = &mut file.items[2] else {
                unreachable!()
            };
            output.fields.iter_mut().next().unwrap().ty = syn::parse_quote!(u32);
        }
        if matches!(fault, 15 | 16) {
            let syn::Item::Struct(input) = &mut file.items[0] else {
                unreachable!()
            };
            input.fields.iter_mut().next().unwrap().ty = if fault == 15 {
                syn::parse_quote!(Vec)
            } else {
                syn::parse_quote!(Vec<External>)
            };
        }
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert!(
            super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_none(),
            "fault {fault}"
        );
    }
}

fn multiplied(original: &syn::Expr) -> syn::Expr {
    let mut mutant = original.clone();
    let syn::Expr::While(loop_) = &mut mutant else { unreachable!() };
    let syn::Stmt::Local(local) = &mut loop_.body.stmts[1] else {
        unreachable!()
    };
    *local.init.as_mut().unwrap().expr = syn::parse_quote!(
        input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| cursor * offset)
    );
    mutant
}

#[test]
fn grouping_boundary_requires_same_input_slice_predicate_and_cursor() {
    let proof = grouping();
    for (source, multiplication, accepted) in [
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor + offset)",
            false,
            true,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            true,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor + offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            false,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path == path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.other != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| other.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != other).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..end].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..=end].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[other..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "other.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(other.entries.len(), |offset| cursor * offset)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| offset * cursor)",
            true,
            false,
        ),
        (
            "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * other)",
            true,
            false,
        ),
    ] {
        assert_eq!(
            proof.boundary(&syn::parse_str(source).unwrap(), multiplication).is_some(),
            accepted,
            "{source}"
        );
    }
}

#[test]
fn grouping_recurrence_requires_growth_empty_consumed_fields_and_final_assignment() {
    let original = group_loop();
    let mutant = multiplied(&original);
    assert!(grouping().proves(&original, &mutant));
    assert!(!grouping().proves(&original, &original));

    for fault in 0..14 {
        let mut before = original.clone();
        let syn::Expr::While(loop_) = &mut before else { unreachable!() };
        match fault {
            0 => {
                let _removed = loop_.body.stmts.remove(6);
            }
            1 => {
                let _removed = loop_.body.stmts.pop();
            }
            2 => loop_.body.stmts[7] = syn::parse_quote!(other = end;),
            3 => loop_.body.stmts[7] = syn::parse_quote!(cursor = other;),
            4 => loop_.body.stmts.push(syn::parse_quote!(side_effect();)),
            5 => loop_.body.stmts[6] = syn::parse_quote!(other.push(Output { path, ids });),
            6 => loop_.body.stmts[6] = syn::parse_quote!(files.push(Other { path, ids });),
            7 => loop_.body.stmts[6] = syn::parse_quote!(files.push(Output { path, ids: other });),
            8 => loop_.body.stmts[6] = syn::parse_quote!(files.push(Output { path: other, ids });),
            9 => loop_.body.stmts[2] = syn::parse_quote!(let group = &input.entries[cursor..other];),
            10 => loop_.body.stmts[3] = syn::parse_quote!(let ids = other.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
            11 => loop_.body.stmts[4] = syn::parse_quote!(ids.clear();),
            12 => loop_.body.stmts.insert(
                6,
                syn::parse_quote!(let ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
            ),
            13 => loop_.body.stmts[3] = syn::parse_quote!(let ids = group.collect::<Vec<_>>();),
            _ => unreachable!(),
        }
        let after = multiplied(&before);
        assert!(!grouping().proves(&before, &after), "invalid recurrence {fault}");
    }
    let mut after = mutant;
    let syn::Expr::While(loop_) = &mut after else { unreachable!() };
    *loop_.cond = syn::parse_quote!(cursor <= input.entries.len());
    assert!(
        !grouping().proves(&original, &after),
        "additional condition mutation must not be hidden"
    );
}

#[test]
fn empty_group_callbacks_and_output_consumption_require_the_closed_recurrence_grammar() {
    for (at, statement, expected) in [
        (
            3,
            "let ids = group.iter().filter_map(|entry| Some(entry.ordinal)).collect::<Vec<_>>();",
            true,
        ),
        (
            3,
            "let ids = group.iter().filter(|entry| entry.ordinal != 0).collect::<Vec<_>>();",
            false,
        ),
        (3, "let ids = group.iter().map().collect::<Vec<_>>();", false),
        (
            3,
            "let ids = group.iter().map(|entry| entry.ordinal, |entry| entry.ordinal).collect::<Vec<_>>();",
            false,
        ),
        (
            3,
            "let ids = group.iter().map::<u32>(|entry| entry.ordinal).collect::<Vec<_>>();",
            false,
        ),
        (
            3,
            "let ids = (#[custom] group.iter().map(|entry| entry.ordinal)).collect::<Vec<_>>();",
            false,
        ),
        (4, "ids.sort_by(|left, right| left.cmp(right));", true),
        (4, "ids.sort_by();", false),
        (4, "ids.sort_by(compare);", false),
        (4, "ids.sort_by(|left, right| left.cmp(right), 0);", false),
        (4, "files.sort_by(|left, right| left.cmp(right));", false),
        (4, "ids.dedup(0);", false),
        (4, "files.dedup();", false),
        (6, "files.push(Output { path, ids });", true),
        (6, "files.push(Output { path, ids }, Output { path, ids });", false),
        (6, "files.push(Output { path, ..other });", false),
        (6, "files.push(#[custom] Output { path, ids });", false),
        (6, "files.push(Output { path, ids: group });", false),
        (6, "files.push(Output { path, ids: build() });", false),
    ] {
        let mut before = group_loop();
        let syn::Expr::While(loop_) = &mut before else {
            panic!("the checked-in grouping fixture contains a while loop");
        };
        loop_.body.stmts[at] = syn::parse_str(statement).expect("the fixed recurrence statement is valid Rust syntax");
        assert_eq!(grouping().proves(&before, &multiplied(&before)), expected, "{statement}");
    }
    let mut before = group_loop();
    let syn::Expr::While(loop_) = &mut before else {
        panic!("the checked-in grouping fixture contains a while loop");
    };
    loop_.body.stmts.insert(
        4,
        syn::parse_quote!(let unused = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
    );
    assert!(
        !grouping().proves(&before, &multiplied(&before)),
        "every empty collection must be consumed by an output field"
    );
}

#[test]
fn grouping_capture_scopes_restore_nested_closure_and_block_bindings() {
    let file = syn::parse_file("fn known() {}").expect("the fixed helper declaration is valid Rust syntax");
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    for (expression, expected) in [
        ("|entry| { let local = entry; local }", true),
        ("|entry| { { let local = entry; } local }", false),
        ("|entry| { (|inner| inner)(entry); entry }", true),
        ("|entry| { (|inner| inner)(entry); inner }", false),
        ("|entry| { known(); outside }", true),
        ("|entry| { unknown(); outside }", false),
    ] {
        let mut captures = super::Captures {
            sources: &sources,
            source: "",
            inline: &[],
            bound: crate::HashSet::default(),
            allowed: crate::HashSet::from_iter(["outside".into()]),
            remaining: 256,
            valid: true,
        };
        syn::visit::Visit::visit_expr(
            &mut captures,
            &syn::parse_str(expression).expect("the fixed closure is valid Rust syntax"),
        );
        assert_eq!(captures.valid, expected, "{expression}");
        assert_eq!(captures.bound, crate::HashSet::default(), "{expression}");
        assert_eq!(captures.allowed, crate::HashSet::from_iter(["outside".into()]), "{expression}");
    }
}
