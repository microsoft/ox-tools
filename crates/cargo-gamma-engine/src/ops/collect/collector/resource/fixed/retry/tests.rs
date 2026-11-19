// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::fmt::Write as _;

use syn::parse::Parser as _;
use syn::spanned::Spanned as _;
use syn::{File, parse_str};

use super::*;

const SOURCE: &str = r"
    struct Candidate { ordinal: u32 }
    struct Search;
    struct Proofs {
        local: u32,
        results: std::collections::HashMap<Vec<u32>, Option<bool>>,
        attributed: std::collections::HashMap<u32, u32>,
    }
    impl Proofs {
        fn key(active: &[&Candidate]) -> Vec<u32> {
            let mut key = active.iter().map(|candidate| candidate.ordinal).collect();
            key.sort_unstable();
            key
        }
    }
    impl Search {
        fn check(&mut self, active: &[&Candidate], proofs: &mut Proofs) -> Result<Option<bool>, u32> {
            if !proofs.attributed.is_empty() { return Ok(None); }
            let key = Proofs::key(active);
            if let Some(cached) = proofs.results.get(&key) { return Ok(*cached); }
            proofs.local += 1;
            external_build(active, proofs)?;
            Ok(Some(false))
        }
    }
";

fn resolve(source: &str) -> Option<CachePrefix> {
    CachePrefix::resolve(&parse_str(source).unwrap(), "Search", "check")
}

#[test]
fn standard_map_does_not_confuse_a_local_generic_record_with_a_standard_cache() {
    let standard: Type = parse_str("std::collections::HashMap<Vec<u32>, Option<bool>>").unwrap();
    assert_eq!(
        standard_map(&standard),
        Some((&syn::parse_quote!(Vec<u32>), &syn::parse_quote!(Option<bool>)))
    );
    let declaration: syn::ItemStruct = syn::parse_quote!(
        struct xyzzy<K, V>(K, V);
    );
    let local: Type = parse_str(&format!("{}<Vec<u32>, Option<bool>>", declaration.ident)).unwrap();
    assert_eq!(standard_map(&local), None);
}

#[test]
fn nominal_arguments_do_not_resolve_other_owners_aliases_or_associated_collections() {
    let standard: Type = parse_str("std::vec::Vec<u32>").unwrap();
    assert_eq!(arguments(&standard, "Vec", "std::vec::Vec"), Some(vec![&syn::parse_quote!(u32)]));
    for source in [
        "fn take(_: std::collections::HashSet<u32>) {}",
        "type Vec = std::vec::Vec<u32>; fn take(_: Vec) {}",
        "mod std { pub trait vec { type Vec<T>; } }
         fn take<T: std::vec>(_: <T as std::vec>::Vec<u32>) {}",
    ] {
        let file: File = parse_str(source).unwrap();
        let Item::Fn(function) = file.items.last().unwrap() else {
            unreachable!()
        };
        let FnArg::Typed(parameter) = &function.sig.inputs[0] else {
            unreachable!()
        };
        assert!(arguments(&parameter.ty, "Vec", "std::vec::Vec").is_none(), "{source}");
    }
}

#[test]
fn a_copy_record_name_does_not_resolve_a_shadowing_type_parameters_associated_item() {
    let file: File = parse_str(
        "#[derive(Copy, Clone)] struct Context;
         fn take<Context: Iterator>(_: Context::Item) {}",
    )
    .unwrap();
    let scope = Scope::new(&file).unwrap();
    assert!(scope.copy_record(&syn::parse_quote!(Context)));
    let Item::Fn(function) = &file.items[1] else { unreachable!() };
    let FnArg::Typed(parameter) = &function.sig.inputs[0] else {
        unreachable!()
    };
    assert!(!scope.copy_record(&parameter.ty));
}

#[test]
fn key_projection_does_not_resolve_receivers_or_unit_returning_helpers() {
    for (method, expected) in [
        (
            "fn key(active: &[&Candidate]) -> Vec<u32> {
                let mut key = active.iter().map(|candidate| candidate.ordinal).collect();
                key.sort_unstable();
                key
            }",
            Some("ordinal"),
        ),
        ("fn key(&self) -> Vec<u32> { Vec::new() }", None),
        ("fn key(active: &[&Candidate]) {}", None),
        (
            "fn key(active: &[&Candidate], ignored: ()) -> Vec<u32> {
                let mut key = active.iter().map(|candidate| candidate.ordinal).collect();
                key.sort_unstable();
                key
            }",
            None,
        ),
    ] {
        let file: File = parse_str(&format!(
            "struct Candidate {{ ordinal: u32 }}
             struct Proofs;
             impl Proofs {{ {method} }}"
        ))
        .unwrap();
        let scope = Scope::new(&file).unwrap();
        let Item::Impl(implementation) = &file.items[2] else {
            unreachable!()
        };
        let syn::ImplItem::Fn(function) = &implementation.items[0] else {
            unreachable!()
        };
        assert_eq!(scope.key_projection(function, "Candidate").as_deref(), expected, "{method}");
    }
}

#[test]
fn resolved_population_hit_preserves_verdicts_and_repeats_without_using_the_miss_suffix() {
    let prefix = resolve(SOURCE).unwrap();
    assert_eq!(prefix.population(), ("active", "ordinal"));
    assert_eq!(prefix.cache(), ["proofs", "results"]);
    assert_eq!(prefix.empty_guard().unwrap(), ["proofs", "attributed"]);
    let entries = vec![(vec![2, 5], Some(true)), (vec![1, 3], Some(false)), (vec![4, 4], None)];
    let mut budget = 256;
    for (population, verdict) in [
        (vec![5, 2], Some(true)),
        (vec![3, 1], Some(false)),
        (vec![4, 4], None),
        (vec![2, 5], Some(true)),
    ] {
        assert_eq!(prefix.hit(&population, &entries, Some(true), &mut budget), Some(verdict));
    }
    // Neither a different population nor a fresh proof is assigned the cached result.
    assert_eq!(prefix.hit(&[2, 3], &entries, Some(true), &mut budget), None);
    assert_eq!(prefix.hit(&[2, 2, 5], &entries, Some(true), &mut budget), None);
    assert_eq!(prefix.hit(&[2, 5], &[], Some(true), &mut budget), None);
    assert_eq!(prefix.hit(&[2, 5], &entries, None, &mut budget), None);
    assert_eq!(prefix.hit(&[2, 5], &entries, Some(false), &mut budget), Some(None));
}

#[test]
fn neither_fresh_proof_progress_nor_errors_are_summarized_as_pure_cache_hits() {
    for prefix in [
        "proofs.local += 1;",
        "self.advance();",
        "unknown(proofs);",
        "fallible(proofs)?;",
        "let alias = &mut proofs.results;",
        "assert!(proofs.local < 3);",
        "if proofs.local >= 3 { return Err(4); }",
        "return Err(4);",
    ] {
        let changed = SOURCE.replace(
            "if !proofs.attributed.is_empty()",
            &format!("{prefix} if !proofs.attributed.is_empty()"),
        );
        assert!(resolve(&changed).is_none(), "{prefix}");
    }
    for changed in [
        SOURCE.replace("return Ok(*cached)", "proofs.local += 1; return Ok(*cached)"),
        SOURCE.replace("return Ok(*cached)", "return Err(7)"),
        SOURCE.replace("return Ok(*cached)", "return Ok(Some(true))"),
        SOURCE.replace("proofs.results.get", "proofs.results.remove"),
        SOURCE.replace("Proofs::key(active)", "unresolved(active)"),
        SOURCE.replace("candidate.ordinal", "candidate.ordinal + 1"),
        SOURCE.replace("candidate.ordinal", "unknown(candidate)"),
        SOURCE.replace("key.sort_unstable();", "key.sort_by(custom);"),
        SOURCE.replace("key.sort_unstable();", "key.push(7); key.sort_unstable();"),
        SOURCE.replace("key.sort_unstable();", ""),
    ] {
        assert!(resolve(&changed).is_none(), "{changed}");
    }
}

#[test]
fn declaration_shadowing_configuration_and_custom_types_withhold_evidence() {
    for declaration in [
        "use external::Proofs;",
        "use external::*;",
        "type Result<T, E> = external::Result<T, E>;",
        "struct std;",
        "struct Vec;",
        "struct Option;",
        "struct Some;",
        "struct Ok;",
        "struct u32;",
        "declare_helpers!();",
        "#[cfg(feature = \"other\")] impl Proofs { fn key(active: &[&Candidate]) -> Vec<u32> { unknown() } }",
        "impl Drop for Candidate { fn drop(&mut self) { external(); } }",
    ] {
        assert!(resolve(&format!("{SOURCE} {declaration}")).is_none(), "{declaration}");
    }
    for changed in [
        SOURCE.replace("struct Candidate", "#[cfg(feature=\"other\")] struct Candidate"),
        SOURCE.replace("fn key(", "#[cfg(feature=\"other\")] fn key("),
        SOURCE.replace("fn check(", "#[cfg(feature=\"other\")] fn check("),
        SOURCE.replace("ordinal: u32", "ordinal: external::Ordinal"),
        SOURCE.replace("std::collections::HashMap<Vec<u32>, Option<bool>>", "external::Cache"),
        SOURCE.replace("let key = Proofs::key", "let key = custom::key"),
        SOURCE.replace(
            "let key = Proofs::key(active);",
            "let Proofs = shadow; let key = Proofs::key(active);",
        ),
        SOURCE.replace("if let Some(cached)", "#[cfg(feature=\"other\")] if let Some(cached)"),
        SOURCE.replace("key.sort_unstable();", "#[cfg(feature=\"other\")] key.sort_unstable();"),
        SOURCE.replace("|candidate| candidate.ordinal", "|ref candidate| candidate.ordinal"),
        SOURCE.replace("-> Result<Option<bool>, u32>", "-> external::Result<Option<bool>, u32>"),
        SOURCE.replace("proofs: &mut Proofs)", "proofs: &mut Proofs, guard: external::Guard)"),
    ] {
        assert!(resolve(&changed).is_none(), "{changed}");
    }
    assert!(resolve(&SOURCE.replace(".collect();", ".collect::<Vec<_>>();")).is_some());
    assert!(resolve(&SOURCE.replace(".collect();", ".collect::<std::vec::Vec<u32>>();")).is_some());
    assert!(resolve(&SOURCE.replace(".collect();", ".collect::<external::Vec<_>>();")).is_none());
}

