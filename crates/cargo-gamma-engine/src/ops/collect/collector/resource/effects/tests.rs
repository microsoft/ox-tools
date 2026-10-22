// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::fixture;
use crate::cfg::CfgSet;
use crate::ops::collect::collect_in;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

#[test]
fn character_width_helpers_require_the_pinned_dependency_contract() {
    let file = syn::parse_file(
        "use width::UnicodeWidthChar;
         fn helper(value: char) -> usize { value.width().unwrap_or(0) }",
    )
    .unwrap();
    let mut sources = super::Namespaces::of(&file, &CfgSet::unconditional());
    assert!(super::certify(&sources, sources.function("", &[], &["helper".into()]).unwrap()).is_none());
    sources.dependencies(&[crate::ops::collect::ResourceDependency {
        crate_root: String::new(),
        import_name: "width".into(),
        package_id: "registry package identity".into(),
        package_name: "unicode-width".into(),
        version: "0.2.2".into(),
        source: "registry+https://github.com/rust-lang/crates.io-index".into(),
        features: Vec::new(),
        registry_closure: true,
        library_root: None,
    }]);
    let certified = super::certify(&sources, sources.function("", &[], &["helper".into()]).unwrap()).unwrap();
    assert_eq!(certified.parameters, [super::Binding::Char]);
    assert_eq!(certified.output, super::Binding::Integer);
}

#[test]
fn helper_effect_and_cfg_uncertainty_preserve_finite_padding_signal() {
    let source = include_str!("fixture.rs");
    let file = SourceFile::parse("effects.rs", source.into()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("stmt.delete_call").unwrap(), &CfgSet::unconditional());
    for (function, retained) in [("pure_padding", false), ("conditional_padding", true), ("volatile_padding", true)] {
        let found = candidates
            .iter()
            .filter(|candidate| &*candidate.item_path == function && file.slice(&candidate.span).starts_with("rows.push"))
            .count();
        assert_eq!(found, usize::from(retained), "{function}");
    }
}

#[test]
fn opaque_helper_has_a_compiled_finite_error_counterexample() {
    let mut rows = vec!["initial".into()];
    let mut reads = 0;
    fixture::volatile_padding(&mut rows, 2, &mut reads);
    assert_eq!(rows.len(), 2);
    assert_eq!(reads, 1);
    rows.truncate(1);
    reads = 0;
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixture::volatile_deleted(&mut rows, 2, &mut reads)));
    assert!(failure.is_err());
    assert_eq!(reads, 3);
    assert_eq!(rows.len(), 1);
    fixture::pure_padding(&mut rows, 2);
    fixture::conditional_padding(&mut rows, 3);
    assert_eq!(rows.len(), 3);
}

#[test]
fn scalar_and_string_helpers_certify_exact_parameter_and_result_bindings() {
    use super::{Binding, Namespaces, certify};

    for (source, parameters, output) in [
        (
            "fn helper(value: &str) -> usize { value.len() }",
            vec![Binding::StrReference],
            Binding::Integer,
        ),
        (
            "fn helper(value: String) -> usize { value.len() }",
            vec![Binding::String],
            Binding::Integer,
        ),
        (
            "fn helper(value: char) -> bool { matches!(value, 'a'..='z' | 'A'..='Z' | '_') }",
            vec![Binding::Char],
            Binding::Bool,
        ),
        (
            "fn helper(value: bool) -> bool { if value { true } else { false } }",
            vec![Binding::Bool],
            Binding::Bool,
        ),
        (
            "fn helper(value: &str) -> String { let mut result = String::with_capacity(value.len()); for ch in value.chars() { result.push(ch); } result }",
            vec![Binding::StrReference],
            Binding::String,
        ),
        (
            "fn helper(value: &str) -> String { let mut result = String::with_capacity(value.len()); let mut chars = value.chars(); while let Some(ch) = chars.next() { result.push(ch); } result }",
            vec![Binding::StrReference],
            Binding::String,
        ),
        (
            "fn helper(value: &str) -> char { let value: char = 'x'; value }",
            vec![Binding::StrReference],
            Binding::Char,
        ),
        (
            "fn helper(value: char, other: char) -> bool { value == other }",
            vec![Binding::Char, Binding::Char],
            Binding::Bool,
        ),
        (
            "fn helper(value: &str) -> bool { for ch in value.chars() { if matches!(ch, 'a' | 'b') { continue; } break; } true }",
            vec![Binding::StrReference],
            Binding::Bool,
        ),
        (
            "fn helper(value: &str) -> usize { nested(value) } fn nested(value: &str) -> usize { value.len() }",
            vec![Binding::StrReference],
            Binding::Integer,
        ),
    ] {
        let file = syn::parse_file(source).unwrap();
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        let function = sources.function("", &[], &["helper".into()]).unwrap();
        let call = certify(&sources, function).unwrap_or_else(|| panic!("should certify {source}"));
        assert_eq!(call.parameters, parameters, "{source}");
        assert_eq!(call.output, output, "{source}");
        assert!(!call.char_option);
    }
}

