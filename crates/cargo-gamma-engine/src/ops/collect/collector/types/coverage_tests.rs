// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::punctuated::Punctuated;
use syn::token::Group;
use syn::{TypeArray, TypeReference, parse_file, parse_quote, parse_str};

use super::*;
use crate::cfg::CfgSet;

#[test]
fn block_type_negative_evidence_checks_the_qualified_root_not_the_terminal_name() {
    with_types("mod child { pub struct Missing; }", |types| {
        let ty = parse_quote!(child::Missing);
        assert!(types.package_lacks_default(&ty));
        let mut imports = types.imports.clone();
        imports.insert(BLOCK_TYPES.into(), Some(vec!["child".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert!(!scoped.package_lacks_default(&ty));
    });
}

#[test]
fn standard_options_prove_defaults_without_requiring_defaultable_payloads() {
    with_types("", |types| {
        assert!(!types.has_proven_default(&parse_quote!(std::fs::File)));
        for source in ["Option<std::fs::File>", "core::option::Option<std::fs::File>"] {
            let ty = parse_str(source).unwrap();
            assert!(!types.package_declares_default(&ty));
            assert!(!types.local_generic_default(&ty));
            assert!(types.has_proven_default(&ty), "{source}");
        }
    });
}

#[test]
fn local_options_require_complete_proven_default_templates() {
    let imports = HashMap::from_iter([(LOCAL_MODULE.into(), Some(Vec::new()))]);
    for (source, complete_expected) in [("struct Option<T>(T);", false), ("#[derive(Default)] struct Option<T>(T);", true)] {
        let ast = parse_file(source).unwrap();
        for (defaults, expected) in [
            (Defaults::of(&ast), complete_expected),
            (Defaults::optimistic_of_in(&ast, &CfgSet::unconditional()), false),
        ] {
            let types = Types {
                abstracts: &[],
                defaulted: &[],
                imports: &imports,
                defaults: &defaults,
                aliases: None,
                self_type: None,
                self_associated: None,
            };
            assert_eq!(types.has_proven_default(&parse_quote!(Option<bool>)), expected, "{source}");
            assert!(!types.has_proven_default(&parse_quote!(Option<std::fs::File>)), "{source}");
        }
    }
}

#[test]
fn qualified_unsized_string_types_supply_their_own_negative_default_evidence() {
    with_types("", |types| {
        for source in ["str", "core::primitive::str", "std::primitive::str"] {
            let ty = parse_str(source).unwrap();
            assert!(types.is_unsized_str(&ty), "{source}");
            assert!(types.lacks_default(&ty), "{source}");
        }
    });
}

#[test]
fn wrappers_only_inherit_negative_default_evidence_from_nondefault_payloads() {
    with_types("", |types| {
        for source in ["Box<bool>", "Rc<u8>", "Arc<u16>"] {
            let ty = parse_str(source).unwrap();
            assert!(types.supports_wrapper_api(&ty), "{source}");
            assert!(!types.lacks_default(&ty), "{source}");
        }
        assert!(types.lacks_default(&parse_quote!(Box<std::fs::File>)));
    });
}

#[test]
fn nested_nongeneric_option_names_keep_optimistic_default_evidence_for_standard_options() {
    with_types("mod unrelated { #[derive(Default)] struct Option; }", |types| {
        let ty = parse_quote!(Option<u8>);
        assert_eq!(types.value_kind(&ty), Kind::Option);
        assert!(!types.supports_option_api(&ty));
        assert!(types.package_has_default(&ty));
        assert!(!types.local_generic_default(&ty));
        assert!(types.has_default(&ty));
    });
}

#[test]
fn generic_local_wrappers_use_their_default_template_before_standard_wrapper_admission() {
    with_types("#[derive(Default)] struct Box<T>(T);", |types| {
        let ty = parse_quote!(Box<u16>);
        assert_eq!(types.value_kind(&ty), Kind::Wrapper);
        assert!(!types.package_declares_default(&ty));
        assert!(types.local_generic_default(&ty));
        assert!(!types.supports_wrapper_api(&ty));
        assert!(types.has_proven_default(&ty));
        assert!(!types.has_proven_default(&parse_quote!(Box<std::fs::File>)));
    });
}

#[test]
fn qualified_self_associated_types_never_resolve_through_same_named_lexical_aliases() {
    with_types("type Alias = Vec<u8>;", |types| {
        let aliases = HashMap::from_iter([(
            "Alias".into(),
            Some(Alias {
                parameters: Vec::new(),
                target: parse_quote!(Vec<u8>),
            }),
        )]);
        let types = Types {
            aliases: Some(&aliases),
            ..*types
        };
        for source in ["<Owner>::Alias", "<T>::Alias", "(<T>::Alias)", "<T as Trait>::Alias"] {
            let ty = parse_str(source).unwrap();
            assert!(
                core::ptr::eq(core::ptr::from_ref(types.resolve_alias(&ty)), core::ptr::from_ref(&ty)),
                "{source}"
            );
        }
        assert_eq!(types.resolve_alias(&parse_quote!(Alias)), &parse_quote!(Vec<u8>));
    });
}

#[test]
fn repeated_generic_alias_instantiations_do_not_capture_a_root_alias_named_after_the_parameter() {
    with_types("type T = Vec<u8>; type Id<T> = T;", |types| {
        let aliases = HashMap::from_iter([
            (
                "T".into(),
                Some(Alias {
                    parameters: Vec::new(),
                    target: parse_quote!(Vec<u8>),
                }),
            ),
            (
                "Id".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(T),
                }),
            ),
            (
                "Cycle".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(Cycle<T>),
                }),
            ),
            (
                "Grow".into(),
                Some(Alias {
                    parameters: vec!["T".into()],
                    target: parse_quote!(Grow<Option<T>>),
                }),
            ),
        ]);
        let types = Types {
            aliases: Some(&aliases),
            ..*types
        };
        for ty in [parse_quote!(Id<bool>), parse_quote!(Id<Id<bool>>), parse_quote!(Id<Id<Id<bool>>>)] {
            assert_eq!(types.instantiate_alias(&ty), Some(parse_quote!(bool)));
            assert_eq!(types.resolve_alias(&ty), &ty);
            let values = super::super::values::values_for(&ty, 3, &types);
            assert_eq!(
                values.iter().map(|(_, text, _)| text.as_str()).collect::<Vec<_>>(),
                ["true", "false"]
            );
        }
        assert_eq!(types.instantiate_alias(&parse_quote!(Cycle<bool>)), None);
        assert_eq!(types.instantiate_alias(&parse_quote!(Grow<bool>)), Some(parse_quote!(_)));
    });
}

#[test]
fn renamed_option_and_result_types_without_payloads_keep_their_unknown_syntactic_kind() {
    with_types("", |types| {
        let imports = HashMap::from_iter([
            ("Maybe".into(), Some(vec!["std".into(), "option".into(), "Option".into()])),
            ("Outcome".into(), Some(vec!["std".into(), "io".into(), "Result".into()])),
        ]);
        let types = Types {
            imports: &imports,
            ..*types
        };
        for ty in [parse_quote!(Maybe), parse_quote!(Outcome)] {
            assert_eq!(types.value_kind(&ty), Kind::Unknown);
        }
        assert_eq!(types.value_kind(&parse_quote!(Maybe<u8>)), Kind::Option);
        assert_eq!(types.value_kind(&parse_quote!(Outcome<u8>)), Kind::Result);
    });
}

