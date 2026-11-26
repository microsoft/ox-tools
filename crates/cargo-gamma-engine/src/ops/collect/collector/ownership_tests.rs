// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ownership::unbounded_parameters;
use crate::cfg::CfgSet;
use crate::ops::collect::collect_in;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

fn sites(source: &str, selectors: &str) -> Vec<String> {
    let file = SourceFile::parse("ownership.rs", source.into()).unwrap();
    collect_in(&file, &Selection::parse(selectors).unwrap(), &CfgSet::unconditional())
        .iter()
        .map(|candidate| file.text[candidate.span.clone()].to_owned())
        .collect()
}

#[test]
fn ownership_generation_nested_retry_requires_generic_restoration() {
    let source = r"
        fn retry<P>(mut prepared: P, mut spawn: impl FnMut(P) -> Result<(), P>,
                    mut backoff: impl FnMut(P) -> P) {
            for _ in 0..3 {
                match spawn(prepared) {
                    Ok(()) => return,
                    Err(returned) => { prepared = backoff(returned); }
                }
            }
        }";
    assert_eq!(sites(source, "stmt.delete_assign"), Vec::<String>::new());
    for bound in ["P: Copy", "P: Arbitrary"] {
        let source = source.replace("<P>", &format!("<{bound}>"));
        assert_eq!(sites(&source, "stmt.delete_assign").len(), 1, "{bound}");
    }
}

#[test]
fn ownership_generation_nested_restoration_preserves_exits_and_unknown_tails() {
    let template = r"
        fn stop() -> ! { loop {} }
        fn retry<P>(mut value: P, take: impl Fn(P) -> P) {
            loop {
                match true {
                    true => { let returned = take(value); value = returned; SUFFIX }
                    false => return,
                }
            }
        }";
    for suffix in ["return;", "break;", "stop();", "unknown();"] {
        let source = template.replace("SUFFIX", suffix);
        assert_eq!(sites(&source, "stmt.delete_assign").len(), 1, "{suffix}");
    }
    assert_eq!(sites(&template.replace("SUFFIX", ""), "stmt.delete_assign"), Vec::<String>::new());
    let exhausted = template.replace("SUFFIX", &"0;".repeat(40));
    assert_eq!(sites(&exhausted, "stmt.delete_assign").len(), 1);
}

#[test]
fn ownership_generation_return_annotation_recovers_exact_moved_binding() {
    let source = r"
        fn f(flag: bool) -> String {
            loop {
                let permit = unknown();
                if flag { drop(permit); continue; }
                return permit;
            }
        }";
    assert_eq!(sites(source, "loop.delete_continue"), Vec::<String>::new());
    assert_eq!(sites(&source.replace("-> String", "-> u32"), "loop.delete_continue").len(), 1);
    let shadowed = source.replace("return permit;", "let permit = String::new(); return permit;");
    assert_eq!(sites(&shadowed, "loop.delete_continue").len(), 1);
    let permit = source.replace("-> String", "-> tokio::sync::OwnedSemaphorePermit");
    assert_eq!(sites(&permit, "loop.delete_continue"), Vec::<String>::new());
    let local_permit = format!("mod tokio {{ pub mod sync {{ #[derive(Clone, Copy)] pub struct OwnedSemaphorePermit; }} }} {permit}");
    assert_eq!(sites(&local_permit, "loop.delete_continue").len(), 1);
    let shadowed_return = r"
        #[derive(Clone, Copy)] struct Output;
        fn f(flag: bool, mut make: impl FnMut() -> Output) -> Output {
            loop {
                use std::string::String as Output;
                let value = make();
                if flag { drop(value); continue; }
                return value;
            }
        }";
    assert_eq!(sites(shadowed_return, "loop.delete_continue").len(), 1);
}

#[test]
fn ownership_generation_exit_deletion_preserves_alternate_divergence() {
    let source = r"
        fn stop() -> ! { loop {} }
        fn f(value: String) {
            loop {
                consume(value);
                break;
                SUFFIX
            }
        }";
    for suffix in ["return;", "stop();", "unknown();"] {
        assert_eq!(sites(&source.replace("SUFFIX", suffix), "loop.delete_break").len(), 1, "{suffix}");
    }
    let before_use = r"
        fn stop() -> ! { loop {} }
        fn f(flag: bool) {
            loop {
                let value = String::new();
                if flag { consume(value); continue; }
                stop();
                observe(&value);
            }
        }";
    assert_eq!(sites(before_use, "loop.delete_continue").len(), 1);
}