#[test]
fn helper_certification_rejects_effects_shadowing_recursion_and_wrong_types() {
    use super::{Namespaces, certify};

    for source in [
        "fn helper(ref value: char) -> bool { true }",
        "fn helper(value @ _: char) -> bool { true }",
        "fn helper((value,): (char,)) -> bool { true }",
        "fn helper(value: char) -> bool { let (a, b) = value; true }",
        "fn helper(value: char) -> bool { let ref a = value; true }",
        "fn helper(value: char) -> bool { const X: u8 = 0; true }",
        "fn helper(value: char) -> bool { 1.5 }",
        "fn helper(value: char) -> bool { nested(value, value) } fn nested(value: char) -> bool { true }",
        "fn helper(value: char) -> bool { nested(true) } fn nested(value: char) -> bool { true }",
        "fn helper(value: &str) -> bool { for (ch,) in value.chars() {} true }",
        "fn helper(value: &str) -> bool { let mut chars = value.chars(); while let (ch,) = chars.next() {} true }",
        "fn helper(value: &str) -> bool { let mut chars = value.chars(); while let Some((ch,)) = chars.next() {} true }",
        "fn helper(value: &str) -> bool { let mut chars = value.chars(); while let None = chars.next() {} true }",
        "fn helper(value: &str) -> bool { let mut chars = value.chars(); while let Other(ch) = chars.next() {} true }",
        "async fn helper(value: &str) -> usize { value.len() }",
        "unsafe fn helper(value: &str) -> usize { value.len() }",
        "extern \"C\" fn helper(value: &str) -> usize { value.len() }",
        "fn helper<T>(value: &str) -> usize { value.len() }",
        "fn helper(value: &mut str) -> usize { value.len() }",
        "fn helper(value: u32) -> usize { 0 }",
        "fn helper(value: &str) -> usize { unknown(value) }",
        "fn helper(value: &str) -> usize { helper(value) }",
        "fn helper(value: &str) -> usize { nested(value) } fn nested(value: &str) -> usize { helper(value) }",
        "fn helper(value: &str) -> usize { value.len(1) }",
        "fn helper(value: &str) -> usize { value.len::<usize>() }",
        "fn helper(value: &str) -> usize { value.clear(); 0 }",
        "fn helper(value: &str) -> usize { let value: char = 'x'; value.len() }",
        "fn helper(value: &str) -> usize { let size: bool = value.len(); 0 }",
        "fn helper(value: &str) -> usize { if true { value.len() } else { false } }",
        "fn helper(value: &str) -> usize { if 'x' { value.len() } else { 0 } }",
        "fn helper(value: &str) -> usize { for ch in value { } 0 }",
        "fn helper(value: &str) -> usize { while true { } 0 }",
        "fn helper(value: &str) -> usize { while let None = value.chars().next() {} 0 }",
        "fn helper(value: &str) -> usize { continue; 0 }",
        "fn helper(value: &str) -> usize { break; 0 }",
        "fn helper(value: &str) -> usize { return false; }",
        "fn helper(value: &str) -> bool { matches!(value, 'a') }",
        "fn helper(value: char) -> bool { matches!(value, 1..=7) }",
        "fn helper(value: char) -> bool { matches!(value, 'a'..) }",
        "fn helper(value: char) -> bool { matches!(value, _) }",
        "fn helper(value: char) -> bool { matches!(value, 'a' | 7) }",
        "fn helper(value: &str) -> bool { custom!(value) }",
    ] {
        let file = syn::parse_file(source).unwrap();
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        let function = sources.function("", &[], &["helper".into()]).unwrap();
        assert!(certify(&sources, function).is_none(), "{source}");
    }
}