#[test]
fn duplicate_cache_entries_arity_and_exhausted_bounds_remain_unknown() {
    let prefix = resolve(SOURCE).unwrap();
    for entries in [
        vec![(vec![1], Some(true)), (vec![1], Some(false))],
        vec![(vec![1; 33], Some(true))],
        (0..33).map(|key| (vec![key], Some(true))).collect(),
    ] {
        assert!(prefix.hit(&[1], &entries, Some(true), &mut 256).is_none());
    }
    assert!(prefix.hit(&[1; 33], &[], Some(true), &mut 256).is_none());
    for budget in 0..4 {
        assert!(prefix.hit(&[1], &[(vec![1], Some(true))], Some(true), &mut { budget }).is_none());
    }
    assert_eq!(prefix.hit(&[1], &[(vec![1], Some(true))], Some(true), &mut 4), Some(Some(true)));
    for changed in [
        SOURCE.replace("results.get(&key)", "results.get()"),
        SOURCE.replace("results.get(&key)", "results.get(&key, &key)"),
        SOURCE.replace("Proofs::key(active)", "Proofs::key(active, active)"),
        SOURCE.replace("Some(cached)", "Some(cached, other)"),
        SOURCE.replace("attributed.is_empty()", "attributed.is_empty(unknown())"),
        SOURCE.replace("key.sort_unstable()", "key.sort_unstable(unknown())"),
    ] {
        assert!(resolve(&changed).is_none(), "{changed}");
    }
    let crowded = format!("{SOURCE} {}", "struct Extra;".repeat(64));
    assert!(resolve(&crowded).is_none());
}

#[test]
fn unguarded_prefix_is_supported_but_is_not_a_blanket_retry_exclusion() {
    let source = SOURCE.replace("if !proofs.attributed.is_empty() { return Ok(None); }", "");
    let prefix = resolve(&source).unwrap();
    assert_eq!(prefix.empty_guard(), None);
    assert_eq!(prefix.hit(&[3, 1], &[(vec![1, 3], Some(false))], None, &mut 256), Some(Some(false)));
    assert_eq!(prefix.hit(&[3, 1], &[(vec![1, 3], Some(true))], Some(true), &mut 256), None);
    let fixture = syn::parse_file(include_str!("../../fixtures/campaign/isolation.rs")).unwrap();
    assert!(CachePrefix::resolve(&fixture, "Converger", "minimize_interaction").is_none());
    assert!(CachePrefix::resolve(&fixture, "Converger", "subset_fails").is_none());
    let declarations = Scope::new(&fixture).unwrap();
    assert!(!declarations.records.contains_key("Proofs"));
    assert!(!declarations.methods.contains_key(&("Proofs".into(), "key".into())));
}

fn retry_proof_with_constructor(body: &str, cache: Vec<(Vec<u32>, Option<bool>)>, constructor: Option<&str>) -> bool {
    use super::super::{Binding, Primitive, proves_with_retry_effects};
    let prefix = resolve(SOURCE).unwrap();
    let effects = Effects {
        calls: HashMap::from_iter([("search.check".into(), &prefix)]),
        populations: HashMap::from_iter([("active".into(), vec![3, 1]), ("alternate".into(), vec![2])]),
        caches: HashMap::from_iter([("proofs.results".into(), cache)]),
        some_constructor: constructor.map(str::to_owned),
        counts: HashMap::default(),
        scalars: HashMap::default(),
        ordinals: HashSet::default(),
        globals: HashMap::default(),
    };
    let bindings = HashMap::from_iter([
        ("search".into(), Binding::Opaque),
        ("proofs".into(), Binding::Opaque),
        ("proofs.results".into(), Binding::Opaque),
        ("proofs.attributed".into(), Binding::Bool),
        ("proofs.local".into(), Binding::Primitive(Primitive::Unsigned(32))),
        ("active".into(), Binding::Opaque),
        ("alternate".into(), Binding::Opaque),
        ("uncertain".into(), Binding::Bool),
    ]);
    let seeds = HashMap::from_iter([
        ("proofs.local".into(), syn::parse_quote!(0u32)),
        ("proofs.attributed".into(), syn::parse_quote!(true)),
    ]);
    let original = syn::parse_str("while proofs.local < 4u32 { break; }").unwrap();
    let mutated = syn::parse_str(&format!("while proofs.local < 4u32 {{ {body} }}")).unwrap();
    proves_with_retry_effects(
        &original,
        &mutated,
        &bindings,
        &seeds,
        &HashMap::default(),
        true,
        &HashMap::default(),
        Some(&effects),
    )
}

fn retry_proof(body: &str, cache: Vec<(Vec<u32>, Option<bool>)>) -> bool {
    retry_proof_with_constructor(body, cache, Some("Some"))
}

#[test]
fn warm_population_hits_prove_a_repeat_state_but_not_fresh_proofs_or_changed_populations() {
    let stalled = "
        let mut reduced = true;
        if search.check(&active, proofs)? == Some(true) { active = alternate; }
        if reduced { continue; }
        break;
    ";
    assert!(retry_proof(stalled, vec![(vec![1, 3], Some(false))]));
    assert!(retry_proof(stalled, vec![(vec![1, 3], None)]));
    assert!(!retry_proof(
        &stalled.replace("reduced = true", "reduced = false"),
        vec![(vec![1, 3], Some(false))]
    ));
    assert!(
        !retry_proof(stalled, vec![]),
        "a miss may consume budgets, perform IO, or return an error"
    );
    assert!(
        !retry_proof(stalled, vec![(vec![1, 3], Some(true))]),
        "a successful subset changes the population"
    );
    assert!(
        !retry_proof_with_constructor(stalled, vec![(vec![1, 3], Some(false))], None),
        "an unresolved Some helper may have effects"
    );
    for body in [
        "proofs.local += 1; let answer = search.check(&active, proofs)?;",
        "let answer = search.check(&active, proofs)?; proofs.local += 1;",
        "let answer = search.check(&active, proofs)?; active = alternate;",
        "let answer = search.check(&active, proofs)?; unknown(proofs);",
        "let answer = search.check(&active, proofs)?; let alias = &mut proofs;",
        "let answer = search.check(&active, proofs)?; assert!(proofs.local < 4u32);",
        "let answer = search.check(&active, proofs)?; return Err(4);",
        "let answer = search.check(&active, proofs)?; if search.check(&alternate, proofs)? == Some(true) { break; }",
        "let search = true; let answer = search.check(&active, proofs)?;",
        "let search = proofs; let answer = search.check(&active, proofs)?;",
        "let answer = search.unknown(&active, proofs)?;",
        "let answer = search.check(unknown(), proofs)?;",
    ] {
        assert!(!retry_proof(body, vec![(vec![1, 3], Some(false)), (vec![2], Some(true))]), "{body}");
    }
    let exhausted = "let answer = search.check(&active, proofs)?;".repeat(2048);
    assert!(!retry_proof(&exhausted, vec![(vec![1, 3], Some(false))]));
}

#[test]
fn actual_missing_minimizer_sites_stay_eligible_without_population_and_declaration_evidence() {
    use crate::cfg::CfgSet;
    use crate::ops::collect::collect_in;
    use crate::ops::registry::Selection;
    use crate::parse::SourceFile;

    let source = include_str!("../../fixtures/campaign/isolation.rs");
    let file = SourceFile::parse("isolation.rs", source.into()).unwrap();
    let selection = Selection::parse("literal.bool_flip,stmt.delete_assign,loop.delete_break").unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());
    let reduced = source.find("let mut reduced = false").unwrap() + "let mut reduced = ".len();
    let refinement = source
        .find("granularity = granularity.saturating_mul(2).min(active.len());")
        .unwrap();
    let terminal = source.find("if granularity >= active.len() {").unwrap();
    let terminal = terminal + source[terminal..].find("break;").unwrap();
    for (selector, offset) in [
        ("literal.bool_flip", reduced),
        ("stmt.delete_assign", refinement),
        ("loop.delete_break", terminal),
    ] {
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.mutator == selector && candidate.span.start == offset),
            "{selector} at {offset}"
        );
    }
}

fn empty_effects() -> Effects<'static> {
    Effects {
        calls: HashMap::default(),
        populations: HashMap::default(),
        caches: HashMap::default(),
        some_constructor: None,
        counts: HashMap::default(),
        scalars: HashMap::default(),
        ordinals: HashSet::default(),
        globals: HashMap::default(),
    }
}

fn machine<'a>(effects: Option<&'a Effects<'a>>) -> super::super::Machine<'a> {
    super::super::Machine {
        values: HashMap::default(),
        budget: 4096,
        writes: HashSet::default(),
        locals: HashSet::default(),
        facts: HashMap::default(),
        helpers: None,
        restarts: false,
        numeric_names: true,
        stable_calls: None,
        paths: HashMap::default(),
        assumptions: Vec::new(),
        retry: effects,
        breaks: false,
    }
}

#[test]
fn exact_snapshot_limits_include_the_last_key_population_and_entry() {
    let prefix = resolve(SOURCE).unwrap();
    for length in [31, 32, 33] {
        let key = (0..length).collect::<Vec<_>>();
        let mut population = key.clone();
        population.reverse();
        let mut budget = 256;
        assert_eq!(
            prefix.hit(&population, &[(key.clone(), Some(false))], Some(true), &mut budget),
            (length <= 32).then_some(Some(false)),
            "population and key length {length}"
        );
        let entries = (0..length).map(|ordinal| (vec![ordinal], Some(true))).collect::<Vec<_>>();
        assert_eq!(
            prefix.hit(&[0], &entries, Some(true), &mut 256),
            (length <= 32).then_some(Some(true)),
            "entry count {length}"
        );
        let entries = vec![(vec![99], Some(true)), (key, None)];
        assert_eq!(
            prefix.hit(&[99], &entries, Some(true), &mut 256),
            (length <= 32).then_some(Some(true)),
            "an unrelated key still participates in validation"
        );
    }
    for entries in [
        vec![(vec![1], Some(true)), (vec![2], None), (vec![2], None)],
        vec![(vec![1], Some(true)), (vec![2], None), (vec![2], Some(false))],
    ] {
        assert_eq!(prefix.hit(&[1], &entries, Some(true), &mut 256), None);
    }
    assert_eq!(prefix.hit(&[1, 2], &[(vec![2, 1], Some(true))], Some(true), &mut 256), None);
    let entries = [(vec![], None), (vec![1, 2], Some(false)), (vec![3], Some(true))];
    for remaining in [8, 9, 10] {
        let mut budget = remaining;
        assert_eq!(
            prefix.hit(&[2, 1], &entries, Some(true), &mut budget),
            (remaining >= 9).then_some(Some(false))
        );
        assert_eq!(budget, if remaining >= 9 { remaining - 9 } else { remaining });
    }
    assert_eq!(prefix.hit(&[], &[(vec![], None)], Some(true), &mut 2), Some(None));
}

#[test]
fn population_snapshot_limit_is_checked_before_the_empty_guard_shortcut() {
    let prefix = resolve(SOURCE).unwrap();
    for length in [31, 32, 33] {
        let population = vec![1; length];
        let mut budget = 256;
        assert_eq!(
            prefix.hit(&population, &[], Some(false), &mut budget),
            (length <= 32).then_some(None),
            "population length {length}"
        );
        assert_eq!(budget, if length <= 32 { 256 - length - 1 } else { 256 });
    }
}

