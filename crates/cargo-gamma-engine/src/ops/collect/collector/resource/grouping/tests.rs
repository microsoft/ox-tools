// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use proc_macro2::Span;
use syn::spanned::Spanned as _;
use syn::token::Paren;
use syn::{
    Expr, ExprGroup, ExprParen, ExprTuple, ExprWhile, Fields, Ident, ImplItem, Item, ItemStruct, PathArguments, Stmt, Type, parse_file,
    parse_quote, parse_str,
};

use super::fixture::{self, Entry, Input};
use crate::cfg::CfgSet;
use crate::ops::collect::collect_in;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

#[test]
fn exact_grouping_boundaries_require_unary_closures_and_a_binary_mapper_body() {
    for source in [
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset, extra| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry, extra| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor)",
    ] {
        assert_eq!(grouping().boundary(&parse_str(source).unwrap(), true), None, "{source}");
    }
}

#[test]
fn exact_grouping_schema_requires_an_actual_first_generic_argument() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let original = file.clone();
    let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
    let Fields::Named(fields) = &mut input.fields else { unreachable!() };
    // rustfmt removes empty arguments from parse_quote! input, but preserves this string.
    fields.named[0].ty = parse_str("Vec<>").expect("empty angle arguments are valid Rust type syntax");
    let Type::Path(vector) = &fields.named[0].ty else {
        panic!("the input field is a vector path");
    };
    let PathArguments::AngleBracketed(arguments) = &vector.path.segments[0].arguments else {
        panic!("the empty angle brackets must reach generic argument extraction");
    };
    assert_eq!(arguments.args.len(), 0);
    let Item::Fn(function) = file
        .items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    assert!(sources.closed_methods_for("", &[]));
    assert!(sources.local_record_for("", &[], "Input"));
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    let original_sources = crate::ops::collect::resource_namespaces::Namespaces::of(&original, &CfgSet::unconditional());
    assert_eq!(
        super::Grouping::new(&original, expression, &bindings, &original_sources, "", &[]).map(|proof| (proof.cursor, proof.boundary)),
        Some(("cursor".into(), "end".into())),
        "the unchanged schema must pass every prerequisite before generic argument extraction"
    );
    assert!(super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_none());
}

#[test]
fn exact_grouping_requires_closed_methods_even_when_primitive_and_record_names_are_known() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    file.items.push(parse_quote!(
        trait Extension {
            fn clone(&self);
        }
    ));
    let Item::Fn(function) = file
        .items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    assert!(sources.primitive_scope_for("", &[]));
    assert!(!sources.closed_methods_for("", &[]));
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    assert!(super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_none());
}

#[test]
fn exact_grouping_loop_metadata_preserves_real_containing_function_spans() {
    for prefix in ["#[custom] ", "'outer: "] {
        let source = include_str!("../fixtures/grouping.rs").replacen("while cursor <", &format!("{prefix}while cursor <"), 1);
        let file = parse_file(&source).unwrap();
        let Item::Fn(function) = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
            unreachable!()
        };
        assert!(function.block.span().byte_range().contains(&expression.span().byte_range().start));
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert!(
            super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_none(),
            "{prefix}"
        );
    }
}

#[test]
fn grouping_setup_reads_the_first_push_argument_but_recurrence_still_requires_unary_push() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Fn(function) = file
        .items
        .iter_mut()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
        unreachable!()
    };
    loop_.body.stmts[6] = parse_quote!(files.push(Output {path,ids}, false););
    let expression = Expr::While(loop_.clone());
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    let proof = super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]);
    assert!(proof.is_some(), "setup reads Output rather than the trailing boolean argument");
    assert!(!proof.unwrap().proves(&expression, &multiplied(&expression)));
}

#[test]
fn grouping_boundary_rejects_a_different_left_cursor_with_the_correct_right_offset() {
    let boundary = parse_quote!(
        input.entries[cursor..]
            .iter()
            .position(|entry| entry.path != path)
            .map_or(input.entries.len(), |offset| other * offset)
    );
    assert!(grouping().boundary(&boundary, true).is_none());
}

