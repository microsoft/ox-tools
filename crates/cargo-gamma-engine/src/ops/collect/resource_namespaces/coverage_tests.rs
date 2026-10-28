// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Expr, ExprArray, Fields, Ident, ItemFn, ItemStruct, Type, TypePath, parse_file, parse_quote, parse_str};

use super::*;

/// Leaves contract evidence absent so size-validation tests can isolate individual fields.
fn empty_dependency() -> ResourceDependency {
    ResourceDependency {
        crate_root: String::new(),
        import_name: String::new(),
        package_id: String::new(),
        package_name: String::new(),
        version: String::new(),
        source: String::new(),
        features: Vec::new(),
        registry_closure: false,
        library_root: None,
    }
}

#[test]
fn nearest_module_functions_are_not_redirected_by_same_named_outer_value_imports() {
    let ns = namespaces(
        "mod original { pub fn nested() {} }
         use original::nested;
         mod nested { pub fn helper() {} }",
    );
    let function = ns.function_in("", &[], &path(&["nested", "helper"])).unwrap();
    assert_eq!(function.module, path(&["nested"]));
    assert_eq!(function.syntax, "pub fn helper () { }");
    assert!(ns.nontrait("", &[], &path(&["nested", "helper"])));

    let ns = namespaces("fn helper() {} mod helper {}");
    let function = ns.function_in("", &[], &path(&["helper"])).unwrap();
    assert!(function.module.is_empty());
    assert_eq!(function.syntax, "fn helper () { }");
}

#[test]
fn a_macro_name_does_not_prove_an_unknown_member_of_a_same_named_module_is_nontrait() {
    let ns = namespaces("macro_rules! nested { () => {} } mod nested {}");
    assert!(ns.local.scopes[&Vec::new()].as_ref().unwrap().macro_names.contains("nested"));
    assert!(!ns.nontrait("", &[], &path(&["nested", "Missing"])));
}

#[test]
fn an_ambiguous_parent_module_cannot_redirect_its_known_child_through_an_ancestor_import() {
    let ns = namespaces(
        "#[cfg(feature = \"first\")]
         mod ambiguous { pub mod member {} }
         #[cfg(all(not(feature = \"first\"), feature = \"second\"))]
         mod ambiguous {}
         mod fallback { pub struct member; }
         #[cfg(not(any(feature = \"first\", feature = \"second\")))]
         use fallback as ambiguous;
         mod bridge { use crate::ambiguous as reexport; }",
    );
    assert!(ns.local.scopes[&path(&["ambiguous"])].is_none());
    assert!(ns.local.scopes[&path(&["ambiguous", "member"])].is_some());
    assert!(!ns.nontrait("", &[], &path(&["ambiguous", "member"])));
    assert!(!ns.nontrait("", &[], &path(&["bridge", "reexport", "member"])));
    assert!(ns.nontrait("", &[], &path(&["fallback", "member"])));
}

#[test]
fn ambiguous_module_itself_is_not_nontrait_even_when_its_parent_is_transparent() {
    let ns = namespaces(
        "#[cfg(feature = \"first\")] mod ambiguous {}
         #[cfg(not(feature = \"first\"))] mod ambiguous {}
         mod known {}",
    );
    assert_eq!(ns.local.scopes.get(&path(&["ambiguous"])), Some(&None));
    assert!(ns.local.transparent(&[], |namespace| ns.opaque("", namespace)));
    assert!(!ns.nontrait("", &[], &path(&["ambiguous"])));
    assert!(ns.nontrait("", &[], &path(&["known"])));
}

#[test]
fn missing_roots_scopes_and_ambiguous_contexts_remain_absent_in_direct_queries() {
    let mut ns = namespaces("struct Record; fn helper() {}");
    assert!(ns.modules("missing").is_none());
    assert!(!ns.primitive_scope("missing", &[]));
    assert!(!ns.standard_macro("missing", &[], "assert"));
    assert!(!ns.nontrait("missing", &[], &path(&["Record"])));
    assert!(ns.scope_for("", &path(&["missing"])).is_none());
    ns.local.scopes.insert(path(&["ambiguous"]), None);
    assert!(ns.scope_for("", &path(&["ambiguous"])).is_none());
    assert!(
        ns.external_target("", &path(&["ambiguous"]), &path(&["dependency", "helper"]))
            .is_none()
    );
    ns.label("lib.rs");
    ns.bind(&[("lib.rs".into(), "root.rs".into(), Vec::new())]);
    assert!(ns.modules("").is_none());
    assert!(ns.scope_for("missing.rs", &[]).is_none());
    ns.alias("lib.rs", "lib.rs");
    assert!(ns.scope_for("lib.rs", &[]).is_none());

    let mut ns = namespaces("struct Record;");
    ns.label(&"x".repeat(32769));
    assert!(ns.exhausted);
    assert!(ns.scope_for("", &[]).is_none());
}

#[test]
fn source_costs_are_normalized_and_exhausted_inventories_cannot_accumulate_more_evidence() {
    let source = "fn helper() {}";
    let mut ns = namespaces(source);
    ns.label("folder\\source.rs");
    assert_eq!(ns.costs.get("folder/source.rs"), Some(&source.len()));
    assert!(ns.pending.contains_key("folder/source.rs"));

    let mut other = namespaces(source);
    other.label("other.rs");
    for operation in ["label", "absorb", "extend", "costs"] {
        let mut ns = Namespaces {
            exhausted: true,
            ..Namespaces::default()
        };
        match operation {
            "label" => ns.label("new.rs"),
            "absorb" => {
                let mut owned = namespaces(source);
                owned.label("other.rs");
                ns.absorb(owned);
            }
            "extend" => ns.extend(&other),
            "costs" => ns.costs(&other),
            _ => unreachable!(),
        }
        assert!(ns.exhausted, "{operation}");
        assert!(ns.costs.is_empty(), "{operation}");
        assert!(ns.pending.is_empty(), "{operation}");
    }
    let mut ns = namespaces(source);
    ns.extend(&Namespaces {
        exhausted: true,
        ..Namespaces::default()
    });
    assert!(ns.exhausted);

    let mut ns = Namespaces {
        costs: HashMap::from_iter([("first.rs".into(), 8 * 1024 * 1024)]),
        ..Namespaces::default()
    };
    ns.costs(&Namespaces {
        costs: HashMap::from_iter([("second.rs".into(), 1)]),
        ..Namespaces::default()
    });
    assert!(ns.exhausted);
    assert!(ns.costs.is_empty());

    let mut ns = namespaces(source);
    ns.label("lib.rs");
    ns.label(&"x".repeat(32769));
    ns.bind(&[("lib.rs".into(), "root.rs".into(), Vec::new())]);
    assert!(ns.exhausted);
    assert!(ns.pending.is_empty());
    assert!(ns.roots.is_empty());
    assert!(ns.contexts.is_empty());
}