#[test]
fn prefix_resolution_checks_receiver_arguments_and_each_pure_projection_stage() {
    for parameter in ["number: u32", "enabled: bool", "context: Context"] {
        let source = format!(
            "{} #[derive(Clone)] #[derive(Copy)] struct Context;",
            SOURCE.replace("proofs: &mut Proofs)", &format!("proofs: &mut Proofs, {parameter})"))
        );
        assert!(resolve(&source).is_some(), "{parameter}");
    }
    for (from, to) in [
        ("&mut self,", "&self,"),
        ("&mut self,", "self,"),
        ("&mut self,", ""),
        ("&mut self,", "#[custom] &mut self,"),
        ("active: &[&Candidate]", "#[custom] active: &[&Candidate]"),
        ("active: &[&Candidate]", "active: &mut [&Candidate]"),
        ("active: &[&Candidate]", "active: &[&mut Candidate]"),
        ("active: &[&Candidate]", "active: &[Candidate]"),
        ("proofs: &mut Proofs)", "proofs: &mut Proofs, owned: Proofs)"),
        ("proofs: &mut Proofs)", "proofs: &mut Proofs, active: bool)"),
        ("fn check(", "async fn check("),
        ("fn check(", "unsafe fn check("),
        ("fn check(", "extern \"C\" fn check("),
        ("fn check(", "fn check<T>("),
        (
            "-> Result<Option<bool>, u32>",
            "-> std::result::Result<std::option::Option<bool>, u32> where Self: Sized",
        ),
        ("-> Result<Option<bool>, u32>", "-> Option<bool>"),
        ("return Ok(None);", "return Ok(Some(false));"),
        ("return Ok(None);", "side_effect(); return Ok(None);"),
        ("!proofs.attributed.is_empty()", "proofs.attributed.is_empty()"),
        ("!proofs.attributed.is_empty()", "-proofs.attributed.is_empty()"),
        ("return Ok(None); }", "return Ok(None); } else {}"),
        (
            "attributed: std::collections::HashMap<u32, u32>",
            "attributed: std::collections::HashMap<bool, u32>",
        ),
        ("let key = Proofs::key(active);", "let active = Proofs::key(active);"),
        ("let key = Proofs::key(active);", "#[custom] let key = Proofs::key(active);"),
        ("Proofs::key(active)", "::Proofs::key(active)"),
        ("Proofs::key(active)", "<Proofs>::key(active)"),
        (
            "let key = Proofs::key(active);",
            "let key = Proofs::key(active) else { return Err(7); };",
        ),
        ("let key = Proofs::key(active);", "let proofs = Proofs::key(active);"),
        ("proofs: &mut Proofs)", "proofs: &mut Proofs, None: bool)"),
        ("proofs: &mut Proofs)", "proofs: &mut Proofs, Some: bool)"),
        ("proofs: &mut Proofs)", "proofs: &mut Proofs, Ok: bool)"),
        ("let key = Proofs::key(active);", "let None = Proofs::key(active);"),
        ("let key = Proofs::key(active);", "let Some = Proofs::key(active);"),
        ("let key = Proofs::key(active);", "let Ok = Proofs::key(active);"),
        ("Proofs::key(active)", "Proofs::key::<u32>(active)"),
        ("Proofs::key(active)", "Proofs::<u32>::key(active)"),
        ("Proofs::key(active)", "Proofs::key(&active)"),
        ("results.get(&key)", "results.get(&mut key)"),
        ("results.get(&key)", "results.get(&active)"),
        ("return Ok(*cached); }", "return Ok(*cached); } else {}"),
        ("Some(cached)", "Other(cached)"),
        ("Some(cached)", "Some(ref cached)"),
        ("Vec<u32>, Option<bool>", "Vec<bool>, Option<bool>"),
        ("Vec<u32>, Option<bool>", "Vec<u32>, Option<u32>"),
        ("fn key(", "async fn key("),
        ("fn key(", "fn key<T>("),
        ("fn key(active: &[&Candidate])", "fn key(active: &[&Search])"),
        ("-> Vec<u32>", "-> Vec<bool>"),
        ("let mut key = active", "let mut active = active"),
        ("key.sort_unstable();", "active.sort_unstable();"),
        ("key\n        }", "active\n        }"),
        ("active.iter()", "other.iter()"),
        ("active.iter()", "active.iter(1)"),
        (".map(|candidate|", ".filter(|candidate|"),
        (".collect();", ".collect(1);"),
        (".collect();", ".collect::<Vec<bool>>();"),
        ("|candidate| candidate.ordinal", "|candidate| other.ordinal"),
        ("|candidate| candidate.ordinal", "|candidate| candidate.missing"),
        ("|candidate| candidate.ordinal", "move |candidate| candidate.ordinal"),
        ("|candidate| candidate.ordinal", "async |candidate| candidate.ordinal"),
        ("|candidate| candidate.ordinal", "const |candidate| candidate.ordinal"),
        ("|candidate| candidate.ordinal", "|candidate| -> u32 { candidate.ordinal }"),
    ] {
        let source = SOURCE.replace(from, to);
        assert_ne!(source, SOURCE, "{from}");
        assert!(resolve(&source).is_none(), "{from} -> {to}");
    }
    let source = SOURCE
        .replace("Vec<u32>", "std::vec::Vec<u32>")
        .replace("Option<bool>", "std::option::Option<bool>")
        .replace("Result<", "std::result::Result<");
    assert!(resolve(&source).is_some());
}

#[test]
fn closed_scope_rejects_ambiguity_and_preserves_exact_declaration_windows() {
    for declaration in [
        "struct Candidate { ordinal: u32 }",
        "impl Proofs { fn key(active: &[&Candidate]) -> Vec<u32> { unknown() } }",
        "impl Missing { fn unrelated() {} }",
        "impl Self { fn unrelated() {} }",
        "impl<T> Search { fn unrelated() {} }",
        "impl Search where Self: Sized { fn unrelated() {} }",
        "#[custom] impl Search { fn unrelated() {} }",
        "struct Generic<T> { value: T }",
        "struct Conditional where Self: Sized;",
        "struct Attributed { #[custom] ordinal: u32 }",
        "#[derive(Clone, Custom)] struct Context;",
        "#[custom(Copy)] struct Context;",
        "#[custom] fn unrelated() {}",
        "fn Some() {}",
        "const None: bool = false;",
        "const Other: custom::usize = 3;",
        "#[custom] const Other: usize = 3;",
        "use ::std::collections::HashSet;",
        "#[custom] use std::collections::HashSet;",
    ] {
        assert!(resolve(&format!("{SOURCE} {declaration}")).is_none(), "{declaration}");
    }
    for declaration in [
        "const OTHER: u32 = 3;",
        "const OTHER: bool = false;",
        "use std::collections::HashSet;",
        "fn unrelated() -> u32 { 7 }",
    ] {
        let prefix = resolve(&format!("{SOURCE} {declaration}")).unwrap_or_else(|| panic!("{declaration}"));
        assert_eq!(
            prefix.hit(&[3, 1], &[(vec![1, 3], Some(false))], Some(true), &mut 256),
            Some(Some(false)),
            "{declaration}"
        );
    }
    assert!(resolve(&format!("#![custom] {SOURCE}")).is_none());
    for limit in [63, 64, 65] {
        let items = (0..limit - 5).fold(String::new(), |mut output, index| {
            write!(output, "struct Extra{index};").unwrap();
            output
        });
        assert_eq!(resolve(&format!("{SOURCE} {items}")).is_some(), limit <= 64, "items {limit}");
        let methods = (0..limit - 2).fold(String::new(), |mut output, index| {
            write!(output, "fn extra{index}() {{}}").unwrap();
            output
        });
        assert_eq!(
            resolve(&format!("{SOURCE} impl Search {{ {methods} }}")).is_some(),
            limit <= 64,
            "methods {limit}"
        );
    }
    let base_cost: usize = syn::parse_file(SOURCE)
        .unwrap()
        .items
        .iter()
        .map(|item| item.span().byte_range().len())
        .sum();
    for cost in [65535, 65536, 65537] {
        let source = SOURCE.replace("Ok(Some(false))", &format!("{}Ok(Some(false))", " ".repeat(cost - base_cost)));
        assert_eq!(resolve(&source).is_some(), cost <= 65536, "declaration bytes {cost}");
    }
    for visits in [2047, 2048, 2049] {
        let block = syn::parse_str(&format!("{{ {} }}", "true;".repeat(visits))).unwrap();
        assert_eq!(shadow_free(&block), visits <= 2048, "expression visits {visits}");
    }
    for body in [
        "{ if true { declare!(); } }",
        "{ if true { fn Some() {} } }",
        "{ let (value, Some) = pair; }",
    ] {
        assert!(!shadow_free(&syn::parse_str(body).unwrap()), "{body}");
    }
}

#[test]
fn effect_installation_validates_independent_bounds_and_exact_binding_kinds() {
    use super::super::{Binding, Value};
    let prefix = resolve(SOURCE).unwrap();
    for count in [31, 32, 33] {
        let mut effects = empty_effects();
        effects.calls = (0..count).map(|index| (format!("search.check{index}"), &prefix)).collect();
        assert_eq!(effects.install(&mut machine(None)), (count <= 32).then_some(()));
        for cache in [false, true] {
            let mut effects = empty_effects();
            if cache {
                effects
                    .caches
                    .insert("value".into(), (0..count).map(|index| (vec![index], None)).collect());
            } else {
                effects.populations.insert("value".into(), (0..count).collect());
            }
            let mut evaluator = machine(None);
            evaluator
                .values
                .insert("value".into(), Value::Symbol("value".into(), Binding::Opaque));
            assert_eq!(effects.install(&mut evaluator), (count <= 32).then_some(()));
        }
        let mut effects = empty_effects();
        effects.caches.insert(
            "value".into(),
            vec![(vec![1], None), (vec![2; usize::try_from(count).unwrap()], Some(true))],
        );
        let mut evaluator = machine(None);
        evaluator
            .values
            .insert("value".into(), Value::Symbol("value".into(), Binding::Opaque));
        assert_eq!(effects.install(&mut evaluator), (count <= 32).then_some(()));
    }
    for count in [63, 64, 65] {
        let mut effects = empty_effects();
        let mut evaluator = machine(None);
        for index in 0..count {
            let name = format!("value{index}");
            evaluator.values.insert(name.clone(), Value::Symbol(name.clone(), Binding::Opaque));
            if index < 32 {
                effects.populations.insert(name, vec![1, 3]);
            } else {
                effects.caches.insert(name, vec![(vec![1, 3], Some(false))]);
            }
        }
        assert_eq!(
            effects.install(&mut evaluator),
            (count <= 64).then_some(()),
            "combined snapshots {count}"
        );
        if count <= 64 {
            assert_eq!(evaluator.values["value0"], Value::Population(vec![1, 3]));
            assert_eq!(evaluator.values["value32"], Value::PopulationCache(vec![(vec![1, 3], Some(false))]));
        }
    }
    for value in [None, Some(Value::Bool(true))] {
        for cache in [false, true] {
            let mut effects = empty_effects();
            if cache {
                effects.caches.insert("value".into(), vec![]);
            } else {
                effects.populations.insert("value".into(), vec![]);
            }
            let mut evaluator = machine(None);
            if let Some(value) = &value {
                evaluator.values.insert("value".into(), value.clone());
            }
            assert_eq!(effects.install(&mut evaluator), None);
        }
    }
    let mut effects = empty_effects();
    effects.populations.insert("value".into(), vec![]);
    effects.caches.insert("value".into(), vec![]);
    let mut evaluator = machine(None);
    evaluator
        .values
        .insert("value".into(), Value::Symbol("value".into(), Binding::Opaque));
    assert_eq!(effects.install(&mut evaluator), None);
    for count in [32766, 32767, 32768] {
        let mut effects = empty_effects();
        effects.counts.insert("count".into(), count);
        assert_eq!(effects.install(&mut machine(None)), None);
        let mut evaluator = machine(None);
        evaluator
            .values
            .insert("count".into(), Value::Symbol("count".into(), Binding::Integer));
        assert_eq!(effects.install(&mut evaluator), (count <= 32767).then_some(()));
        if count <= 32767 {
            assert_eq!(evaluator.values["count"], Value::Count(count));
        }
    }
}

