// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use syn::punctuated::Punctuated;
use syn::token::Group;

use super::*;
use crate::cfg::CfgSet;

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
            scoped.source_type(&syn::parse_quote!((T::Item, D::Item, Local, u16))),
            syn::parse_quote!((_, _, _, u16))
        );
        assert_eq!(
            scoped.canonical_type(&syn::parse_quote!((T::Item, D::Item, Local, u16))),
            syn::parse_quote!((_, _, _, ::core::primitive::u16))
        );
        assert_eq!(
            scoped.source_type(&syn::parse_quote!(<T as Trait>::Item)),
            syn::parse_quote!(<T as Trait>::Item)
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
                target: syn::parse_quote!((T, Local)),
            }),
        )]);
        let types = Types {
            aliases: Some(&aliases),
            ..*types
        };
        let ty = syn::parse_quote!(&Pair<u8>);
        assert_eq!(types.source_type(&ty), syn::parse_quote!(&(u8, crate::Local)));
        assert_eq!(types.canonical_type(&ty), syn::parse_quote!(&(::core::primitive::u8, crate::Local)));
        assert_eq!(types.declaration_type(&ty), syn::parse_quote!(&Pair<::core::primitive::u8>));
        assert_eq!(
            types.canonical_type(&syn::parse_quote!((std::primitive::u8, core::primitive::u16))),
            syn::parse_quote!((::core::primitive::u8, ::core::primitive::u16))
        );
    });
}

#[test]
fn campaign_source_type_qualification_shares_its_work_limit_across_tuple_siblings() {
    with_types("struct Local;", |types| {
        let within = Type::Tuple(syn::TypeTuple {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            elems: core::iter::repeat_n::<Type>(syn::parse_quote!(Local), 63).collect(),
        });
        let expected = Type::Tuple(syn::TypeTuple {
            attrs: Vec::new(),
            paren_token: syn::token::Paren::default(),
            elems: core::iter::repeat_n::<Type>(syn::parse_quote!(crate::Local), 63).collect(),
        });
        assert_eq!(types.source_type(&within), expected);
        let Type::Tuple(mut over) = within else { unreachable!() };
        over.elems.push(syn::parse_quote!(Local));
        let Type::Tuple(mut expected_over) = expected else { unreachable!() };
        expected_over.elems.push(syn::parse_quote!(_));
        assert_eq!(types.source_type(&Type::Tuple(over)), Type::Tuple(expected_over));
    });
}

#[test]
fn parent_module_qualification_retains_negative_default_evidence() {
    let defaults = Defaults::of(&syn::parse_file("struct Missing; mod child {}").unwrap());
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
    assert!(types.package_lacks_default(&syn::parse_quote!(super::Missing)));
    assert!(!types.package_lacks_default(&syn::parse_quote!(super::super::Missing)));
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
        assert_eq!(types.source_type(&empty), syn::parse_quote!(_));
        assert!(!types.supports_collection_constructor(&empty));
        assert!(!types.supports_collection_constructor(&syn::parse_quote!(Vec)));
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
    assert_eq!(types.source_type(&syn::parse_quote!(Ambiguous)), syn::parse_quote!(_));
    assert!(!types.standard_constructor_root(&syn::parse_quote!(std::vec::Vec<u8>)));
    with_types(
        "#[derive(Default)] struct Vec<T>(T); #[derive(Default)] struct HashMap<K, V>(K, V);",
        |types| {
            assert!(types.has_default(&syn::parse_quote!(Vec<u8>)));
            assert!(types.has_default(&syn::parse_quote!(HashMap<u8, bool>)));
        },
    );
}