#[test]
fn alias_resolution_budgets_count_redirects_and_check_path_width_before_shortening() {
    for redirects in [30, 31, 32, 33] {
        let mut source = "pub fn helper() {}".to_owned();
        for ordinal in 0..redirects {
            let target = if ordinal + 1 == redirects {
                "helper".into()
            } else {
                format!("alias{}", ordinal + 1)
            };
            write!(source, "use crate::{target} as alias{ordinal};").unwrap();
        }
        let ns = namespaces(&source);
        assert_eq!(ns.function_in("", &[], &path(&["alias0"])).is_some(), redirects < 32, "{redirects}");
        assert_eq!(ns.nontrait("", &[], &path(&["crate", "alias0"])), redirects < 32, "{redirects}");
    }

    let mut source = "use crate as reset; fn helper() {}".to_owned();
    for _ in 0..32 {
        source = format!("fn helper() {{}} mod nested {{ {source} }}");
    }
    let ns = namespaces(&source);
    for suffix_depth in [0, 30, 31, 32] {
        let mut target = vec!["nested".to_owned(); 32];
        target.push("reset".into());
        target.extend(vec!["nested".to_owned(); suffix_depth]);
        target.push("helper".into());
        let function = ns.function_in("", &[], &target);
        assert_eq!(function.is_some(), target.len() <= 64, "{suffix_depth}");
        if let Some(function) = function {
            assert_eq!(function.module, vec!["nested".to_owned(); suffix_depth]);
        }
        assert_eq!(ns.nontrait("", &[], &target), target.len() <= 64, "{suffix_depth}");
    }
}

#[test]
fn source_layout_limits_check_each_field_independently_and_consume_pending_inputs() {
    for (field, maximum) in [("source", 32768), ("root", 32768), ("depth", 32), ("width", 256), ("count", 4096)] {
        for length in [maximum - 1, maximum, maximum + 1] {
            let mut layout = ("lib.rs".to_owned(), "root.rs".to_owned(), Vec::new());
            match field {
                "source" => layout.0 = "x".repeat(length),
                "root" => layout.1 = "x".repeat(length),
                "depth" => layout.2 = vec!["nested".into(); length],
                "width" => layout.2 = vec!["x".repeat(length)],
                "count" => {}
                _ => unreachable!(),
            }
            let mut ns = namespaces("fn helper() {}");
            ns.label(&layout.0);
            let layouts = if field == "count" { vec![layout; length] } else { vec![layout] };
            ns.bind(&layouts);
            assert!(ns.bound, "{field}: {length}");
            assert_eq!(ns.exhausted, length > maximum, "{field}: {length}");
            assert!(ns.pending.is_empty(), "{field}: {length}");
        }
    }
    let mut ns = namespaces("fn helper() {}");
    ns.label("lib.rs");
    ns.bind(&[
        ("lib.rs".into(), "root.rs".into(), Vec::new()),
        ("missing.rs".into(), "root.rs".into(), vec!["x".repeat(257)]),
    ]);
    assert!(ns.exhausted);
    assert!(ns.pending.is_empty());
}

#[test]
fn namespace_readers_keep_opacity_ancestor_transparency_and_function_exhaustion_independent() {
    let ns = namespaces("#[expand] struct Record; fn helper() {}");
    assert!(ns.function_in("", &[], &path(&["helper"])).is_none());
    assert!(!ns.closed_methods("", &[]));
    assert!(!ns.standard_macro("", &[], "assert"));

    let ns = namespaces("unknown!(); mod nested { struct Record; }");
    assert!(!ns.closed_methods("", &[]));
    assert!(!ns.standard_macro("", &[], "assert"));
    assert!(!ns.nontrait("", &[], &path(&["crate", "nested", "Record"])));

    let mut source = "use crate::nested::helper as selected; mod nested { pub fn helper() {} }".to_owned();
    for ordinal in 0..257 {
        write!(source, "fn ordinary{ordinal}() {{}}").unwrap();
    }
    let ns = namespaces(&source);
    assert!(ns.local.scopes[&Vec::new()].as_ref().unwrap().functions_exhausted);
    assert!(ns.local.transparent(&[], |namespace| ns.opaque("", namespace)));
    assert!(!ns.primitive_scope("", &[]));
    assert!(ns.function_in("", &[], &path(&["selected"])).is_none());
}

#[test]
fn a_known_module_is_nontrait_without_trusting_its_body_but_its_parent_must_be_transparent() {
    assert!(namespaces("").nontrait("", &[], &path(&["crate"])));
    assert!(!namespaces("unknown!();").nontrait("", &[], &path(&["crate"])));
    assert!(namespaces("mod nested { unknown!(); }").nontrait("", &[], &path(&["crate", "nested"])));
    assert!(!namespaces("mod outer { unknown!(); mod nested {} }").nontrait("", &[], &path(&["crate", "outer", "nested"])));
    let mut ns = namespaces("");
    ns.label("lib.rs");
    let mut nested = namespaces("unknown!();");
    nested.label("nested.rs");
    ns.absorb(nested);
    ns.bind(&[
        ("lib.rs".into(), "lib.rs".into(), Vec::new()),
        ("nested.rs".into(), "lib.rs".into(), path(&["nested"])),
    ]);
    assert!(ns.nontrait("lib.rs", &[], &path(&["crate", "nested"])));
}

#[test]
fn binding_skips_missing_layouts_and_normalizes_the_source_before_transparency_lookup() {
    let mut ns = namespaces("fn helper() {}");
    ns.label("src\\lib.rs");
    ns.bind(&[
        ("missing.rs".into(), "root.rs".into(), Vec::new()),
        ("src\\lib.rs".into(), "crate\\lib.rs".into(), Vec::new()),
    ]);
    assert!(ns.transparent("src\\lib.rs", &[]));
    assert_eq!(ns.function("src\\lib.rs", &[], &path(&["helper"])).unwrap().root, "crate/lib.rs");
    assert!(ns.pending.is_empty());
    ns.bind(&[("src\\lib.rs".into(), "crate\\lib.rs".into(), Vec::new())]);
    assert!(ns.transparent("src\\lib.rs", &[]));
}

#[test]
fn alias_source_and_display_name_limits_are_independent() {
    for field in ["source", "alias"] {
        for length in [32767, 32768, 32769] {
            let wide = "x".repeat(length);
            let (source, alias) = if field == "source" {
                (wide.as_str(), "alias")
            } else {
                ("source", wide.as_str())
            };
            let mut ns = Namespaces::default();
            ns.alias(source, alias);
            assert_eq!(ns.exhausted, length > 32768, "{field}: {length}");
        }
    }
}

/// Supplies synthetic trusted registry provenance and a closed registry graph for a pinned
/// package contract, with a nonempty placeholder identity but no library-root mapping.
fn pinned_dependency(alias: &str, package: &str, version: &str) -> ResourceDependency {
    ResourceDependency {
        import_name: alias.into(),
        package_id: "registry package identity".into(),
        package_name: package.into(),
        version: version.into(),
        source: "registry+https://github.com/rust-lang/crates.io-index".into(),
        registry_closure: true,
        ..empty_dependency()
    }
}

#[test]
fn bound_character_width_contract_does_not_switch_to_string_checks() {
    let mut ns = namespaces("use width::UnicodeWidthChar;");
    ns.label("lib.rs");
    ns.bind(&[("lib.rs".into(), "root.rs".into(), Vec::new())]);
    let mut dependency = pinned_dependency("width", "unicode-width", "0.2.2");
    dependency.crate_root = "root.rs".into();
    ns.dependencies(&[dependency]);
    assert!(ns.width_contract_for("lib.rs", &[]));
    assert!(!ns.width_contract("root.rs", &[], false));
}