#[test]
fn effect_checker_handles_unit_return_parentheses_and_bounded_work() {
    use super::*;
    let file = syn::parse_file("fn helper() { let x = (); return; }").unwrap();
    let sources = Namespaces::of(&file, &CfgSet::unconditional());
    let function = sources.function("", &[], &["helper".into()]).unwrap();
    let mut analysis = Analysis {
        sources: &sources,
        remaining: 512,
        active: HashSet::default(),
    };
    let (parameters, result) = analysis.function(function).unwrap();
    assert!(parameters.is_empty());
    assert!(matches!(result, Kind::Unit));
    let mut checker = Checker {
        analysis: &mut analysis,
        root: String::new(),
        module: Vec::new(),
        values: HashMap::default(),
        output: Kind::Unit,
        loops: 0,
    };
    assert!(matches!(checker.expr(&syn::parse_quote!((true))), Some(Kind::Bool)));
    assert!(matches!(checker.expr(&syn::parse_quote!("fixed")), Some(Kind::Str)));
    checker.module.push("missing".into());
    assert!(checker.expr(&syn::parse_quote!("fixed".len())).is_none());
    checker.module.clear();
    assert!(matches!(checker.expr(&syn::parse_quote!(if true {})), Some(Kind::Unit)));
    checker.analysis.remaining = 0;
    assert!(checker.expr(&syn::parse_quote!(true)).is_none());
    assert!(
        checker
            .block(&syn::parse_quote!({
                true;
            }))
            .is_none()
    );
    let function = sources.function("", &[], &["helper".into()]).unwrap();
    assert!(checker.analysis.function(function).is_none());
    assert!(Kind::Chars.binding().is_none());
}

#[test]
fn helper_blocks_and_iterator_bindings_restore_the_enclosing_parameter_kind() {
    use super::{Binding, Namespaces, certify};

    for source in [
        "fn helper(value: &str) -> usize { { let value = 'x'; } value.len() }",
        "fn helper(value: &str) -> usize { for value in value.chars() { if matches!(value, 'x') { continue; } break; } value.len() }",
        "fn helper(value: &str) -> usize { let mut chars = value.chars(); while let Some(value) = chars.next() { if matches!(value, 'x') { continue; } break; } value.len() }",
    ] {
        let file = syn::parse_file(source).expect("the fixed helper declaration is valid Rust syntax");
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        let function = sources
            .function("", &[], &["helper".into()])
            .expect("the fixture declares one unambiguous helper");
        let call = certify(&sources, function).expect("only a finite character iterator and local shadows affect the helper");
        assert_eq!(
            (call.parameters, call.output, call.char_option),
            (vec![Binding::StrReference], Binding::Integer, false)
        );
    }
    for source in [
        "fn helper(value: &str) -> usize { #[custom] let size = value.len(); size }",
        "fn helper(value: &str) -> usize { let size @ _ = value.len(); size }",
        "fn helper(value: &str) -> usize { let size; 0 }",
        "fn helper(value: &str) -> usize { let size = value.len() else { return 0; }; size }",
        "fn helper(value: &str) -> usize { { let local = value.len(); } local }",
        "fn helper(value: &str) -> char { for ch in value.chars() {} ch }",
    ] {
        let file = syn::parse_file(source).expect("syn accepts the fixed unsupported helper syntax");
        let sources = Namespaces::of(&file, &CfgSet::unconditional());
        let function = sources
            .function("", &[], &["helper".into()])
            .expect("unsupported local syntax does not hide the helper declaration");
        assert!(certify(&sources, function).is_none(), "{source}");
    }
}

#[test]
fn helper_certification_accepts_eight_active_functions_but_not_nine() {
    use super::{Binding, Namespaces, certify};

    let file = syn::parse_file(
        "fn helper() -> char { a() }
         fn a() -> char { b() }
         fn b() -> char { c() }
         fn c() -> char { d() }
         fn d() -> char { e() }
         fn e() -> char { f() }
         fn f() -> char { g() }
         fn g() -> char { h() }
         fn h() -> char { 'x' }",
    )
    .expect("the fixed acyclic helper chain is valid Rust syntax");
    let sources = Namespaces::of(&file, &CfgSet::unconditional());
    for (name, expected) in [
        ("b", Some((Vec::new(), Binding::Char, false))),
        ("a", Some((Vec::new(), Binding::Char, false))),
        ("helper", None),
    ] {
        let function = sources
            .function("", &[], &[name.into()])
            .expect("every selected entry is declared in the fixed helper chain");
        assert_eq!(
            certify(&sources, function).map(|call| (call.parameters, call.output, call.char_option)),
            expected,
            "{name}"
        );
    }
}

#[test]
fn certification_work_budget_accounts_for_declarations_statements_and_expressions() {
    use super::*;

    let file = syn::parse_file("fn helper() -> bool { true }").expect("the fixed helper is valid Rust syntax");
    let sources = Namespaces::of(&file, &CfgSet::unconditional());
    for (remaining, expected) in [(2, None), (3, Some((0, Some(Binding::Bool)))), (4, Some((0, Some(Binding::Bool))))] {
        let mut analysis = Analysis {
            sources: &sources,
            remaining,
            active: HashSet::default(),
        };
        let function = sources
            .function("", &[], &["helper".into()])
            .expect("the fixture declares one unambiguous helper");
        assert_eq!(
            analysis
                .function(function)
                .map(|(parameters, output)| (parameters.len(), output.binding())),
            expected,
            "work allowance {remaining}"
        );
    }
}