#[test]
fn campaign_type_qualification_does_not_freeze_generic_or_block_shadowed_roots_as_primitives() {
    with_types("struct Local;", |types| {
        let abstracts = ["T".into()];
        let defaulted = ["D".into()];
        let mut imports = types.imports.clone();
        imports.insert(BLOCK_TYPES.into(), Some(vec!["Local".into()]));
        let scoped = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            imports: &imports,
            ..*types
        };
        assert_eq!(
            scoped.source_type(&parse_quote!((T::Item, D::Item, Local, u16))),
            parse_quote!((_, _, _, u16))
        );
        assert_eq!(
            scoped.canonical_type(&parse_quote!((T::Item, D::Item, Local, u16))),
            parse_quote!((_, _, _, ::core::primitive::u16))
        );
        assert_eq!(
            scoped.source_type(&parse_quote!(<T as Trait>::Item)),
            parse_quote!(<T as Trait>::Item)
        );
    });
}

#[test]
fn campaign_type_freezing_distinguishes_declaration_aliases_from_expanded_evidence() {
    with_types("struct Local;", |types| {
        let aliases = HashMap::from_iter([(
            "Pair".into(),
            Some(Alias {
                parameters: vec!["T".into()],
                target: parse_quote!((T, Local)),
            }),
        )]);
        let types = Types {
            aliases: Some(&aliases),
            ..*types
        };
        let ty = parse_quote!(&Pair<u8>);
        assert_eq!(types.source_type(&ty), parse_quote!(&(u8, crate::Local)));
        assert_eq!(types.canonical_type(&ty), parse_quote!(&(::core::primitive::u8, crate::Local)));
        assert_eq!(types.declaration_type(&ty), parse_quote!(&Pair<::core::primitive::u8>));
        assert_eq!(
            types.canonical_type(&parse_quote!((std::primitive::u8, core::primitive::u16))),
            parse_quote!((::core::primitive::u8, ::core::primitive::u16))
        );
    });
}

#[test]
fn campaign_source_type_qualification_shares_its_work_limit_across_tuple_siblings() {
    with_types("struct Local;", |types| {
        let within = Type::Tuple(syn::TypeTuple {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            elems: core::iter::repeat_n::<Type>(parse_quote!(Local), 63).collect(),
        });
        let expected = Type::Tuple(syn::TypeTuple {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            elems: core::iter::repeat_n::<Type>(parse_quote!(crate::Local), 63).collect(),
        });
        assert_eq!(types.source_type(&within), expected);
        let Type::Tuple(mut over) = within else { unreachable!() };
        over.elems.push(parse_quote!(Local));
        let Type::Tuple(mut expected_over) = expected else { unreachable!() };
        expected_over.elems.push(parse_quote!(_));
        assert_eq!(types.source_type(&Type::Tuple(over)), Type::Tuple(expected_over));
    });
}

#[test]
fn parent_module_qualification_retains_negative_default_evidence() {
    let defaults = Defaults::of(&parse_file("struct Missing; mod child {}").unwrap());
    let imports = HashMap::from_iter([(LOCAL_MODULE.into(), Some(vec!["child".into()]))]);
    let types = Types {
        abstracts: &[],
        defaulted: &[],
        imports: &imports,
        defaults: &defaults,
        aliases: None,
        self_type: None,
        self_associated: None,
    };
    assert!(types.package_lacks_default(&parse_quote!(super::Missing)));
    assert!(!types.package_lacks_default(&parse_quote!(super::super::Missing)));
}

#[test]
fn malformed_or_ambiguous_type_paths_fail_closed_without_panicking() {
    let empty = Type::Path(syn::TypePath {
        attrs: Vec::new(),
        qself: None,
        path: syn::Path {
            leading_colon: None,
            segments: Punctuated::default(),
        },
    });
    with_types("", |types| {
        assert_eq!(types.source_type(&empty), parse_quote!(_));
        assert!(!types.supports_collection_constructor(&empty));
        assert!(!types.supports_collection_constructor(&parse_quote!(Vec)));
        assert!(!types.supports_collection_api(&empty));
        assert!(!types.supports_nonzero_api(&empty));
    });
    let imports = HashMap::from_iter([("Ambiguous".into(), None), ("std".into(), None)]);
    let defaults = Defaults::default();
    let types = Types {
        abstracts: &[],
        defaulted: &[],
        imports: &imports,
        defaults: &defaults,
        aliases: None,
        self_type: None,
        self_associated: None,
    };
    assert_eq!(types.source_type(&parse_quote!(Ambiguous)), parse_quote!(_));
    assert!(!types.standard_constructor_root(&parse_quote!(std::vec::Vec<u8>)));
    with_types(
        "#[derive(Default)] struct Vec<T>(T); #[derive(Default)] struct HashMap<K, V>(K, V);",
        |types| {
            assert!(types.has_default(&parse_quote!(Vec<u8>)));
            assert!(types.has_default(&parse_quote!(HashMap<u8, bool>)));
        },
    );
}