fn with_types(source: &str, check: impl FnOnce(&Types<'_>)) {
    let defaults = Defaults::of(&syn::parse_file(source).unwrap());
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
            let ty: Type = syn::parse_str(key).unwrap();
            assert_eq!(types.collection_key_proven(&ty, 8), supported, "{key}");
            assert!(!types.collection_key_proven(&ty, 0));
        }
        assert!(!types.supports_collection_constructor(&syn::parse_quote!(&u8)));
        for (source, missing) in [
            ("!", true),
            ("[std::fs::File; 1]", true),
            ("[std::fs::File; 0]", false),
            ("[std::fs::File; N]", false),
            ("Box<std::fs::File>", true),
        ] {
            assert_eq!(types.lacks_default(&syn::parse_str(source).unwrap()), missing, "{source}");
        }
        assert!(types.supports_iterator_trait(&syn::parse_quote!(impl 'static + Iterator<Item = u8>), &[]));
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
    let file = syn::parse_file("#[derive(Default)] struct Root; enum Choice { A } mod child {}").unwrap();
    let mut defaults = Defaults::of_source_in(&file, &cfg, "lib.rs");
    let child = syn::parse_file("use super::Root as Alias;").unwrap();
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
    assert!(check(&imports, &defaults).source_same_type(&syn::parse_quote!(Alias), &syn::parse_quote!(crate::Root)));
    assert!(check(&imports, &defaults).source_is_enum(&syn::parse_quote!(crate::Choice)));
    assert!(check(&imports, &defaults).has_proven_default(&syn::parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).source_same_type(&syn::parse_quote!(::Root), &syn::parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).source_same_type(&syn::parse_quote!(Alias), &syn::parse_quote!(::Root)));
    imports.insert(BLOCK_TYPES.into(), Some(vec!["Alias".into(), "super".into()]));
    assert!(!check(&imports, &defaults).source_is_enum(&syn::parse_quote!(Alias)));
    assert!(!check(&imports, &defaults).package_declares_default(&syn::parse_quote!(Alias)));
    imports.insert("*".into(), None);
    assert!(!check(&imports, &defaults).source_is_enum(&syn::parse_quote!(crate::Choice)));
}

#[test]
fn collection_defaults_respect_local_names_and_external_known_hash_aliases() {
    with_types(
        "#[derive(Default)] struct Vec<T>(T); #[derive(Default)] struct HashMap<K, V>(K, V);",
        |types| {
            assert!(types.has_proven_default(&syn::parse_quote!(Vec<u8>)));
            assert!(types.has_proven_default(&syn::parse_quote!(HashMap<u8, bool>)));
            assert!(!types.has_proven_default(&syn::parse_quote!(Vec<Missing>)));
        },
    );
    with_types("", |types| {
        assert!(!types.has_proven_default(&syn::parse_quote!(rustc_hash::FxHashMap<u8, bool>)));
        assert!(!types.has_proven_default(&syn::parse_quote!(rustc_hash::FxHashSet<u8>)));
        assert!(!types.has_proven_default(&syn::parse_quote!(unknown::Map<u8, bool>)));
    });
}

#[test]
fn source_qualification_and_substitution_preserve_structural_types_and_attribute_budgets() {
    with_types("struct Local;", |types| {
        let ty = syn::parse_quote!((&Local, [Local; 2], (Local)));
        let qualified = types.source_type(&ty);
        assert_eq!(qualified, syn::parse_quote!((&crate::Local, [crate::Local; 2], (crate::Local))));
        let group = syn::TypeGroup {
            attrs: Vec::new(),
            group_token: Group::default(),
            elem: Box::new(syn::parse_quote!(Local)),
        };
        let qualified = types.source_type(&Type::Group(group));
        assert!(matches!(qualified, Type::Group(group) if *group.elem == syn::parse_quote!(crate::Local)));
    });
    let mut ty: Type = syn::parse_quote!([u8; 1]);
    let Type::Array(array) = &mut ty else { unreachable!() };
    array.attrs.push(syn::parse_quote!(#[allow(dead_code)]));
    let mut budget = TYPE_EXPANSION_LIMIT;
    assert!(reserve_type(&ty, &mut budget));
    assert_eq!(substitute_type(&ty, &HashMap::default()), ty);
}
