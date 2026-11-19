// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;

use super::*;

const FROZEN: &str = include_str!("../fixtures/campaign/isolation.rs");

fn entry_source(setup: &str, body: &str) -> String {
    format!(
        r"
        struct Candidate {{ ordinal: u32 }}
        struct Other {{ ordinal: u32 }}
        struct Search;
        struct Proofs {{
            local: usize,
            results: std::collections::HashMap<Vec<u32>, Option<bool>>,
            empty: bool,
        }}
        impl Proofs {{
            fn key(active: &[&Candidate]) -> Vec<u32> {{
                let mut key = active.iter().map(|candidate| candidate.ordinal).collect::<Vec<_>>();
                key.sort_unstable();
                key
            }}
        }}
        impl Search {{
            fn check(&mut self, active: &[&Candidate], proofs: &mut Proofs) -> Result<Option<bool>, u32> {{
                let key = Proofs::key(active);
                if let Some(cached) = proofs.results.get(&key) {{ return Ok(*cached); }}
                Err(1)
            }}
            fn run(&mut self) -> Result<(), u32> {{
                let ready = true;
                {setup}
                while ready {{ {body} }}
                Ok(())
            }}
        }}
        "
    )
}

const ENTRY: &str = r"
    let first = Candidate { ordinal: 3u32 };
    let second = Candidate { ordinal: 1u32 };
    let active = [&first, &second].to_vec();
    let mut proofs: Proofs = Proofs {
        local: 0,
        empty: true,
        results: std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))]),
    };
";

fn entry_retry(source: &str) -> Option<Retry> {
    let file = syn::parse_file(source).unwrap();
    Retry::new(&file, &target(&file))
}

fn empty_retry() -> Retry {
    Retry {
        calls: HashMap::default(),
        bindings: HashMap::default(),
        seeds: HashMap::default(),
        populations: HashMap::default(),
        caches: HashMap::default(),
        counts: HashMap::default(),
        scalars: HashMap::default(),
        ordinals: HashSet::default(),
        globals: HashMap::default(),
    }
}

#[test]
fn source_entry_snapshots_preserve_population_order_cache_verdicts_and_last_field_writes() {
    for (verdict, expected) in [("Some(false)", Some(false)), ("Some(true)", Some(true)), ("None", None)] {
        let source = entry_source(
            &format!(
                "{} proofs.local = 7usize; proofs.empty = false;",
                ENTRY.replace("Some(false)", verdict)
            ),
            "let answer = self.check(&active, &mut proofs)?; break;",
        );
        let file = syn::parse_file(&source).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert_eq!(retry.populations["active"], [3, 1]);
        assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], expected)]);
        assert_eq!(retry.counts["proofs.local"], 7);
        assert_eq!(retry.seeds["proofs.empty"], syn::parse_quote!(false));
        assert_eq!(retry.bindings["proofs.local"], Binding::Integer);
        assert_eq!(retry.bindings["proofs.empty"], Binding::Bool);
        assert!(retry.calls.contains_key("self.check"));
        assert_eq!(retry.ordinals, HashSet::from_iter(["ordinal".into()]));
        let stalled = syn::parse_str("while ready { let answer = self.check(&active, &mut proofs)?; continue; }").unwrap();
        assert!(retry.proves(&original, &stalled));
        assert!(!retry.proves(&original, &original));
        assert!(
            !retry.proves(&stalled, &stalled),
            "an already spinning baseline is not newly stagnant"
        );
        assert!(!retry.proves(&original, &syn::parse_str("while ready { break; }").unwrap()));
    }
    let source = entry_source(ENTRY, "let answer = self.check(&active, &mut proofs)?; break;");
    for cache in [
        "std::collections::HashMap::from([])",
        "std::collections::HashMap::from([(std::vec::Vec::from([3u32, 1]), Some(false))])",
        "std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false)), (std::vec::Vec::from([1u32, 3]), Some(false))])",
    ] {
        let changed = source.replace(
            "std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))])",
            cache,
        );
        let file = syn::parse_file(&changed).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert!(
            !retry.proves(
                &original,
                &syn::parse_str("while ready { let answer = self.check(&active, &mut proofs)?; continue; }").unwrap()
            ),
            "{cache}"
        );
    }
}

#[test]
fn source_entry_grammar_rejects_effects_mixed_records_inexact_fields_and_custom_cache_constructors() {
    for (from, to) in [
        ("let ready = true;", "#[custom] let ready = true;"),
        ("let ready = true;", "let ref ready = true;"),
        ("let ready = true;", "let ready @ _ = true;"),
        ("let ready = true;", "let (ready, other) = input;"),
        ("let ready = true;", "let ready;"),
        ("let ready = true;", "let ready = true else { return Err(1); };"),
        ("3u32", "3u64"),
        ("3u32", "4294967296u32"),
        ("3u32", "#[custom] 3u32"),
        ("Candidate { ordinal: 3u32 }", "Candidate { ordinal: 3u32, ..unknown }"),
        ("Candidate { ordinal: 3u32 }", "Candidate {}"),
        ("Candidate { ordinal: 3u32 }", "Candidate { ordinal: 3u32, ordinal: 4u32 }"),
        ("Candidate { ordinal: 3u32 }", "Candidate { missing: 3u32 }"),
        ("Candidate { ordinal: 3u32 }", "Candidate { #[custom] ordinal: 3u32 }"),
        ("Candidate { ordinal: 3u32 }", "#[custom] Candidate { ordinal: 3u32 }"),
        ("Candidate { ordinal: 1u32 }", "Other { ordinal: 1u32 }"),
        ("[&first, &second]", "[&first, true]"),
        ("[&first, &second]", "[&mut first, &second]"),
        ("[&first, &second]", "#[custom] [&first, &second]"),
        (".to_vec()", ".clone()"),
        (".to_vec()", ".to_vec(unknown())"),
        (".to_vec()", ".to_vec::<u32>()"),
        ("let active = [&first, &second].to_vec();", "let active = true.to_vec();"),
        (
            "let active = [&first, &second].to_vec();",
            "let active = [&first, &second].to_vec(); unknown();",
        ),
        ("std::collections::HashMap::from", "custom::HashMap::from"),
        ("std::collections::HashMap::from", "std::collections::HashMap::new"),
        ("std::vec::Vec::from", "custom::Vec::from"),
        ("std::vec::Vec::from([1u32, 3])", "std::vec::Vec::from([1u32, 3], [])"),
        ("std::vec::Vec::from([1u32, 3])", "std::vec::Vec::from(unknown)"),
        ("[1u32, 3]", "[1u64, 3]"),
        ("[1u32, 3]", "[first, 3]"),
        ("[1u32, 3]", "[#[custom] 1u32, 3]"),
        ("[1u32, 3]", "#[custom] [1u32, 3]"),
        ("Some(false)", "Some(false, true)"),
        ("Some(false)", "Some(0)"),
        ("Some(false)", "Some(#[custom] false)"),
        ("Some(false)", "#[custom] Some(false)"),
        ("Some(false)", "Other(false)"),
        ("Some(false)", "missing"),
    ] {
        let source = entry_source(ENTRY, "let answer = self.check(&active, &mut proofs)?; break;");
        let changed = source.replace(from, to);
        assert_ne!(changed, source, "{from}");
        assert!(entry_retry(&changed).is_none(), "{from} -> {to}");
    }
    for statement in [
        "proofs.local = unknown;",
        "proofs.local = 32768usize;",
        "proofs.empty = unknown;",
        "proofs.missing = 0usize;",
        "missing.local = 0usize;",
        "proofs.local += 1usize;",
        "proofs.results = active;",
        "proofs = Proofs { local: 0, empty: true, results: std::collections::HashMap::from([]) };",
        "unknown(proofs);",
    ] {
        assert!(
            entry_retry(&entry_source(&format!("{ENTRY} {statement}"), "self.check(&active, &mut proofs)?;")).is_none(),
            "{statement}"
        );
    }
    let source = entry_source(
        &format!("{ENTRY} proofs.local = 9usize; proofs.results = std::collections::HashMap::from([(std::vec::Vec::from([1,3]), None)]);"),
        "self.check(&active, &mut proofs)?;",
    );
    let retry = entry_retry(&source).unwrap();
    assert_eq!(retry.counts["proofs.local"], 9);
    assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], None)]);
}