#[test]
fn resolved_hit_forwarding_requires_exact_receiver_places_and_snapshot_provenance() {
    use super::super::{Binding, Value};
    let prefix = resolve(SOURCE).unwrap();
    let effects = Effects {
        calls: HashMap::from_iter([("runner.check".into(), &prefix)]),
        ..empty_effects()
    };
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("runner".into(), Value::Symbol("runner".into(), Binding::Opaque)),
        ("selected".into(), Value::Population(vec![3, 1])),
        ("state".into(), Value::Symbol("state".into(), Binding::Opaque)),
        ("state.results".into(), Value::PopulationCache(vec![(vec![1, 3], Some(false))])),
        ("state.attributed".into(), Value::Bool(true)),
    ]);
    for call in ["runner.check(&selected, &mut state)", "runner.check(selected, state)"] {
        let before = evaluator.values.clone();
        assert_eq!(
            effects.hit(&mut evaluator, &parse_str(call).unwrap()),
            Some(Value::MaybeBool(Some(false)))
        );
        assert_eq!(evaluator.values, before);
    }
    for call in [
        "#[custom] runner.check(&selected, state)",
        "runner.check::<u32>(&selected, state)",
        "runner.check(&selected)",
        "runner.check(&selected, state, state)",
        "runner.unknown(&selected, state)",
        "missing.check(&selected, state)",
        "factory().check(&selected, state)",
        "runner.check(unknown(), state)",
        "runner.check(&missing, state)",
        "runner.check(&selected[0], state)",
        "runner.check(&*selected, state)",
        "runner.check(#[custom] &selected, state)",
    ] {
        assert_eq!(effects.hit(&mut evaluator, &parse_str(call).unwrap()), None, "{call}");
    }
    for (name, value) in [
        ("runner", Value::Bool(true)),
        ("runner", Value::Symbol("different".into(), Binding::Opaque)),
        ("runner", Value::Symbol("runner".into(), Binding::Bool)),
        ("selected", Value::Count(2)),
        ("state.results", Value::Bool(true)),
        ("state.attributed", Value::Count(0)),
    ] {
        let previous = evaluator.values.insert(name.into(), value).unwrap();
        assert_eq!(
            effects.hit(&mut evaluator, &parse_str("runner.check(&selected, state)").unwrap()),
            None,
            "{name}"
        );
        evaluator.values.insert(name.into(), previous);
    }
    evaluator.values.insert("state.attributed".into(), Value::Bool(false));
    assert_eq!(
        effects.hit(&mut evaluator, &parse_str("runner.check(&selected, state)").unwrap()),
        Some(Value::MaybeBool(None))
    );
}

#[test]
fn scalar_helpers_preserve_argument_types_globals_results_and_shared_work() {
    use super::super::Value;
    for (function, call, expected) in [
        ("fn scalar() -> usize { 0 }", "Proofs::scalar()", Some(Value::Count(0))),
        ("fn scalar() -> usize { 7 }", "Proofs::scalar()", Some(Value::Count(7))),
        ("fn scalar(n: usize) -> usize { n }", "Proofs::scalar(count)", Some(Value::Count(9))),
        (
            "fn scalar(n: usize) -> usize { n.max(LIMIT) }",
            "Proofs::scalar(count)",
            Some(Value::Count(12)),
        ),
        (
            "fn scalar(enabled: bool) -> bool { enabled }",
            "Proofs::scalar(false)",
            Some(Value::Bool(false)),
        ),
        (
            "fn scalar(enabled: bool) -> bool { enabled }",
            "Proofs::scalar(true)",
            Some(Value::Bool(true)),
        ),
        (
            "fn scalar(LIMIT: usize) -> usize { LIMIT }",
            "Proofs::scalar(count)",
            Some(Value::Count(9)),
        ),
        ("fn scalar(n: usize) -> usize { n }", "Proofs::scalar(true)", None),
        ("fn scalar(n: usize) -> usize { n }", "Proofs::scalar(1)", None),
        ("fn scalar(enabled: bool) -> bool { enabled }", "Proofs::scalar(count)", None),
        ("fn scalar(n: u32) -> usize { 0 }", "Proofs::scalar(count)", None),
        ("fn scalar() -> bool { count }", "Proofs::scalar()", None),
        ("fn scalar() -> usize { Proofs::scalar() }", "Proofs::scalar()", None),
        ("fn scalar() -> custom::usize { 0 }", "Proofs::scalar()", None),
        ("fn scalar() { true }", "Proofs::scalar()", None),
        ("fn scalar() -> bool { let x = true; x }", "Proofs::scalar()", None),
        ("fn scalar(&self) -> bool { true }", "Proofs::scalar(count)", None),
        ("fn scalar(#[custom] n: usize) -> usize { n }", "Proofs::scalar(count)", None),
        ("fn scalar(ref n: usize) -> usize { n }", "Proofs::scalar(count)", None),
        ("fn scalar() -> bool { true }", "Proofs::scalar(count)", None),
        ("fn scalar() -> bool { true }", "#[custom] Proofs::scalar()", None),
        ("fn scalar() -> bool { true }", "Other::scalar()", None),
    ] {
        let mut effects = empty_effects();
        effects.scalars.insert("Proofs :: scalar".into(), parse_str(function).unwrap());
        effects.globals.insert("LIMIT".into(), 12);
        let mut evaluator = machine(Some(&effects));
        evaluator.values.insert("count".into(), Value::Count(9));
        assert_eq!(
            effects.scalar(&mut evaluator, &parse_str(call).unwrap()),
            expected,
            "{function}: {call}"
        );
    }
    for body in ["true", "unknown"] {
        let mut effects = empty_effects();
        effects.scalars.insert(
            "Proofs :: scalar".into(),
            parse_str(&format!("fn scalar() -> bool {{ {body} }}")).unwrap(),
        );
        for remaining in [0, 1, 2] {
            let mut evaluator = machine(Some(&effects));
            evaluator.budget = remaining;
            assert_eq!(
                effects.scalar(&mut evaluator, &parse_str("Proofs::scalar()").unwrap()),
                (remaining >= 1 && body == "true").then_some(Value::Bool(true))
            );
            assert_eq!(
                evaluator.budget,
                remaining.saturating_sub(1),
                "nested work must be charged on failure too"
            );
        }
    }
}

#[test]
fn population_comparisons_and_count_methods_cover_order_equality_zero_and_limits() {
    use super::super::Value;
    for (operator, results) in [
        ("<", [false, false, true]),
        ("<=", [false, true, true]),
        (">", [true, false, false]),
        (">=", [true, true, false]),
        ("==", [false, true, false]),
        ("!=", [true, false, true]),
    ] {
        for (right, expected) in [2, 3, 4].into_iter().zip(results) {
            let mut evaluator = machine(None);
            evaluator.values.insert("count".into(), Value::Count(3));
            assert_eq!(
                population::eval(&mut evaluator, &parse_str(&format!("count {operator} {right}")).unwrap()),
                Some(Value::Bool(expected))
            );
        }
    }
    for (expression, expected) in [
        ("count == 0", Some(Value::Bool(false))),
        ("count.max(4)", Some(Value::Count(4))),
        ("count.max(2)", Some(Value::Count(3))),
        ("count.min(4)", Some(Value::Count(3))),
        ("count.min(2)", Some(Value::Count(2))),
        ("count.div_ceil(2)", Some(Value::Count(2))),
        ("count.div_ceil(0)", None),
        ("count.saturating_sub(4)", Some(Value::Count(0))),
        ("count.saturating_sub(2)", Some(Value::Count(1))),
        ("count.saturating_mul(0)", Some(Value::Count(0))),
        ("count.saturating_mul(2)", Some(Value::Count(6))),
        ("count + 2", None),
        ("true == 3", None),
        ("count == unknown", None),
        ("count == true", None),
        ("count.other(2)", None),
        ("count.max::<usize>(2)", None),
        ("count.max()", None),
        ("count.max(1, 2)", None),
        ("#[custom] count.max(2)", None),
        ("#[custom] (count <= 3)", None),
    ] {
        let mut evaluator = machine(None);
        evaluator.values.insert("count".into(), Value::Count(3));
        assert_eq!(
            population::eval(&mut evaluator, &parse_str(expression).unwrap()),
            expected,
            "{expression}"
        );
    }
    for (count, multiplier, expected) in [
        (32766, 1, Some(Value::Count(32766))),
        (32767, 1, Some(Value::Count(32767))),
        (16384, 2, None),
        (32767, 3, None),
    ] {
        let mut evaluator = machine(None);
        evaluator.values.insert("count".into(), Value::Count(count));
        assert_eq!(
            population::eval(&mut evaluator, &parse_str(&format!("count.saturating_mul({multiplier})")).unwrap()),
            expected
        );
    }
    for ordinals in [vec![], vec![7, 7, 2]] {
        for (expression, expected) in [
            ("active.len()", Value::Count(u16::try_from(ordinals.len()).unwrap())),
            ("active.is_empty()", Value::Bool(ordinals.is_empty())),
            ("active.clone()", Value::Population(ordinals.clone())),
            ("active.to_vec()", Value::Population(ordinals.clone())),
        ] {
            let mut evaluator = machine(None);
            evaluator.values.insert("active".into(), Value::Population(ordinals.clone()));
            assert_eq!(population::eval(&mut evaluator, &parse_str(expression).unwrap()), Some(expected));
        }
    }
}