#[test]
fn serde_grouping_imports_require_every_path_component_dependency_and_direct_alias() {
    for (source, module, name, pinned, expected) in [
        ("", "de", "IgnoredAny", true, true),
        ("", "other", "IgnoredAny", true, false),
        ("", "de", "Other", true, false),
        ("", "de", "IgnoredAny", false, false),
        ("use external as ser;", "de", "IgnoredAny", true, false),
    ] {
        let mut ns = namespaces(source);
        if pinned {
            ns.dependencies(&[pinned_dependency("ser", "serde", "1.0.228")]);
            assert!(ns.dependency("", "ser", "serde", "1.0.228"));
        }
        let namespace = ns.local.scopes[&Vec::new()].as_ref().unwrap();
        assert_eq!(
            ns.grouping_import("", namespace, &path(&["ser", module, name])),
            expected,
            "{source}: {module}: {name}: {pinned}"
        );
    }
    let mut ns = namespaces("use external as ser;");
    ns.dependencies(&[pinned_dependency("ser", "serde", "1.0.228")]);
    assert!(ns.dependency("", "ser", "serde", "1.0.228"));
    assert!(!ns.grouping_import("", ns.local.scopes[&Vec::new()].as_ref().unwrap(), &path(&["ser", "Serialize"])));
}

#[test]
fn serde_transparency_checks_every_derive_and_does_not_rewrite_a_qualified_self_import() {
    for (source, transparent) in [
        (
            "#[derive(ser::Serialize, unpinned::Deserialize)] struct Record { field: bool }",
            false,
        ),
        ("use ser; #[derive(ser::Serialize)] struct Record { field: bool }", true),
        ("#[allow(dead_code)] #[derive(ser::Serialize)] struct Record { field: bool }", true),
    ] {
        let mut ser = pinned_dependency("ser", "serde", "1.0.228");
        ser.features.push("serde_derive".into());
        let mut ns = namespaces(source);
        ns.dependencies(&[ser, pinned_dependency("ser::serde_derive", "serde_derive", "1.0.228")]);
        assert_eq!(ns.transparent("", &[]), transparent, "{source}");
    }
}

#[test]
fn qualified_associated_paths_cannot_inherit_an_unrelated_imported_path_buffer_proof() {
    let mut ns = namespaces("use paths::Utf8PathBuf as Path;");
    ns.dependencies(&[pinned_dependency("paths", "camino", "1.2.5")]);
    assert!(ns.stable_path_for("", &[], &parse_quote!(Path)));
    assert!(!ns.stable_path_for("", &[], &parse_quote!(<T>::Path)));
    assert!(ns.stable_path_for("", &[], &parse_quote!(paths::Utf8PathBuf)));
    let qualified: TypePath = parse_quote!(<T as paths>::Utf8PathBuf);
    assert!(qualified.qself.is_some());
    assert!(qualified.path.leading_colon.is_none());
    assert!(!ns.stable_path_for("", &[], &Type::Path(qualified)));
}

#[test]
fn per_source_byte_budget_accepts_exactly_one_mebibyte_of_inert_declaration_text() {
    let prefix = "#[doc = \"";
    let suffix = "\"] struct Record;";
    for length in [1024 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1] {
        let source = format!("{prefix}{}{suffix}", "x".repeat(length - prefix.len() - suffix.len()));
        assert_eq!(source.len(), length);
        let ns = namespaces(&source);
        assert_eq!(ns.local_record_for("", &[], "Record"), length <= 1024 * 1024, "{length}");
    }
}

fn set_dependency_field(dependency: &mut ResourceDependency, field: &str, length: usize) {
    match field {
        "crate_root" => dependency.crate_root = "x".repeat(length),
        "import_name" => dependency.import_name = "x".repeat(length),
        "package_name" => dependency.package_name = "x".repeat(length),
        "package_id" => dependency.package_id = "x".repeat(length),
        "source" => dependency.source = "x".repeat(length),
        "version" => dependency.version = "x".repeat(length),
        "library_root" => dependency.library_root = Some("x".repeat(length)),
        "features" => dependency.features = vec![String::new(); length],
        "feature_width" => dependency.features = vec!["x".repeat(length)],
        _ => panic!("the test table must name a dependency field"),
    }
}

#[test]
fn dependency_field_and_inventory_limits_accept_the_exact_maximum_but_reject_one_more() {
    let mut ns = Namespaces::default();
    ns.dependencies(&[]);
    assert!(!ns.exhausted);
    let mut invalid = empty_dependency();
    invalid.import_name = "x".repeat(257);
    ns.dependencies(&[empty_dependency(), invalid]);
    assert!(ns.exhausted);
    for (field, maximum) in [
        ("crate_root", 32768),
        ("import_name", 256),
        ("package_name", 256),
        ("package_id", 32768),
        ("source", 1024),
        ("version", 256),
        ("library_root", 32768),
        ("features", 256),
        ("feature_width", 256),
    ] {
        for length in [maximum - 1, maximum, maximum + 1] {
            let mut dependency = empty_dependency();
            set_dependency_field(&mut dependency, field, length);
            let mut ns = Namespaces::default();
            ns.dependencies(&[dependency]);
            assert_eq!(ns.exhausted, length > maximum, "{field}: {length}");
            assert_eq!(ns.dependencies.is_empty(), length > maximum, "{field}: {length}");
        }
    }
    for length in [65535, 65536, 65537] {
        let mut ns = Namespaces::default();
        ns.dependencies(&vec![empty_dependency(); length]);
        assert_eq!(ns.exhausted, length > 65536, "{length}");
        assert_eq!(ns.dependencies.is_empty(), length > 65536, "{length}");
    }
}

#[test]
fn every_dependency_text_field_counts_toward_the_exact_eight_mebibyte_aggregate_limit() {
    let mut dependency = empty_dependency();
    dependency.crate_root = "x".repeat(32768);
    let mut dependencies = vec![dependency; 256];
    let mut ns = Namespaces::default();
    ns.dependencies(&dependencies);
    assert!(!ns.exhausted);
    dependencies.push(empty_dependency());
    for field in [
        "crate_root",
        "import_name",
        "package_name",
        "package_id",
        "source",
        "version",
        "library_root",
        "feature_width",
    ] {
        let mut extra = empty_dependency();
        set_dependency_field(&mut extra, field, 1);
        *dependencies.last_mut().unwrap() = extra;
        let mut ns = Namespaces::default();
        ns.dependencies(&dependencies);
        assert!(ns.exhausted, "{field}");
        assert!(ns.dependencies.is_empty(), "{field}");
    }
}