#[test]
fn entry_population_cache_key_record_and_binding_limits_accept_only_complete_bounded_sources() {
    for length in [31, 32, 33] {
        let population = std::iter::repeat_n("&first", length).collect::<Vec<_>>().join(",");
        let setup = ENTRY.replace("[&first, &second]", &format!("[{population}]"));
        let retry = entry_retry(&entry_source(&setup, "self.check(&active, &mut proofs)?;"));
        assert_eq!(retry.is_some(), length <= 32, "population {length}");
        if let Some(retry) = retry {
            assert_eq!(retry.populations["active"], vec![3; length]);
        }
        let keys = std::iter::repeat_n("1u32", length).collect::<Vec<_>>().join(",");
        let setup = ENTRY.replace("[1u32, 3]", &format!("[{keys}]"));
        assert_eq!(
            entry_retry(&entry_source(&setup, "self.check(&active, &mut proofs)?;")).is_some(),
            length <= 32,
            "key {length}"
        );
        let entries = (0..length)
            .map(|index| format!("(std::vec::Vec::from([{index}u32]), None)"))
            .collect::<Vec<_>>()
            .join(",");
        let setup = ENTRY.replace("(std::vec::Vec::from([1u32, 3]), Some(false))", &entries);
        assert_eq!(
            entry_retry(&entry_source(&setup, "self.check(&active, &mut proofs)?;")).is_some(),
            length <= 32,
            "entries {length}"
        );
        let fields = (1..length).fold(String::new(), |mut output, index| {
            write!(output, ", field{index}: bool").unwrap();
            output
        });
        let values = (1..length).fold(String::new(), |mut output, index| {
            write!(output, ", field{index}: true").unwrap();
            output
        });
        let setup = ENTRY
            .replace("let second = Candidate { ordinal: 1u32 };", "")
            .replace("[&first, &second]", "[&first]");
        let source = entry_source(&setup, "self.check(&active, &mut proofs)?;")
            .replace(
                "struct Candidate { ordinal: u32 }",
                &format!("struct Candidate {{ ordinal: u32 {fields} }}"),
            )
            .replace("Candidate { ordinal: 3u32 }", &format!("Candidate {{ ordinal: 3u32 {values} }}"))
            .replace("Candidate { ordinal: 1u32 }", &format!("Candidate {{ ordinal: 1u32 {values} }}"));
        assert_eq!(entry_retry(&source).is_some(), length <= 32, "record fields {length}");
    }
    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?;");
    let base = entry_retry(&source).unwrap().bindings.len();
    for bindings in [63, 64, 65] {
        let extra = (0..bindings - base).fold(String::new(), |mut output, index| {
            write!(output, "let extra{index} = false;").unwrap();
            output
        });
        assert_eq!(
            entry_retry(&entry_source(&format!("{ENTRY} {extra}"), "self.check(&active, &mut proofs)?;")).is_some(),
            bindings <= 64,
            "bindings {bindings}"
        );
    }
}

#[test]
fn entry_statement_work_budget_and_shadowed_roots_do_not_reuse_stale_projection_values() {
    let file =
        syn::parse_file("struct Search; struct Proofs { local: usize } impl Search { fn run(&mut self, proofs: &mut Proofs) {} }").unwrap();
    let Item::Impl(implementation) = &file.items[2] else {
        unreachable!()
    };
    let syn::ImplItem::Fn(function) = &implementation.items[0] else {
        unreachable!()
    };
    for statements in [127, 128, 129] {
        let mut state = entry::State::new(&file, "Search", &function.sig, "ordinal").unwrap();
        for _ in 0..statements.min(128) {
            assert_eq!(state.statement(&syn::parse_str("let ready = true;").unwrap()), Some(()));
        }
        assert_eq!(
            state.statement(&syn::parse_str("let ready = true;").unwrap()),
            (statements < 128).then_some(())
        );
    }
    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?;");
    let setup = format!("{ENTRY} proofs.local = 9usize; let proofs = false;");
    let retry = entry_retry(&entry_source(&setup, "self.check(&active, &mut proofs)?;")).unwrap();
    assert!(!retry.counts.contains_key("proofs.local"));
    assert!(!retry.caches.contains_key("proofs.results"));
    assert_eq!(retry.seeds["proofs"], syn::parse_quote!(false));
    let nested = source
        .replace(
            "struct Search;",
            "struct Search { nested: Nested } struct Nested { proofs: Proofs }",
        )
        .replace("let ready = true;", "let ready = true; self.nested.proofs.local = 9usize;");
    let retry = entry_retry(&nested).unwrap();
    assert_eq!(retry.counts["self.nested.proofs.local"], 9);
}

#[test]
fn inferred_usize_entry_counts_reach_the_retry_condition_without_a_type_annotation() {
    for count in [0, 7, 32767] {
        let source = entry_source(
            &format!("{ENTRY} let limit = {count}usize; let expected: usize = {count};"),
            "self.check(&active, &mut proofs)?; break;",
        );
        let file = syn::parse_file(&source).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert_eq!(retry.counts["limit"], count);
        assert_eq!(retry.bindings["limit"], Binding::Integer);
        let steady =
            syn::parse_str("while ready { if limit != expected { break; } self.check(&active, &mut proofs)?; continue; }").unwrap();
        assert!(retry.proves(&original, &steady), "inferred count {count}");
        let escaping =
            syn::parse_str("while ready { if limit == expected { break; } self.check(&active, &mut proofs)?; continue; }").unwrap();
        assert!(!retry.proves(&original, &escaping));
    }
    for initializer in ["32768usize", "7u64", "unknown()"] {
        assert!(
            entry_retry(&entry_source(
                &format!("{ENTRY} let limit = {initializer};"),
                "self.check(&active, &mut proofs)?; break;"
            ))
            .is_none(),
            "{initializer}"
        );
    }
}