#[test]
fn population_collection_preserves_partition_order_and_filters_exact_certified_ordinals() {
    use super::super::Value;
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("active".into(), Value::Population(vec![3, 1, 3, 2, 4])),
        ("removed".into(), Value::Ordinals(vec![3, 4])),
        ("parts".into(), Value::Partitions(vec![vec![3, 1], vec![2]])),
    ]);
    for (expression, expected) in [
        (
            "active.chunks(2).map(<[_]>::to_vec).collect()",
            Value::Partitions(vec![vec![3, 1], vec![3, 2], vec![4]]),
        ),
        (
            "active.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>>()",
            Value::Partitions(vec![vec![3, 1], vec![3, 2], vec![4]]),
        ),
        (
            "active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()",
            Value::Ordinals(vec![3, 1, 3, 2, 4]),
        ),
        (
            "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
            Value::Population(vec![1, 2]),
        ),
    ] {
        assert_eq!(
            population::eval(&mut evaluator, &parse_str(expression).unwrap()),
            Some(expected),
            "{expression}"
        );
    }
    for expression in [
        "active.chunks(0).map(<[_]>::to_vec).collect::<Vec<_>>()",
        "active.chunks(2).map(custom).collect::<Vec<_>>()",
        "active.chunks(2).map(<[_]>::to_vec).collect::<HashSet<_>>()",
        "active.chunks(2).map(<[_]>::to_vec).collect::<custom::Vec<_>>()",
        "active.chunks(2).map(<[_]>::to_vec).collect::<Vec<u32>>()",
        "active.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>, Vec<_>>()",
        "active.chunks(2).map(<[_]>::to_vec).collect(unknown())",
        "active.chunks::<usize>(2).map(<[_]>::to_vec).collect()",
        "active.iter().map(|entry| entry.ordinal).collect()",
        "active.iter().map(|entry| entry.ordinal).collect::<Vec<_>>()",
        "active.iter().map(|entry| entry.missing).collect::<HashSet<_>>()",
        "active.iter().map(|entry| other.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|entry| entry.0).collect::<HashSet<_>>()",
        "active.iter().map(|entry| entry).collect::<HashSet<_>>()",
        "active.iter().map(|entry: &&Candidate| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|&entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(Candidate::ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|_| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|ref entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|entry @ _| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|entry, other| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(move |entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(async |entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(const |entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().map(|entry| -> u32 { entry.ordinal }).collect::<HashSet<_>>()",
        "active.iter().copied().filter(|entry| removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| -removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !active.contains(&entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&mut entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&other.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&entry.missing)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<HashSet<_>>()",
        "active.iter().cloned().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
        "parts.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>>()",
        "parts.iter().map(|part| part.len()).collect::<HashSet<_>>()",
        "removed.iter().copied().filter(|id| !removed.contains(id)).collect::<Vec<_>>()",
    ] {
        assert_eq!(
            population::eval(&mut evaluator, &parse_str(expression).unwrap()),
            None,
            "{expression}"
        );
    }
}

#[test]
fn population_collection_does_not_infer_a_modeled_element_from_a_bare_type_alias() {
    use super::super::Value;
    let file: syn::File = syn::parse_quote! {
        struct Candidate { ordinal: u32 }
        fn ordinals(active: &[Candidate]) -> std::collections::HashSet<u32> {
            type HashSet = std::collections::HashSet<u32>;
            active.iter().map(|entry| entry.ordinal).collect::<HashSet>()
        }
    };
    let Item::Fn(function) = &file.items[1] else { unreachable!() };
    let Stmt::Expr(collection, None) = &function.block.stmts[1] else {
        unreachable!()
    };
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("active".into(), Value::Population(vec![3, 1, 3]));
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        Some(Value::Ordinals(vec![3, 1, 3]))
    );
    assert_eq!(population::eval(&mut evaluator, collection), None);
}

#[test]
fn population_collection_rejects_an_associated_collection_of_a_bounded_type_parameter() {
    use super::super::Value;
    let file: syn::File = syn::parse_quote! {
        trait Collections {
            type HashSet<T>: FromIterator<T> where T: Eq + std::hash::Hash;
        }
        struct Candidate { ordinal: u32 }
        fn ordinals<T: Collections>(active: &[Candidate]) -> T::HashSet<u32> {
            active.iter().map(|entry| entry.ordinal).collect::<<T>::HashSet<_>>()
        }
    };
    let Item::Fn(function) = &file.items[2] else { unreachable!() };
    let Stmt::Expr(collection, None) = &function.block.stmts[0] else {
        unreachable!()
    };
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("active".into(), Value::Population(vec![3, 1, 3]));
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        Some(Value::Ordinals(vec![3, 1, 3]))
    );
    assert_eq!(population::eval(&mut evaluator, collection), None);
}

#[test]
fn population_collection_rejects_a_nominal_collections_inherent_associated_type() {
    use super::super::Value;
    let file: syn::File = syn::parse_quote! {
        #![feature(inherent_associated_types)]
        #![allow(incomplete_features)]
        struct HashSet<T>(T);
        impl<T: Eq + std::hash::Hash> HashSet<T> {
            type Collected = std::collections::HashSet<T>;
        }
        struct Candidate { ordinal: u32 }
        fn ordinals(active: &[Candidate]) -> std::collections::HashSet<u32> {
            active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>::Collected>()
        }
    };
    let Item::Fn(function) = &file.items[3] else { unreachable!() };
    let Stmt::Expr(collection, None) = &function.block.stmts[0] else {
        unreachable!()
    };
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("active".into(), Value::Population(vec![3, 1, 3]));
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        Some(Value::Ordinals(vec![3, 1, 3]))
    );
    assert_eq!(population::eval(&mut evaluator, collection), None);
}

#[test]
fn partition_execution_restores_locals_and_obeys_break_continue_and_work_boundaries() {
    use super::super::Value;
    let effects = empty_effects();
    for (body, expected) in [
        ("last = part.len();", Value::Count(2)),
        ("last = part.len(); break; last = unknown();", Value::Count(1)),
        ("last = part.len(); continue; last = unknown();", Value::Count(2)),
        ("if true { break; } last = unknown();", Value::Count(0)),
    ] {
        for previous_local in [false, true] {
            let mut evaluator = machine(Some(&effects));
            evaluator.values.extend([
                ("parts".into(), Value::Partitions(vec![vec![1], vec![2, 3]])),
                ("part".into(), Value::Bool(false)),
                ("last".into(), Value::Count(0)),
            ]);
            if previous_local {
                evaluator.locals.insert("part".into());
            }
            assert_eq!(
                population::for_loop(&mut evaluator, &parse_str(&format!("for part in &parts {{ {body} }}")).unwrap()),
                Some(())
            );
            assert_eq!(evaluator.values["part"], Value::Bool(false));
            assert_eq!(evaluator.values["last"], expected);
            assert_eq!(evaluator.locals.contains("part"), previous_local);
            assert!(!evaluator.breaks);
            assert!(!evaluator.restarts);
        }
    }
    for length in [0, 31, 32, 33] {
        for budget in [31, 32, 33, 256] {
            let mut evaluator = machine(Some(&effects));
            evaluator.values.insert("parts".into(), Value::Partitions(vec![vec![1]; length]));
            evaluator.budget = budget;
            assert_eq!(
                population::for_loop(&mut evaluator, &parse_str("for part in parts {}").unwrap()),
                (length <= 32 && budget > length).then_some(())
            );
            if length <= 32 && budget > length {
                assert!(!evaluator.values.contains_key("part"));
                assert!(!evaluator.locals.contains("part"));
                assert_eq!(evaluator.budget, budget - length - 1);
            }
        }
    }
    for expression in [
        "#[custom] for part in parts {}",
        "'outer: for part in parts {}",
        "for ref part in parts {}",
        "for part @ _ in parts {}",
        "for (part, other) in parts {}",
        "for part in &mut parts {}",
        "for part in #[custom] &parts {}",
        "for part in unknown {}",
        "for part in active {}",
        "for part in parts { unknown(); }",
    ] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("parts".into(), Value::Partitions(vec![vec![1]])),
            ("active".into(), Value::Population(vec![1])),
        ]);
        assert_eq!(
            population::for_loop(&mut evaluator, &parse_str(expression).unwrap()),
            None,
            "{expression}"
        );
    }
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("parts".into(), Value::Partitions(vec![])),
        ("part.ordinal".into(), Value::Count(1)),
    ]);
    assert_eq!(
        population::for_loop(&mut evaluator, &parse_str("for part in parts {}").unwrap()),
        None
    );
}

#[test]
fn population_clone_from_tracks_writes_and_rejects_nonpopulation_or_effectful_places() {
    use super::super::Value;
    let effects = empty_effects();
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("active".into(), Value::Population(vec![1, 2])),
        ("alternate".into(), Value::Population(vec![3])),
        ("count".into(), Value::Count(1)),
    ]);
    assert_eq!(evaluator.effect(&parse_str("active.clone_from(&alternate)").unwrap()), None);
    assert_eq!(evaluator.effect(&parse_str("active.clone_from(alternate)").unwrap()), Some(()));
    assert_eq!(evaluator.values["active"], Value::Population(vec![3]));
    assert!(evaluator.writes.contains("active"));
    for expression in [
        "#[custom] active.clone_from(alternate)",
        "active.clone_from::<u32>(alternate)",
        "active.clone_from()",
        "active.clone_from(alternate, alternate)",
        "count.clone_from(alternate)",
        "active.clone_from(count)",
        "factory().clone_from(alternate)",
        "active.clone_from(unknown())",
        "active.other(alternate)",
    ] {
        assert_eq!(evaluator.effect(&parse_str(expression).unwrap()), None, "{expression}");
    }
    assert_eq!(evaluator.effect(&parse_str("active = alternate").unwrap()), Some(()));
    assert_eq!(evaluator.values["active"], Value::Population(vec![3]));
    let mut plain = machine(None);
    plain.values = evaluator.values.clone();
    assert_eq!(plain.effect(&parse_str("active = alternate").unwrap()), None);
    assert_eq!(plain.effect(&parse_str("active.clone_from(alternate)").unwrap()), None);
    assert_eq!(plain.effect(&parse_str("for part in parts {}").unwrap()), None);
}

