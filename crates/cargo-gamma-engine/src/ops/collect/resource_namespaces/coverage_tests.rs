// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[test]
fn macro_and_initializer_budgets_do_not_admit_partial_namespace_proofs() {
    let mut array: syn::ExprArray = syn::parse_quote!([0]);
    array.elems.extend(core::iter::repeat_n(array.elems[0].clone(), 512));
    assert!(!namespace_input(&syn::Expr::Array(array)));
    let mut mac: Macro = syn::parse_quote!(thread_local! {});
    let declaration = quote::quote!(
        static LONG_NAME: u8 = 0;
    );
    mac.tokens = core::iter::repeat_n(declaration, 512).collect();
    assert!(!thread_local_body(&mac));
    let mut mac: Macro = syn::parse_quote!(macro_rules! { () => {} });
    let arm = mac.tokens.clone();
    mac.tokens = core::iter::repeat_n(arm, 65).collect();
    assert!(!harmless_definition(&mac));
    let token = quote::quote!(ident);
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
    let file = syn::parse_file(&source).unwrap();
    let modules = Modules::of(&file, &CfgSet::unconditional());
    assert!(modules.scopes[&Vec::new()].as_ref().unwrap().opaque);
    assert!(!modules.transparent(&[], |namespace| namespace.opaque));
    let source = format!("struct Oversized {{ {} value: u8 }}", " ".repeat(16384));
    assert!(!serde_input(&syn::parse_str(&source).unwrap()));
}

fn namespaces(source: &str) -> Namespaces {
    Namespaces::of(&syn::parse_file(source).unwrap(), &CfgSet::unconditional())
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
    assert!(!ns.stable_path_for("", &path(&["missing"]), &syn::parse_quote!(String)));
    let opaque = namespaces("use dependency::*;");
    assert!(!opaque.nontrait("", &[], &path(&["self", "Missing"])));
    assert!(!opaque.stable_path_for("", &[], &syn::parse_quote!(String)));
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
    let alias: Item = syn::parse_quote!(
        trait Alias = Iterator;
    );
    assert!(item_attributes(&alias).is_some());
    assert!(
        namespaces("trait Alias = Iterator;").local.scopes[&Vec::new()]
            .as_ref()
            .unwrap()
            .opaque
    );
    assert!(item_attributes(&Item::Verbatim(proc_macro2::TokenStream::new())).is_none());
    for attributes in [
        vec![syn::parse_quote!(#[serde(crate = "custom")])],
        vec![syn::parse_quote!(#[custom])],
    ] {
        assert!(serde_attributes(&attributes).is_none());
    }
}

#[test]
fn namespace_budgets_reject_large_inputs_without_retaining_partial_evidence() {
    let mut file = syn::parse_file("struct Record;").unwrap();
    let mut record: syn::ItemStruct = syn::parse_quote!(
        struct Other;
    );
    record.ident.set_span(proc_macro2::Span::call_site());
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

    let mut record: syn::ItemStruct = syn::parse_quote!(
        struct Many(u8);
    );
    let field = record.fields.iter().next().unwrap().clone();
    let syn::Fields::Unnamed(fields) = &mut record.fields else {
        unreachable!()
    };
    fields.unnamed.extend(core::iter::repeat_n(field, 513));
    assert!(!serde_input(&Item::Struct(record)));
    let mut record: syn::ItemStruct = syn::parse_quote!(
        struct Many;
    );
    record.attrs = vec![syn::parse_quote!(#[allow(dead_code)]); 513];
    assert!(!serde_input(&Item::Struct(record)));
}