#[test]
fn enclosing_borrowed_parameters_require_plain_unconditional_bindings() {
    let source =
        entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;").replace("fn run(&mut self)", "fn run(&mut self, context: &Other)");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    assert_eq!(retry.bindings["context"], Binding::Opaque);
    let steady = syn::parse_str("while ready { self.check(&active, &mut proofs)?; continue; }").unwrap();
    assert!(retry.proves(&original, &steady));
    assert!(!retry.proves(&original, &original));
    for parameter in [
        "#[cfg(feature = \"other\")] context: &Other",
        "ref context: &Other",
        "context @ _: &Other",
        "(context, other): (&Other, &Other)",
    ] {
        assert!(entry_retry(&source.replace("context: &Other", parameter)).is_none(), "{parameter}");
    }
}

#[test]
fn retry_global_counts_and_source_owned_scalar_helpers_preserve_binding_precedence() {
    let source = format!(
        "{} const FACTOR: usize = 7; const LIMIT: usize = FACTOR * 3; impl Search {{ fn scalar(n: usize) -> usize {{ n.max(LIMIT) }} }} impl Other {{ fn foreign() -> usize {{ 2 }} }}",
        entry_source(
            &format!("{ENTRY} let LIMIT: usize = 4;"),
            "self.check(&active, &mut proofs)?; break;"
        )
    );
    let retry = entry_retry(&source).unwrap();
    assert_eq!(retry.globals["FACTOR"], 7);
    assert_eq!(retry.globals["LIMIT"], 21);
    assert_eq!(retry.counts["FACTOR"], 7);
    assert_eq!(retry.counts["LIMIT"], 4);
    assert!(retry.scalars.contains_key("Search :: scalar"));
    assert!(retry.scalars.contains_key("Self :: scalar"));
    assert!(retry.scalars.contains_key("Other :: foreign"));
    assert!(!retry.scalars.contains_key("Self :: foreign"));
    for (function, accepted) in [
        ("fn scalar(n: usize, enabled: bool) -> bool { enabled }", true),
        ("const fn scalar() -> usize { 2 }", true),
        ("fn scalar(&self) -> usize { 2 }", false),
        ("#[custom] fn scalar() -> usize { 2 }", false),
        ("fn scalar<T>() -> usize { 2 }", false),
        ("fn scalar() -> usize where Self: Sized { 2 }", false),
        ("async fn scalar() -> usize { 2 }", false),
        ("unsafe fn scalar() -> usize { 2 }", false),
        ("extern \"C\" fn scalar() -> usize { 2 }", false),
        ("extern \"C\" fn scalar(n: usize, ...) -> usize { 2 }", false),
        ("fn scalar(#[custom] n: usize) -> usize { 2 }", false),
        ("fn scalar(ref n: usize) -> usize { 2 }", false),
        ("fn scalar(n @ _: usize) -> usize { 2 }", false),
        ("fn scalar(n: custom::usize) -> usize { 2 }", false),
        ("fn scalar(n: u32) -> usize { 2 }", false),
        ("fn scalar() -> usize { let result = 2; result }", false),
    ] {
        let function = syn::parse_str(function).unwrap();
        assert_eq!(scalar(&function), accepted, "{function:?}");
    }
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    let steady = syn::parse_str(
        "while ready { let answer = self.check(&active, &mut proofs)?; let maximum = Self::scalar(proofs.local); continue; }",
    )
    .unwrap();
    assert!(retry.proves(&original, &steady));
    let unresolved =
        syn::parse_str("while ready { let answer = self.check(&active, &mut proofs)?; let maximum = Self::foreign(); continue; }").unwrap();
    assert!(!retry.proves(&original, &unresolved));
}

#[test]
fn constant_count_grammar_and_recursion_budget_preserve_zero_maximum_and_checked_products() {
    let known = HashMap::from_iter([("LIMIT".into(), 32767), ("FACTOR".into(), 3)]);
    for (expression, expected) in [
        ("0", Some(0)),
        ("1usize", Some(1)),
        ("32766u32", Some(32766)),
        ("32767", Some(32767)),
        ("32768", None),
        ("65536", None),
        ("1u64", None),
        ("true", None),
        ("unknown", None),
        ("LIMIT", Some(32767)),
        ("FACTOR * 7", Some(21)),
        ("0 * LIMIT", Some(0)),
        ("LIMIT * 1", Some(32767)),
        ("LIMIT * 2", None),
        ("LIMIT * 3", None),
        ("2 + 3", None),
        ("2 - 1", None),
        ("2 / 1", None),
        ("#[custom] (2 * 3)", None),
        ("#[custom] 3", None),
    ] {
        assert_eq!(
            constant_count(&syn::parse_str(expression).unwrap(), &known, 64),
            expected,
            "{expression}"
        );
    }
    for budget in [0, 1, 2, 3] {
        assert_eq!(constant_count(&syn::parse_quote!(7), &known, budget), (budget >= 1).then_some(7));
        assert_eq!(
            constant_count(&syn::parse_quote!(2 * 3), &known, budget),
            (budget >= 2).then_some(6)
        );
        assert_eq!(
            constant_count(&syn::parse_quote!(2 * 3 * 4), &known, budget),
            (budget >= 3).then_some(24)
        );
    }
}

#[test]
fn entry_cache_tuple_configuration_cannot_become_a_definite_warm_snapshot() {
    let pair = "(std::vec::Vec::from([1u32, 3]), Some(false))";
    let configured_pair = format!("#[cfg(feature = \"warm\")] {pair}");
    let constructor: Expr = syn::parse_str(&format!("std::collections::HashMap::from([{configured_pair}])")).unwrap();
    let Expr::Call(call) = constructor else { unreachable!() };
    assert_eq!(call.attrs, Vec::<syn::Attribute>::new());
    let Expr::Array(entries) = &call.args[0] else { unreachable!() };
    assert_eq!(entries.attrs, Vec::<syn::Attribute>::new());
    let Expr::Tuple(tuple) = &entries.elems[0] else { unreachable!() };
    assert_eq!(tuple.elems.len(), 2);
    assert_eq!(tuple.attrs.len(), 1);
    let Expr::Call(key) = &tuple.elems[0] else { unreachable!() };
    assert_eq!(key.attrs, Vec::<syn::Attribute>::new());

    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    let steady = syn::parse_str("while ready { self.check(&active, &mut proofs)?; continue; }").unwrap();
    assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], Some(false))]);
    assert!(retry.proves(&original, &steady));
    assert!(!retry.proves(&original, &original));
    let configured = source.replace(pair, &configured_pair);
    assert_ne!(configured, source);
    let file = syn::parse_file(&configured).unwrap();
    assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
    let original = target(&file);
    let configured_retry = Retry::new(&file, &original);
    assert!(!configured_retry.as_ref().is_some_and(|retry| retry.proves(&original, &steady)));
    assert!(configured_retry.is_none());
    for setup in [ENTRY.replace(pair, ""), ENTRY.replace("[1u32, 3]", "[1u32, 4]")] {
        let file = syn::parse_file(&entry_source(&setup, "self.check(&active, &mut proofs)?; break;")).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert!(!retry.proves(&original, &steady));
    }
}