#[test]
fn retry_cycles_require_an_unchanged_population_and_a_repeat_before_the_visit_limit() {
    use super::super::{Primitive, Value};
    let effects = empty_effects();
    for period in [1, 2, 30, 31, 32, 33] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("active".into(), Value::Population(vec![1, 2])),
            ("step".into(), Value::Int(0, Primitive::Unsigned(8))),
        ]);
        let body = parse_str(&format!(
            "{{ if step == {}u8 {{ step = 0u8; }} else {{ step += 1u8; }} }}",
            period - 1
        ))
        .unwrap();
        assert_eq!(
            population::repeats(&mut evaluator, &parse_str("true").unwrap(), &body),
            period <= 31,
            "cycle {period}"
        );
    }
    for (condition, body, expected) in [
        ("ready", "{ ready = !ready; }", false),
        ("true", "{ ready = !ready; }", true),
        ("true", "{ continue; unknown(); }", true),
        ("true", "{ ready = !ready; if ready { break; } continue; }", false),
        ("true", "{ break; unknown(); }", false),
        ("true", "{ unknown(); }", false),
        ("unknown", "{}", false),
        ("true", "{ active = alternate; }", false),
        ("step < 3u8", "{ step += 1u8; }", false),
        ("true", "{ step += 1u8; }", false),
    ] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("active".into(), Value::Population(vec![1, 2])),
            ("alternate".into(), Value::Population(vec![3])),
            ("ready".into(), Value::Bool(true)),
            ("step".into(), Value::Int(0, Primitive::Unsigned(8))),
        ]);
        assert_eq!(
            population::repeats(&mut evaluator, &parse_str(condition).unwrap(), &parse_str(body).unwrap()),
            expected,
            "{condition}: {body}"
        );
    }
    let mut evaluator = machine(Some(&effects));
    evaluator.budget = 1;
    assert!(!population::repeats(
        &mut evaluator,
        &parse_str("true").unwrap(),
        &parse_str("{}").unwrap()
    ));
}

#[test]
fn retry_option_comparison_and_escape_grammar_do_not_invent_constructor_or_break_evidence() {
    use super::super::Value;
    let mut effects = empty_effects();
    effects.some_constructor = Some("Some".into());
    for left in [None, Some(false), Some(true)] {
        for right in [None, Some(false), Some(true)] {
            for (operator, expected) in [("==", left == right), ("!=", left != right)] {
                let mut evaluator = machine(Some(&effects));
                evaluator
                    .values
                    .extend([("left".into(), Value::MaybeBool(left)), ("right".into(), Value::MaybeBool(right))]);
                assert_eq!(
                    evaluator.eval(&parse_str(&format!("left {operator} right")).unwrap(), None),
                    Some(Value::Bool(expected))
                );
            }
        }
    }
    let mut evaluator = machine(Some(&effects));
    assert_eq!(
        evaluator.eval(&parse_str("Some(false)").unwrap(), None),
        Some(Value::MaybeBool(Some(false)))
    );
    for expression in [
        "Other(false)",
        "Some()",
        "Some(false, true)",
        "Some(0)",
        "#[custom] Some(false)",
        "#[custom] runner.check()?",
    ] {
        assert_eq!(evaluator.eval(&parse_str(expression).unwrap(), None), None, "{expression}");
    }
    evaluator.values.insert("Some".into(), Value::Bool(true));
    assert_eq!(evaluator.eval(&parse_str("Some(false)").unwrap(), None), None);
    evaluator.values.clear();
    let helpers = HashMap::from_iter([("Some".into(), parse_str("fn Some(value: bool) -> bool { value }").unwrap())]);
    evaluator.helpers = Some(&helpers);
    assert_eq!(evaluator.eval(&parse_str("Some(false)").unwrap(), None), Some(Value::Bool(false)));
    for block in ["{ break; unknown(); }", "{ if true { break; } unknown(); }"] {
        assert_eq!(machine(Some(&effects)).block(&parse_str(block).unwrap()), Some(()));
        assert_eq!(machine(None).block(&parse_str(block).unwrap()), None);
    }
    for block in ["{ #[custom] break; }", "{ break 'outer; }", "{ break true; }"] {
        assert_eq!(machine(Some(&effects)).block(&parse_str(block).unwrap()), None, "{block}");
    }
}

#[test]
fn fallible_calls_without_retry_evidence_remain_unknown_instead_of_panicking() {
    use super::super::{Binding, Value};
    let prefix = resolve(SOURCE).unwrap();
    let effects = Effects {
        calls: HashMap::from_iter([("runner.check".into(), &prefix)]),
        ..empty_effects()
    };
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("runner".into(), Value::Symbol("runner".into(), Binding::Opaque)),
        ("selected".into(), Value::Population(vec![3, 1])),
        ("state".into(), Value::Symbol("state".into(), Binding::Opaque)),
        ("state.results".into(), Value::PopulationCache(vec![(vec![1, 3], Some(false))])),
        ("state.attributed".into(), Value::Bool(true)),
    ]);
    let expression = parse_str("runner.check(&selected, &mut state)?").unwrap();
    assert_eq!(evaluator.eval(&expression, None), Some(Value::MaybeBool(Some(false))));
    let mut unresolved = machine(None);
    unresolved.values = evaluator.values.clone();
    assert_eq!(unresolved.eval(&expression, None), None);
    let bindings = HashMap::from_iter([
        ("runner".into(), Binding::Opaque),
        ("selected".into(), Binding::Opaque),
        ("state".into(), Binding::Opaque),
    ]);
    assert!(!super::super::proves(
        &syn::parse_quote!(while true {
            break;
        }),
        &syn::parse_quote!(while true {
            runner.check(&selected, &mut state)?;
            continue;
        }),
        &bindings,
        &HashMap::default(),
        &HashMap::default(),
        true,
    ));
}

fn attribute_at(block: &mut syn::Block, index: usize) -> usize {
    fn visit_block(block: &mut syn::Block, index: usize, visits: &mut usize) {
        for statement in &mut block.stmts {
            match statement {
                Stmt::Local(local) => visit_expr(&mut local.init.as_mut().unwrap().expr, index, visits),
                Stmt::Expr(expression, _) => visit_expr(expression, index, visits),
                _ => panic!("the fixture contains only initialized locals and expressions"),
            }
        }
    }
    fn visit_expr(expression: &mut Expr, index: usize, visits: &mut usize) {
        if *visits == index {
            let attrs = match expression {
                Expr::MethodCall(node) => &mut node.attrs,
                Expr::Closure(node) => &mut node.attrs,
                Expr::Field(node) => &mut node.attrs,
                Expr::Path(node) => &mut node.attrs,
                Expr::Call(node) => &mut node.attrs,
                Expr::Unary(node) => &mut node.attrs,
                Expr::Reference(node) => &mut node.attrs,
                Expr::If(node) => &mut node.attrs,
                Expr::Let(node) => &mut node.attrs,
                Expr::Return(node) => &mut node.attrs,
                Expr::Lit(node) => &mut node.attrs,
                _ => panic!("the fixture expression has an attribute-bearing node"),
            };
            attrs.push(syn::parse_quote!(#[custom]));
        }
        *visits += 1;
        match expression {
            Expr::MethodCall(node) => {
                visit_expr(&mut node.receiver, index, visits);
                for argument in &mut node.args {
                    visit_expr(argument, index, visits);
                }
            }
            Expr::Call(node) => {
                visit_expr(&mut node.func, index, visits);
                for argument in &mut node.args {
                    visit_expr(argument, index, visits);
                }
            }
            Expr::If(node) => {
                visit_expr(&mut node.cond, index, visits);
                visit_block(&mut node.then_branch, index, visits);
                assert!(node.else_branch.is_none());
            }
            Expr::Closure(node) => visit_expr(&mut node.body, index, visits),
            Expr::Field(node) => visit_expr(&mut node.base, index, visits),
            Expr::Unary(node) => visit_expr(&mut node.expr, index, visits),
            Expr::Reference(node) => visit_expr(&mut node.expr, index, visits),
            Expr::Let(node) => visit_expr(&mut node.expr, index, visits),
            Expr::Return(node) => visit_expr(node.expr.as_mut().unwrap(), index, visits),
            Expr::Path(_) | Expr::Lit(_) => {}
            _ => panic!("the fixture uses only the certified prefix grammar"),
        }
    }
    let mut visits = 0;
    visit_block(block, index, &mut visits);
    visits
}

#[test]
fn prefix_and_key_projection_reject_metadata_on_each_exact_expression_node() {
    for (implementation, prefix_only) in [(3, false), (4, true)] {
        let file = syn::parse_file(SOURCE).unwrap();
        let Item::Impl(owner) = &file.items[implementation] else {
            unreachable!()
        };
        let syn::ImplItem::Fn(function) = &owner.items[0] else {
            unreachable!()
        };
        let mut body = function.block.clone();
        if prefix_only {
            body.stmts.truncate(3);
        }
        let visits = attribute_at(&mut body, usize::MAX);
        for index in 0..visits {
            let mut changed = file.clone();
            let Item::Impl(owner) = &mut changed.items[implementation] else {
                unreachable!()
            };
            let syn::ImplItem::Fn(function) = &mut owner.items[0] else {
                unreachable!()
            };
            function.block = body.clone();
            attribute_at(&mut function.block, index);
            assert!(
                CachePrefix::resolve(&changed, "Search", "check").is_none(),
                "implementation {implementation}, expression {index}"
            );
        }
    }
}

#[test]
fn standard_type_certificates_reject_custom_qualified_and_ambiguous_generic_forms() {
    for (ty, vector, option, population) in [
        ("Vec<u32>", true, false, None),
        ("std::vec::Vec<u32>", true, false, None),
        ("Option<bool>", false, true, None),
        ("std::option::Option<bool>", false, true, None),
        ("&[&Candidate]", false, false, Some("Candidate")),
        ("&[&[Candidate]]", false, false, None),
        ("&mut [&Candidate]", false, false, None),
        ("&[&mut Candidate]", false, false, None),
        ("&[Candidate]", false, false, None),
        ("&Vec<&Candidate>", false, false, None),
        ("&[&module::Candidate]", false, false, None),
        ("&[&<Candidate as Trait>::Type]", false, false, None),
        ("::std::vec::Vec<u32>", false, false, None),
        ("::Option<bool>", false, false, None),
        ("custom::Vec<u32>", false, false, None),
        ("Vec<bool>", false, false, None),
        ("Vec", false, false, None),
        ("Vec<'static>", false, false, None),
        ("Vec<3>", false, false, None),
        ("Vec<u32, u32>", false, false, None),
        ("std::vec::<bool>::Vec<u32>", false, false, None),
        ("<Vec<u32> as Trait>::Type", false, false, None),
        ("Option<u32>", false, false, None),
        ("Option<bool, bool>", false, false, None),
        ("custom::Option<bool>", false, false, None),
    ] {
        let ty = parse_str(ty).unwrap();
        assert_eq!(vector_u32(&ty), vector, "{ty:?}");
        assert_eq!(option_bool(&ty), option, "{ty:?}");
        assert_eq!(population_element(&ty).as_deref(), population, "{ty:?}");
    }
    for (ty, accepted) in [
        ("std::collections::HashMap<Vec<u32>, Option<bool>>", true),
        ("custom::HashMap<Vec<u32>, Option<bool>>", false),
        ("std::collections::HashMap<Vec<u32>>", false),
        ("std::collections::HashMap<Vec<u32>, Option<bool>, u32>", false),
    ] {
        assert_eq!(standard_map(&parse_str(ty).unwrap()).is_some(), accepted, "{ty}");
    }
    for (output, accepted) in [
        ("-> Result<Option<bool>, u32>", true),
        ("-> std::result::Result<std::option::Option<bool>, u32>", true),
        ("-> Result<Option<bool>, bool>", false),
        ("-> Result<Option<u32>, u32>", false),
        ("-> Result<bool, u32>", false),
        ("-> Result<Option<bool>, u32, u32>", false),
        ("", false),
    ] {
        let function: syn::ImplItemFn = parse_str(&format!("fn check() {output} {{}}")).unwrap();
        assert_eq!(fallible_verdict(&function.sig.output), accepted, "{output}");
    }
    let scope_file = syn::parse_file(&format!("{SOURCE} #[derive(Copy)] struct Context;")).unwrap();
    let scope = Scope::new(&scope_file).unwrap();
    for (ty, accepted) in [
        ("Context", true),
        ("(Context,)", false),
        ("module::Context", false),
        ("::Context", false),
        ("<Context as Trait>::Type", false),
        ("Proofs", false),
    ] {
        assert_eq!(scope.copy_record(&parse_str(ty).unwrap()), accepted, "{ty}");
    }
}

#[test]
fn population_collection_rejects_metadata_on_each_iterator_projection_and_filter_operand() {
    use super::super::Value;
    // Numeric widths and the standard to_vec function path are compared or evaluated as
    // whole expressions; test the attribute-bearing pipeline nodes separately.
    struct Nodes(Vec<usize>, usize);
    impl<'ast> syn::visit::Visit<'ast> for Nodes {
        fn visit_expr(&mut self, expression: &'ast Expr) {
            if matches!(
                expression,
                Expr::MethodCall(_) | Expr::Closure(_) | Expr::Field(_) | Expr::Reference(_) | Expr::Unary(_)
            ) {
                self.0.push(self.1);
            }
            self.1 += 1;
            syn::visit::visit_expr(self, expression);
        }
    }
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    for expression in [
        "active.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>>()",
        "active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
    ] {
        let block: syn::Block = parse_str(&format!("{{ {expression}; }}")).unwrap();
        let mut nodes = Nodes(Vec::new(), 0);
        syn::visit::Visit::visit_block(&mut nodes, &block);
        for index in nodes.0 {
            let mut changed = block.clone();
            attribute_at(&mut changed, index);
            let Stmt::Expr(changed, _) = &changed.stmts[0] else {
                unreachable!()
            };
            let mut evaluator = machine(Some(&effects));
            evaluator.values.extend([
                ("active".into(), Value::Population(vec![1, 3])),
                ("removed".into(), Value::Ordinals(vec![1])),
            ]);
            assert_eq!(population::eval(&mut evaluator, changed), None, "{expression}, node {index}");
        }
    }
}

#[test]
fn iterator_arguments_are_not_discarded_on_the_warm_retry_path() {
    use super::super::Value;
    let supported =
        "let parts = active.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>>(); let answer = search.check(&active, proofs)?; continue;";
    assert!(retry_proof(supported, vec![(vec![1, 3], Some(false))]));
    assert!(!retry_proof(
        &supported.replace("chunks(2)", "chunks(2, unknown())"),
        vec![(vec![1, 3], Some(false))]
    ));
    assert!(!retry_proof(supported, vec![]));
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    for (expression, expected, invalid) in [
        (
            "active.chunks(2).map(<[_]>::to_vec).collect::<Vec<_>>()",
            Value::Partitions(vec![vec![3, 1], vec![3]]),
            vec![
                "active.chunks(2, unknown()).map(<[_]>::to_vec).collect::<Vec<_>>()",
                "active.chunks(2).map(<[_]>::to_vec, unknown()).collect::<Vec<_>>()",
            ],
        ),
        (
            "active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()",
            Value::Ordinals(vec![3, 1, 3]),
            vec![
                "active.iter(unknown()).map(|entry| entry.ordinal).collect::<HashSet<_>>()",
                "active.iter().map(|entry| entry.ordinal, unknown()).collect::<HashSet<_>>()",
            ],
        ),
        (
            "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
            Value::Population(vec![1]),
            vec![
                "active.iter(unknown()).copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
                "active.iter().copied(unknown()).filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
                "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal), unknown()).collect::<Vec<_>>()",
                "active.iter().copied().filter(|entry| !removed.contains(&entry.ordinal, unknown())).collect::<Vec<_>>()",
            ],
        ),
    ] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("active".into(), Value::Population(vec![3, 1, 3])),
            ("removed".into(), Value::Ordinals(vec![3])),
        ]);
        assert_eq!(population::eval(&mut evaluator, &parse_str(expression).unwrap()), Some(expected));
        for invalid in invalid {
            assert_eq!(population::eval(&mut evaluator, &parse_str(invalid).unwrap()), None, "{invalid}");
        }
    }
}