#[test]
fn ownership_generation_moved_exit_rejects_deletion_and_backedge_transition() {
    let source = r"
        use std::cell::Cell;
        fn notify(progress: &Cell<usize>) { progress.set(progress.get() + 1); }
        fn moved_exit(mut line: String, word: &str, out: &mut Vec<String>, progress: &Cell<usize>) {
            for flag in [false, true] {
                if flag {
                    out.push(line);
                    notify(progress);
                    break;
                }
                line.push_str(word);
            }
        }";
    for selector in ["loop.delete_break", "loop.break_to_continue"] {
        assert_eq!(sites(source, selector), Vec::<String>::new(), "{selector}");
    }
}

#[test]
fn ownership_generation_arbitrary_receiver_methods_do_not_prove_moves() {
    let source = r"
        trait Inspect { fn into_vec(&self); }
        impl Inspect for String { fn into_vec(&self) {} }
        fn inspect(_: &String) {}
        fn f(flag: bool) {
            loop {
                let value = String::new();
                value.into_vec();
                if flag { continue; }
                inspect(&value);
                return;
            }
        }";
    assert_eq!(sites(source, "loop.delete_continue").len(), 1);
}

#[test]
fn ownership_generation_wildcard_trait_shadow_keeps_continue_deletion_for_known_vector() {
    let source = r"
        mod custom {
            pub trait IntoIterator { fn into_iter(&self) {} }
            impl IntoIterator for ::std::vec::Vec<u8> {}
        }
        fn inspect(_: &::std::vec::Vec<u8>) {}
        fn primitive_move_only(flag: bool) {
            loop {
                let value: ::std::vec::Vec<u8> = ::std::vec::Vec::new();
                if flag {
                    value.into_iter();
                    continue;
                }
                inspect(&value);
                return;
            }
        }";
    assert_eq!(sites(source, "loop.delete_continue"), Vec::<String>::new());
    assert_eq!(sites(&format!("use custom::*; {source}"), "loop.delete_continue").len(), 1);
}

#[test]
fn ownership_generation_nested_exit_uses_ancestor_move_but_not_sibling_branch() {
    let invalid = r"
        fn f(outer: bool) {
            loop {
                let value = String::new();
                if outer { consume(value); { continue; } }
                observe(&value);
                return;
            }
        }";
    assert_eq!(sites(invalid, "loop.delete_continue"), Vec::<String>::new());
    let valid = r"
        fn f(flag: bool) {
            loop {
                let value = String::new();
                if flag { consume(value); continue; } else { observe(&value); }
                return;
            }
        }";
    assert_eq!(sites(valid, "loop.delete_continue").len(), 1);
}

#[test]
fn ownership_generation_reversal_proves_string_path_move_before_format_capture() {
    let source = r#"
        fn f(directory: &std::path::Path, name: String) {
            let candidates = vec![directory.join(format!("{name}.rs")), directory.join(name).join("mod.rs")];
        }"#;
    assert_eq!(sites(source, "collection.reverse_vec"), Vec::<String>::new());
    assert_eq!(
        sites(
            &format!("type Text = String; {}", source.replace("name: String", "name: Text")),
            "collection.reverse_vec"
        ),
        Vec::<String>::new()
    );
    for safe in [
        source.replace("join(name)", "join(&name)"),
        source.replace("join(name)", "join(\"independent\")"),
        source
            .replace("name: String", "name: u32")
            .replace("join(name)", "join(name.to_string())"),
        source.replace("format!(\"{name}.rs\")", "format!(\"{{name}}.rs\")"),
        source.replace("name: String", "name: Unknown"),
    ] {
        assert_eq!(sites(&safe, "collection.reverse_vec").len(), 1, "{safe}");
    }
}