#[test]
fn attributed_scalar_bodies_are_not_admitted_by_the_source_retry_registry() {
    let base = entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;");
    let stalled =
        syn::parse_str("while ready { let stopped = Self::gate(); if stopped { break; } self.check(&active, &mut proofs)?; continue; }")
            .unwrap();
    for (helper, expected, admitted) in [
        ("fn gate() -> bool { false }", true, true),
        ("fn gate() -> bool { true }", false, true),
        (
            "#[cfg_attr(feature = \"trace\", tracing::instrument)] fn gate() -> bool { false }",
            false,
            false,
        ),
    ] {
        let file = syn::parse_file(&format!("{base} impl Search {{ {helper} }}")).unwrap();
        assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], Some(false))]);
        assert_eq!(retry.proves(&original, &stalled), expected, "{helper}");
        assert_eq!(retry.scalars.contains_key("Self :: gate"), admitted, "{helper}");
        assert_eq!(retry.scalars.contains_key("Search :: gate"), admitted, "{helper}");
    }
}

#[test]
fn empty_populations_do_not_turn_unsupported_key_helpers_into_warm_hits() {
    let setup = ENTRY.replace("[&first, &second]", "[]").replace("[1u32, 3]", "[]");
    let source = entry_source(&setup, "self.check(&active, &mut proofs)?; break;");
    let original_projection = "let mut key = active.iter().map(|candidate| candidate.ordinal).collect::<Vec<_>>();";
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    let stalled = syn::parse_str("while ready { self.check(&active, &mut proofs)?; continue; }").unwrap();
    assert_eq!(retry.populations["active"], Vec::<u32>::new());
    assert_eq!(retry.caches["proofs.results"], [(vec![], Some(false))]);
    assert!(retry.proves(&original, &stalled));
    assert!(!retry.proves(&original, &original));

    for projection in [
        "let mut key = std::vec::Vec::from([7u32]);",
        "#[allow(unused_mut)] let mut key = std::vec::Vec::from([7u32]);",
        "let mut key = (std::vec::Vec::from([7u32]).into_iter()).collect::<Vec<_>>();",
        "let mut key = (std::vec::Vec::from([7u32]).into_iter()).map(|ordinal| ordinal).collect::<Vec<_>>();",
        "let mut key = active.iter().map(make_mapper()).collect::<Vec<_>>();",
    ] {
        let changed = source.replace(original_projection, projection);
        assert_ne!(changed, source);
        let changed = format!(
            "{changed} fn make_mapper() -> fn(&Candidate) -> u32 {{ panic!(\"mapper construction terminates before a cache hit\") }}"
        );
        let file = syn::parse_file(&changed).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original);
        assert!(
            !retry.as_ref().is_some_and(|retry| retry.proves(&original, &stalled)),
            "{projection}"
        );
        assert!(CachePrefix::resolve(&file, "Search", "check").is_none(), "{projection}");
        assert!(retry.is_none(), "{projection}");
    }
}

#[test]
fn unsummarized_scalar_and_chunk_arguments_withhold_retry_evidence_without_panicking() {
    let setup = format!("{ENTRY} proofs.local = 2usize;");
    let helpers = r#"
        impl Search { fn width(value: usize) -> usize { value } }
        fn unmodeled_width() -> usize { panic!("an argument can terminate before the warm hit") }
    "#;
    for operation in [
        "let width = Self::width(proofs.local);",
        "let width = active.len().max(proofs.local);",
        "let parts = active.chunks(proofs.local).map(<[_]>::to_vec).collect::<Vec<_>>();",
    ] {
        let body = format!("{operation} self.check(&active, &mut proofs)?; break;");
        let source = format!("{} {helpers}", entry_source(&setup, &body));
        let file = syn::parse_file(&source).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert!(retry.scalars.contains_key("Self :: width"));
        let stalled = syn::parse_str(&format!(
            "while ready {{ {operation} self.check(&active, &mut proofs)?; continue; }}"
        ))
        .unwrap();
        assert!(retry.proves(&original, &stalled), "{operation}");
        assert!(!retry.proves(&original, &original));

        let unknown = operation.replace("proofs.local", "unmodeled_width()");
        let source = format!(
            "{} {helpers}",
            entry_source(&setup, &format!("{unknown} self.check(&active, &mut proofs)?; break;"))
        );
        let file = syn::parse_file(&source).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], Some(false))]);
        let stalled = syn::parse_str(&format!("while ready {{ {unknown} self.check(&active, &mut proofs)?; continue; }}")).unwrap();
        assert!(!retry.proves(&original, &stalled), "{unknown}");
    }
}

#[test]
fn borrowed_proof_roots_do_not_supply_missing_cache_or_empty_guard_snapshots() {
    let population = r"
        let first = Candidate { ordinal: 3u32 };
        let second = Candidate { ordinal: 1u32 };
        let active = [&first, &second].to_vec();
    ";
    let cache = "proofs.results = std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))]);";
    let guard = "proofs.attributed = std::collections::HashMap::from([]);";
    let stalled = syn::parse_str("while ready { self.check(&active, &mut proofs)?; continue; }").unwrap();
    for (snapshots, missing) in [
        (format!("{cache} {guard}"), None),
        (guard.into(), Some("proofs.results")),
        (cache.into(), Some("proofs.attributed")),
    ] {
        let source = entry_source(&format!("{population} {snapshots}"), "self.check(&active, &mut proofs)?; break;")
            .replace("fn run(&mut self)", "fn run(&mut self, proofs: &mut Proofs)")
            .replace("empty: bool,", "empty: bool, attributed: std::collections::HashMap<u32, u32>,")
            .replace(
                "let key = Proofs::key(active);",
                "if !proofs.attributed.is_empty() { return Ok(None); } let key = Proofs::key(active);",
            );
        let file = syn::parse_file(&source).unwrap();
        let prefix = CachePrefix::resolve(&file, "Search", "check").unwrap();
        assert_eq!(prefix.empty_guard().unwrap(), ["proofs", "attributed"]);
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        assert_eq!(retry.bindings["proofs"], Binding::Opaque);
        assert_eq!(retry.populations["active"], [3, 1]);
        assert_eq!(retry.proves(&original, &stalled), missing.is_none(), "{missing:?}");
        assert!(!retry.proves(&original, &original));
        if let Some(missing) = missing {
            assert!(!retry.bindings.contains_key(missing), "{missing}");
        } else {
            assert_eq!(retry.caches["proofs.results"], [(vec![1, 3], Some(false))]);
            assert_eq!(retry.seeds["proofs.attributed"], syn::parse_quote!(true));
        }
    }
}

#[test]
fn resolved_retry_calls_do_not_imply_that_the_original_loop_lacks_a_hazard() {
    let source = entry_source(ENTRY, "if false { self.check(&active, &mut proofs)?; } continue;").replace("while ready", "while true");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    assert!(retry.calls.contains_key("self.check"));
    assert!(!retry.proves(&original, &original));
}