#[test]
fn unknown_branch_escape_disagreement_withholds_a_retry_certificate() {
    use super::super::{Binding, Value};
    let effects = empty_effects();
    for (body, effect, repeats) in [
        ("{ if uncertain {} else {} }", Some(()), true),
        ("{ if uncertain { break; } else { break; } }", Some(()), false),
        ("{ if uncertain { break; } }", None, false),
        ("{ if uncertain {} else { break; } }", None, false),
        ("{ if uncertain { continue; } }", None, false),
    ] {
        assert_eq!(
            retry_proof(
                &format!("{body} let answer = search.check(&active, proofs)?;"),
                vec![(vec![1, 3], Some(false))]
            ),
            repeats,
            "{body}"
        );
        let mut evaluator = machine(Some(&effects));
        evaluator
            .values
            .insert("uncertain".into(), Value::Symbol("uncertain".into(), Binding::Bool));
        let block: syn::Block = parse_str(body).unwrap();
        let Stmt::Expr(branch, _) = &block.stmts[0] else { unreachable!() };
        assert_eq!(evaluator.effect(branch), effect, "{body}");
        let mut evaluator = machine(Some(&effects));
        evaluator
            .values
            .insert("uncertain".into(), Value::Symbol("uncertain".into(), Binding::Bool));
        assert_eq!(
            population::repeats(&mut evaluator, &syn::parse_quote!(true), &block),
            repeats,
            "{body}"
        );
    }
}

#[test]
fn conditional_metadata_on_the_binary_node_is_not_a_cache_verdict() {
    use super::super::Value;
    let effects = empty_effects();
    for (expression, expected) in [("left == right", false), ("left != right", true), ("count < 4", true)] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("left".into(), Value::MaybeBool(Some(false))),
            ("right".into(), Value::MaybeBool(Some(true))),
            ("count".into(), Value::Count(3)),
        ]);
        let mut expression: Expr = parse_str(expression).unwrap();
        assert_eq!(evaluator.eval(&expression, None), Some(Value::Bool(expected)));
        let Expr::Binary(binary) = &mut expression else { unreachable!() };
        binary.attrs.push(syn::parse_quote!(#[cfg(feature = "other")]));
        assert_eq!(evaluator.eval(&expression, None), None, "{expression:?}");
    }
    let mut block: syn::Block = parse_str("{ if left == right { break; } }").unwrap();
    let fresh = || {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("left".into(), Value::MaybeBool(Some(false))),
            ("right".into(), Value::MaybeBool(Some(true))),
        ]);
        evaluator
    };
    assert!(population::repeats(&mut fresh(), &syn::parse_quote!(true), &block));
    let Stmt::Expr(Expr::If(branch), _) = &mut block.stmts[0] else {
        unreachable!()
    };
    let Expr::Binary(binary) = &mut *branch.cond else { unreachable!() };
    binary.attrs.push(syn::parse_quote!(#[cfg(feature = "other")]));
    assert!(!population::repeats(&mut fresh(), &syn::parse_quote!(true), &block));
}

#[test]
fn nov16_prefix_truncation_withholds_evidence_without_panicking() {
    let file = syn::parse_file(SOURCE).unwrap();
    for statements in 0..=3 {
        let mut changed = file.clone();
        let Item::Impl(implementation) = &mut changed.items[4] else {
            unreachable!()
        };
        let syn::ImplItem::Fn(function) = &mut implementation.items[0] else {
            unreachable!()
        };
        function.block.stmts.truncate(statements);
        assert_eq!(
            CachePrefix::resolve(&changed, "Search", "check").is_some(),
            statements == 3,
            "prefix statements {statements}"
        );
    }
    for (from, to) in [
        ("let key = Proofs::key(active);", "let key;"),
        ("let key = Proofs::key(active);", "let (key, other) = Proofs::key(active);"),
        ("Proofs::key(active)", "Proofs::key(missing)"),
        ("proofs.results.get", "proofs.missing.get"),
        ("!proofs.attributed.is_empty()", "!proofs.missing.is_empty()"),
        ("|candidate| candidate.ordinal", "|candidate| candidate.ordinal.extra"),
        ("key.sort_unstable();", "key.sort_unstable::<u32>();"),
        ("active.iter()", "active.iter::<u32>()"),
        (".map(|candidate|", ".map::<u32>(|candidate|"),
    ] {
        let changed = SOURCE.replace(from, to);
        assert_ne!(changed, SOURCE);
        assert!(resolve(&changed).is_none(), "{from} -> {to}");
    }
    assert!(resolve(SOURCE).is_some());
}

#[test]
fn nov16_nested_at_patterns_and_closure_parameter_metadata_are_not_shadow_free() {
    use super::super::Value;

    assert!(shadow_free(&parse_str("{ let value @ other = true; }").unwrap()));
    assert!(!shadow_free(&parse_str("{ let value @ std = true; }").unwrap()));
    assert!(shadow_free(&parse_str("{ let callback = |value| value; }").unwrap()));
    assert!(!shadow_free(
        &parse_str("{ let callback = |#[cfg(feature = \"other\")] value| value; }").unwrap()
    ));
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("active".into(), Value::Population(vec![1]));
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|value| value.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        Some(Value::Ordinals(vec![1]))
    );
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|#[cfg(feature = \"other\")] value| value.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        None
    );
}

#[test]
fn nov16_supported_unmodeled_key_owners_parameters_and_projections_return_unknown() {
    let constant_projection = format!(
        "{} const ORDINAL: u32 = 7;",
        SOURCE.replace("|candidate| candidate.ordinal", "|candidate| ORDINAL")
    );
    let generic_parameter = SOURCE
        .replace("fn key(active: &[&Candidate])", "fn key(active: impl core::marker::Sized)")
        .replace("active.iter().map(|candidate| candidate.ordinal).collect()", "Vec::new()");
    let qualified_owner = format!("{SOURCE} impl self::Search {{ fn unrelated() {{}} }}");
    for source in [constant_projection, generic_parameter, qualified_owner] {
        let _parsed = syn::parse_file(&source).unwrap();
        assert!(resolve(&source).is_none(), "{source}");
    }
    assert!(resolve(SOURCE).is_some());
}