fn with_types(source: &str, check: impl FnOnce(&Types<'_>)) {
    let defaults = Defaults::of(&parse_file(source).unwrap());
    let imports = HashMap::from_iter([(LOCAL_MODULE.into(), Some(Vec::new()))]);
    check(&Types {
        abstracts: &[],
        defaulted: &[],
        imports: &imports,
        defaults: &defaults,
        aliases: None,
        self_type: None,
        self_associated: None,
    });
}

#[test]
fn syntax_work_limits_charge_exact_type_expression_and_attribute_costs() {
    // Work counts each type, path segment, expression, and attribute visit, not punctuation.
    // Thus a primitive costs type + segment; an array adds its type and each length expression.
    // Substitution reserves both the original input and expanded output, even without changes.
    for (source, size) in [
        ("u8", 2),
        ("::std::primitive::u8", 4),
        ("&[u8]", 4),
        ("[u8; 1]", 4),
        ("[u8; 1 + 2]", 6),
        ("(u8, bool)", 5),
        ("Vec<Option<u8>>", 6),
    ] {
        let ty: Type = parse_str(source).unwrap();
        assert_eq!(type_size(&ty, None, size), Some(size), "{source}");
        assert_eq!(type_size(&ty, None, size - 1), None, "{source}");
        let mut remaining = size + 1;
        assert!(reserve_type(&ty, &mut remaining), "{source}");
        assert_eq!(remaining, 1, "{source}");
        assert!(!reserve_type(&ty, &mut remaining), "{source}");
        assert_eq!(remaining, 1, "{source}");
    }
    let mut ty: Type = parse_quote!([u8; 1]);
    let Type::Array(array) = &mut ty else { unreachable!() };
    array.attrs.push(parse_quote!(#[allow(dead_code)]));
    assert_eq!(type_size(&ty, None, 6), Some(6));
    assert_eq!(type_size(&ty, None, 5), None);
    assert_eq!(substitute_type_with_budget(&ty, &HashMap::default(), &mut 11), parse_quote!(_));
    let mut remaining = 12;
    assert_eq!(substitute_type_with_budget(&ty, &HashMap::default(), &mut remaining), ty);
    assert_eq!(remaining, 0);
}

#[test]
fn syntax_depth_is_bounded_separately_from_available_work() {
    let mut ty: Type = parse_quote!(u8);
    for _ in 0..62 {
        ty = Type::Reference(TypeReference {
            attrs: Vec::new(),
            and_token: syn::token::And::default(),
            lifetime: None,
            mutability: None,
            elem: Box::new(ty),
        });
    }
    assert_eq!(type_size(&ty, None, TYPE_EXPANSION_LIMIT), Some(64));
    ty = Type::Reference(TypeReference {
        attrs: Vec::new(),
        and_token: syn::token::And::default(),
        lifetime: None,
        mutability: None,
        elem: Box::new(ty),
    });
    assert_eq!(type_size(&ty, None, TYPE_EXPANSION_LIMIT), None);
}

#[test]
fn syntax_budget_restores_substitution_and_sibling_depth_after_expressions_and_attributes() {
    // Replacement syntax is charged instead of the parameter's type/path visits and is not
    // substituted recursively. Sibling depth is restored; each attribute adds its own visit
    // plus its metadata path segment, and every call argument adds an expression visit.
    let substitutions = HashMap::from_iter([("T".into(), parse_quote!(Vec<u8>))]);
    let ty: Type = parse_quote!((T, T));
    assert_eq!(type_size(&ty, Some(&substitutions), 9), Some(9));
    assert_eq!(type_size(&ty, Some(&substitutions), 8), None);
    assert_eq!(
        substitute_type_with_budget(&ty, &substitutions, &mut 14),
        parse_quote!((Vec<u8>, Vec<u8>))
    );
    assert_eq!(substitute_type_with_budget(&ty, &substitutions, &mut 13), parse_quote!(_));
    let mut ty: Type = parse_quote!(u8);
    let Type::Path(path) = &mut ty else { unreachable!() };
    path.attrs = vec![parse_quote!(#[allow(dead_code)]); 100];
    assert_eq!(type_size(&ty, None, 202), Some(202));
    let mut ty: Type = parse_quote!([u8; sum()]);
    let Type::Array(array) = &mut ty else { unreachable!() };
    let syn::Expr::Call(call) = &mut array.len else { unreachable!() };
    call.args.extend(core::iter::repeat_n::<syn::Expr>(parse_quote!(1), 100));
    assert_eq!(type_size(&ty, None, 106), Some(106));
}

#[test]
fn every_collection_key_container_spends_one_depth_level_before_accepting_its_payload() {
    with_types("", |types| {
        for shape in ["option", "result", "tuple", "array", "wrapper"] {
            let mut ty: Type = parse_quote!(bool);
            for depth in 0..12 {
                assert!(!types.collection_key_proven(&ty, depth), "{shape}: {depth}");
                assert!(types.collection_key_proven(&ty, depth + 1), "{shape}: {depth}");
                ty = match shape {
                    "option" => parse_quote!(Option<#ty>),
                    "result" => parse_quote!(Result<#ty, #ty>),
                    "tuple" => parse_quote!((#ty,)),
                    "array" => parse_quote!([#ty; 1]),
                    "wrapper" => parse_quote!(Box<#ty>),
                    _ => unreachable!(),
                };
            }
        }
        for source in [
            "custom::Option<bool>",
            "custom::Result<bool, bool>",
            "custom::Box<bool>",
            "Result<Unknown, bool>",
            "Result<bool, Unknown>",
        ] {
            assert!(!types.collection_key_proven(&parse_str(source).unwrap(), 64), "{source}");
        }
    });
}

#[test]
fn collection_element_proofs_keep_the_exact_default_depth_of_sixty_four() {
    with_types("", |types| {
        let mut element: Type = parse_quote!(bool);
        for depth in 1..=64 {
            let mut array: TypeArray = parse_quote!([bool; 1]);
            array.elem = Box::new(element);
            element = Type::Array(array);
            if depth >= 62 {
                let mut set: Type = parse_quote!(std::collections::HashSet<bool>);
                let Type::Path(path) = &mut set else { unreachable!() };
                let syn::PathArguments::AngleBracketed(arguments) = &mut path.path.segments.last_mut().unwrap().arguments else {
                    unreachable!()
                };
                *arguments.args.first_mut().unwrap() = syn::GenericArgument::Type(element.clone());
                assert_eq!(types.collection_elements_proven(&set), depth < 64, "{depth}");
            }
        }
    });
}

#[test]
fn iterator_and_character_proofs_preserve_mutability_and_exact_standard_families() {
    with_types("", |types| {
        for (source, expected) in [
            ("std::iter::Peekable<core::str::Chars<'static>>", true),
            ("&mut std::iter::Peekable<std::str::Chars<'static>>", true),
            ("&std::iter::Peekable<std::str::Chars<'static>>", false),
            ("std::iter::Peekable<other::str::Chars<'static>>", false),
            ("other::iter::Peekable<std::str::Chars<'static>>", false),
            ("std::iter::Other<std::str::Chars<'static>>", false),
            ("std::iter::Peekable<std::str::Other<'static>>", false),
            ("std::iter::Peekable<std::str::Chars<'static>, bool>", false),
            ("std::iter::Peekable", false),
            ("bool", false),
        ] {
            assert_eq!(types.supports_peekable_chars(&parse_str(source).unwrap()), expected, "{source}");
        }
        for (source, expected) in [
            ("T", true),
            ("&mut T", true),
            ("&T", false),
            ("U", false),
            ("impl Iterator<Item = u8>", true),
            ("impl core::iter::ExactSizeIterator<Item = u8>", true),
            ("impl custom::Iterator<Item = u8>", false),
            ("impl 'static + custom::Iterator<Item = u8>", false),
            ("dyn Iterator<Item = u8>", false),
        ] {
            assert_eq!(
                types.supports_iterator_trait(&parse_str(source).unwrap(), &["T".into()]),
                expected,
                "{source}"
            );
        }
        for (source, expected) in [
            ("std::ops::Range<u8>", true),
            ("core::ops::Range<i16>", true),
            ("std::ops::Range<bool>", false),
            ("std::ops::Range<Custom>", false),
            ("std::ops::Range<u8, u16>", false),
            ("std::ops::Range", false),
            ("custom::ops::Range<u8>", false),
        ] {
            assert_eq!(types.supports_range_iterator(&parse_str(source).unwrap()), expected, "{source}");
        }
    });
}

#[test]
fn empty_constructor_equivalence_excludes_hashers_allocators_and_nonstandard_roots() {
    with_types("", |types| {
        for (source, expected) in [
            ("String", true),
            ("std::string::String", true),
            ("String<u8>", false),
            ("Vec", true),
            ("Vec<u8>", true),
            ("Vec<u8, Allocator>", false),
            ("std::collections::VecDeque<u8>", true),
            ("alloc::collections::LinkedList<u8>", true),
            ("std::collections::BinaryHeap<u8>", true),
            ("std::collections::BTreeMap<u8, bool>", true),
            ("std::collections::BTreeMap<u8, bool, Allocator>", false),
            ("std::collections::BTreeSet<u8>", true),
            ("std::collections::HashMap<u8, bool>", false),
            ("std::collections::HashSet<u8>", false),
            ("VecDeque<u8>", false),
            ("custom::vec::Vec<u8>", false),
            ("std::vec::Other<u8>", false),
            ("&Vec<u8>", false),
        ] {
            assert_eq!(
                types.supports_empty_default_constructor(&parse_str(source).unwrap()),
                expected,
                "{source}"
            );
        }
    });
}

#[test]
fn standard_type_proofs_distinguish_nonzero_wrappers_hashers_and_reference_shapes() {
    with_types("", |types| {
        for name in [
            "NonZeroI8",
            "NonZeroI16",
            "NonZeroI32",
            "NonZeroI64",
            "NonZeroI128",
            "NonZeroIsize",
            "NonZeroU8",
            "NonZeroU16",
            "NonZeroU32",
            "NonZeroU64",
            "NonZeroU128",
            "NonZeroUsize",
        ] {
            for root in ["std", "core"] {
                assert!(types.supports_nonzero_api(&parse_str(&format!("{root}::num::{name}")).unwrap()));
            }
        }
        for source in [
            "custom::num::NonZeroU8",
            "std::other::NonZeroU8",
            "std::num::Other",
            "&std::num::NonZeroU8",
        ] {
            assert!(!types.supports_nonzero_api(&parse_str(source).unwrap()), "{source}");
        }
        for (source, expected) in [
            ("Box<u8>", true),
            ("std::boxed::Box<u8>", true),
            ("alloc::rc::Rc<u8>", true),
            ("std::sync::Arc<u8>", true),
            ("std::rc::Arc<u8>", false),
            ("core::boxed::Box<u8>", false),
            ("other::sync::Arc<u8>", false),
        ] {
            assert_eq!(types.supports_wrapper_api(&parse_str(source).unwrap()), expected, "{source}");
        }
        for (source, expected) in [
            ("HashMap<u8, bool>", Some(2)),
            ("std::collections::HashSet<u8>", Some(1)),
            ("Vec<u8>", None),
            ("custom::HashMap<u8, bool>", None),
            ("&HashMap<u8, bool>", None),
        ] {
            assert_eq!(types.collection_hasher_index(&parse_str(source).unwrap()), expected, "{source}");
        }
        for (source, expected) in [
            ("&str", true),
            ("&core::primitive::str", true),
            ("&std::primitive::str", true),
            ("str", false),
            ("&String", false),
            ("&other::primitive::str", false),
            ("&std::other::str", false),
        ] {
            assert_eq!(types.supports_str_reference(&parse_str(source).unwrap()), expected, "{source}");
        }
    });
}

#[test]
fn source_paths_require_source_metadata_and_do_not_guess_under_wildcards_or_block_shadowing() {
    with_types("", |types| {
        assert_eq!(types.source_path(&parse_quote!(Root)), None);
        assert!(!types.source_same_type(&parse_quote!(Root), &parse_quote!(Root)));
        let mut imports = types.imports.clone();
        for metadata in [None, Some(Vec::new())] {
            imports.insert(SOURCE_FILE.into(), metadata);
            assert_eq!(
                (Types {
                    imports: &imports,
                    ..*types
                })
                .source_path(&parse_quote!(Root)),
                None
            );
        }
        imports.insert(SOURCE_FILE.into(), Some(vec!["source.rs".into(), "not-the-source.rs".into()]));
        imports.insert(LOCAL_MODULE.into(), Some(vec!["inner".into(), "nested".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(
            scoped.source_path(&parse_quote!(Root)),
            Some(("source.rs", &["inner".into(), "nested".into()][..], vec!["Root".into()]))
        );
        assert_eq!(scoped.source_path(&parse_quote!(::Root)), None);
        imports.insert(BLOCK_TYPES.into(), Some(vec!["Root".into()]));
        assert_eq!(
            (Types {
                imports: &imports,
                ..*types
            })
            .source_path(&parse_quote!(Root)),
            None
        );
        imports.remove(BLOCK_TYPES);
        imports.insert("*".into(), None);
        assert_eq!(
            (Types {
                imports: &imports,
                ..*types
            })
            .source_path(&parse_quote!(Root)),
            None
        );
    });
}

#[test]
fn self_type_resolution_requires_exact_self_or_one_associated_segment() {
    with_types("", |types| {
        let concrete: Type = parse_quote!(u16);
        let associated = HashMap::from_iter([("Value".into(), parse_quote!(bool))]);
        let scoped = Types {
            self_type: Some(&concrete),
            self_associated: Some(&associated),
            ..*types
        };
        for (source, expected) in [
            ("Self", Some(parse_quote!(u16))),
            ("Self::Value", Some(parse_quote!(bool))),
            ("Self::Unknown", None),
            ("Self::Value::Nested", None),
            ("Other", None),
            ("Other::Value", None),
            ("&Self", None),
            ("<Self as Trait>::Value", None),
            ("<Owner>::Self", None),
        ] {
            assert_eq!(
                scoped.concrete_self_type_or_associated(&parse_str(source).unwrap()).cloned(),
                expected,
                "{source}"
            );
        }
    });
}

#[test]
fn constructor_argument_count_rejects_non_type_arguments_and_nonterminal_generics() {
    for (source, expected) in [
        ("Vec", Some(0)),
        ("Vec<u8>", Some(1)),
        ("std::collections::BTreeMap<u8, bool>", Some(2)),
        ("Outer<u8>::Inner", None),
        ("Array<3>", None),
        ("Borrowed<'static>", None),
        ("Iterator<Item = u8>", None),
        ("Mixed<u8, 3>", None),
        ("Mixed<'static, u8>", None),
    ] {
        assert_eq!(constructor_argument_count(&parse_str(source).unwrap()), expected, "{source}");
    }
    assert_eq!(
        constructor_argument_count(&Path {
            leading_colon: None,
            segments: syn::punctuated::Punctuated::new()
        }),
        None
    );
}

#[test]
fn standard_collection_paths_keep_module_identity_constructor_arity_and_key_requirements() {
    with_types("", |types| {
        for (name, module, arguments, constructor, keys) in [
            ("Vec", "vec", "u8", true, true),
            ("VecDeque", "collections::vec_deque", "u8", true, true),
            ("LinkedList", "collections::linked_list", "u8", true, true),
            ("BinaryHeap", "collections::binary_heap", "u8", true, true),
            ("BTreeSet", "collections::btree_set", "u8", true, true),
            ("HashSet", "collections::hash_set", "u8", true, true),
            ("HashMap", "collections::hash_map", "u8, bool", true, true),
            ("BTreeMap", "collections::btree_map", "u8, bool", true, true),
            ("HashSet", "collections::hash_set", "Custom", true, false),
            ("HashMap", "collections::hash_map", "Custom, bool", true, false),
            ("HashMap", "collections::hash_map", "u8, Custom", true, true),
            ("HashSet", "collections::hash_set", "u8, Hasher", false, true),
            ("HashMap", "collections::hash_map", "u8, bool, Hasher", false, true),
        ] {
            for root in ["std", "alloc"] {
                let spelling = format!("{root}::{module}::{name}<{arguments}>");
                let ty = parse_str(&spelling).unwrap();
                assert!(types.supports_collection_api(&ty), "{spelling}");
                assert_eq!(types.supports_collection_constructor(&ty), constructor, "{spelling}");
                assert_eq!(types.collection_elements_proven(&ty), keys, "{spelling}");
            }
        }
        for spelling in [
            "custom::collections::HashMap<u8, bool>",
            "custom::collections::hash_map::HashMap<u8, bool>",
            "std::collections::hash_set::HashMap<u8, bool>",
            "std::other::hash_map::HashMap<u8, bool>",
            "alloc::collections::vec_deque::BTreeMap<u8, bool>",
            "std::collections::hash_map::Other<u8, bool>",
            "std::other::HashMap<u8, bool>",
            "std::vec::Other<u8>",
            "std::collections::Other<u8>",
            "rustc_hash::Other<u8>",
        ] {
            assert!(!types.supports_collection_api(&parse_str(spelling).unwrap()), "{spelling}");
        }
        for spelling in ["HashMap<u8, bool, Extra>", "Vec", "Vec<u8, Extra>"] {
            assert!(!types.supports_collection_constructor(&parse_str(spelling).unwrap()), "{spelling}");
        }
    });
}

#[test]
fn direct_unshadowed_path_queries_keep_each_ambiguity_guard_and_do_not_confuse_none_with_empty_paths() {
    with_types("struct Local;", |types| {
        let abstracts = ["Abstract".into()];
        let defaulted = ["Defaulted".into()];
        let scoped = Types {
            abstracts: &abstracts,
            defaulted: &defaulted,
            ..*types
        };
        for spelling in ["Abstract", "Abstract::Item", "Defaulted", "Defaulted::Item", "Local"] {
            assert_eq!(scoped.unshadowed_type_path(&parse_str(spelling).unwrap()), None, "{spelling}");
        }
        assert_eq!(
            scoped.unshadowed_type_path(&parse_quote!(Local::Item)),
            Some(vec!["Local".into(), "Item".into()])
        );
        let mut imports = types.imports.clone();
        imports.insert("*".into(), None);
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.unshadowed_type_path(&parse_quote!(Root)), None);
        imports.remove("*");
        imports.insert("Root".into(), Some(vec![ABSOLUTE_ROOT.into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.unshadowed_type_path(&parse_quote!(Root)), None);
        imports.insert("std".into(), Some(vec![ABSOLUTE_ROOT.into(), "std".into(), "collections".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.unshadowed_type_path(&parse_quote!(std::VecDeque<u8>)), None);
        assert!(!scoped.supports_empty_default_constructor(&parse_quote!(std::VecDeque<u8>)));
        imports.insert("other".into(), Some(vec![ABSOLUTE_ROOT.into(), "other".into(), "nested".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert!(scoped.standard_constructor_root(&parse_quote!(other::Value)));
    });
}

#[test]
fn absolute_import_resolution_stops_before_a_shadowed_root_can_rebind_the_absolute_path() {
    let imports = HashMap::from_iter([
        (
            "Alias".into(),
            Some(vec![ABSOLUTE_ROOT.into(), "std".into(), "vec".into(), "Vec".into()]),
        ),
        ("std".into(), None),
    ]);
    assert_eq!(
        resolved_type_path(&parse_quote!(Alias<u8>), &imports),
        Some(vec!["std".into(), "vec".into(), "Vec".into()])
    );
}

#[test]
fn known_nondefault_standard_types_and_defaultable_referents_keep_distinct_negative_evidence() {
    with_types("", |types| {
        for source in [
            "std::time::Instant",
            "std::time::SystemTime",
            "core::cmp::Ordering",
            "std::process::Command",
            "std::process::Stdio",
            "std::process::Output",
            "std::process::Child",
            "std::fs::File",
            "std::sync::MutexGuard<'static, u8>",
            "std::sync::RwLockReadGuard<'static, u8>",
            "std::sync::RwLockWriteGuard<'static, u8>",
            "std::sync::Barrier",
            "std::thread::JoinHandle<u8>",
            "std::sync::mpsc::Sender<u8>",
            "std::sync::mpsc::SyncSender<u8>",
            "std::sync::mpsc::Receiver<u8>",
        ] {
            assert!(types.lacks_default(&parse_str(source).unwrap()), "{source}");
        }
        for source in ["&bool", "&u8", "&str", "&core::primitive::str", "&std::primitive::str"] {
            assert!(!types.lacks_default(&parse_str(source).unwrap()), "{source}");
        }
    });
}

#[test]
fn alias_proofs_do_not_replace_abstract_defaulted_or_imported_names_with_same_named_local_aliases() {
    with_types("", |types| {
        let aliases = HashMap::from_iter([(
            "T".into(),
            Some(Alias {
                parameters: Vec::new(),
                target: parse_quote!(bool),
            }),
        )]);
        let ty = parse_quote!(T);
        let scoped = Types {
            aliases: Some(&aliases),
            ..*types
        };
        assert_eq!(scoped.instantiate_alias(&ty), Some(parse_quote!(bool)));
        assert_eq!(*scoped.resolve_alias(&ty), parse_quote!(bool));
        let names = ["T".into()];
        for (abstracts, defaulted) in [(&names[..], &[][..]), (&[][..], &names[..])] {
            let scoped = Types {
                abstracts,
                defaulted,
                ..scoped
            };
            assert_eq!(scoped.instantiate_alias(&ty), None);
            assert_eq!(scoped.resolve_alias(&ty), &ty);
        }
        for imported in [None, Some(vec!["other".into(), "T".into()])] {
            let mut imports = types.imports.clone();
            imports.insert("T".into(), imported);
            let scoped = Types {
                imports: &imports,
                ..scoped
            };
            assert_eq!(scoped.instantiate_alias(&ty), None);
            assert_eq!(scoped.resolve_alias(&ty), &ty);
        }
    });
}

#[test]
fn alias_and_qualification_budget_failures_remain_explicit_unknown_types_rather_than_absent_aliases() {
    with_types("", |types| {
        let aliases = HashMap::default();
        let scoped = Types {
            aliases: Some(&aliases),
            ..*types
        };
        for available in [0, 3] {
            let mut budget = available;
            assert_eq!(
                scoped.instantiate_alias_with_budget(&parse_quote!(Vec<bool>), &mut budget),
                Some(parse_quote!(_))
            );
            assert_eq!(budget, available);
        }
        assert_eq!(
            types.source_type_at(&parse_quote!(bool), &mut 64, false, true, &mut 0),
            parse_quote!(_)
        );
    });
}

#[test]
fn primitive_qualification_keeps_local_declarations_alias_ambiguity_imports_and_wildcard_modes_separate() {
    with_types("struct u8;", |types| {
        assert_eq!(types.canonical_type(&parse_quote!(u8)), parse_quote!(crate::u8));
    });
    with_types("struct Local;", |types| {
        let aliases = HashMap::from_iter([("u8".into(), None)]);
        let scoped = Types {
            aliases: Some(&aliases),
            ..*types
        };
        assert_eq!(scoped.canonical_type(&parse_quote!(u8)), parse_quote!(crate::u8));
        let mut imports = types.imports.clone();
        imports.insert("u8".into(), Some(vec!["other".into(), "u8".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.canonical_type(&parse_quote!(u8)), parse_quote!(other::u8));
        imports.insert("u8".into(), Some(vec!["u8".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.canonical_type(&parse_quote!(u8)), parse_quote!(u8));
        imports.remove("u8");
        imports.insert("*".into(), None);
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.source_type(&parse_quote!(u8)), parse_quote!(u8));
        assert_eq!(scoped.source_type(&parse_quote!(Local)), parse_quote!(crate::Local));
        assert_eq!(scoped.canonical_type(&parse_quote!(Local)), parse_quote!(crate::Local));
    });
    with_types("mod unrelated { struct Local; }", |types| {
        assert!(types.defaults.defines("Local"));
        let mut imports = types.imports.clone();
        imports.insert("Local".into(), Some(vec!["dependency".into(), "Actual".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(
            scoped.source_type(&parse_quote!(Local::Associated)),
            parse_quote!(dependency::Actual::Associated)
        );
    });
}

#[test]
fn primitive_kind_proofs_require_the_expected_kind_and_exact_unshadowed_standard_namespace() {
    with_types("", |types| {
        for (source, kind, expected) in [
            ("bool", Kind::Bool, true),
            ("std::primitive::u8", Kind::Unsigned, true),
            ("core::primitive::i16", Kind::Signed, true),
            ("&mut std::primitive::u8", Kind::Unsigned, true),
            ("other::primitive::u8", Kind::Unsigned, false),
            ("std::other::u8", Kind::Unsigned, false),
            ("std::primitive::u8", Kind::Signed, false),
        ] {
            assert_eq!(
                types.supports_primitive_kind(&parse_str(source).unwrap(), kind),
                expected,
                "{source}"
            );
        }
    });
    with_types("struct bool;", |types| {
        assert!(!types.supports_primitive_kind(&parse_quote!(bool), Kind::Bool));
    });
}

#[test]
fn io_result_payloads_keep_the_fixed_error_slot_and_never_replace_the_success_payload() {
    with_types("", |types| {
        let ty: Type = parse_quote!(std::io::Result<bool>);
        assert!(types.supports_result_api(&ty));
        assert!(types.is_io_result(&ty));
        assert_eq!(types.payload(&ty, 0).as_deref(), Some(&parse_quote!(bool)));
        assert_eq!(types.payload(&ty, 1).as_deref(), Some(&parse_quote!(std::io::Error)));
        assert_eq!(types.payload(&ty, 2), None);
        for spelling in ["custom::io::Result<bool>", "std::other::Result<bool>", "std::io::Other<bool>"] {
            assert!(!types.is_io_result(&parse_str(spelling).unwrap()), "{spelling}");
        }
    });
}

#[test]
fn reference_wrapped_alias_cycles_stop_with_unknown_primitive_kind() {
    with_types("", |types| {
        let aliases = HashMap::from_iter([(
            "Alias".into(),
            Some(Alias {
                parameters: Vec::new(),
                target: parse_quote!(&Alias),
            }),
        )]);
        let scoped = Types {
            aliases: Some(&aliases),
            ..*types
        };
        let mut visits = 0;
        assert_eq!(
            scoped.primitive_kind_with(&parse_quote!(Alias), || {
                visits += 1;
                assert!(visits <= 3, "a repeated alias must end primitive resolution");
            }),
            Kind::Unknown
        );
        assert_eq!(visits, 3);
    });
}

#[test]
fn qualified_primitive_named_roots_do_not_freeze_their_associated_type_as_a_primitive() {
    with_types("", |types| {
        for source in ["u8::Associated", "bool::Associated", "str::Associated"] {
            let ty: Type = parse_str(source).unwrap();
            assert_eq!(types.canonical_type(&ty), ty, "{source}");
        }
        assert_eq!(types.canonical_type(&parse_quote!(u8)), parse_quote!(::core::primitive::u8));
    });
}

#[test]
fn explicit_local_negative_evidence_preserves_a_module_named_xyzzy_after_merging() {
    let source = "mod xyzzy { pub struct Missing; }";
    with_types(source, |types| {
        let mut defaults = Defaults::of(&parse_file(source).unwrap());
        defaults.absorb(Defaults::of(&parse_file("struct Other;").unwrap()));
        let mut imports = types.imports.clone();
        imports.insert(LOCAL_MODULE.into(), Some(vec!["xyzzy".into()]));
        let scoped = Types {
            defaults: &defaults,
            imports: &imports,
            ..*types
        };
        assert!(!defaults.has_single_source_evidence());
        assert!(scoped.package_lacks_default(&parse_quote!(self::Missing)));
        assert!(scoped.package_lacks_default(&parse_quote!(crate::xyzzy::Missing)));
        assert!(!scoped.package_lacks_default(&parse_quote!(self::Unknown)));
    });
}

#[test]
fn empty_constructor_proofs_do_not_treat_a_local_vec_alias_as_an_inherent_standard_constructor() {
    with_types("type Vec<T> = std::vec::Vec<T>;", |types| {
        let aliases = HashMap::from_iter([(
            "Vec".into(),
            Some(Alias {
                parameters: vec!["T".into()],
                target: parse_quote!(std::vec::Vec<T>),
            }),
        )]);
        let scoped = Types {
            aliases: Some(&aliases),
            ..*types
        };
        assert!(!scoped.supports_empty_default_constructor(&parse_quote!(Vec<u8>)));
    });
}

#[test]
fn empty_constructor_roots_respect_nominal_evidence_for_the_written_root_name() {
    with_types("mod unrelated { struct std; }", |types| {
        assert!(types.defaults.defines("std"));
        assert!(!types.defaults.defines("Vec"));
        assert!(types.supports_collection_api(&parse_quote!(std::vec::Vec<u8>)));
        assert!(!types.supports_empty_default_constructor(&parse_quote!(std::vec::Vec<u8>)));
    });
}

#[test]
fn ordinary_generic_return_payloads_do_not_acquire_result_error_obligations() {
    with_types("struct Pair<T, E>(T, E);", |types| {
        assert!(!returns_undefaultable_error(&parse_quote!(-> Pair<u8, std::fs::File>), types));
        assert!(returns_undefaultable_error(&parse_quote!(-> Result<u8, std::fs::File>), types));
    });
}

#[test]
fn self_alias_collection_defaults_distinguish_templates_incomplete_evidence_and_known_absence() {
    let declarations = "
        mod std { pub mod collections { pub struct HashSet<T>(T); pub struct HashMap<K,V>(K,V); } }
        type HashSet<T> = std::collections::HashSet<T>;
        type HashMap<K,V> = std::collections::HashMap<K,V>;";
    for (additional, expected, complete) in [
        ("", false, true),
        ("", true, false),
        (
            "impl<T:Default> Default for HashSet<T> { fn default() -> Self { loop {} } }
             impl<K:Default,V:Default> Default for HashMap<K,V> { fn default() -> Self { loop {} } }",
            true,
            true,
        ),
    ] {
        with_types(&format!("{declarations} {additional}"), |types| {
            let mut defaults = Defaults::of(&parse_file(&format!("{declarations} {additional}")).unwrap());
            if !complete {
                defaults.mark_incomplete();
            }
            let types = Types {
                defaults: &defaults,
                ..*types
            };
            assert_eq!(types.defaults.is_complete(), complete);
            let aliases = HashMap::from_iter([
                (
                    "HashSet".into(),
                    Some(Alias {
                        parameters: vec!["T".into()],
                        target: parse_quote!(std::collections::HashSet<T>),
                    }),
                ),
                (
                    "HashMap".into(),
                    Some(Alias {
                        parameters: vec!["K".into(), "V".into()],
                        target: parse_quote!(std::collections::HashMap<K,V>),
                    }),
                ),
            ]);
            for concrete in [parse_quote!(HashSet<u8>), parse_quote!(HashMap<u8,bool>)] {
                let scoped = Types {
                    aliases: Some(&aliases),
                    self_type: Some(&concrete),
                    ..types
                };
                assert!(scoped.supports_collection_api(&concrete));
                assert!(scoped.package_defines(&concrete));
                assert_eq!(scoped.has_default(&parse_quote!(Self)), expected, "{additional}");
            }
        });
    }
}

#[test]
fn package_default_queries_respect_block_shadows_and_keep_partial_option_defaults_optimistic_only() {
    with_types("#[derive(Default)] struct Option;", |types| {
        let ty = parse_quote!(Option);
        assert!(types.has_default(&ty));
        let mut imports = types.imports.clone();
        imports.insert(BLOCK_TYPES.into(), Some(vec!["Option".into()]));
        let scoped = Types {
            imports: &imports,
            ..*types
        };
        assert_eq!(scoped.package_type_name(&ty), None);
        assert!(!scoped.package_declares_default(&ty));
        let mut partial = Defaults::of(&parse_file("#[derive(Default)] struct Option;").unwrap());
        partial.mark_incomplete();
        let scoped = Types {
            defaults: &partial,
            ..*types
        };
        assert!(scoped.has_default(&ty));
        assert!(!scoped.package_declares_default(&ty));
        assert!(!scoped.has_proven_default(&ty));
    });
}

#[test]
fn package_negative_evidence_keeps_crate_self_super_and_renamed_external_imports_distinct() {
    let source = "struct RootMissing; mod child { struct Missing; } #[derive(Default)] struct Present;";
    with_types(source, |types| {
        let mut merged = Defaults::of(&parse_file(source).unwrap());
        merged.absorb(Defaults::of(&parse_file("struct Other;").unwrap()));
        for defaults in [types.defaults, &merged] {
            let mut imports = types.imports.clone();
            imports.insert(LOCAL_MODULE.into(), Some(vec!["child".into()]));
            let scoped = Types {
                defaults,
                imports: &imports,
                ..*types
            };
            for spelling in ["crate::RootMissing", "self::Missing", "super::RootMissing"] {
                assert!(scoped.package_lacks_default(&parse_str(spelling).unwrap()), "{spelling}");
            }
        }
        assert!(!merged.has_single_source_evidence());
        let scoped = Types {
            defaults: &merged,
            ..*types
        };
        assert!(scoped.package_lacks_default(&parse_quote!(RootMissing)));
        assert!(scoped.package_declares_default(&parse_quote!(Present)));
        let mut imports = types.imports.clone();
        imports.insert("Renamed".into(), Some(vec!["external".into(), "Missing".into()]));
        let scoped = Types {
            defaults: &merged,
            imports: &imports,
            ..*types
        };
        assert!(!scoped.package_lacks_default(&parse_quote!(Renamed)));
    });
}

#[test]
fn generic_local_primitive_named_types_keep_their_explicit_default_templates() {
    with_types("#[derive(Default)] struct bool<T>(T);", |types| {
        let ty = parse_quote!(bool<u16>);
        assert!(!types.package_declares_default(&ty));
        assert!(types.local_generic_default(&ty));
        assert!(types.has_proven_default(&ty));
    });
}

#[test]
fn renamed_fx_collection_proofs_keep_the_resolved_family_when_generic_source_defaults_are_unknown() {
    let file = parse_file(
        "mod rustc_hash {
            #[derive(Default)] pub struct FxHashMap<K,V>(K,V);
            #[derive(Default)] pub struct FxHashSet<T>(T);
        }
        use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};",
    )
    .unwrap();
    let cfg = CfgSet::unconditional();
    let mut defaults = Defaults::of_source_in(&file, &cfg, "lib.rs");
    defaults.bind_sources(&[("lib.rs".into(), "lib.rs".into(), Vec::new())]);
    let imports = HashMap::from_iter([
        (SOURCE_FILE.into(), Some(vec!["lib.rs".into()])),
        (LOCAL_MODULE.into(), Some(Vec::new())),
        ("HashMap".into(), Some(vec!["rustc_hash".into(), "FxHashMap".into()])),
        ("HashSet".into(), Some(vec!["rustc_hash".into(), "FxHashSet".into()])),
    ]);
    let types = Types {
        abstracts: &[],
        defaulted: &[],
        imports: &imports,
        defaults: &defaults,
        aliases: None,
        self_type: None,
        self_associated: None,
    };
    for ty in [parse_quote!(HashMap<u8,bool>), parse_quote!(HashSet<u8>)] {
        assert!(!types.package_declares_default(&ty));
        assert!(!types.local_generic_default(&ty));
        assert!(!types.supports_collection_api(&ty));
        assert!(types.has_proven_default(&ty));
    }
}

#[test]
fn standard_default_traits_reject_generics_shadowed_roots_and_foreign_names() {
    with_types("", |types| {
        for (spelling, supported) in [
            ("Default", true),
            ("std::default::Default", true),
            ("core::default::Default", true),
            ("other::default::Default", false),
            ("std::other::Default", false),
            ("std::default::Other", false),
            ("Default<u8>", false),
            ("std::default::Default<u8>", false),
            ("&Default", false),
        ] {
            assert_eq!(types.supports_default_trait(&parse_str(spelling).unwrap()), supported, "{spelling}");
        }
        for (root, imported, supported) in [
            ("std", Some(vec!["std".into()]), true),
            ("std", Some(vec![ABSOLUTE_ROOT.into(), "std".into()]), true),
            ("std", Some(vec!["other".into()]), false),
            ("std", None, false),
            ("core", Some(vec!["core".into()]), true),
            ("core", Some(vec!["core".into(), "nested".into()]), false),
        ] {
            let mut imports = types.imports.clone();
            imports.insert(root.into(), imported);
            let scoped = Types {
                imports: &imports,
                ..*types
            };
            let spelling = format!("{root}::default::Default");
            assert_eq!(
                scoped.supports_default_trait(&parse_str(&spelling).unwrap()),
                supported,
                "{spelling}"
            );
        }
    });
    with_types("struct Default;", |types| {
        assert!(!types.supports_default_trait(&parse_quote!(Default)));
        assert!(types.supports_default_trait(&parse_quote!(core::default::Default)));
    });
}

#[test]
fn type_substitution_reaches_all_owned_positions_without_rewriting_qualified_associated_types() {
    let substitutions = HashMap::from_iter([("T".into(), parse_quote!(u16)), ("U".into(), parse_quote!(bool))]);
    let original: Type = parse_quote!((&T, *const T, *mut U, [T; 3], [U], (T, U), Result<Vec<T>, Box<U>>, T::Item));
    let expected: Type = parse_quote!((
        &u16,
        *const u16,
        *mut bool,
        [u16; 3],
        [bool],
        (u16, bool),
        Result<Vec<u16>, Box<bool>>,
        T::Item
    ));
    assert_eq!(substitute_type(&original, &substitutions), expected);
    let substitutions = HashMap::from_iter([("T".into(), parse_quote!(_))]);
    assert_eq!(substitute_type(&parse_quote!(T::Item), &substitutions), parse_quote!(_));
    assert_eq!(
        substitute_type(&parse_quote!(<T as Trait>::Item), &substitutions),
        parse_quote!(<T as Trait>::Item)
    );
}

#[test]
fn map_key_proofs_recurse_through_standard_owned_shapes_but_reject_unknown_payloads() {
    with_types("", |types| {
        for (key, supported) in [
            ("String", true),
            ("Result<u8, bool>", true),
            ("(u8, bool)", true),
            ("()", true),
            ("[u8; 3]", true),
            ("Box<u8>", true),
            ("Result<u8, Custom>", false),
            ("Box<Custom>", false),
        ] {
            let ty: Type = parse_str(key).unwrap();
            assert_eq!(types.collection_key_proven(&ty, 8), supported, "{key}");
            assert!(!types.collection_key_proven(&ty, 0));
        }
        assert!(!types.supports_collection_constructor(&parse_quote!(&u8)));
        for (source, missing) in [
            ("!", true),
            ("[std::fs::File; 1]", true),
            ("[std::fs::File; 0]", false),
            ("[std::fs::File; N]", false),
            ("Box<std::fs::File>", true),
        ] {
            assert_eq!(types.lacks_default(&parse_str(source).unwrap()), missing, "{source}");
        }
        assert!(types.supports_iterator_trait(&parse_quote!(impl 'static + Iterator<Item = u8>), &[]));
    });
}

#[test]
fn labeled_type_identity_handles_enum_aliases_relative_modules_and_shadowing() {
    fn check<'a>(imports: &'a HashMap<String, Option<Vec<String>>>, defaults: &'a Defaults) -> Types<'a> {
        Types {
            abstracts: &[],
            defaulted: &[],
            imports,
            defaults,
            aliases: None,
            self_type: None,
            self_associated: None,
        }
    }

    let cfg = CfgSet::unconditional();
    let file = parse_file("#[derive(Default)] struct Root; enum Choice { A } mod child {}").unwrap();
    let mut defaults = Defaults::of_source_in(&file, &cfg, "lib.rs");
    let child = parse_file("use super::Root as Alias;").unwrap();
    defaults.absorb(Defaults::of_source_in(&child, &cfg, "child.rs"));
    defaults.bind_sources(&[
        ("lib.rs".into(), "lib.rs".into(), Vec::new()),
        ("child.rs".into(), "lib.rs".into(), vec!["child".into()]),
    ]);
    defaults.alias_source("child.rs", "report.rs");
    let mut imports = HashMap::from_iter([
        (SOURCE_FILE.into(), Some(vec!["report.rs".into()])),
        ("Alias".into(), Some(vec!["super".into(), "Root".into()])),
    ]);
    assert!(check(&imports, &defaults).source_same_type(&parse_quote!(Alias), &parse_quote!(crate::Root)));
    assert!(check(&imports, &defaults).source_is_enum(&parse_quote!(crate::Choice)));
    assert!(check(&imports, &defaults).has_proven_default(&parse_quote!(Alias)));
    assert!(check(&imports, &defaults).package_has_default(&parse_quote!(Alias)));
    assert!(check(&imports, &defaults).package_declares_default(&parse_quote!(Alias)));
    assert!(check(&imports, &defaults).package_defines(&parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).source_same_type(&parse_quote!(::Root), &parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).source_same_type(&parse_quote!(Alias), &parse_quote!(::Root)));
    imports.insert(BLOCK_TYPES.into(), Some(vec!["Alias".into(), "super".into()]));
    assert!(!check(&imports, &defaults).source_is_enum(&parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).package_declares_default(&parse_quote!(Alias)));
    imports.insert("*".into(), None);
    assert!(!check(&imports, &defaults).source_is_enum(&parse_quote!(crate::Choice)));
}

#[test]
fn collection_defaults_respect_local_names_and_external_known_hash_aliases() {
    with_types(
        "#[derive(Default)] struct Vec<T>(T); #[derive(Default)] struct HashMap<K, V>(K, V);",
        |types| {
            assert!(types.has_proven_default(&parse_quote!(Vec<u8>)));
            assert!(types.has_proven_default(&parse_quote!(HashMap<u8, bool>)));
            assert!(!types.has_proven_default(&parse_quote!(Vec<Missing>)));
        },
    );
    with_types("", |types| {
        assert!(!types.has_proven_default(&parse_quote!(rustc_hash::FxHashMap<u8, bool>)));
        assert!(!types.has_proven_default(&parse_quote!(rustc_hash::FxHashSet<u8>)));
        assert!(!types.has_proven_default(&parse_quote!(unknown::Map<u8, bool>)));
    });
}

#[test]
fn source_qualification_and_substitution_preserve_structural_types_and_attribute_budgets() {
    with_types("struct Local;", |types| {
        let ty = parse_quote!((&Local, [Local; 2], (Local)));
        let qualified = types.source_type(&ty);
        assert_eq!(qualified, parse_quote!((&crate::Local, [crate::Local; 2], (crate::Local))));
        let group = syn::TypeGroup {
            attrs: Vec::new(),
            group_token: Group::default(),
            elem: Box::new(parse_quote!(Local)),
        };
        let qualified = types.source_type(&Type::Group(group));
        assert!(matches!(qualified, Type::Group(group) if *group.elem == parse_quote!(crate::Local)));
    });
    let mut ty: Type = parse_quote!([u8; 1]);
    let Type::Array(array) = &mut ty else { unreachable!() };
    array.attrs.push(parse_quote!(#[allow(dead_code)]));
    let mut budget = TYPE_EXPANSION_LIMIT;
    assert!(reserve_type(&ty, &mut budget));
    assert_eq!(substitute_type(&ty, &HashMap::default()), ty);
}