#[test]
fn source_retry_requires_supported_enclosing_signatures_complete_windows_and_nested_call_visits() {
    use syn::spanned::Spanned as _;

    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?;");
    for (from, to) in [
        ("fn run(&mut self)", "fn run(&self)"),
        ("fn run(&mut self)", "fn run(self)"),
        ("fn run(&mut self)", "fn run()"),
        ("fn run(&mut self)", "fn run(#[custom] &mut self)"),
        ("fn run(&mut self)", "fn run<T>(&mut self)"),
        ("fn run(&mut self)", "async fn run(&mut self)"),
        ("fn run(&mut self)", "unsafe fn run(&mut self)"),
        ("fn run(&mut self)", "extern \"C\" fn run(&mut self)"),
        (
            "fn run(&mut self) -> Result<(), u32>",
            "fn run(&mut self) -> Result<(), u32> where Self: Sized",
        ),
        ("fn run(&mut self)", "#[custom] fn run(&mut self)"),
        ("self.check(&active, &mut proofs)?;", "other.check(&active, &mut proofs)?;"),
        ("self.check(&active, &mut proofs)?;", "self.unknown(&active, &mut proofs)?;"),
    ] {
        assert!(entry_retry(&source.replace(from, to)).is_none(), "{from} -> {to}");
    }
    let nested = entry_source(ENTRY, "self.unknown(self.check(&active, &mut proofs)?)?;");
    assert!(
        entry_retry(&nested).is_some(),
        "call resolution visits nested receivers and arguments"
    );
    for calls in [127, 128, 129] {
        let body = format!("{} self.check(&active, &mut proofs)?;", "active.len();".repeat(calls - 1));
        assert_eq!(
            entry_retry(&entry_source(ENTRY, &body)).is_some(),
            calls <= 128,
            "call visits {calls}"
        );
    }
    let file = syn::parse_file(&source).unwrap();
    let length = target(&file).span().byte_range().len();
    for bytes in [4095, 4096, 4097] {
        let changed = source.replace("while ready {", &format!("while ready {{{}", " ".repeat(bytes - length)));
        assert_eq!(entry_retry(&changed).is_some(), bytes <= 4096, "loop bytes {bytes}");
    }
    let Item::Impl(implementation) = &file.items[5] else {
        unreachable!()
    };
    let syn::ImplItem::Fn(function) = &implementation.items[1] else {
        unreachable!()
    };
    let baseline = function.block.stmts.len();
    for statements in [63, 64, 65] {
        let extra = "proofs.local = 0usize;".repeat(statements - baseline);
        assert_eq!(
            entry_retry(&entry_source(&format!("{ENTRY} {extra}"), "self.check(&active, &mut proofs)?;")).is_some(),
            statements <= 64,
            "enclosing statements {statements}"
        );
    }
}

// The minimizer loop is unchanged. The Copy build-context layouts, key, warm prefix, budget
// guard, and reset follow the workspace declarations; unused borrowed service objects are opaque.
fn complete_source() -> String {
    let declarations = r"
        use std::collections::HashSet;
        const MAX_ISOLATION_PROOFS: usize = 32;
        const MAX_ISOLATION_CONTEXTS: usize = 64;
        const MAX_INVOCATION_ISOLATION_PROOFS: usize = MAX_ISOLATION_CONTEXTS * MAX_ISOLATION_PROOFS;
        struct Workspace;
        struct Plan;
        #[derive(Clone, Copy)]
        struct BuildScope<'a> {
            roots: Option<&'a [String]>,
            mutants: Option<&'a [String]>,
            publish_progress: bool,
        }
        #[derive(Clone, Copy)]
        struct BuildLimits {
            timeout: Option<core::time::Duration>,
            multiplier: Option<f64>,
            rollback_rounds: u32,
        }
        struct Events;
        struct Mutant { ordinal: u32 }
        struct IsolationBudget { proofs: usize }
        struct Proofs {
            local: usize,
            results: std::collections::HashMap<Vec<u32>, Option<bool>>,
            attributed: std::collections::HashMap<u32,u32>,
        }
        impl Proofs {
            fn key(active: &[&Mutant]) -> Vec<u32> {
                let mut key = active.iter().map(|mutant| mutant.ordinal).collect::<Vec<_>>();
                key.sort_unstable();
                key
            }
        }
        impl Converger {
            fn minimization_can_continue(active: usize, local: usize, invocation: usize) -> bool {
                active > 1 && local < MAX_ISOLATION_PROOFS && invocation < MAX_INVOCATION_ISOLATION_PROOFS
            }
            const fn reduced_chunk_granularity() -> usize { 2 }
            fn subset_fails(
                &mut self, work: &Workspace, plan: &Plan, scope: BuildScope<'_>,
                verb: &[&str], limits: BuildLimits, events: &mut Events,
                population: &[&Mutant], active: &[&Mutant], proofs: &mut Proofs,
            ) -> Result<Option<bool>, u32> {
                if !proofs.attributed.is_empty() { return Ok(None); }
                let key = Proofs::key(active);
                if let Some(cached) = proofs.results.get(&key) { return Ok(*cached); }
                proofs.local = proofs.local.saturating_add(1);
                self.isolation_budget.proofs = self.isolation_budget.proofs.saturating_add(1);
                Err(1)
            }
        }
    ";
    let setup = r"
        self.isolation_budget.proofs = 0usize;
        proofs.local = 0usize;
        proofs.attributed = std::collections::HashMap::from([]);
        proofs.results = std::collections::HashMap::from([
            (std::vec::Vec::from([1u32,2]), Some(false)),
            (std::vec::Vec::from([3u32,4]), Some(false)),
            (std::vec::Vec::from([1u32]), Some(false)),
            (std::vec::Vec::from([2u32]), Some(false)),
            (std::vec::Vec::from([3u32]), Some(false)),
            (std::vec::Vec::from([4u32]), Some(false)),
            (std::vec::Vec::from([2u32,3,4]), Some(false)),
            (std::vec::Vec::from([1u32,3,4]), Some(false)),
            (std::vec::Vec::from([1u32,2,4]), Some(false)),
            (std::vec::Vec::from([1u32,2,3]), Some(false)),
        ]);
        let a = Mutant { ordinal: 1u32 };
        let b = Mutant { ordinal: 2u32 };
        let c = Mutant { ordinal: 3u32 };
        let d = Mutant { ordinal: 4u32 };
        let failing = [&a, &b, &c, &d];
    ";
    format!(
        "{declarations} {}",
        FROZEN
            .replace("struct Converger;", "struct Converger { isolation_budget: IsolationBudget }")
            .replace("dyn Events", "Events")
            .replace("Result<Option<Vec<u32>>>", "Result<Option<Vec<u32>>, u32>")
            .replace(
                "let mut active = failing.to_vec();",
                &format!("{setup} let mut active = failing.to_vec();")
            )
            .replace("let mut granularity = 2;", "let mut granularity: usize = 2;")
    )
}

fn target(file: &syn::File) -> Expr {
    for item in &file.items {
        let Item::Impl(implementation) = item else { continue };
        for item in &implementation.items {
            if let syn::ImplItem::Fn(function) = item {
                for statement in &function.block.stmts {
                    if let Stmt::Expr(expression @ Expr::While(_), _) = statement {
                        return expression.clone();
                    }
                }
            }
        }
    }
    panic!("the complete source contains its minimizer loop")
}