#[test]
fn nov16_exact_pattern_and_place_grammar_does_not_invent_default_names() {
    for (source, expected) in [
        ("value", Some("value")),
        ("mut value", Some("value")),
        ("_", None),
        ("ref value", None),
        ("value @ _", None),
    ] {
        let pattern = syn::Pat::parse_single;
        assert_eq!(ident(&pattern.parse_str(source).unwrap()).as_deref(), expected, "{source}");
    }
    let mut pattern: Pat = syn::parse_quote!(value);
    let Pat::Ident(binding) = &mut pattern else { unreachable!() };
    binding.attrs.push(syn::parse_quote!(#[cfg(feature = "other")]));
    assert_eq!(ident(&pattern), None);
    for (source, expected) in [
        ("proofs", Some(vec!["proofs"])),
        ("proofs.results", Some(vec!["proofs", "results"])),
        ("proofs.results.extra", None),
        ("proofs.0", None),
        ("unknown()", None),
        ("#[cfg(feature = \"other\")] proofs.results", None),
    ] {
        assert_eq!(
            place(&parse_str(source).unwrap()),
            expected.map(|path| path.into_iter().map(str::to_owned).collect::<Vec<_>>()),
            "{source}"
        );
    }
    for source in ["&[&[Candidate]]", "::Vec<u32>", "&Vec<u32>"] {
        assert!(arguments(&parse_str(source).unwrap(), "Vec", "std::vec::Vec").is_none(), "{source}");
    }
}

#[test]
fn nov16_try_node_metadata_and_nonmethod_operands_withhold_cache_evidence() {
    use super::super::{Binding, Value};
    let prefix = resolve(SOURCE).unwrap();
    let effects = Effects {
        calls: HashMap::from_iter([("runner.check".into(), &prefix)]),
        ..empty_effects()
    };
    let mut evaluator = machine(Some(&effects));
    evaluator.values.extend([
        ("runner".into(), Value::Symbol("runner".into(), Binding::Opaque)),
        ("active".into(), Value::Population(vec![1])),
        ("proofs".into(), Value::Symbol("proofs".into(), Binding::Opaque)),
        ("proofs.results".into(), Value::PopulationCache(vec![(vec![1], Some(false))])),
        ("proofs.attributed".into(), Value::Bool(true)),
    ]);
    let mut expression: Expr = parse_str("runner.check(&active, &mut proofs)?").unwrap();
    assert_eq!(evaluator.eval(&expression, None), Some(Value::MaybeBool(Some(false))));
    let Expr::Try(node) = &mut expression else { unreachable!() };
    node.attrs.push(syn::parse_quote!(#[cfg(feature = "other")]));
    assert_eq!(evaluator.eval(&expression, None), None);
    assert_eq!(evaluator.eval(&syn::parse_quote!(unknown()?), None), None);
}

#[test]
fn nov16_focused_character_retry_keeps_selector_and_escape_obligations() {
    use super::super::{Binding, proves_with_retry_effects};
    let effects = empty_effects();
    let bindings = HashMap::from_iter([("ready".into(), Binding::Bool), ("ch".into(), Binding::Char)]);
    let original = syn::parse_quote!(while ready {
        match ch {
            'x' => {
                ready = false;
            }

            _ => {}
        }
    });
    for (body, expected) in [("", true), ("break;", false), ("ready = false;", false)] {
        let changed = parse_str(&format!("while ready {{ match ch {{ 'x' => {{ {body} }} _ => {{}} }} }}")).unwrap();
        assert_eq!(
            proves_with_retry_effects(
                &original,
                &changed,
                &bindings,
                &HashMap::default(),
                &HashMap::default(),
                true,
                &HashMap::default(),
                Some(&effects),
            ),
            expected,
            "{body}"
        );
    }
}

#[test]
fn nov16_retry_scalar_comparisons_keep_short_circuiting_and_literal_type_hints() {
    use super::super::{Primitive, Value};
    let effects = empty_effects();
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("byte".into(), Value::Int(7, Primitive::Unsigned(8)));
    for (expression, expected) in [
        ("false && unknown", Some(Value::Bool(false))),
        ("true || unknown", Some(Value::Bool(true))),
        ("true && unknown", None),
        ("false || unknown", None),
        ("byte == 7", Some(Value::Bool(true))),
        ("byte == 8", Some(Value::Bool(false))),
    ] {
        assert_eq!(evaluator.eval(&parse_str(expression).unwrap(), None), expected, "{expression}");
    }
}

#[test]
fn nov16_retry_assignment_does_not_admit_other_opaque_value_kinds() {
    let cache = vec![(vec![1, 3], Some(false))];
    let supported = "let mut answer = search.check(&active, proofs)?; continue;";
    assert!(retry_proof(supported, cache.clone()));
    assert!(!retry_proof(
        "let mut answer = search.check(&active, proofs)?; answer = Some(false); continue;",
        cache
    ));
    assert!(!retry_proof(supported, vec![]));
}

#[test]
fn nov16_prefix_parameter_metadata_and_key_shadowing_reach_their_own_guards() {
    let renamed = SOURCE
        .replace("let key = Proofs::key(active);", "let snapshot = Proofs::key(active);")
        .replace("results.get(&key)", "results.get(&snapshot)");
    assert!(resolve(&renamed).is_some());
    let shadowed = SOURCE
        .replace("let key = Proofs::key(active);", "let active = Proofs::key(active);")
        .replace("results.get(&key)", "results.get(&active)");
    let file = syn::parse_file(&shadowed).unwrap();
    let scope = Scope::new(&file).unwrap();
    assert!(shadow_free(&scope.methods[&("Search".into(), "check".into())].block));
    assert!(resolve(&shadowed).is_none());
    let source = SOURCE.replace("proofs: &mut Proofs) -> Result", "proofs: &mut Proofs, enabled: bool) -> Result");
    assert!(resolve(&source).is_some());
    let configured = source.replace("enabled: bool", "#[cfg(feature = \"other\")] enabled: bool");
    let file = syn::parse_file(&configured).unwrap();
    assert!(Scope::new(&file).is_some());
    assert!(resolve(&configured).is_none());
    assert!(resolve(&SOURCE.replace("Proofs", "xyzzy")).is_some());
}

#[test]
fn nov16_key_helper_metadata_and_parameter_shadowing_are_checked_independently() {
    assert!(resolve(SOURCE).is_some());
    for (from, to) in [
        ("fn key(active:", "fn key(#[cfg(feature = \"other\")] active:"),
        (
            "let mut key = active.iter()",
            "#[cfg(feature = \"other\")] let mut key = active.iter()",
        ),
    ] {
        let changed = SOURCE.replace(from, to);
        assert_ne!(changed, SOURCE);
        let file = syn::parse_file(&changed).unwrap();
        let scope = Scope::new(&file).unwrap();
        assert!(shadow_free(&scope.methods[&("Search".into(), "check".into())].block));
        assert!(resolve(&changed).is_none(), "{to}");
    }
    let shadowed = SOURCE
        .replace("let mut key =", "let mut active =")
        .replace("key.sort_unstable()", "active.sort_unstable()")
        .replace("key\n        }", "active\n        }");
    let file = syn::parse_file(&shadowed).unwrap();
    assert!(Scope::new(&file).is_some());
    assert!(resolve(&shadowed).is_none());
}

#[test]
fn nov16_population_receivers_and_projection_bases_propagate_unknown_evaluation() {
    use super::super::Value;
    let mut effects = empty_effects();
    effects.ordinals.insert("ordinal".into());
    for expression in [
        "factory().len()",
        "factory().chunks(1).map(<[_]>::to_vec).collect::<Vec<_>>()",
        "factory().iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()",
        "factory().iter().copied().filter(|entry| !removed.contains(&entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().map(|entry| factory().ordinal).collect::<HashSet<_>>()",
        "active.iter().copied().filter(|entry| !factory().contains(&entry.ordinal)).collect::<Vec<_>>()",
        "active.iter().copied().filter(|entry| !removed.contains(&factory().ordinal)).collect::<Vec<_>>()",
    ] {
        let mut evaluator = machine(Some(&effects));
        evaluator.values.extend([
            ("active".into(), Value::Population(vec![1, 3])),
            ("removed".into(), Value::Ordinals(vec![1])),
        ]);
        assert_eq!(
            population::eval(&mut evaluator, &parse_str(expression).unwrap()),
            None,
            "{expression}"
        );
    }
    let mut evaluator = machine(Some(&effects));
    evaluator.values.insert("active".into(), Value::Population(vec![1, 3]));
    assert_eq!(
        population::eval(
            &mut evaluator,
            &parse_str("active.iter().map(|entry| entry.ordinal).collect::<HashSet<_>>()").unwrap()
        ),
        Some(Value::Ordinals(vec![1, 3]))
    );
}

#[test]
fn nov16_supported_destructuring_and_at_patterns_withhold_closed_prefix_names() {
    let source = SOURCE.replace(
        "proofs: &mut Proofs) -> Result",
        "proofs: &mut Proofs, context: &(bool, bool)) -> Result",
    );
    assert!(resolve(&source).is_some());
    let destructured = source.replace("context: &(bool, bool)", "&(first, second): &(bool, bool)");
    assert!(resolve(&destructured).is_none());
    for (from, to) in [("fn key(active:", "fn key(active @ _:"), ("let mut key =", "let mut key @ _ =")] {
        let changed = SOURCE.replace(from, to);
        assert_ne!(changed, SOURCE);
        let file = syn::parse_file(&changed).unwrap();
        assert!(Scope::new(&file).is_some());
        assert!(resolve(&changed).is_none(), "{to}");
    }
}

#[test]
fn nov16_unmodeled_self_and_standard_record_paths_do_not_panic_during_resolution() {
    let receiver_cache = SOURCE
        .replace(
            "struct Search;",
            "struct Search { results: std::collections::HashMap<Vec<u32>, Option<bool>> }",
        )
        .replace("proofs.results.get", "self.results.get");
    let self_record = SOURCE
        .replace("struct Search;", "")
        .replace("struct Proofs", "struct Search")
        .replace("impl Proofs", "impl Search")
        .replace("Proofs::key", "Search::key")
        .replace("proofs: &mut Proofs", "proofs: &mut Self");
    let standard_record = SOURCE
        .replace("if !proofs.attributed.is_empty() { return Ok(None); }", "")
        .replace(
            "proofs: &mut Proofs",
            "proofs: &mut std::ops::Range<std::collections::HashMap<Vec<u32>, Option<bool>>>",
        )
        .replace("proofs.results.get", "proofs.start.get");
    let self_population = SOURCE
        .replace("struct Search;", "struct Search { ordinal: u32 }")
        .replace("impl Proofs", "impl Search")
        .replace("Proofs::key", "Search::key")
        .replace("&[&Candidate]", "&[&Self]");
    for source in [receiver_cache, self_record, standard_record, self_population] {
        let file = syn::parse_file(&source).unwrap();
        assert!(Scope::new(&file).is_some());
        assert!(resolve(&source).is_none(), "{source}");
    }
    assert!(resolve(SOURCE).is_some());
}