#[test]
fn ownership_generation_reversal_distinguishes_independent_fields_and_copy_values() {
    let source = r"
        struct Pair { left: String, right: String }
        fn borrow(value: &String) -> usize { value.len() }
        fn take(value: String) -> usize { value.len() }
        fn f(pair: Pair) -> [usize; 2] { [borrow(&pair.left), take(pair.right)] }";
    assert_eq!(sites(source, "collection.reverse_array").len(), 1);
    assert_eq!(
        sites(&source.replace("take(pair.right)", "take(pair.left)"), "collection.reverse_array"),
        Vec::<String>::new()
    );
    assert_eq!(
        sites("fn f(value: u32) -> [u32; 2] { [value, 0] }", "collection.reverse_array").len(),
        1
    );
}

#[test]
fn ownership_generation_reversal_keeps_shadowed_macros_and_binding_scopes() {
    let source = r#"
        fn f(directory: &std::path::Path, name: String) {
            let candidates = vec![directory.join(format!("{name}.rs")), directory.join(name)];
        }"#;
    for prefix in [
        "macro_rules! format { ($value:literal) => { \"fixed\" } }",
        "use custom::format;",
        "#![no_implicit_prelude]",
        "use custom::*;",
        "declare_macros!();",
        "#[macro_use] extern crate custom;",
    ] {
        assert_eq!(sites(&format!("{prefix}\n{source}"), "collection.reverse_vec").len(), 1, "{prefix}");
    }
    let shadowed = source.replace("format!(\"{name}.rs\")", "{ let name = String::new(); format!(\"{name}.rs\") }");
    assert_eq!(sites(&shadowed, "collection.reverse_vec").len(), 1);
    let qualified = source.replace("format!", "std::format!");
    assert_eq!(sites(&qualified, "collection.reverse_vec"), Vec::<String>::new());
    assert_eq!(sites(&format!("use custom as std; {qualified}"), "collection.reverse_vec").len(), 1);
}

#[test]
fn ownership_generation_generic_bounds_may_supply_copy_indirectly() {
    for signature in [
        "fn f<P: Copy>()",
        "fn f<P: Trait>()",
        "fn f<P>() where P: Trait",
        "fn f<P, Q: Trait<Item = P>>()",
        "fn f<P, Q>() where Q: Trait<Item = P>",
    ] {
        let signature = syn::parse_str::<syn::Signature>(signature).unwrap();
        assert!(
            !unbounded_parameters(&signature.generics).contains(&"P".to_owned()),
            "{signature:?}"
        );
    }
    let signature: syn::Signature = syn::parse_quote!(fn f<'a, P: 'a>());
    assert_eq!(unbounded_parameters(&signature.generics), ["P"]);
}

#[test]
fn ownership_generation_generic_mentions_are_nested_specific_and_trait_bounded() {
    for (signature, expected) in [
        ("fn f<T, U>()", vec!["T", "U"]),
        ("fn f<T: Trait<Item = T>, U>()", vec!["U"]),
        ("fn f<T, U: Trait<Item = Vec<T>>>()", vec![]),
        ("fn f<'a, T, U>() where U: Trait, 'a: 'static", vec!["T"]),
        ("fn f<'a, T, U>() where T: 'a, U: Trait", vec!["T"]),
        ("fn f<'a, T, U>() where U: 'a", vec!["T", "U"]),
        ("fn f<'a, T, U>() where T: 'a + Trait, U: Trait", vec![]),
        ("fn f<T, U>() where U: Trait, T: Other", vec![]),
        ("fn f<T, U>() where Vec<T>: Trait, U: Other", vec![]),
    ] {
        let signature = syn::parse_str::<syn::Signature>(signature).unwrap();
        assert_eq!(unbounded_parameters(&signature.generics), expected, "{signature:?}");
    }
}

#[test]
fn ownership_generation_macro_shadow_detection_visits_statements_and_mixed_attributes() {
    for (source, expected) in [
        ("", false),
        ("macro_rules! unrelated { () => {} }", false),
        ("macro_rules! format { () => {} }", true),
        ("declare_macros!();", true),
        ("fn f() { declare_macros!(); }", true),
        ("extern crate custom;", false),
        ("#[allow(unused)] extern crate custom;", false),
        ("#[allow(unused)] #[macro_use] extern crate custom;", true),
        ("#[macro_use] #[allow(unused)] extern crate custom;", true),
    ] {
        let file = syn::parse_file(source).unwrap();
        let actual = super::lexical::evidence(&file, &CfgSet::parse(""));
        assert_eq!(actual.read_only.format_macro_shadowed, expected, "{source}");
    }
}