fn proof(source: &str, from: &str, to: &str) -> bool {
    let file = syn::parse_file(source).unwrap();
    let original = target(&file);
    let Some(retry) = Retry::new(&file, &original) else { return false };
    let span = original.span().byte_range();
    let body = &source[span.clone()];
    assert!(body.contains(from), "{from}");
    let mutated = syn::parse_str(&body.replacen(from, to, 1)).unwrap();
    let result = retry.proves(&original, &mutated);
    let edit = source[span.clone()].find(from).unwrap() + span.start;
    let integrated = crate::ops::collect::collector::resource::LoopProof::new_retry(&original, &file).unwrap();
    assert_eq!(integrated.omits(source, &(edit..edit + from.len()), to), result);
    result
}

#[test]
fn complete_minimizer_source_proves_warm_partition_retry_fixed_points() {
    let source = complete_source();
    for (from, to) in [
        ("let mut reduced = false;", "let mut reduced = true;"),
        ("if reduced {", "if true {"),
        ("granularity = granularity.saturating_mul(2).min(active.len());", ""),
        (
            "if granularity >= active.len() {\n                break;",
            "if granularity >= active.len() {",
        ),
        ("granularity.saturating_mul(2)", "granularity.saturating_mul(1)"),
        ("granularity >= active.len()", "granularity > active.len()"),
    ] {
        assert!(proof(&source, from, to), "{from} -> {to}");
    }
    let chunk_fails = source.replace(
        "(std::vec::Vec::from([1u32,2]), Some(false))",
        "(std::vec::Vec::from([1u32,2]), Some(true))",
    );
    assert!(proof(&chunk_fails, "active.clone_from(chunk);", ""));
    let complement_fails = source.replace(
        "(std::vec::Vec::from([2u32,3,4]), Some(false))",
        "(std::vec::Vec::from([2u32,3,4]), Some(true))",
    );
    assert!(
        proof(&complement_fails, "active = complement;", ""),
        "the complete 3/4-partition cycle repeats without reducing population"
    );
}

#[test]
fn exact_cache_misses_and_finite_alternate_results_are_not_assumed_stagnant() {
    let source = complete_source();
    let edit = "granularity = granularity.saturating_mul(2).min(active.len());";
    for changed in [
        source.replace("(std::vec::Vec::from([1u32,2]), Some(false)),", ""),
        source.replace(
            "(std::vec::Vec::from([1u32,2]), Some(false))",
            "(std::vec::Vec::from([1u32,2]), Some(true))",
        ),
        source.replace(
            "(std::vec::Vec::from([3u32,4]), Some(false))",
            "(std::vec::Vec::from([3u32,4]), Some(true))",
        ),
        source.replace(
            "self.isolation_budget.proofs = 0usize;",
            "self.isolation_budget.proofs = 2048usize;",
        ),
        source.replace("proofs.local = 0usize;", "proofs.local = 32usize;"),
    ] {
        assert!(!proof(&changed, edit, ""), "{changed}");
    }
    let alternate = source.replace(
        "(std::vec::Vec::from([2u32,3,4]), Some(false))",
        "(std::vec::Vec::from([2u32,3,4]), Some(true))",
    );
    assert!(!proof(&alternate, "granularity >= active.len()", "false"));
    assert!(!proof(&source, "granularity.saturating_mul(2)", "granularity.saturating_mul(0)"));
}

#[test]
fn progress_assertions_effects_aliases_and_shadowed_helpers_remain_eligible() {
    let source = complete_source();
    let edit = "granularity = granularity.saturating_mul(2).min(active.len());";
    for operation in [
        "assert!(active.len() < 4 || granularity > 2);",
        "unknown(proofs);",
        "proofs.local += 1;",
        "self.isolation_budget.proofs += 1;",
        "let alias = &mut proofs.results;",
        "return Err(1);",
    ] {
        let changed = source.replace("let width = active.len()", &format!("{operation} let width = active.len()"));
        assert!(!proof(&changed, edit, ""), "{operation}");
    }
    for changed in [
        source.replace("fn subset_fails(", "#[cfg(feature=\"other\")] fn subset_fails("),
        source.replace("fn key(", "#[cfg(feature=\"other\")] fn key("),
        source.replace("let key = Proofs::key(active);", "side_effect(); let key = Proofs::key(active);"),
        source.replace("mutant.ordinal", "unknown(mutant)"),
        source.replace("struct Mutant { ordinal: u32 }", "struct Mutant { ordinal: external::Ordinal }"),
        source.replace(
            "proofs.results = std::collections::HashMap::from",
            "proofs.results = custom_cache::from",
        ),
        source.replace("active.chunks(width)", "unknown_partition(active, width)"),
        source.replace("#[derive(Clone, Copy)]", "#[derive(Clone)]"),
        source.replace(
            "Err(1)\n            }",
            "fn Ok(value: Option<bool>) -> Result<Option<bool>, u32> { unknown(); std::result::Result::Ok(value) } Err(1)\n            }",
        ),
        source.replace(
            "let key = Proofs::key(active);",
            "let key = Proofs::key(active); declare_helpers!();",
        ),
        source
            .replace(
                "let mut granularity: usize = 2;",
                "let mut granularity: usize = 2; let mut previous = None;",
            )
            .replace(
                "let width = active.len()",
                r#"assert!(
                previous.is_none_or(|(length, partitions)| active.len() < length || (active.len() == length && granularity > partitions)),
                "interaction minimization must remove candidates or refine its partition before retrying cached proofs"
            ); previous = Some((active.len(), granularity)); let width = active.len()"#,
            ),
        source.replace(
            "let mut granularity: usize = 2;",
            "let mut granularity: usize = 2; let alias = &mut proofs.results;",
        ),
        source.replace(
            "let mut granularity: usize = 2;",
            "let mut granularity: usize = 2; fn subset_fails() {}",
        ),
        source.replace(
            "Ok(\n            (proofs.local",
            "fn Some(value: bool) -> Option<bool> { unknown(); std::option::Option::Some(value) } Ok(\n            (proofs.local",
        ),
    ] {
        assert!(!proof(&changed, edit, ""), "{changed}");
    }
}

#[test]
fn incomplete_frozen_fixture_does_not_acquire_declarations_from_history_or_helper_names() {
    let file = syn::parse_file(FROZEN).unwrap();
    assert!(Retry::new(&file, &target(&file)).is_none());
}

#[test]
fn collector_omits_only_resolved_warm_retry_stagnation() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::collect_in;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    let source = complete_source();
    let statement = "granularity = granularity.saturating_mul(2).min(active.len());";
    for (source, retained) in [
        (source.clone(), false),
        (source.replace("(std::vec::Vec::from([1u32,2]), Some(false)),", ""), true),
        (
            source.replace("let width = active.len()", "assert!(granularity > 1); let width = active.len()"),
            true,
        ),
        (FROZEN.to_owned(), true),
    ] {
        let at = source.find(statement).unwrap();
        let file = SourceFile::parse("retry.rs", source).unwrap();
        let candidates = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
        assert_eq!(candidates.iter().any(|candidate| candidate.span.start == at), retained);
    }
}