#[test]
fn direct_grouping_checks_isolate_callback_output_growth_and_final_assignment_requirements() {
    for fault in 0..10 {
        let mut before = group_loop();
        let Expr::While(node) = &mut before else { unreachable!() };
        match fault {
            0 => node.body.stmts[3] = parse_quote!(let ids=group.other().map(|entry|entry.ordinal).collect::<Vec<_>>();),
            1 => node.body.stmts[3] = parse_quote!(let ids=get_group().iter().map(|entry|entry.ordinal).collect::<Vec<_>>();),
            2 => node.body.stmts[6] = parse_quote!(get_files().push(Output {path,ids});),
            3 => node.body.stmts[4] = parse_quote!(ids.other(|left,right|left.cmp(right));),
            4 => node.body.stmts[6] = parse_quote!(files.other(Output {path,ids});),
            5 => node.body.stmts[6] = parse_quote!(files.push(Output {path,ids,..other});),
            6 => node.body.stmts.push(parse_quote!(cursor=end;)),
            7 => {
                node.body.stmts.truncate(2);
                node.body.stmts.push(parse_quote!(cursor=end;));
            }
            8 => {
                let Stmt::Local(local) = &mut node.body.stmts[3] else {
                    unreachable!()
                };
                let Expr::MethodCall(collect) = &mut *local.init.as_mut().unwrap().expr else {
                    unreachable!()
                };
                let Expr::MethodCall(adapter) = &mut *collect.receiver else {
                    unreachable!()
                };
                adapter.attrs.push(parse_quote!(#[custom]));
            }
            9 => node.body.stmts[4] = parse_quote!(ids.push(|left,right|left.cmp(right));),
            _ => unreachable!(),
        }
        assert_eq!(
            grouping().check(&before, &multiplied(&before)),
            None,
            "unsupported recurrence step {fault}"
        );
    }
    let before = group_loop();
    assert_eq!(grouping().check(&before, &multiplied(&before)), Some(()));
}

#[test]
fn direct_capture_attribute_visit_invalidates_without_spending_work_or_mutating_bindings() {
    let file = parse_file("").unwrap();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    let mut captures = super::Captures {
        sources: &sources,
        source: "",
        inline: &[],
        bound: crate::HashSet::from_iter(["entry".into()]),
        allowed: crate::HashSet::from_iter(["input".into()]),
        remaining: 7,
        valid: true,
    };
    syn::visit::Visit::visit_attribute(&mut captures, &parse_quote!(#[custom]));
    assert!(!captures.valid);
    assert_eq!(captures.remaining, 7);
    assert_eq!(captures.bound, crate::HashSet::from_iter(["entry".into()]));
    assert_eq!(captures.allowed, crate::HashSet::from_iter(["input".into()]));
}

#[test]
fn grouping_method_and_record_helpers_require_exact_names_metadata_and_arity() {
    for (source, requested, arguments, expected) in [
        ("items.iter()", "iter", 0, true),
        ("items.other()", "iter", 0, false),
        ("items.iter(1)", "iter", 0, false),
        ("#[custom] items.iter()", "iter", 0, false),
        ("items.iter::<u8>()", "iter", 0, false),
        ("items.collect::<Vec<_>>()", "collect", 0, true),
    ] {
        assert_eq!(
            super::method(&parse_str(source).unwrap(), requested, arguments).is_some(),
            expected,
            "{source}"
        );
    }
    let file = parse_file("struct Record<T> { value: T }").unwrap();
    assert!(super::record(&file, "Record").is_none());
    for count in [63, 64, 65, 66] {
        let fields = (0..count).map(|at| format!("field_{at}: u8")).collect::<Vec<_>>().join(",");
        let record = parse_str(&format!("struct Record {{ {fields} }}")).unwrap();
        assert_eq!(super::field(&record, "field_0").is_some(), count <= 64, "{count} fields");
    }
    for depth in [1, 15, 16, 17] {
        let expression = parse_str(&format!("{}input{}", "(".repeat(depth), ")".repeat(depth))).unwrap();
        assert_eq!(
            super::name(&expression).as_deref(),
            (depth <= 16).then_some("input"),
            "{depth} wrappers"
        );
    }
}

#[test]
fn grouping_function_search_respects_signature_and_impl_boundaries() {
    /// Recovers the parsed while node and its span for containing-function boundary checks.
    struct Find(Option<Expr>);
    impl<'ast> syn::visit::Visit<'ast> for Find {
        fn visit_expr_while(&mut self, i: &'ast ExprWhile) {
            self.0 = Some(Expr::While(i.clone()));
        }
    }

    for (source, expected) in [
        ("fn unrelated() {} fn run() { while ready {} }", true),
        ("fn run<T>() { while ready {} }", false),
        ("struct S; impl S { fn unrelated() {} fn run() { while ready {} } }", true),
        ("struct S; impl<T> S { fn run() { while ready {} } }", false),
        ("struct S; impl S { fn run<T>() { while ready {} } }", false),
    ] {
        let file = parse_file(source).unwrap();
        let start = source.find("while").unwrap();
        let mut find = Find(None);
        syn::visit::Visit::visit_file(&mut find, &file);
        let target = find.0.expect("the fixture contains one target while loop");
        assert_eq!(target.span().byte_range().start, start);
        assert_eq!(
            super::function(&file, &target).map(|(signature, _)| signature.ident.to_string()),
            expected.then(|| "run".into())
        );
    }
    let file = parse_file("struct S; impl S { fn unrelated() {} fn run() { while ready {} } }").unwrap();
    let Item::Impl(implementation) = &file.items[1] else {
        unreachable!()
    };
    let unrelated = implementation.items[0].clone();
    let run = implementation.items[1].clone();
    let ImplItem::Fn(function) = &run else { unreachable!() };
    let Stmt::Expr(target, _) = &function.block.stmts[0] else {
        unreachable!()
    };
    for (count, accepted) in [(255, true), (256, true), (257, false), (258, false)] {
        let mut file = file.clone();
        let Item::Impl(implementation) = &mut file.items[1] else {
            unreachable!()
        };
        implementation.items = core::iter::repeat_n(unrelated.clone(), count - 1).chain([run.clone()]).collect();
        assert_eq!(super::function(&file, target).is_some(), accepted, "{count} impl methods");
    }
    let original = parse_file("fn unrelated() {} fn run() { while ready {} }").unwrap();
    let Item::Fn(run) = &original.items[1] else { unreachable!() };
    let Stmt::Expr(target, _) = &run.block.stmts[0] else {
        unreachable!()
    };
    for (count, accepted) in [(255, true), (256, true), (257, false)] {
        let mut file = original.clone();
        file.items = core::iter::repeat_n(original.items[0].clone(), count - 1)
            .chain([original.items[1].clone()])
            .collect();
        assert_eq!(super::function(&file, target).is_some(), accepted, "{count} root functions");
    }
}

#[test]
fn grouping_setup_does_not_promote_unknown_output_factories_to_owned_vectors() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Fn(function) = file
        .items
        .iter_mut()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    function.block.stmts[0] = parse_quote!(let mut files = factory(););
    let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    let expression = expression.clone();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    let proof = super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).unwrap();
    assert!(!proof.proves(&expression, &multiplied(&expression)));
}

#[test]
fn grouping_setup_limits_accept_the_last_complete_schema_and_reject_the_next() {
    for (kind, count, accepted) in [
        ("items", 255, true),
        ("items", 256, true),
        ("items", 257, false),
        ("body", 15, true),
        ("body", 16, true),
        ("body", 17, false),
        ("setup", 63, true),
        ("setup", 64, true),
        ("setup", 65, false),
        ("attribute", 0, false),
        ("label", 0, false),
    ] {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        if kind == "items" {
            while file.items.len() < count {
                let ident = Ident::new(&format!("padding_{}", file.items.len()), Span::call_site());
                file.items.push(parse_quote!(fn #ident() {}));
            }
        }
        let Item::Fn(function) = file
            .items
            .iter_mut()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        if kind == "setup" {
            function.block.stmts.resize(count, parse_quote!(() ;));
        }
        let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        match kind {
            "body" => loop_.body.stmts.resize(count, parse_quote!(() ;)),
            "attribute" => loop_.attrs.push(parse_quote!(#[custom])),
            "label" => loop_.label = Some(parse_quote!('outer:)),
            _ => {}
        }
        let expression = Expr::While(loop_.clone());
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert_eq!(
            super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_some(),
            accepted,
            "{kind}: {count}"
        );
    }
}

#[test]
fn grouping_capture_work_budget_is_spent_on_each_expression() {
    let file = parse_file("fn known() {}").unwrap();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    for (remaining, expected) in [(0, false), (1, false), (2, true), (3, true)] {
        let mut captures = super::Captures {
            sources: &sources,
            source: "",
            inline: &[],
            bound: crate::HashSet::default(),
            allowed: crate::HashSet::default(),
            remaining,
            valid: true,
        };
        syn::visit::Visit::visit_expr(&mut captures, &parse_quote!(|entry| entry));
        assert_eq!(captures.valid, expected, "work allowance {remaining}");
    }
}

#[test]
fn grouping_setup_capture_allowance_requires_one_unspent_visit() {
    /// Counts the original body's expression visits so padding crosses the capture allowance
    /// independently of incidental fixture syntax.
    struct Count(usize);
    impl<'ast> syn::visit::Visit<'ast> for Count {
        fn visit_expr(&mut self, i: &'ast Expr) {
            self.0 += 1;
            syn::visit::visit_expr(self, i);
        }
    }

    let original = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Fn(function) = original
        .items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(Expr::While(loop_), _) = &function.block.stmts[2] else {
        unreachable!()
    };
    let mut count = Count(0);
    syn::visit::Visit::visit_block(&mut count, &loop_.body);
    assert!(count.0 < 254);
    for work in [254, 255, 256, 257] {
        let mut file = original.clone();
        let Item::Fn(function) = file
            .items
            .iter_mut()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        let padding = Expr::Tuple(ExprTuple {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            elems: core::iter::repeat_n::<Expr>(parse_quote!(true), work - count.0 - 1).collect(),
        });
        loop_.body.stmts.push(Stmt::Expr(padding, Some(syn::token::Semi::default())));
        let expression = Expr::While(loop_.clone());
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert_eq!(
            super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_some(),
            work < 256,
            "{work} expression visits"
        );
    }
}

#[test]
fn grouping_setup_rejects_missing_schema_body_and_parameter_evidence() {
    for fault in 0..17 {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        if matches!(fault, 3..=5) {
            let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
            let Fields::Named(fields) = &mut input.fields else { unreachable!() };
            match fault {
                3 => fields.named.clear(),
                4 => fields.named[0].ty = parse_quote!(Vec),
                5 => fields.named[0].ty = parse_quote!(Vec<module::Entry>),
                _ => unreachable!(),
            }
        }
        if fault == 12 {
            let Item::Struct(output) = &mut file.items[2] else { unreachable!() };
            let Fields::Named(fields) = &mut output.fields else {
                unreachable!()
            };
            fields.named[0].ty = parse_quote!(u32);
        }
        if matches!(fault, 14..=16) {
            let index = fault - 14;
            let Item::Struct(record) = &mut file.items[index] else {
                unreachable!()
            };
            record.generics = parse_quote!(<T = u32>);
        }
        if fault == 13 {
            let Item::Struct(entry) = &mut file.items[1] else { unreachable!() };
            let Fields::Named(fields) = &mut entry.fields else { unreachable!() };
            fields.named[0].ty = parse_quote!(Vec<u8>);
            let Item::Struct(output) = &mut file.items[2] else { unreachable!() };
            let Fields::Named(fields) = &mut output.fields else {
                unreachable!()
            };
            fields.named = fields
                .named
                .iter()
                .filter(|field| field.ident.as_ref().is_some_and(|name| name == "ids"))
                .cloned()
                .collect();
        }
        let Item::Fn(function) = file
            .items
            .iter_mut()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        if fault == 1 {
            function.sig.generics = parse_quote!(<T>);
        } else if fault == 2 {
            function.sig.inputs[0] = parse_quote!(other: &Input);
        }
        let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        match fault {
            6 => loop_.body.stmts.clear(),
            7 => loop_.body.stmts[0] = parse_quote!(unknown();),
            8 => loop_.body.stmts.truncate(1),
            9 => loop_.body.stmts[1] = parse_quote!(unknown();),
            13 => loop_.body.stmts[6] = parse_quote!(files.push(Output {ids});),
            _ => {}
        }
        let expression = Expr::While(loop_.clone());
        if fault == 10 {
            function.block.stmts[2] = parse_quote!(unknown(););
        } else if fault == 11 {
            function.block.stmts[2] = parse_quote!({ #expression });
        }
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = if fault == 0 {
            crate::HashMap::default()
        } else {
            crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)])
        };
        assert!(
            super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_none(),
            "missing setup evidence {fault}"
        );
    }
}

#[test]
fn grouping_output_requires_the_same_stable_path_type_as_the_cloned_input() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Struct(output) = &mut file.items[2] else { unreachable!() };
    let Fields::Named(fields) = &mut output.fields else {
        unreachable!()
    };
    let ty = parse_quote!(camino::Utf8PathBuf);
    fields.named[0].ty = ty;
    let Item::Fn(function) = file
        .items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    let mut sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    sources.dependencies(&[crate::ops::collect::ResourceDependency {
        crate_root: String::new(),
        import_name: "camino".into(),
        package_id: "registry package identity".into(),
        package_name: "camino".into(),
        version: "1.2.5".into(),
        source: "registry+https://github.com/rust-lang/crates.io-index".into(),
        features: Vec::new(),
        registry_closure: true,
        library_root: None,
    }]);
    assert!(sources.stable_path_for("", &[], &parse_quote!(String)));
    assert!(sources.stable_path_for("", &[], &parse_quote!(camino::Utf8PathBuf)));
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    assert!(super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_none());
}

#[test]
fn grouping_schema_records_require_active_nominal_declarations_not_aliases() {
    for index in 0..3 {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        let Item::Struct(record) = &mut file.items[index] else {
            unreachable!()
        };
        let name = record.ident.clone();
        let mut actual = record.clone();
        actual.ident = Ident::new(&format!("Actual{name}"), Span::call_site());
        let actual_name = actual.ident.clone();
        record.attrs.push(parse_quote!(#[cfg(any())]));
        file.items.push(Item::Struct(actual));
        file.items.push(parse_quote!(type #name = #actual_name;));
        let Item::Fn(function) = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
            unreachable!()
        };
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        assert!(sources.closed_methods_for("", &[]));
        assert!(sources.primitive_scope_for("", &[]));
        assert!(!sources.local_record_for("", &[], &name.to_string()));
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert!(
            super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_none(),
            "{name}"
        );
    }
}

#[test]
fn grouping_schema_uses_the_vector_element_not_its_allocator_parameter() {
    for ty in [parse_quote!(Vec<Entry>), parse_quote!(Vec<Entry, std::alloc::Global>)] {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
        let Fields::Named(fields) = &mut input.fields else { unreachable!() };
        fields.named[0].ty = ty;
        let Item::Fn(function) = file
            .items
            .iter()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
            unreachable!()
        };
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert!(super::Grouping::new(&file, expression, &bindings, &sources, "", &[]).is_some());
    }
}

#[test]
fn grouping_setup_requires_each_cursor_copy_and_output_evidence_step() {
    for fault in 0..11 {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        let Item::Fn(function) = file
            .items
            .iter_mut()
            .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap()
        else {
            unreachable!()
        };
        match fault {
            3 => function.block.stmts[1] = parse_quote!(let mut other = 0;),
            4 => function.sig.inputs[0] = parse_quote!(input: &other::Input),
            7 => function.block.stmts[1] = parse_quote!(cursor = 0;),
            _ => {}
        }
        let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        match fault {
            0 => *loop_.cond = parse_quote!(cursor <= input.entries.len()),
            1 => *loop_.cond = parse_quote!(cursor < input.entries[0].len()),
            2 => *loop_.cond = parse_quote!(cursor < factory().entries.len()),
            5 => loop_.body.stmts[0] = parse_quote!(let path = input.entries[cursor].path.iter();),
            6 => loop_.body.stmts[0] = parse_quote!(let path = input.other[cursor].path.clone();),
            9 => {
                let _removed = loop_.body.stmts.remove(6);
            }
            10 => loop_.body.stmts[6] = parse_quote!(files.push(Output { unknown: path, ids });),
            _ => {}
        }
        let expression = Expr::While(loop_.clone());
        if fault == 8 {
            function.block.stmts.drain(..2);
        }
        let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
        let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
        assert!(
            super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_none(),
            "setup fault {fault}"
        );
    }
}

#[test]
fn grouping_boundary_rejects_each_missing_receiver_and_predicate_shape() {
    for source in [
        "true",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).other(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.capacity(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(0), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry.path != path).map_or(input.entries.len(), 0)",
        "input.entries[cursor..].iter().other(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].other().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries.iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor].iter().position(|entry| entry.path != path).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| true).map_or(input.entries.len(), |offset| cursor * offset)",
        "input.entries[cursor..].iter().position(|entry| entry != path).map_or(input.entries.len(), |offset| cursor * offset)",
    ] {
        assert!(grouping().boundary(&parse_str(source).unwrap(), true).is_none(), "{source}");
    }
}

#[test]
fn grouping_proof_withholds_missing_boundary_locals_and_unsupported_body_statements() {
    let original = group_loop();
    for fault in 0..6 {
        let mut before = original.clone();
        let mut after = multiplied(&original);
        let Expr::While(before_loop) = &mut before else { unreachable!() };
        let Expr::While(after_loop) = &mut after else { unreachable!() };
        match fault {
            0 => before_loop.body.stmts.truncate(1),
            1 => before_loop.body.stmts[1] = parse_quote!(unknown();),
            2 => after_loop.body.stmts[1] = parse_quote!(unknown();),
            3 => before_loop.body.stmts[1] = parse_quote!(let end = 0;),
            4 => {
                before_loop.body.stmts.insert(
                    3,
                    parse_quote!(
                        const LOCAL: u8 = 0;
                    ),
                );
                after_loop.body.stmts.insert(
                    3,
                    parse_quote!(
                        const LOCAL: u8 = 0;
                    ),
                );
            }
            5 => after_loop.body.stmts.truncate(1),
            _ => unreachable!(),
        }
        assert!(!grouping().proves(&before, &after), "missing proof evidence {fault}");
    }
}

#[test]
fn grouping_helpers_reject_ambiguous_records_and_nonbinding_locals() {
    let grouped = Expr::Group(ExprGroup {
        attrs: Vec::new(),
        group_token: syn::token::Group::default(),
        expr: Box::new(parse_quote!(input)),
    });
    assert_eq!(super::name(&grouped).as_deref(), Some("input"));
    for statement in [
        "let (a, b) = pair;",
        "let ref value = input;",
        "#[cfg(test)] let value = input;",
        "let value @ _ = input;",
        "let value = input else { return; };",
    ] {
        assert!(super::local(&parse_str(statement).unwrap()).is_none(), "{statement}");
    }
    let file = parse_file("struct Record { value: u8 } struct Record { value: bool }").unwrap();
    assert!(super::record(&file, "Record").is_none());
    let mut record: ItemStruct = parse_quote!(
        struct Record {
            value: u8,
        }
    );
    let field = record.fields.iter().next().unwrap().clone();
    let Fields::Named(fields) = &mut record.fields else {
        unreachable!()
    };
    fields.named.extend(core::iter::repeat_n(field, 64));
    assert!(super::field(&record, "value").is_none());
    let mut expression: Expr = parse_quote!(input);
    for _ in 0..17 {
        expression = Expr::Paren(ExprParen {
            attrs: Vec::new(),
            paren_token: Paren::default(),
            expr: Box::new(expression),
        });
    }
    assert!(matches!(super::bare(&expression), Expr::Paren(_)));
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
        assert!(grouping().boundary(&parse_str(source).unwrap(), true).is_none(), "{source}");
    }
    let original = group_loop();
    assert!(!grouping().proves(&parse_quote!(true), &original));
    let mut before = original.clone();
    let Expr::While(loop_) = &mut before else { unreachable!() };
    loop_.body.stmts[3] = parse_quote!(let ids = group.iter().collect::<Vec<_>>(););
    assert!(!grouping().proves(&before, &multiplied(&before)));
    for statement in [
        "ids.dedup::<u8>();",
        "#[cfg(test)] ids.dedup();",
        "files.push(value);",
        "unknown();",
    ] {
        let mut before = original.clone();
        let Expr::While(loop_) = &mut before else { unreachable!() };
        loop_.body.stmts[4] = parse_str(statement).unwrap();
        assert!(!grouping().proves(&before, &multiplied(&before)), "{statement}");
    }
}

#[test]
fn grouping_setup_tolerates_nonbinding_setup_expressions_without_treating_them_as_outputs() {
    let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Fn(function) = file
        .items
        .iter_mut()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    function.block.stmts.insert(0, parse_quote!(setup();));
    let Stmt::Expr(expression, _) = &function.block.stmts[3] else {
        unreachable!()
    };
    let expression = expression.clone();
    let sources = crate::ops::collect::resource_namespaces::Namespaces::of(&file, &CfgSet::unconditional());
    let bindings = crate::HashMap::from_iter([("input.entries".into(), super::Binding::Vec)]);
    assert!(super::Grouping::new(&file, &expression, &bindings, &sources, "", &[]).is_some());
}

#[test]
fn captures_reject_unbound_closure_values_items_macros_attributes_and_budget_exhaustion() {
    let file = parse_file("fn safe() {}").unwrap();
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
        syn::visit::Visit::visit_expr(&mut captures, &parse_str(expression).unwrap());
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
    syn::visit::Visit::visit_expr(&mut captures, &parse_quote!(true));
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

fn group_loop() -> Expr {
    let file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
    let Item::Fn(function) = file
        .items
        .iter()
        .find(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
        .unwrap()
    else {
        unreachable!()
    };
    let Stmt::Expr(expression, _) = &function.block.stmts[2] else {
        unreachable!()
    };
    expression.clone()
}

#[test]
fn grouping_setup_rejects_invalid_parameter_field_and_empty_output_provenance() {
    for fault in 0..20 {
        let mut file = parse_file(include_str!("../fixtures/grouping.rs")).unwrap();
        let function_at = file
            .items
            .iter()
            .position(|item| matches!(item, Item::Fn(function) if function.sig.ident == "groups"))
            .unwrap();
        let Item::Fn(function) = &mut file.items[function_at] else {
            unreachable!()
        };
        match fault {
            0 => function.sig.inputs[0] = parse_quote!(input: Input),
            1 => function.sig.inputs[0] = parse_quote!(input: &mut Input),
            2 => function.block.stmts[1] = parse_quote!(let mut cursor = unknown();),
            3 => {
                function.block.stmts[0] = parse_quote!(
                    struct Local;
                );
            }
            12 => function.block.stmts.extend(core::iter::repeat_n(parse_quote!(after();), 64)),
            13 => function.sig.inputs[0] = parse_quote!((input, other): (&Input, u8)),
            14 => function.sig.inputs[0] = parse_quote!(input: &External),
            _ => {}
        }
        let Stmt::Expr(Expr::While(loop_), _) = &mut function.block.stmts[2] else {
            unreachable!()
        };
        match fault {
            4 => *loop_.cond = parse_quote!(cursor < input.0.len()),
            5 => loop_.body.stmts[0] = parse_quote!(let path = input.entries[cursor].0.clone();),
            6 => loop_.body.stmts[6] = parse_quote!(files.push(Output { 0: path, ids });),
            7 => loop_.body.stmts[6] = parse_quote!(files.push(external::Output { path, ids });),
            8 => loop_.body.stmts[0] = parse_quote!(let path = input.entries[cursor].unknown.clone();),
            9 => *loop_.cond = parse_quote!(cursor < input.entries.len()),
            10 => loop_.body.stmts[2] = parse_quote!(let group = &input.entries[cursor..end];),
            17 => loop_.body.stmts[0] = parse_quote!(let path = input.entries[other].path.clone();),
            18 => loop_.body.stmts[6] = parse_quote!(files.push(External { path, ids });),
            19 => loop_.body.stmts[3] = parse_quote!(let ids = group.iter().map(|entry| unknown).collect::<Vec<_>>();),
            _ => {}
        }
        let expression = Expr::While(loop_.clone());
        if fault == 9 {
            let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
            input.fields.iter_mut().next().unwrap().ty = parse_quote!(&[Entry]);
        }
        if fault == 10 {
            let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
            input.fields.iter_mut().next().unwrap().ty = parse_quote!(Vec<(Entry,)>);
        }
        if fault == 11 {
            let Item::Struct(output) = &mut file.items[2] else { unreachable!() };
            output.fields.iter_mut().next().unwrap().ty = parse_quote!(u32);
        }
        if matches!(fault, 15 | 16) {
            let Item::Struct(input) = &mut file.items[0] else { unreachable!() };
            input.fields.iter_mut().next().unwrap().ty = if fault == 15 {
                parse_quote!(Vec)
            } else {
                parse_quote!(Vec<External>)
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

fn multiplied(original: &Expr) -> Expr {
    let mut mutant = original.clone();
    let Expr::While(loop_) = &mut mutant else { unreachable!() };
    let Stmt::Local(local) = &mut loop_.body.stmts[1] else {
        unreachable!()
    };
    *local.init.as_mut().unwrap().expr = parse_quote!(
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
            proof.boundary(&parse_str(source).unwrap(), multiplication).is_some(),
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
        let Expr::While(loop_) = &mut before else { unreachable!() };
        match fault {
            0 => {
                let _removed = loop_.body.stmts.remove(6);
            }
            1 => {
                let _removed = loop_.body.stmts.pop();
            }
            2 => loop_.body.stmts[7] = parse_quote!(other = end;),
            3 => loop_.body.stmts[7] = parse_quote!(cursor = other;),
            4 => loop_.body.stmts.push(parse_quote!(side_effect();)),
            5 => loop_.body.stmts[6] = parse_quote!(other.push(Output { path, ids });),
            6 => loop_.body.stmts[6] = parse_quote!(files.push(Other { path, ids });),
            7 => loop_.body.stmts[6] = parse_quote!(files.push(Output { path, ids: other });),
            8 => loop_.body.stmts[6] = parse_quote!(files.push(Output { path: other, ids });),
            9 => loop_.body.stmts[2] = parse_quote!(let group = &input.entries[cursor..other];),
            10 => loop_.body.stmts[3] = parse_quote!(let ids = other.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
            11 => loop_.body.stmts[4] = parse_quote!(ids.clear();),
            12 => loop_.body.stmts.insert(
                6,
                parse_quote!(let ids = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
            ),
            13 => loop_.body.stmts[3] = parse_quote!(let ids = group.collect::<Vec<_>>();),
            _ => unreachable!(),
        }
        let after = multiplied(&before);
        assert!(!grouping().proves(&before, &after), "invalid recurrence {fault}");
    }
    let mut after = mutant;
    let Expr::While(loop_) = &mut after else { unreachable!() };
    *loop_.cond = parse_quote!(cursor <= input.entries.len());
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
        let Expr::While(loop_) = &mut before else {
            panic!("the checked-in grouping fixture contains a while loop");
        };
        loop_.body.stmts[at] = parse_str(statement).expect("the fixed recurrence statement is valid Rust syntax");
        assert_eq!(grouping().proves(&before, &multiplied(&before)), expected, "{statement}");
    }
    let mut before = group_loop();
    let Expr::While(loop_) = &mut before else {
        panic!("the checked-in grouping fixture contains a while loop");
    };
    loop_.body.stmts.insert(
        4,
        parse_quote!(let unused = group.iter().map(|entry| entry.ordinal).collect::<Vec<_>>();),
    );
    assert!(
        !grouping().proves(&before, &multiplied(&before)),
        "every empty collection must be consumed by an output field"
    );
}

#[test]
fn grouping_capture_scopes_restore_nested_closure_and_block_bindings() {
    let file = parse_file("fn known() {}").expect("the fixed helper declaration is valid Rust syntax");
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
            &parse_str(expression).expect("the fixed closure is valid Rust syntax"),
        );
        assert_eq!(captures.valid, expected, "{expression}");
        assert_eq!(captures.bound, crate::HashSet::default(), "{expression}");
        assert_eq!(captures.allowed, crate::HashSet::from_iter(["outside".into()]), "{expression}");
    }
}