#[test]
fn function_cache_budgets_check_original_and_rendered_text_independently() {
    let tokens: TokenStream = "fn helper() {}".parse().unwrap();
    for bytes in [16383, 16384, 16385] {
        let mut namespace = Namespace::default();
        namespace.function(path(&["helper"]), &tokens, bytes);
        assert_eq!(namespace.functions[&path(&["helper"])].is_some(), bytes <= 16384);
        assert!(!namespace.functions_exhausted);
    }
    let overhead = "fn helper(){\"\";}".parse::<TokenStream>().unwrap().to_string().len();
    for length in [16383, 16384, 16385] {
        let source = format!("fn helper(){{\"{}\";}}", "x".repeat(length - overhead));
        let tokens: TokenStream = source.parse().unwrap();
        assert_eq!(tokens.to_string().len(), length);
        assert!(source.len() <= 16384);
        let mut namespace = Namespace::default();
        namespace.function(path(&["helper"]), &tokens, source.len());
        assert_eq!(namespace.functions[&path(&["helper"])].is_some(), length <= 16384);
    }
    let mut namespace = Namespace::default();
    for ordinal in 0..256 {
        namespace.function(vec![format!("helper{ordinal}")], &tokens, 14);
        assert_eq!(namespace.functions.len(), ordinal + 1);
        assert!(!namespace.functions_exhausted);
    }
    namespace.function(path(&["overflow"]), &tokens, 14);
    assert!(namespace.functions_exhausted);
    assert!(namespace.functions.is_empty());
    namespace.function(path(&["later"]), &tokens, 14);
    assert!(namespace.functions.is_empty());
}

#[test]
fn namespace_inventory_limits_clear_all_retained_evidence_and_sum_costs_without_overflow() {
    for (count, cost, exhausted) in [
        (4096, 0, false),
        (4097, 0, true),
        (1, 8 * 1024 * 1024, false),
        (1, 8 * 1024 * 1024 + 1, true),
    ] {
        let mut ns = namespaces("fn helper() {}");
        ns.label("lib.rs");
        ns.bind(&[("lib.rs".into(), "lib.rs".into(), Vec::new())]);
        ns.label("next.rs");
        ns.costs = (0..count).map(|ordinal| (ordinal.to_string(), cost)).collect();
        ns.limit();
        assert_eq!(ns.exhausted, exhausted, "{count}: {cost}");
        if exhausted {
            assert!(ns.local.scopes.is_empty());
            assert!(ns.pending.is_empty());
            assert!(ns.roots.is_empty());
            assert!(ns.contexts.is_empty());
            assert!(ns.costs.is_empty());
        }
    }
    let mut ns = Namespaces {
        costs: HashMap::from_iter([("first".into(), usize::MAX), ("second".into(), 1)]),
        ..Namespaces::default()
    };
    ns.limit();
    assert!(ns.exhausted);
    assert!(ns.costs.is_empty());
}

#[test]
fn function_and_nontrait_alias_budgets_accept_the_last_reachable_step_and_exact_path_length() {
    for links in [30, 31, 32, 33] {
        let mut source = "fn helper() {}".to_owned();
        for index in 0..links {
            let target = if index + 1 == links {
                "helper".to_owned()
            } else {
                format!("Alias{}", index + 1)
            };
            write!(source, "use self::{target} as Alias{index};").unwrap();
        }
        let ns = namespaces(&source);
        assert_eq!(ns.function_in("", &[], &path(&["Alias0"])).is_some(), links <= 31, "{links}");
        assert_eq!(ns.nontrait("", &[], &path(&["self", "Alias0"])), links <= 31, "{links}");
    }
    let ns = namespaces("mod a { use crate as b; fn helper() {} mod c { fn helper() {} } }");
    let mut at_limit = (0..31).flat_map(|_| path(&["a", "b"])).collect::<Vec<_>>();
    at_limit.extend(path(&["a", "helper"]));
    assert_eq!(at_limit.len(), 64);
    assert!(ns.function_in("", &[], &at_limit).is_some());
    let mut qualified = path(&["crate"]);
    qualified.extend(at_limit);
    assert!(ns.nontrait("", &[], &qualified));
    let mut beyond_limit = (0..31).flat_map(|_| path(&["a", "b"])).collect::<Vec<_>>();
    beyond_limit.extend(path(&["a", "c", "helper"]));
    assert_eq!(beyond_limit.len(), 65);
    assert!(ns.function_in("", &[], &beyond_limit).is_none());
    let mut qualified = path(&["crate"]);
    qualified.extend(beyond_limit);
    assert!(!ns.nontrait("", &[], &qualified));
}

#[test]
fn module_traversal_checks_depth_and_existing_scope_count_before_inserting_the_current_scope() {
    let cfg = CfgSet::unconditional();
    for depth in [31, 32, 33] {
        let mut modules = Modules::default();
        let mut module = vec!["nested".into(); depth];
        modules.items(&[], &mut module, &cfg);
        assert_eq!(modules.scopes.contains_key(&module), depth <= 32);
    }
    for count in [4095, 4096, 4097] {
        let mut modules = Modules {
            scopes: (0..count)
                .map(|index| (vec![format!("module{index}")], Some(Namespace::default())))
                .collect(),
        };
        modules.items(&[], &mut Vec::new(), &cfg);
        assert_eq!(modules.scopes.contains_key(&Vec::new()), count <= 4096);
    }
}

#[test]
fn enum_character_constructors_require_inert_single_char_payloads_and_keep_the_exact_synthetic_function() {
    for (source, expected) in [
        ("enum Character { Value(char) }", true),
        ("enum Character { #[allow(dead_code)] Value(#[allow(dead_code)] char) }", true),
        ("enum Character { Value() }", false),
        ("enum Character { Value(char, char) }", false),
        ("enum Character { Value { character: char } }", false),
        ("enum Character { Value(u8) }", false),
        ("enum Character { Value(std::primitive::char) }", false),
        ("enum Character { #[expand] Value(char) }", false),
        ("enum Character { Value(#[expand] char) }", false),
        ("enum Character<T> { Value(char), Other(T) }", false),
    ] {
        let ns = namespaces(source);
        let function = ns.function_in("", &[], &path(&["Character", "Value"]));
        assert_eq!(function.is_some(), expected, "{source}");
        if let Some(function) = function {
            assert!(function.char_constructor);
            assert_eq!(function.syntax, "fn value (character : char) -> char { character }");
            assert!(function.module.is_empty());
        }
    }
}

#[test]
fn declaration_variants_keep_trait_and_opacity_flags_independent_from_namespace_membership() {
    for (source, nominal, value, traits, opaque, std_shadow) in [
        ("struct Marker;", true, false, false, false, false),
        ("enum Marker { A }", true, false, false, false, false),
        ("type Marker = bool;", true, false, false, false, false),
        ("union Marker { value: u8 }", true, false, false, false, false),
        ("trait Marker {}", true, false, true, false, false),
        ("const Marker: bool = true;", false, true, false, false, false),
        ("static Marker: bool = true;", false, true, false, false, false),
        ("trait Marker = Iterator;", false, false, false, true, false),
        ("#[expand] use external::Marker;", false, false, false, true, false),
        ("#[expand] mod Marker {}", true, false, false, true, false),
        ("#[expand] macro_rules! Marker { () => {} }", false, false, false, true, false),
        ("extern crate other as std;", false, false, false, true, true),
        ("mod std {}", false, false, false, false, true),
        ("mod core {}", false, false, false, false, true),
        ("mod alloc {}", false, false, false, false, true),
    ] {
        let ns = namespaces(source);
        let namespace = ns.local.scopes[&Vec::new()].as_ref().unwrap();
        assert_eq!(namespace.nominal.contains("Marker"), nominal, "{source}");
        assert_eq!(namespace.values.contains("Marker"), value, "{source}");
        assert_eq!(namespace.traits, traits, "{source}");
        assert_eq!(namespace.opaque, opaque, "{source}");
        assert_eq!(namespace.std_shadow, std_shadow, "{source}");
    }
}