#[test]
fn nov16_constant_product_checks_the_binary_node_and_right_operand() {
    let known = HashMap::default();
    let mut product: Expr = syn::parse_quote!(2 * 3);
    assert_eq!(constant_count(&product, &known, 64), Some(6));
    let Expr::Binary(binary) = &mut product else { unreachable!() };
    binary.attrs.push(syn::parse_quote!(#[cfg(feature = "other")]));
    assert_eq!(constant_count(&product, &known, 64), None);
    assert_eq!(constant_count(&syn::parse_quote!(2 * missing), &known, 64), None);
    assert_eq!(constant_count(&syn::parse_quote!(128 * 256), &known, 64), None);
    assert_eq!(constant_count(&syn::parse_quote!(128 * 255), &known, 64), Some(32640));
}

#[test]
fn nov16_entry_metadata_is_checked_on_the_actual_assignment_and_constructor_nodes() {
    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let steady = syn::parse_str("while ready { self.check(&active, &mut proofs)?; continue; }").unwrap();
    assert!(Retry::new(&file, &original).unwrap().proves(&original, &steady));
    for (from, to) in [
        (
            "std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))])",
            "#[cfg(feature = \"other\")] std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))])",
        ),
        (
            "std::vec::Vec::from([1u32, 3])",
            "#[cfg(feature = \"other\")] std::vec::Vec::from([1u32, 3])",
        ),
        (
            "std::collections::HashMap::from([(std::vec::Vec::from([1u32, 3]), Some(false))])",
            "std::collections::HashMap::from(#[cfg(feature = \"other\")] [(std::vec::Vec::from([1u32, 3]), Some(false))])",
        ),
        ("[1u32, 3]), Some(false)", "[1u32, 3]), #[cfg(feature = \"other\")] None"),
    ] {
        let changed = source.replace(from, to);
        assert_ne!(changed, source);
        let file = syn::parse_file(&changed).unwrap();
        assert!(CachePrefix::resolve(&file, "Search", "check").is_some(), "{to}");
        assert!(Retry::new(&file, &target(&file)).is_none(), "{to}");
    }
    let changed = entry_source(
        &format!("{ENTRY} #[cfg(feature = \"other\")] proofs.local = 9usize;"),
        "self.check(&active, &mut proofs)?; break;",
    );
    let file = syn::parse_file(&changed).unwrap();
    assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
    assert!(Retry::new(&file, &target(&file)).is_none());
}

#[test]
fn entry_state_does_not_apply_a_configured_assignment_to_the_snapshot() {
    let file = syn::parse_file("struct Search;").unwrap();
    let signature = syn::parse_quote!(fn run(&mut self));
    let initial: Stmt = syn::parse_quote!(let mut count: usize = 3;);
    let assignment: Stmt = syn::parse_quote!(count = 9;);
    let mut state = entry::State::new(&file, "Search", &signature, "ordinal").unwrap();
    assert_eq!(state.statement(&initial), Some(()));
    assert_eq!(state.statement(&assignment), Some(()));
    let mut retry = empty_retry();
    assert_eq!(state.install(&mut retry), Some(()));
    assert_eq!(retry.counts["count"], 9);

    let mut configured = assignment;
    let Stmt::Expr(Expr::Assign(assignment), _) = &mut configured else {
        unreachable!()
    };
    assignment.attrs.push(syn::parse_quote!(#[cfg(any())]));
    let mut state = entry::State::new(&file, "Search", &signature, "ordinal").unwrap();
    assert_eq!(state.statement(&initial), Some(()));
    assert_eq!(state.statement(&configured), None);
    assert_eq!(state.install(&mut retry), Some(()));
    assert_eq!(retry.counts["count"], 3);
}

#[test]
fn entry_state_installation_checks_the_combined_binding_limit() {
    let file = syn::parse_file("struct Search;").unwrap();
    let signature = syn::parse_quote!(fn run(&mut self));
    for total in [63, 64, 65] {
        let mut retry = empty_retry();
        let _previous = retry.bindings.insert("existing".into(), Binding::Bool);
        let mut state = entry::State::new(&file, "Search", &signature, "ordinal").unwrap();
        for index in 2..total {
            let statement = syn::parse_str(&format!("let extra{index} = true;")).unwrap();
            assert_eq!(state.statement(&statement), Some(()));
        }
        assert_eq!(state.install(&mut retry), (total <= 64).then_some(()));
        assert_eq!(retry.bindings.len(), total);
        assert_eq!(retry.seeds[&format!("extra{}", total - 1)], syn::parse_quote!(true));
    }
}

#[test]
fn nov16_inferred_assignment_counts_and_global_installation_have_independent_limits() {
    let source = entry_source(&format!("{ENTRY} proofs.local = 17;"), "self.check(&active, &mut proofs)?; break;");
    let retry = entry_retry(&source).unwrap();
    assert_eq!(retry.counts["proofs.local"], 17);
    for (depth, expected) in [(63, Some(1)), (64, Some(1)), (65, None)] {
        let expression = format!("{}1", "1 * ".repeat(depth - 1));
        let source = format!("{source} const DEEP: usize = {expression};");
        let retry = entry_retry(&source).unwrap();
        assert_eq!(retry.globals.get("DEEP").copied(), expected, "depth {depth}");
    }

    let base = retry.bindings.len();
    for total in [63, 64, 65] {
        let constants = (0..total - base).fold(String::new(), |mut output, index| {
            write!(output, "const EXTRA{index}: usize = 1;").unwrap();
            output
        });
        assert_eq!(
            entry_retry(&format!("{source} {constants}")).is_some(),
            total <= 64,
            "bindings after constants {total}"
        );
    }
}

#[test]
fn nov16_unrelated_population_records_still_require_the_certified_ordinal_type() {
    let source = entry_source(
        &format!("{ENTRY} let extra = Other {{ ordinal: 9 }}; let unrelated = [&extra];"),
        "self.check(&active, &mut proofs)?; break;",
    );
    assert!(entry_retry(&source).is_some());
    let changed = source.replace("struct Other { ordinal: u32 }", "struct Other { ordinal: u16 }");
    let file = syn::parse_file(&changed).unwrap();
    assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
    assert!(Retry::new(&file, &target(&file)).is_none());
}

#[test]
fn nov16_entry_work_limit_is_reached_inside_a_supported_initializer() {
    let elements = std::iter::repeat_n("&first", 32).collect::<Vec<_>>().join(",");
    let mut setup = ENTRY.to_owned();
    for index in 0..3 {
        write!(setup, "let population{index} = [{elements}];").unwrap();
    }
    for index in 0..19 {
        write!(setup, "let flag{index} = false;").unwrap();
    }
    let source = entry_source(&setup, "self.check(&active, &mut proofs)?; break;");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    assert_eq!(retry.populations["population2"], vec![3; 32]);
    assert_eq!(retry.seeds["flag18"], syn::parse_quote!(false));
    let changed = source.replace("let flag18 = false;", "let flag18 = &false;");
    let file = syn::parse_file(&changed).unwrap();
    assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
    assert!(Retry::new(&file, &target(&file)).is_none());
}

#[test]
fn nov16_distinct_resolved_ordinal_projections_do_not_share_an_entry_snapshot() {
    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;")
        .replace("struct Candidate { ordinal: u32 }", "struct Candidate { ordinal: u32, other: u32 }")
        .replace("Candidate { ordinal: 3u32 }", "Candidate { ordinal: 3u32, other: 3u32 }")
        .replace("Candidate { ordinal: 1u32 }", "Candidate { ordinal: 1u32, other: 1u32 }");
    let source = format!(
        "{source} impl Proofs {{ fn other_key(active: &[&Candidate]) -> Vec<u32> {{
            let mut key = active.iter().map(|candidate| candidate.other).collect::<Vec<_>>();
            key.sort_unstable(); key
        }} }} impl Search {{
            fn second(&mut self, active: &[&Candidate], proofs: &mut Proofs) -> Result<Option<bool>, u32> {{
                let key = Proofs::other_key(active);
                if let Some(cached) = proofs.results.get(&key) {{ return Ok(*cached); }}
                Err(1)
            }}
        }}"
    );
    assert!(entry_retry(&source).is_some());
    let changed = source.replace(
        "self.check(&active, &mut proofs)?; break;",
        "self.check(&active, &mut proofs)?; self.second(&active, &mut proofs)?; break;",
    );
    let file = syn::parse_file(&changed).unwrap();
    assert_eq!(CachePrefix::resolve(&file, "Search", "check").unwrap().population().1, "ordinal");
    assert_eq!(CachePrefix::resolve(&file, "Search", "second").unwrap().population().1, "other");
    assert!(Retry::new(&file, &target(&file)).is_none());
}

#[test]
fn nov16_retry_keeps_typed_scalar_locals_and_exact_recurrence_work_budget() {
    let source = entry_source(ENTRY, "let byte: u8 = 0u8; self.check(&active, &mut proofs)?; break;");
    let file = syn::parse_file(&source).unwrap();
    let original = target(&file);
    let retry = Retry::new(&file, &original).unwrap();
    let steady = syn::parse_str("while ready { let byte: u8 = 0u8; self.check(&active, &mut proofs)?; continue; }").unwrap();
    assert!(retry.proves(&original, &steady));
    assert!(!retry.proves(&original, &original));
    for (negations, expected) in [(0, true), (1, true), (2, false)] {
        let body = format!(
            "{}{} self.check(&active, &mut proofs)?;",
            "r;".repeat(1360 - negations),
            "!r;".repeat(negations),
        );
        let source = entry_source(&format!("{ENTRY} let r = true;"), &format!("{body} break;"));
        let file = syn::parse_file(&source).unwrap();
        let original = target(&file);
        let retry = Retry::new(&file, &original).unwrap();
        let steady = syn::parse_str(&format!("while ready {{ {body} }}")).unwrap();
        assert_eq!(retry.proves(&original, &steady), expected, "extra work {negations}");
        assert!(!retry.proves(&original, &original));
    }
}

#[test]
fn nov16_entry_references_arrays_and_paths_check_metadata_at_their_actual_nodes() {
    let setup = ENTRY.replace("[&first, &second].to_vec()", "[&first, &second]");
    let source = entry_source(&setup, "self.check(&active, &mut proofs)?; break;");
    assert!(entry_retry(&source).is_some());
    for (from, to) in [
        ("[&first, &second]", "#[cfg(feature = \"other\")] [&first, &second]"),
        ("[&first, &second]", "[#[cfg(feature = \"other\")] &first, &second]"),
        ("[&first, &second]", "[&first, #[cfg(feature = \"other\")] second]"),
    ] {
        let changed = source.replace(from, to);
        assert_ne!(changed, source);
        let file = syn::parse_file(&changed).unwrap();
        assert!(CachePrefix::resolve(&file, "Search", "check").is_some());
        assert!(Retry::new(&file, &target(&file)).is_none(), "{to}");
    }
}

#[test]
fn nov16_valid_but_unmodeled_entry_places_and_record_types_do_not_panic() {
    for (setup, signature, declaration) in [
        (
            "let unused = std::ops::Range { start: 0usize, end: 1usize };",
            "fn run(&mut self)",
            "struct Search;",
        ),
        (
            "let unused = Self { flag: true };",
            "fn run(&mut self)",
            "struct Search { flag: bool }",
        ),
        (
            "other.flag = true;",
            "fn run(&mut self, other: &mut Self)",
            "struct Search { flag: bool }",
        ),
        (
            "range.start = 1usize;",
            "fn run(&mut self, range: &mut std::ops::Range<usize>)",
            "struct Search;",
        ),
        ("*flag = true;", "fn run(&mut self, flag: &mut bool)", "struct Search;"),
        ("let unused = std::primitive::u32::MAX;", "fn run(&mut self)", "struct Search;"),
        ("let MAX = true; let unused = <u32>::MAX;", "fn run(&mut self)", "struct Search;"),
    ] {
        let source = entry_source(&format!("{ENTRY} {setup}"), "self.check(&active, &mut proofs)?; break;")
            .replace("fn run(&mut self)", signature)
            .replace("struct Search;", declaration);
        let file = syn::parse_file(&source).unwrap();
        assert!(CachePrefix::resolve(&file, "Search", "check").is_some(), "{setup}");
        assert!(Retry::new(&file, &target(&file)).is_none(), "{setup}");
    }
    let source = entry_source(
        &format!("{ENTRY} let unused = Search {{ flag: true }};"),
        "self.check(&active, &mut proofs)?; break;",
    )
    .replace("struct Search;", "struct Search { flag: bool }");
    assert!(entry_retry(&source).is_some());
}

#[test]
fn nov16_empty_entry_population_does_not_bypass_a_key_helpers_let_else_guard() {
    let setup = ENTRY.replace("[&first, &second]", "[]").replace("[1u32, 3]", "[]");
    let source = entry_source(&setup, "self.check(&active, &mut proofs)?; break;");
    assert!(entry_retry(&source).is_some());
    for collection in [".collect::<Vec<_>>() else { return Vec::new(); };", ".collect::<>();"] {
        let changed = source.replace(".collect::<Vec<_>>();", collection);
        assert_ne!(changed, source);
        let file = syn::parse_file(&changed).unwrap();
        assert!(CachePrefix::resolve(&file, "Search", "check").is_none());
        assert!(Retry::new(&file, &target(&file)).is_none());
    }
}

#[test]
fn nov16_full_declaration_window_does_not_confuse_free_functions_with_snapshot_bindings() {
    let source = entry_source(ENTRY, "self.check(&active, &mut proofs)?; break;");
    let baseline = syn::parse_file(&source).unwrap().items.len();
    for total in [63, 64, 65] {
        let declarations = (0..total - baseline).fold(String::new(), |mut output, index| {
            write!(output, "fn unused{index}() {{}}").unwrap();
            output
        });
        assert_eq!(
            entry_retry(&format!("{source} {declarations}")).is_some(),
            total <= 64,
            "declarations {total}"
        );
    }
}