#[test]
fn every_supported_declaration_exposes_its_own_attributes() {
    for source in [
        "const VALUE: bool = true;",
        "enum Choice { A }",
        "extern crate dependency;",
        "fn helper() {}",
        "unsafe extern \"C\" { fn foreign(); }",
        "impl Record {}",
        "macro_rules! empty { () => {} }",
        "mod nested {}",
        "static VALUE: bool = true;",
        "struct Record;",
        "trait Contract {}",
        "trait Alias = Iterator;",
        "type Alias = bool;",
        "union Record { value: bool }",
        "use dependency::Record;",
    ] {
        let item: Item = parse_str(&format!("#[doc = \"contract\"] {source}")).unwrap();
        let expected: Attribute = parse_quote!(#[doc = "contract"]);
        assert_eq!(item_attributes(&item), Some(&[expected][..]), "{source}");
    }
}

#[test]
fn declaration_attributes_require_known_derive_identity_and_validate_every_conditional_attribute() {
    for (attribute, accepted) in [
        (
            parse_quote!(#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]),
            true,
        ),
        (parse_quote!(#[custom(Clone)]), false),
        (parse_quote!(#[derive(custom::Clone)]), false),
        (parse_quote!(#[derive(Unknown)]), false),
        (parse_quote!(#[cfg_attr(test, allow(dead_code))]), true),
        (parse_quote!(#[cfg_attr(test, custom, allow(dead_code))]), false),
        (parse_quote!(#[cfg_attr(test, allow(dead_code), custom)]), false),
        (parse_quote!(#[cfg_attr(test, cfg_attr(feature = "x", custom))]), false),
        (parse_quote!(#[cfg_attr(test)]), false),
        (parse_quote!(#[cfg_attr]), false),
    ] {
        assert_eq!(declaration_attributes(&[attribute]), accepted);
    }
}

#[test]
fn namespace_initializer_budget_accepts_exactly_512_expression_nodes_and_checks_attributes() {
    let mut array: ExprArray = parse_quote!([]);
    array.elems.extend(core::iter::repeat_n::<Expr>(parse_quote!(0), 511));
    assert!(namespace_input(&Expr::Array(array.clone())));
    array.elems.push(parse_quote!(0));
    assert!(!namespace_input(&Expr::Array(array)));
    for (expression, accepted) in [
        (
            parse_quote!(
                #[allow(dead_code)]
                true
            ),
            true,
        ),
        (
            parse_quote!(
                #[custom]
                true
            ),
            false,
        ),
        (
            parse_quote!((
                true,
                #[custom]
                false
            )),
            false,
        ),
    ] {
        assert_eq!(namespace_input(&expression), accepted);
    }
}

#[test]
fn thread_local_input_limit_is_inclusive_and_rejects_nonstatic_or_expanding_items() {
    let mut mac: Macro = parse_quote!(thread_local! {});
    let empty = "";
    let overhead = quote!(static VALUE: &str = #empty;).to_string().len();
    for (length, accepted) in [(8191, true), (8192, true), (8193, false)] {
        let text = "a".repeat(length - overhead);
        mac.tokens = quote!(static VALUE: &str = #text;);
        assert_eq!(mac.tokens.to_string().len(), length);
        assert_eq!(thread_local_body(&mac), accepted, "{length}");
    }
    for (tokens, accepted) in [
        (quote!(), false),
        (
            quote!(
                const VALUE: u8 = 0;
            ),
            false,
        ),
        (
            quote!(
                static A: u8 = 0;
                static B: u8 = 1;
            ),
            true,
        ),
        (
            quote!(
                static VALUE: u8 = unknown!();
            ),
            false,
        ),
        (
            quote!(
                #[custom]
                static VALUE: u8 = 0;
            ),
            false,
        ),
        (quote!(static VALUE: u8 = ;), false),
    ] {
        mac.tokens = tokens;
        assert_eq!(thread_local_body(&mac), accepted);
    }
}

#[test]
fn identifier_capture_budget_counts_every_group_and_forbids_nonidentifier_fragments() {
    let token = quote!(name);
    for (count, accepted) in [(255, true), (256, true), (257, false)] {
        assert_eq!(
            identifier_captures(core::iter::repeat_n(token.clone(), count).collect()),
            accepted,
            "{count}"
        );
    }
    for (tokens, accepted) in [
        (quote!($value:ident), true),
        (quote!(($value:ident)), true),
        (quote!($value:expr), false),
        (quote!(($value:expr)), false),
        (quote!($value:), false),
    ] {
        assert_eq!(identifier_captures(tokens), accepted);
    }
    let nested: TokenStream = core::iter::repeat_n(token, 256).collect();
    let group = TokenTree::Group(proc_macro2::Group::new(Delimiter::Parenthesis, nested));
    assert!(!identifier_captures(core::iter::once(group).collect()));
}

#[test]
fn generated_module_body_limits_spend_exact_work_and_reject_escaping_expansions() {
    let token = quote!(name);
    let tokens: TokenStream = core::iter::repeat_n(token, 8).collect();
    for (budget, accepted, remaining) in [(7, false, 7), (8, true, 0), (9, true, 1)] {
        let mut actual = budget;
        assert_eq!(safe_module_body(tokens.clone(), &mut actual), accepted);
        assert_eq!(actual, remaining);
    }
    for (tokens, accepted) in [
        (
            quote!(
                fn helper() {}
            ),
            true,
        ),
        (
            quote!(
                #[allow(dead_code)]
                fn helper() {}
            ),
            true,
        ),
        (
            quote!(
                #[custom]
                fn helper() {}
            ),
            false,
        ),
        (quote!(unknown!()), false),
        (quote!({ unknown!() }), false),
        (quote!(macro_export), false),
        (quote!(macro_use), false),
        (quote!(#(allow(dead_code))), false),
    ] {
        assert_eq!(safe_module_body(tokens, &mut 256), accepted);
    }
}

#[test]
fn generated_modules_are_isolated_and_may_not_shadow_any_modeled_builtin() {
    for name in [
        "std", "core", "alloc", "Vec", "String", "Option", "Some", "None", "str", "char", "bool", "usize", "isize", "u8", "u16", "u32",
        "u64", "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64",
    ] {
        let name = Ident::new(name, Span::call_site());
        assert!(!only_modules(quote!(mod #name {})), "{name}");
    }
    for (tokens, accepted) in [
        (
            quote!(
                mod child {}
            ),
            true,
        ),
        (
            quote!(
                pub mod child {
                    fn helper() {}
                }
            ),
            true,
        ),
        (
            quote!(
                #[allow(dead_code)]
                mod child {}
            ),
            true,
        ),
        (
            quote!(
                fn helper() {}
            ),
            false,
        ),
        (
            quote!(
                struct Record {}
            ),
            false,
        ),
        (
            quote!(
                trait Extension {
                    fn next(&self);
                }
            ),
            false,
        ),
        (
            quote!(
                mod child;
            ),
            false,
        ),
        (quote!(mod child()), false),
        (
            quote!(
                #[custom]
                mod child {}
            ),
            false,
        ),
        (
            quote!(
                mod child {
                    nested!();
                }
            ),
            false,
        ),
        (quote!(mod child {} escaped!();), false),
    ] {
        assert_eq!(only_modules(tokens), accepted);
    }
}

#[test]
fn harmless_macro_definitions_validate_each_arm_and_accept_the_exact_token_budget() {
    let mut mac: Macro = parse_quote!(macro_rules! { () => { mod child {} } });
    let arm = mac.tokens.clone();
    for (count, accepted) in [(63, true), (64, true), (65, false)] {
        mac.tokens = core::iter::repeat_n(arm.clone(), count).collect();
        assert_eq!(harmless_definition(&mac), accepted, "{count}");
    }
    for (arms, separators, accepted) in [(63, 3, true), (64, 0, true), (64, 1, false)] {
        mac.tokens = TokenStream::new();
        for ordinal in 0..arms {
            mac.tokens.extend(arm.clone());
            if ordinal < separators {
                mac.tokens.extend(quote!(;));
            }
        }
        assert_eq!(mac.tokens.clone().into_iter().count(), arms * 4 + separators);
        assert_eq!(harmless_definition(&mac), accepted, "{arms}: {separators}");
    }
    for (tokens, accepted) in [
        (quote!(), false),
        (quote!(() => { mod child {} }), true),
        (quote!(() => { mod child {} }; ($name:ident) => { mod nested {} }), true),
        (quote!(() => { mod child {} }; ($name:expr) => { mod nested {} }), false),
        (quote!(() => { mod child {} }; () => { fn escaped() {} }), false),
        (
            quote!(
                () = {
                    mod child {}
                }
            ),
            false,
        ),
        (quote!(() -> { mod child {} }), false),
        (quote!(() = + { mod child {} }), false),
        (quote!(() => name), false),
        (quote!(name => { mod child {} }), false),
    ] {
        mac.tokens = tokens;
        assert_eq!(harmless_definition(&mac), accepted);
    }
}

#[test]
fn recursive_macro_budgets_reject_an_outer_tail_after_a_group_spends_the_last_token() {
    let inner: TokenStream = core::iter::repeat_n(quote!(name), 255).collect();
    let capture = quote!((#inner) tail);
    assert!(!identifier_captures(capture));

    let mut remaining = 3;
    assert!(!safe_module_body(quote!((first second) tail), &mut remaining));
    assert_eq!(remaining, 0);
}

#[test]
fn generated_module_name_collection_never_returns_partial_names_after_exhaustion() {
    for count in [1023, 1024, 1025] {
        let mut mac: Macro = parse_quote!(macro_rules! {});
        mac.tokens = quote!(mod retained);
        mac.tokens.extend(core::iter::repeat_n(quote!(name), count - 2));
        assert_eq!(
            generated_modules(&mac),
            (count <= 1024).then(|| HashSet::from_iter(["retained".to_owned()])),
            "{count}"
        );
    }
    for inner_count in [1023, 1024] {
        let inner: TokenStream = core::iter::repeat_n(quote!(name), inner_count).collect();
        let mut mac: Macro = parse_quote!(macro_rules! {});
        mac.tokens = quote!((#inner) tail);
        assert!(generated_modules(&mac).is_none(), "{inner_count}");
    }
}

#[test]
fn macro_and_initializer_budgets_do_not_admit_partial_namespace_proofs() {
    let mut array: ExprArray = parse_quote!([0]);
    array.elems.extend(core::iter::repeat_n(array.elems[0].clone(), 512));
    assert!(!namespace_input(&Expr::Array(array)));
    let mut mac: Macro = parse_quote!(thread_local! {});
    let declaration = quote!(
        static LONG_NAME: u8 = 0;
    );
    mac.tokens = core::iter::repeat_n(declaration, 512).collect();
    assert!(!thread_local_body(&mac));
    let mut mac: Macro = parse_quote!(macro_rules! { () => {} });
    let arm = mac.tokens.clone();
    mac.tokens = core::iter::repeat_n(arm, 65).collect();
    assert!(!harmless_definition(&mac));
    let token = quote!(ident);
    assert!(!identifier_captures(core::iter::repeat_n(token, 1025).collect()));
    let mut ns = Namespaces {
        local_cost: 1024 * 1024 * 1024,
        ..Namespaces::default()
    };
    ns.label("lib.rs");
    assert!(ns.exhausted);
    assert!(ns.pending.is_empty());
}

#[test]
fn source_span_limits_reject_oversized_namespace_and_serde_inputs() {
    let source = format!("struct Head; {} struct Tail;", " ".repeat(1024 * 1024));
    let file = parse_file(&source).unwrap();
    let modules = Modules::of(&file, &CfgSet::unconditional());
    assert!(modules.scopes[&Vec::new()].as_ref().unwrap().opaque);
    assert!(!modules.transparent(&[], |namespace| namespace.opaque));
    let source = format!("struct Oversized {{ {} value: u8 }}", " ".repeat(16384));
    assert!(!serde_input(&parse_str(&source).unwrap()));
}

fn namespaces(source: &str) -> Namespaces {
    Namespaces::of(&parse_file(source).unwrap(), &CfgSet::unconditional())
}

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).into()).collect()
}

#[test]
fn nontrait_import_chains_resolve_functions_macros_and_nominal_types_but_not_cycles() {
    let ns = namespaces(
        "struct Record; fn helper() {} macro_rules! empty { () => {} }
         use self::Record as First; use self::First as Second;
         use self::helper as Helper; use self::empty as Macro;
         use self::LoopB as LoopA; use self::LoopA as LoopB;
         use external::Missing as Foreign;
         mod nested { pub struct Value; }
         use self::nested::Value as Nested;",
    );
    for (name, expected) in [
        ("Second", true),
        ("Helper", true),
        ("Macro", true),
        ("Nested", true),
        ("LoopA", false),
        ("Foreign", false),
        ("missing", false),
    ] {
        assert_eq!(ns.nontrait("", &[], &path(&["self", name])), expected, "{name}");
    }
    assert!(!ns.closed_methods("", &path(&["missing"])));
    assert!(!ns.width_contract("", &path(&["missing"]), true));
    assert!(!ns.stable_path_for("", &path(&["missing"]), &parse_quote!(String)));
    let opaque = namespaces("use dependency::*;");
    assert!(!opaque.nontrait("", &[], &path(&["self", "Missing"])));
    assert!(!opaque.stable_path_for("", &[], &parse_quote!(String)));
}

#[test]
fn nontrait_external_reexports_preserve_dependency_root_and_reject_empty_package_identity() {
    let mut library = namespaces("pub struct Record; pub use self::Record as Exported;");
    library.label("dependency.rs");
    let mut ns = namespaces("use dependency::Exported as Imported;");
    ns.label("lib.rs");
    ns.absorb(library);
    ns.bind(&[
        ("lib.rs".into(), "lib.rs".into(), Vec::new()),
        ("dependency.rs".into(), "dependency.rs".into(), Vec::new()),
    ]);
    let edge = ResourceDependency {
        crate_root: "lib.rs".into(),
        import_name: "dependency".into(),
        package_id: "dependency 1.0.0".into(),
        package_name: "dependency".into(),
        version: "1.0.0".into(),
        source: String::new(),
        features: Vec::new(),
        registry_closure: false,
        library_root: Some("dependency.rs".into()),
    };
    ns.dependencies(core::slice::from_ref(&edge));
    assert!(ns.nontrait("lib.rs", &[], &path(&["self", "Imported"])));
    ns.dependencies.insert(
        ("lib.rs".into(), "dependency".into()),
        Some(ResourceDependency {
            package_id: String::new(),
            ..edge
        }),
    );
    assert!(!ns.nontrait("lib.rs", &[], &path(&["self", "Imported"])));
}

#[test]
fn declaration_and_serde_classification_rejects_expansion_and_accepts_nominal_derive_shapes() {
    let ns = namespaces(
        r#"#[derive(serde::Serialize)] #[serde(rename = "record")] struct Record { value: u8 } #[derive(serde::Deserialize)] #[serde(rename = "choice")] enum Choice { A }"#,
    );
    let namespace = ns.local.scopes[&Vec::new()].as_ref().unwrap();
    assert!(namespace.serde_nominal.contains("Record"));
    assert!(namespace.serde_nominal.contains("Choice"));
    let ordinary = namespaces("struct Record; enum Choice { A }");
    assert!(ordinary.local.scopes[&Vec::new()].as_ref().unwrap().serde_nominal.is_empty());
    let alias: Item = parse_quote!(
        trait Alias = Iterator;
    );
    assert!(item_attributes(&alias).is_some());
    assert!(
        namespaces("trait Alias = Iterator;").local.scopes[&Vec::new()]
            .as_ref()
            .unwrap()
            .opaque
    );
    assert!(item_attributes(&Item::Verbatim(TokenStream::new())).is_none());
    for attributes in [vec![parse_quote!(#[serde(crate = "custom")])], vec![parse_quote!(#[custom])]] {
        assert!(serde_attributes(&attributes).is_none());
    }
}

#[test]
fn inactive_declarations_leave_no_namespace_evidence_and_expanding_attributes_cannot_admit_nominals() {
    for declaration in [
        "struct Marker;",
        "enum Marker { A }",
        "type Marker = bool;",
        "union Marker { value: bool }",
        "trait Marker {}",
        "const Marker: bool = true;",
        "static Marker: bool = true;",
        "fn Marker() {}",
        "impl Marker { fn helper() {} }",
        "macro_rules! Marker { () => {} }",
        "use external::Marker;",
        "extern crate external as std;",
        "mod Marker {}",
    ] {
        let ns = namespaces(&format!("#[cfg(any())] {declaration}"));
        assert_eq!(ns.local.scopes[&Vec::new()], Some(Namespace::default()), "{declaration}");
    }
    for declaration in [
        "struct Marker;",
        "enum Marker { A }",
        "type Marker = bool;",
        "union Marker { value: bool }",
        "fn Marker() {}",
    ] {
        let ns = namespaces(&format!("#[expand] {declaration}"));
        let namespace = ns.local.scopes[&Vec::new()].as_ref().unwrap();
        assert!(namespace.opaque, "{declaration}");
        assert!(namespace.nominal.is_empty(), "{declaration}");
        assert!(namespace.records.is_empty(), "{declaration}");
        assert!(namespace.functions.is_empty(), "{declaration}");
    }
    let ns = namespaces("#[expand] trait Contract { fn helper() {} }");
    assert_eq!(
        ns.local.scopes[&Vec::new()].as_ref().unwrap().trait_methods.get("Contract"),
        Some(&None)
    );
}

#[test]
fn conditional_inherent_methods_keep_the_owners_full_path_even_without_a_cached_body() {
    let ns = namespaces("struct Owner; impl Owner { #[cfg(unix)] fn conditional() {} }");
    assert_eq!(
        ns.local.scopes[&Vec::new()]
            .as_ref()
            .unwrap()
            .functions
            .get(&path(&["Owner", "conditional"])),
        Some(&None)
    );
    assert!(ns.nontrait("", &[], &path(&["crate", "Owner", "conditional"])));
    assert!(ns.function_in("", &[], &path(&["Owner", "conditional"])).is_none());
}

#[test]
fn serde_input_spends_attribute_and_type_allowances_independently_and_ignores_doc_attributes() {
    for count in [510, 511, 512, 513] {
        let mut record: ItemStruct = parse_quote!(
            struct Record;
        );
        record.attrs = vec![parse_quote!(#[allow(dead_code)]); count];
        assert_eq!(serde_input(&Item::Struct(record)), count < 512, "attributes: {count}");

        let mut record: ItemStruct = parse_quote!(
            struct Record(u8);
        );
        let Fields::Unnamed(fields) = &mut record.fields else {
            unreachable!()
        };
        let field = fields.unnamed[0].clone();
        fields.unnamed = core::iter::repeat_n(field, count).collect();
        assert_eq!(serde_input(&Item::Struct(record)), count < 512, "types: {count}");
    }
    let mut record: ItemStruct = parse_quote!(
        struct Record;
    );
    record.attrs = vec![parse_quote!(#[doc = "retained documentation"]); 513];
    assert!(serde_input(&Item::Struct(record)));
}

#[test]
fn serde_input_byte_budget_is_inclusive_even_when_the_whole_input_is_documentation() {
    let prefix = "#[doc = \"";
    let suffix = "\"] struct Record;";
    for length in [16383, 16384, 16385] {
        let source = format!("{prefix}{}{suffix}", "x".repeat(length - prefix.len() - suffix.len()));
        assert_eq!(source.len(), length);
        assert_eq!(serde_input(&parse_str(&source).unwrap()), length <= 16384, "{length}");
    }
}

#[test]
fn conditional_derive_metadata_is_not_inert_on_imports_modules_or_macro_definitions() {
    for attribute in ["#[cfg_attr(any(), derive(Clone))]", "#[must_use]"] {
        for declaration in ["use external::Record;", "mod nested {}", "macro_rules! empty { () => {} }"] {
            let ns = namespaces(&format!("{attribute} {declaration}"));
            assert!(ns.local.scopes[&Vec::new()].as_ref().unwrap().opaque, "{attribute} {declaration}");
        }
    }
}

#[test]
fn partial_layouts_and_qualified_or_wildcard_macro_calls_cannot_be_treated_as_transparent() {
    let mut ns = namespaces("struct Record;");
    ns.label("nested.rs");
    ns.bind(&[("nested.rs".into(), "lib.rs".into(), path(&["nested"]))]);
    assert!(!ns.transparent("nested.rs", &[]));

    for source in [
        "macro_rules! safe { () => {} } mod safe { pub use external::unknown as other; }
         safe::other!(); mod nested {}",
        "use external::*; thread_local! { static VALUE: u8 = 0; } mod nested {}",
    ] {
        assert!(!namespaces(source).transparent("", &path(&["nested"])), "{source}");
    }
}

#[test]
fn non_cfg_attributes_cannot_disable_items_or_make_unconditional_functions_conditional() {
    let attributes = vec![parse_quote!(#[custom(any())])];
    assert!(potentially_active(&CfgSet::unconditional(), &attributes));
    assert!(!namespaces("#[custom(any())] struct Record;").primitive_scope("", &[]));
    assert!(cfg_invariant(&[parse_quote!(#[doc = "unconditional"])]));
    assert!(
        namespaces("#[doc = \"unconditional\"] fn helper() {}")
            .function_in("", &[], &path(&["helper"]))
            .is_some()
    );
}

#[test]
fn namespace_budgets_reject_large_inputs_without_retaining_partial_evidence() {
    let mut file = parse_file("struct Record;").unwrap();
    let mut record: ItemStruct = parse_quote!(
        struct Other;
    );
    record.ident.set_span(Span::call_site());
    file.items.push(Item::Struct(record));
    let mut modules = Modules::default();
    modules.items(&file.items, &mut vec!["deep".into(); 33], &CfgSet::unconditional());
    assert!(modules.scopes.is_empty());
    assert!(!modules.transparent(&path(&["missing"]), |_| false));
    let mut ns = Namespaces {
        exhausted: true,
        ..Namespaces::default()
    };
    ns.costs(&Namespaces::default());
    ns.extend(&Namespaces::default());
    assert!(ns.pending.is_empty());

    let mut record: ItemStruct = parse_quote!(
        struct Many(u8);
    );
    let field = record.fields.iter().next().unwrap().clone();
    let Fields::Unnamed(fields) = &mut record.fields else {
        unreachable!()
    };
    fields.unnamed.extend(core::iter::repeat_n(field, 513));
    assert!(!serde_input(&Item::Struct(record)));
    let mut record: ItemStruct = parse_quote!(
        struct Many;
    );
    record.attrs = vec![parse_quote!(#[allow(dead_code)]); 513];
    assert!(!serde_input(&Item::Struct(record)));
}

#[test]
fn primitive_shadowing_preserves_root_and_sibling_scope_evidence() {
    for name in ["str", "String", "Vec", "char", "usize", "bool", "Some", "None", "Option"] {
        for declaration in [
            format!("struct {name};"),
            format!("const {name}: u8 = 0;"),
            format!("fn {name}() {{}}"),
            format!("use external::Other as {name};"),
        ] {
            let source = format!("mod affected {{ {declaration} mod nested {{}} }} mod unaffected {{}}");
            let ns = namespaces(&source);
            assert!(ns.primitive_scope_for("", &[]), "{declaration}");
            assert!(ns.primitive_scope_for("", &path(&["unaffected"])), "{declaration}");
            assert!(!ns.primitive_scope_for("", &path(&["affected"])), "{declaration}");
        }
    }
}

#[test]
fn bound_namespace_queries_use_the_file_prefix_and_inline_module_together() {
    let mut ns = namespaces("mod branch;");
    ns.label("src\\lib.rs");
    let mut branch = namespaces("mod nested { struct Record; use width::UnicodeWidthChar; fn value() -> char { 'q' } }");
    branch.label("src\\branch.rs");
    ns.absorb(branch);
    ns.bind(&[
        ("src\\lib.rs".into(), "src\\lib.rs".into(), Vec::new()),
        ("src\\branch.rs".into(), "src\\lib.rs".into(), path(&["branch"])),
    ]);
    ns.dependencies(&[ResourceDependency {
        crate_root: "src\\lib.rs".into(),
        import_name: "width".into(),
        package_id: "unicode-width 0.2.2".into(),
        package_name: "unicode-width".into(),
        version: "0.2.2".into(),
        source: "registry+https://github.com/rust-lang/crates.io-index".into(),
        features: Vec::new(),
        registry_closure: true,
        library_root: None,
    }]);
    ns.alias("src\\branch.rs", "display\\branch.rs");
    for source in ["src/branch.rs", "display\\branch.rs"] {
        assert!(ns.transparent(source, &path(&["nested"])));
        assert!(ns.primitive_scope_for(source, &path(&["nested"])));
        assert!(ns.closed_methods_for(source, &path(&["nested"])));
        assert!(ns.width_contract_for(source, &path(&["nested"])));
        assert!(ns.local_record_for(source, &path(&["nested"]), "Record"));
        assert!(ns.stable_path_for(source, &path(&["nested"]), &parse_quote!(String)));
        let function = ns.function(source, &path(&["nested"]), &path(&["value"])).unwrap();
        assert_eq!(function.root, "src/lib.rs");
        assert_eq!(function.module, path(&["branch", "nested"]));
        let parsed: ItemFn = parse_str(function.syntax).unwrap();
        assert_eq!(parsed.block, parse_quote!({ 'q' }));
        assert!(!function.char_constructor);
        assert!(!ns.width_contract_for(source, &[]));
        assert!(!ns.local_record_for(source, &[], "Record"));
    }
    assert!(!ns.width_contract_for("src/lib.rs", &[]));
    assert!(!ns.width_contract_for("unknown", &path(&["nested"])));
}

#[test]
fn grouping_dependency_imports_reject_lookalike_modules_and_type_names() {
    for (package, version, import, accepted) in [
        ("serde", "1.0.228", "dep::de::IgnoredAny", true),
        ("serde", "1.0.228", "dep::other::IgnoredAny", false),
        ("serde", "1.0.228", "dep::de::Other", false),
        ("serde", "1.0.228", "dep::IgnoredAny", false),
        ("serde", "1.0.228", "dep::de::IgnoredAny::Nested", false),
        ("serde", "1.0.228", "dep::Serialize", true),
        ("serde", "1.0.228", "dep::Serializer", true),
        ("serde", "1.0.228", "dep::Deserialize", true),
        ("serde", "1.0.228", "dep::Deserializer", true),
        ("camino", "1.2.5", "dep::Utf8Path", true),
        ("camino", "1.2.5", "dep::Utf8PathBuf", true),
        ("saphyr-parser", "0.0.12", "dep::Event", true),
        ("saphyr-parser", "0.0.12", "dep::Parser", true),
        ("tick", "0.7.0", "dep::SimpleClock", true),
        ("tick", "0.7.0", "dep::SystemTimeExt", true),
        ("tick", "0.7.0", "dep::Other", false),
        ("other", "1.0.228", "dep::Serialize", false),
        ("serde", "1.0.227", "dep::Serialize", false),
    ] {
        let mut ns = namespaces(&format!("use {import};"));
        ns.dependencies(&[ResourceDependency {
            crate_root: String::new(),
            import_name: "dep".into(),
            package_id: format!("{package} {version}"),
            package_name: package.into(),
            version: version.into(),
            source: "registry+https://github.com/rust-lang/crates.io-index".into(),
            features: Vec::new(),
            registry_closure: true,
            library_root: None,
        }]);
        assert_eq!(ns.closed_methods_for("", &[]), accepted, "{package} {version}: {import}");
    }
}
