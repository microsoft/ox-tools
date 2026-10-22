// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

//! Compiler-backed regression contracts for mutation generation, with no child processes.

extern crate gamma_rt as runtime;
extern crate self as gamma_rt;

use core::num::NonZeroU8;

#[expect(dead_code, reason = "immutable compiler-backed sources retain every inference shape")]
#[expect(
    clippy::box_collection,
    clippy::needless_pass_by_value,
    clippy::never_loop,
    clippy::unchecked_time_subtraction,
    clippy::unnecessary_box_returns,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    reason = "compiler fixtures preserve the exact wrapper, receiver, arithmetic and control-flow shapes being collected"
)]
#[path = "fixtures/correctness/type_evidence.rs"]
mod type_evidence_fixture;
mod type_evidence_tests;

#[expect(dead_code, reason = "immutable operator fixtures retain uncalled original functions")]
#[path = "fixtures/correctness/custom_operators.rs"]
mod custom_operator_fixture;

#[path = "fixtures/correctness/custom_not.rs"]
mod custom_not_fixture;

#[expect(dead_code, reason = "immutable temporal fixtures retain uncalled originals")]
#[path = "fixtures/correctness/temporal_operators.rs"]
mod temporal_operator_fixture;

#[expect(
    dead_code,
    clippy::default_trait_access,
    clippy::unnecessary_wraps,
    reason = "multi-file compiler fixture preserves qualified aliases and enum constructors"
)]
#[path = "fixtures/correctness/package_identities/lib.rs"]
mod package_identity_fixture;

#[expect(
    dead_code,
    clippy::unnecessary_wraps,
    reason = "metadata feature fixture retains both the local enum and standard Option control"
)]
#[path = "fixtures/correctness/metadata_features/lib.rs"]
mod metadata_features_fixture;

#[expect(dead_code, reason = "conditional module fixture is compiled without executing original source")]
#[path = "fixtures/correctness/conditional_modules/lib.rs"]
mod conditional_module_fixture;

#[expect(
    dead_code,
    clippy::duplicate_mod,
    reason = "multiply mounted module fixture preserves distinct nominal declarations"
)]
#[path = "fixtures/correctness/ambiguous_modules/lib.rs"]
mod ambiguous_module_fixture;

#[expect(dead_code, reason = "compiler-backed reader fixture retains unexecuted file-taking originals")]
#[path = "fixtures/correctness/reader_api_identity.rs"]
mod reader_api_fixture;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::task::{Context, Poll, Waker};

use cargo_gamma_lib::internals::cfg::CfgSet;
use cargo_gamma_lib::internals::ops::collect::{self, Candidate, Shape};
use cargo_gamma_lib::internals::ops::registry::Selection;
use cargo_gamma_lib::internals::parse::SourceFile;
use cargo_gamma_lib::internals::schema;
pub use runtime::{Either, decrement, default_after, increment, panic_on_failure};
use syn::spanned::Spanned as _;
use syn::visit::{self, Visit};

#[expect(
    unused_parens,
    unused_braces,
    clippy::needless_bool,
    clippy::match_bool,
    clippy::single_char_add_str,
    clippy::bool_to_int_with_if,
    clippy::let_and_return,
    clippy::never_loop,
    clippy::needless_continue,
    reason = "the module is the exact emitted schema for source shapes under test"
)]
#[path = "fixtures/correctness/borrows.expected.rs"]
mod borrows;
#[expect(
    dead_code,
    unused_parens,
    clippy::double_parens,
    clippy::unused_self,
    clippy::needless_lifetimes,
    clippy::if_then_some_else_none,
    clippy::if_same_then_else,
    clippy::unnecessary_wraps,
    clippy::unused_async,
    reason = "identity and lifetime fixtures retain the source shapes that expose incorrect inference"
)]
#[path = "fixtures/correctness/identities.expected.rs"]
mod identities;
#[expect(
    unused_parens,
    clippy::double_parens,
    clippy::unnecessary_wraps,
    reason = "the module is the exact emitted schema for generic type-inference fixtures"
)]
#[path = "fixtures/correctness/inference.expected.rs"]
mod inference;
#[expect(
    clippy::default_trait_access,
    clippy::unnecessary_map_on_constructor,
    clippy::unused_async,
    clippy::trivially_copy_pass_by_ref,
    reason = "opaque iterator schema fixtures retain their generated guards, borrowed character receiver, and async signatures"
)]
#[path = "fixtures/correctness/iterators.expected.rs"]
mod iterators;

#[expect(
    unused_parens,
    unused_braces,
    clippy::collapsible_else_if,
    clippy::double_parens,
    clippy::needless_return,
    clippy::default_trait_access,
    clippy::unnecessary_wraps,
    clippy::single_match,
    clippy::single_match_else,
    clippy::match_bool,
    clippy::match_like_matches_macro,
    reason = "the fixture preserves nested returns, binding patterns, generic inference, and exact iterator guard lowering"
)]
#[path = "fixtures/correctness/edges.expected.rs"]
mod edges;

#[expect(
    unused_parens,
    clippy::unnecessary_lazy_evaluations,
    reason = "compiler-backed campaign shapes retain their exact adapters and divergent match arms"
)]
#[path = "fixtures/correctness/campaign.expected.rs"]
mod campaign;

#[expect(
    dead_code,
    unused_parens,
    clippy::needless_pass_by_ref_mut,
    clippy::unnecessary_wraps,
    reason = "the exact generated schema retains reader reborrows, inference constraints, and an unexecuted thread-spawn compilation control"
)]
#[path = "fixtures/correctness/generation.expected.rs"]
mod generation;

thread_local! {
    static ACTIVE: Cell<u32> = const { Cell::new(0) };
    static HITS: Cell<usize> = const { Cell::new(0) };
}

/// Thread-local test guard; unlike process selection it can exercise both branches in one test.
#[must_use]
pub fn a(ordinal: u32) -> bool {
    let selected = ACTIVE.get() == ordinal;
    if selected {
        HITS.set(HITS.get() + 1);
    }
    selected
}

fn active<T>(ordinal: u32, run: impl FnOnce() -> T) -> T {
    struct Reset(u32, usize);
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.set(self.0);
            HITS.set(self.1);
        }
    }
    let _reset = Reset(ACTIVE.replace(ordinal), HITS.replace(0));
    run()
}

fn tokens(source: &str) -> String {
    // Rustfmt adds optional call commas and semicolons after diverging tails in the compiled fixtures.
    #[derive(Default)]
    struct OptionalPunctuation(Vec<core::ops::Range<usize>>);
    impl<'ast> Visit<'ast> for OptionalPunctuation {
        fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
            if let Some(comma) = i.args.pairs().next_back().and_then(|pair| pair.punct().copied()) {
                self.0.push(comma.span().byte_range());
            }
            visit::visit_expr_call(self, i);
        }
        fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
            if let Some(comma) = i.args.pairs().next_back().and_then(|pair| pair.punct().copied()) {
                self.0.push(comma.span().byte_range());
            }
            visit::visit_expr_method_call(self, i);
        }
        fn visit_block(&mut self, i: &'ast syn::Block) {
            if let Some(syn::Stmt::Expr(syn::Expr::Break(_) | syn::Expr::Continue(_) | syn::Expr::Return(_), Some(semi))) = i.stmts.last() {
                self.0.push(semi.span().byte_range());
            }
            visit::visit_block(self, i);
        }
    }
    let mut punctuation = OptionalPunctuation::default();
    if let Ok(file) = syn::parse_file(source) {
        punctuation.visit_file(&file);
    } else if let Ok(expression) = syn::parse_str::<syn::Expr>(source) {
        punctuation.visit_expr(&expression);
    }
    punctuation.0.sort_by_key(|span| span.start);
    let mut source = source.to_owned();
    for span in punctuation.0.into_iter().rev() {
        source.replace_range(span, "");
    }
    source
        .parse::<proc_macro2::TokenStream>()
        .expect("checked-in Rust tokenizes")
        .to_string()
}

fn candidates(source: &str, mutators: &str) -> (SourceFile, Vec<Candidate>) {
    let file = SourceFile::parse("correctness.rs", source.to_owned()).expect("checked-in Rust parses");
    let selection = Selection::parse(mutators).expect("mutator names resolve");
    let defaults = collect::Defaults::of(file.ast());
    let candidates = collect::collect_with(&file, &selection, &CfgSet::parse(""), &defaults);
    (file, candidates)
}

fn assert_schema(source: &str, expected: &str, sites: &[(&str, &str, &str)]) {
    let mutators = sites.iter().map(|(_, mutator, _)| *mutator).collect::<Vec<_>>().join(",");
    let (file, candidates) = candidates(source, &mutators);
    let mut selected = Vec::new();
    for &(item, mutator, replacement) in sites {
        let (item, original) = item.split_once('@').map_or((item, None), |(item, original)| (item, Some(original)));
        let (item, occurrence) = item.split_once('#').map_or((item, None), |(item, occurrence)| {
            (item, Some(occurrence.parse::<u32>().expect("checked-in occurrence is numeric")))
        });
        let found = candidates
            .iter()
            .filter(|candidate| {
                candidate.item_path.as_ref() == item
                    && candidate.mutator == mutator
                    && tokens(&candidate.replacement) == tokens(replacement)
                    && occurrence.is_none_or(|occurrence| candidate.identity_occurrence == Some(occurrence))
                    && original.is_none_or(|original| file.slice(&candidate.span).trim() == original)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found.len(),
            1,
            "expected one {item} {mutator} -> {replacement}; got {candidates:#?}"
        );
        selected.push(found[0].clone());
    }
    let mut mutants = collect::into_mutants(&file, "subject", selected);
    assert_eq!(
        mutants.iter().map(|mutant| &mutant.id).collect::<HashSet<_>>().len(),
        mutants.len(),
        "distinct source sites retain distinct identities after consuming-site lifting"
    );
    for (index, mutant) in mutants.iter_mut().enumerate() {
        mutant.ordinal = u32::try_from(index + 1).expect("a small checked-in site list fits u32");
    }
    let refs = mutants.iter().collect::<Vec<_>>();
    let actual = schema::instrument(file.text(), &refs).expect("selected definitions form a schema");
    assert!(syn::parse_file(&actual).is_ok(), "the emitted schema must parse as Rust");
    assert_eq!(
        tokens(&actual),
        tokens(expected),
        "the compiled golden module must equal the emitted schema"
    );
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the fixture contains no suspension point"),
    }
}

fn assert_propagation_panic<T>(run: impl FnOnce() -> T + std::panic::UnwindSafe) {
    let panic = catch_unwind(run).err().expect("the active propagation mutant must panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"gamma: propagation replaced by panic"));
}

#[test]
fn campaign_generation_repairs_compile_and_preserve_semantically_valid_mutations() {
    let source = include_str!("fixtures/correctness/generation.rs");
    let (_, readers) = candidates(source, "iter.take_to_skip");
    assert!(readers.is_empty(), "a match-bound reader must not acquire iterator-only methods");
    assert_schema(
        source,
        include_str!("fixtures/correctness/generation.expected.rs"),
        &[
            ("increment", "expr.increment", "(*remaining) + 1"),
            ("sentence", "string.upper_to_lower", "first.to_lowercase()"),
            ("slug", "string.lower_to_upper", "character.to_uppercase()"),
            (
                "vectors",
                "collection.reverse_vec",
                "vec![directory.join(name).join(\"mod.rs\"), directory.join(format!(\"{name}.rs\"))]",
            ),
            (
                "spawned",
                "try.propagate_to_unwrap",
                "::gamma_rt::panic_on_failure(std::thread::Builder::new().spawn(move || {}))?",
            ),
        ],
    );
    let mut remaining = 4;
    assert_eq!(active(0, || generation::increment(&mut remaining)), 4);
    assert_eq!(active(1, || generation::increment(&mut remaining)), 5);
    assert_eq!(active(0, || generation::sentence("hello")), "Hello");
    assert_eq!(active(2, || generation::sentence("hello")), "hello");
    assert_eq!(active(0, || generation::slug("Aß")), "aß");
    assert_eq!(active(3, || generation::slug("Aß")), "ASS");
    assert_eq!(generation::reader(b"abcdef").unwrap(), b"abc");
    let root = Path::new("root");
    assert_eq!(
        active(0, || generation::vectors(root, "child", None)),
        [root.join("child.rs"), root.join("child").join("mod.rs")]
    );
    assert_eq!(
        active(4, || generation::vectors(root, "child", None)),
        [root.join("child").join("mod.rs"), root.join("child.rs")]
    );
}

#[test]
fn campaign_borrows_and_mandatory_control_flow_compile_without_losing_valid_mutations() {
    let source = include_str!("fixtures/correctness/campaign.rs");
    assert_schema(
        source,
        include_str!("fixtures/correctness/campaign.expected.rs"),
        &[
            (
                "fragment",
                "try.propagate_to_unwrap",
                "Ok(::gamma_rt::panic_on_failure(date(calls).get(..10).ok_or_else(|| ()))?.to_owned())",
            ),
            (
                "fragment_direct",
                "try.propagate_to_unwrap",
                "Ok(::gamma_rt::panic_on_failure(date(calls).get(..10).ok_or(()))?.to_owned())",
            ),
            ("matched", "loop.continue_to_break", "break"),
            ("rebuilt@state.values.push(String::from(\"tail\"));", "stmt.delete_call", ""),
        ],
    );
    for ordinal in [0, 1, 2] {
        let calls = Cell::new(0);
        assert_eq!(active(ordinal, || campaign::fragment(&calls)), Ok("2026-10-15".to_owned()));
        assert_eq!(calls.get(), 1);
        calls.set(0);
        assert_eq!(active(ordinal, || campaign::fragment_direct(&calls)), Ok("2026-10-15".to_owned()));
        assert_eq!(calls.get(), 1);
    }
    let values = [None, Some("kept".to_owned())];
    assert_eq!(active(0, || campaign::matched(&values)), "kept");
    assert_eq!(active(3, || campaign::matched(&values)), "");
    for (ordinal, expected) in [(0, vec!["held", "tail"]), (4, vec!["held"])] {
        let state = active(ordinal, || {
            campaign::rebuilt(campaign::State {
                values: vec!["held".to_owned()],
            })
        });
        assert_eq!(state.values, expected);
    }
    let (file, found) = candidates(source, "stmt.delete_assign,loop.delete_continue");
    assert!(!found.iter().any(|candidate| candidate.item_path.as_ref() == "rebuilt"
        && candidate.mutator == "stmt.delete_assign"
        && file.slice(&candidate.span).contains("state.values =")));
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "matched" && candidate.mutator == "loop.delete_continue")
    );
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "unit_match" && candidate.mutator == "loop.delete_continue")
    );
    let mut out = Vec::new();
    campaign::unit_match(&values, &mut out);
    assert_eq!(out, ["kept".to_owned(), String::new()]);
}

#[test]
fn expected_types_and_propagation_containers_are_preserved() {
    assert_schema(
        include_str!("fixtures/correctness/inference.rs"),
        include_str!("fixtures/correctness/inference.expected.rs"),
        &[
            ("sum", "expr.increment", "::gamma_rt::increment(values.iter().sum())"),
            ("defaulted", "call_result.default", "::gamma_rt::default_after(make(calls))"),
            ("propagate", "try.propagate_to_unwrap", "::gamma_rt::panic_on_failure(value)?"),
            ("optional", "try.propagate_to_unwrap", "::gamma_rt::panic_on_failure(value)?"),
            (
                "inferred_collect",
                "try.propagate_to_unwrap",
                "::gamma_rt::panic_on_failure(values.iter().map(|value| if *value > 0 { Ok(*value) } else { Err(NoDebug) }).collect::<Result<_, _>>())?",
            ),
            ("float_from", "expr.increment", "(f64::from(hits)) + 1.0"),
            ("float_range", "range.inclusive_to_exclusive", "(0.0)..=((end) - 1.0)"),
            (
                "converted",
                "try.propagate_to_unwrap",
                "::gamma_rt::panic_on_failure(produce(value, calls))?",
            ),
        ],
    );
    assert_eq!(active(0, || inference::sum(&[2, 3])), 5);
    assert_eq!(active(1, || inference::sum(&[2, 3])), 6);
    let calls = Cell::new(0);
    assert_eq!(active(0, || inference::defaulted(&calls)), Some(7));
    assert_eq!(active(2, || inference::defaulted(&calls)), None);
    assert_eq!(calls.get(), 2, "each guard branch evaluates the generic call exactly once");
    assert_eq!(active(3, || inference::propagate(Ok(9))).ok(), Some(9));
    let _: inference::NoDebug = active(0, || inference::propagate(Err(inference::NoDebug))).unwrap_err();
    assert_propagation_panic(|| active(3, || inference::propagate(Err(inference::NoDebug))));
    assert_eq!(active(4, || inference::optional(Some(9))), Some(9));
    assert_eq!(active(0, || inference::optional(None)), None);
    assert_propagation_panic(|| active(4, || inference::optional(None)));
    assert_eq!(active(5, || inference::inferred_collect(&[2, 3])).ok(), Some(vec![2, 3]));
    assert_eq!(active(0, || inference::float_from(3)).to_bits(), 3.0_f64.to_bits());
    assert_eq!(active(6, || inference::float_from(3)).to_bits(), 4.0_f64.to_bits());
    assert_eq!(active(0, || *inference::float_range(3.5).end()).to_bits(), 3.5_f64.to_bits());
    assert_eq!(active(7, || *inference::float_range(3.5).end()).to_bits(), 2.5_f64.to_bits());
    assert!(active(7, || *inference::float_range(f64::NAN).end()).is_nan());
    assert_eq!(
        active(7, || *inference::float_range(f64::INFINITY).end()).to_bits(),
        f64::INFINITY.to_bits()
    );
    calls.set(0);
    assert_eq!(active(0, || inference::converted(Ok(9), &calls)).ok(), Some(9));
    assert_eq!(active(8, || inference::converted(Ok(9), &calls)).ok(), Some(9));
    assert!(matches!(
        active(0, || inference::converted(Err(inference::NoDebug), &calls)),
        Err(inference::Converted)
    ));
    assert_propagation_panic(AssertUnwindSafe(|| {
        active(8, || inference::converted(Err(inference::NoDebug), &calls))
    }));
    assert_eq!(
        calls.get(),
        4,
        "each propagation guard evaluates its operand once, including failure"
    );
}

#[test]
fn opaque_iterators_unify_through_success_containers_and_case_conversions() {
    for (json, expected) in [
        ("\"Expr\"", Shape::Expr),
        ("\"Block\"", Shape::Block),
        ("\"IterBlock\"", Shape::IterBlock),
        ("\"IterExpr\"", Shape::IterExpr),
        ("\"Continue\"", Shape::Continue),
        ("\"Break\"", Shape::Break),
        ("\"Stmt\"", Shape::Stmt),
        ("\"Arm\"", Shape::Arm),
        (
            r#"{"IterWrappedBlock":{"depth":2}}"#,
            Shape::IterWrappedBlock {
                depth: NonZeroU8::new(2).unwrap(),
            },
        ),
    ] {
        let shape: Shape = serde_json::from_str(json).expect("persisted shape tags remain readable");
        assert_eq!(shape, expected);
        assert_eq!(serde_json::to_string(&shape).expect("shape serializes"), json);
    }
    serde_json::from_str::<Shape>(r#"{"IterWrappedBlock":{"depth":0}}"#)
        .expect_err("a wrapped iterator must contain at least one success container");
    assert_schema(
        include_str!("fixtures/correctness/iterators.rs"),
        include_str!("fixtures/correctness/iterators.expected.rs"),
        &[
            ("nested", "fn_value.ok", "Ok(Some(core::iter::empty()))"),
            (
                "shadow",
                "parameter.default_shadow",
                "{ let value: Option<&u8> = Default::default(); Ok(Some(core::iter::once(value.copied().unwrap_or(7)))) }",
            ),
            ("asynchronous", "fn_value.ok", "Ok(core::iter::empty())"),
            ("case", "string.lower_to_upper", "value.to_uppercase()"),
            ("borrowed_case", "string.lower_to_upper", "value.to_uppercase()"),
        ],
    );
    let value = 9;
    assert_eq!(
        active(0, || iterators::nested(Some(&value))
            .ok()
            .flatten()
            .map(Iterator::collect::<Vec<_>>)),
        Some(vec![9])
    );
    assert_eq!(
        active(1, || iterators::nested(Some(&value))
            .ok()
            .flatten()
            .map(Iterator::collect::<Vec<_>>)),
        Some(vec![])
    );
    assert_eq!(
        active(2, || iterators::shadow(Some(&value))
            .ok()
            .flatten()
            .map(Iterator::collect::<Vec<_>>)),
        Some(vec![7])
    );
    assert_eq!(
        active(0, || ready(iterators::asynchronous(9)).ok().map(Iterator::collect::<Vec<_>>)),
        Some(vec![9])
    );
    assert_eq!(
        active(3, || ready(iterators::asynchronous(9)).ok().map(Iterator::collect::<Vec<_>>)),
        Some(vec![])
    );
    assert_eq!(active(0, || iterators::case('ß')), "ß");
    assert_eq!(active(4, || iterators::case('ß')), "SS");
    assert_eq!(active(0, || iterators::borrowed_case(&'ß')), "ß");
    assert_eq!(active(5, || iterators::borrowed_case(&'ß')), "SS");
}

fn assert_borrows_schema() {
    assert_schema(
        include_str!("fixtures/correctness/borrows.rs"),
        include_str!("fixtures/correctness/borrows.expected.rs"),
        &[
            ("borrowed", "option.some_to_none", "consume(log, None)"),
            (
                "trimmed",
                "string.trim_end_to_trim_start",
                "record(log, Tracker::new(log).text.trim_start())",
            ),
            (
                "last",
                "try.propagate_to_unwrap",
                "Some(*::gamma_rt::panic_on_failure(make_vec().last())?)",
            ),
            ("configured", "bool_expr.negate", "!({ true })"),
            ("configured_body", "fn_value.bool_false", "false"),
            ("neighbor", "stmt.delete_call", ""),
            ("mandatory_continue", "loop.continue_to_break", "break"),
            ("ownership#0", "stmt.delete_call", ""),
            ("ownership#1", "stmt.delete_call", ""),
            ("reset_exit", "loop.break_to_continue", "continue"),
            ("configured_body_first", "fn_value.bool_false", "false"),
            ("partial_move@notify(progress);", "stmt.delete_call", ""),
            ("partial_move@state.text.push_str(\"x\");", "stmt.delete_call", ""),
            ("partial_move@1", "literal.int_to_zero", "0"),
            ("partial_move", "loop.break_to_continue", "continue"),
            ("borrowed_loop@notify(progress);", "stmt.delete_call", ""),
            ("borrowed_loop@line.push_str(\"z\");", "stmt.delete_call", ""),
            ("borrowed_loop", "loop.break_to_continue", "continue"),
            ("borrowed_loop@touch(line);", "stmt.delete_call", ""),
            ("mandatory_break", "loop.break_to_continue", "continue"),
            ("mandatory_return@3", "return_value.default", "Default::default()"),
            ("reassign_owned", "stmt.delete_assign", ""),
            ("moved_exit@notify(progress);", "stmt.delete_call", ""),
            ("moved_exit@line.push_str(word);", "stmt.delete_call", ""),
            ("guard_pair", "loop.delete_break", ""),
            ("guard_pair", "stmt.delete_assign", "for () in [(), ()] { out.push(line); break; }"),
            (
                "conditional_pair",
                "loop.delete_break",
                "for flag in [false, true] { if flag { out.push(line); } line = refreshed(resets); }",
            ),
            (
                "conditional_pair",
                "stmt.delete_assign",
                "for flag in [false, true] { if flag { out.push(line); break; } }",
            ),
            (
                "unreachable_pair",
                "loop.delete_break",
                "for () in [(), ()] { out.push(line); line = refreshed(resets); }",
            ),
            (
                "unreachable_pair",
                "stmt.delete_assign",
                "for () in [(), ()] { out.push(line); break; }",
            ),
            ("entry_pair", "stmt.delete_assign", ""),
            (
                "entry_pair",
                "stmt.delete_assign",
                "for () in [(), ()] { line = refreshed(resets); out.push(line); }",
            ),
            (
                "entry_exit",
                "loop.continue_to_break",
                "for () in [(), ()] { line = refreshed(resets); out.push(line); break; }",
            ),
        ],
    );
}

#[test]
fn consuming_guards_preserve_temporary_lifetimes_drop_order_and_attributes() {
    assert_borrows_schema();
    let log = RefCell::new(Vec::new());
    assert_eq!(active(0, || borrows::borrowed(&log)), 3);
    assert_eq!(*log.borrow(), ["new", "consume", "drop"]);
    log.borrow_mut().clear();
    assert_eq!(active(1, || borrows::borrowed(&log)), 0);
    assert_eq!(*log.borrow(), ["consume"]);
    log.borrow_mut().clear();
    assert_eq!(active(0, || borrows::trimmed(&log)), " x");
    assert_eq!(*log.borrow(), ["new", "consume", "drop"]);
    log.borrow_mut().clear();
    assert_eq!(active(2, || borrows::trimmed(&log)), "x ");
    assert_eq!(*log.borrow(), ["new", "consume", "drop"]);
    assert_eq!(active(0, borrows::last), Some('z'));
    assert_eq!(active(3, borrows::last), Some('z'));
    assert!(active(0, borrows::configured));
    assert!(!active(4, borrows::configured));
    assert!(active(0, borrows::configured_body));
    assert!(!active(5, borrows::configured_body));
    assert!(active(0, borrows::configured_body_first));
    assert!(!active(11, borrows::configured_body_first));
    assert_eq!(borrows::mandatory(true), 3);
    assert_eq!(active(0, borrows::mandatory_continue), 7);
    assert_eq!(active(7, borrows::mandatory_continue), 0);
    let mut out = Vec::new();
    borrows::ownership("held".to_owned(), &mut out);
    assert_eq!(out, ["held", ""]);
    out.clear();
    active(8, || borrows::ownership("held".to_owned(), &mut out));
    assert_eq!(out, [""]);
    out.clear();
    active(9, || borrows::ownership("held".to_owned(), &mut out));
    assert_eq!(out, ["held"]);
    out.clear();
    active(0, || borrows::reset_exit("held".to_owned(), &mut out));
    assert_eq!(out, ["held", ""]);
    out.clear();
    active(10, || borrows::reset_exit("held".to_owned(), &mut out));
    assert_eq!(out, ["held", "", ""]);
    let mut line = String::new();
    active(6, || borrows::neighbor(&mut line));
    assert!(line.is_empty());
    active(0, || borrows::neighbor(&mut line));
    assert_eq!(line, "x");
    assert_ownership_neighbors();
    assert_eq!(active(0, borrows::mandatory_break), 0);
    assert_eq!(active(20, borrows::mandatory_break), 7);
    assert_eq!(active(0, || borrows::mandatory_return(false)), 3);
    assert_eq!(active(21, || borrows::mandatory_return(false)), 0);
    assert_eq!(active(21, || borrows::mandatory_return(true)), 7);
}

fn assert_ownership_pairs() {
    let mut out = Vec::new();
    let resets = Cell::new(0);
    active(0, || borrows::guard_pair("held".to_owned(), &mut out, &resets));
    assert_eq!(out, ["held"]);
    assert_eq!(resets.get(), 1);
    out.clear();
    borrows::pair_without_reinitialization("held".to_owned(), &mut out);
    assert_eq!(out, ["held"], "the standalone reinitialization deletion compiles");
    out.clear();
    resets.set(0);
    borrows::pair_without_break("held".to_owned(), &mut out, &resets);
    assert_eq!(out, ["held", ""]);
    assert_eq!(resets.get(), 2);
    out.clear();
    resets.set(0);
    active(25, || borrows::guard_pair("held".to_owned(), &mut out, &resets));
    assert_eq!(out, ["held", ""], "the independently viable loop neighbor remains admitted");
    assert_eq!(resets.get(), 2);
    out.clear();
    resets.set(0);
    active(26, || borrows::guard_pair("held".to_owned(), &mut out, &resets));
    assert_eq!(out, ["held"]);
    assert_eq!(resets.get(), 0, "the lifted reinitialization deletion is independently active");
    for (ordinal, expected, reset_count) in [(0, "", 1), (27, "", 2), (28, "held", 0)] {
        out.clear();
        resets.set(0);
        active(ordinal, || borrows::conditional_pair("held".to_owned(), &mut out, &resets));
        assert_eq!(out, [expected]);
        assert_eq!(resets.get(), reset_count, "restoration after a nested exit keeps both viable edits");
    }
    for (ordinal, expected, reset_count) in [(0, vec!["held"], 0), (29, vec!["held", ""], 2), (30, vec!["held"], 0)] {
        out.clear();
        resets.set(0);
        let hits = active(ordinal, || {
            borrows::unreachable_pair("held".to_owned(), &mut out, &resets);
            HITS.get()
        });
        assert_eq!(out, expected);
        assert_eq!(
            resets.get(),
            reset_count,
            "deleting an exit can make its following restoration reachable"
        );
        assert_eq!(hits, usize::from(ordinal != 0));
    }
}

fn assert_loop_entry_restorations() {
    for (ordinal, expected, reset_count) in [(0, vec!["", ""], 4), (31, vec!["held", ""], 2), (32, vec!["", ""], 2)] {
        let mut out = Vec::new();
        let resets = Cell::new(0);
        active(ordinal, || borrows::entry_pair("held".to_owned(), &mut out, &resets));
        assert_eq!(out, expected);
        assert_eq!(
            resets.get(),
            reset_count,
            "entry and backedge restorations remain independently mutable"
        );
    }
    for (ordinal, expected, reset_count) in [(0, vec!["", ""], 2), (33, vec![""], 1)] {
        let mut out = Vec::new();
        let resets = Cell::new(0);
        active(ordinal, || borrows::entry_exit(&mut out, &resets));
        assert_eq!(out, expected);
        assert_eq!(
            resets.get(),
            reset_count,
            "a loop-entry restoration makes this exit transition viable"
        );
    }
}

fn assert_ownership_neighbors() {
    assert_ownership_pairs();
    assert_loop_entry_restorations();
    assert_eq!(active(0, || borrows::reassign_owned("held".to_owned())), "heldx");
    assert_eq!(active(22, || borrows::reassign_owned("held".to_owned())), "held");
    for (ordinal, expected, notified) in [(0, "heldx", 1), (23, "heldx", 0), (24, "held", 1)] {
        let progress = Cell::new(0);
        let mut out = Vec::new();
        active(ordinal, || borrows::moved_exit("held".to_owned(), "x", &mut out, &progress));
        assert_eq!(out, [expected]);
        assert_eq!(progress.get(), notified);
    }
    for (ordinal, count, notified, expected) in [
        (0, 4, 1, vec!["held", "x"]),
        (12, 4, 0, vec!["held", "x"]),
        (13, 4, 1, vec!["held", ""]),
        (14, 3, 1, vec!["held", "x"]),
        (15, 5, 2, vec!["held", "x", "x"]),
    ] {
        let progress = Cell::new(0);
        let mut out = Vec::new();
        let state = borrows::Partial {
            text: "held".to_owned(),
            count: 3,
        };
        assert_eq!(active(ordinal, || borrows::partial_move(state, &mut out, &progress)), count);
        assert_eq!(progress.get(), notified);
        assert_eq!(out, expected);
    }
    for (ordinal, expected, notified) in [(0, "bz", 1), (16, "bz", 0), (17, "b", 1), (18, "bbz", 2), (19, "z", 1)] {
        let progress = Cell::new(0);
        let mut line = String::new();
        active(ordinal, || borrows::borrowed_loop(&mut line, &progress));
        assert_eq!(line, expected);
        assert_eq!(progress.get(), notified);
    }
}

#[test]
fn lexical_bindings_receivers_reference_lifetimes_and_async_calls_are_distinct() {
    assert_schema(
        include_str!("fixtures/correctness/identities.rs"),
        include_str!("fixtures/correctness/identities.expected.rs"),
        &[
            ("qualified", "bool_expr.negate", "!(flag)"),
            ("numeric", "expr.increment", "(file) + 1"),
            ("borrowed_boolean", "unary.remove_not", "*(flag)"),
            ("payload", "option.none_to_some", "Some(root)"),
            ("sync_call", "call_result.default", "::gamma_rt::default_after(synchronous())"),
            ("cfg_parameter", "bool_expr.negate", "!(flag)"),
            ("cfg_receiver", "bool_expr.negate", "!(flag)"),
        ],
    );
    assert!(active(0, || identities::qualified(&identities::TestBinary, true)));
    assert!(!active(1, || identities::qualified(&identities::TestBinary, true)));
    assert!(!active(0, || identities::borrowed_boolean((&true,))));
    assert!(active(3, || identities::borrowed_boolean((&true,))));
    let absolute = std::env::current_dir().expect("the repository working directory exists");
    assert_eq!(active(0, || identities::payload(&absolute)), None);
    assert_eq!(active(4, || identities::payload(&absolute)), Some(absolute.as_path()));
    assert_eq!(identities::shadowed(Path::new("relative")), None);
    assert_eq!(active(0, identities::sync_call), Some(5));
    assert_eq!(active(5, identities::sync_call), None);
    assert_eq!(ready(identities::future()), Some(3));
    assert_eq!(identities::callable(|| "callable".to_owned()), "callable");
    assert_eq!(active(0, || identities::cfg_parameter(true)), 1);
    assert_eq!(active(6, || identities::cfg_parameter(true)), 0);
    assert_eq!(active(0, || identities::cfg_receiver(true)), 2);
    assert_eq!(active(7, || identities::cfg_receiver(true)), 0);
}

fn assert_edges_schema() {
    assert_schema(
        include_str!("fixtures/correctness/edges.rs"),
        include_str!("fixtures/correctness/edges.expected.rs"),
        &[
            ("early", "fn_value.ok", "Ok(Some(core::iter::empty()))"),
            ("absent", "fn_value.ok", "Ok(None)"),
            ("direct_early", "fn_value.empty_collection", "core::iter::empty()"),
            (
                "shadow_early",
                "parameter.default_shadow",
                "{ let value: Option<&u8> = Default::default(); if let Some(value) = value { return Ok(core::iter::once(*value)); } Ok(core::iter::once(7)) }",
            ),
            ("initializer_shadow", "expr.increment", "(value) + 1"),
            ("unknown_borrowed", "unary.remove_not", "*(flag)"),
            ("inferred_range", "range.inclusive_to_exclusive", "(start)..=((end) - 1.0)"),
            (
                "associated_range",
                "range.inclusive_to_exclusive",
                "(0.0)..=((f64::from(hits)) - 1.0)",
            ),
            ("nested_sum", "expr.increment", "::gamma_rt::increment((values.iter().sum()))"),
            (
                "shortcircuited",
                "string.trim_end_to_trim_start",
                "check(log, \" x \".to_owned().trim_start())",
            ),
            ("pattern_scopes", "expr.increment", "(number) + 1"),
            (
                "multiple_temporaries#0",
                "string.trim_end_to_trim_start",
                "combine(log, \" a \".to_owned().trim_start(), \" b \".to_owned().trim_end())",
            ),
            (
                "multiple_temporaries#1",
                "string.trim_end_to_trim_start",
                "combine(log, \" a \".to_owned().trim_end(), \" b \".to_owned().trim_start())",
            ),
            ("local_methods", "fn_value.empty_collection", "core::iter::empty()"),
            (
                "matched",
                "string.trim_end_to_trim_start",
                "match text(log).trim_start() { \" x\" => { log.borrow_mut().push(\"left\"); true } _ => { log.borrow_mut().push(\"right\"); false } }",
            ),
            (
                "match_string",
                "string.trim_end_to_trim_start",
                "match String::from(\" x \").trim_start() { \" x\" => true, _ => false, }",
            ),
            (
                "match_arm",
                "string.trim_end_to_trim_start",
                "check(log, \" x \".to_owned().trim_start())",
            ),
        ],
    );
}

#[test]
fn early_iterator_returns_unknown_borrowed_booleans_and_lazy_consumers_typecheck() {
    assert_edges_schema();
    assert_eq!(
        active(0, || edges::early(true).ok().flatten().map(Iterator::collect::<Vec<_>>)),
        Some(vec![3])
    );
    assert_eq!(
        active(0, || edges::early(false).ok().flatten().map(Iterator::collect::<Vec<_>>)),
        Some(vec![7])
    );
    assert_eq!(
        active(1, || edges::early(true).ok().flatten().map(Iterator::collect::<Vec<_>>)),
        Some(vec![])
    );
    assert_eq!(active(2, || edges::absent().ok().flatten().map(Iterator::collect::<Vec<_>>)), None);
    assert_eq!(active(0, || edges::direct_early(true).collect::<Vec<_>>()), [3]);
    assert_eq!(active(3, || edges::direct_early(true).collect::<Vec<_>>()), Vec::<u8>::new());
    let value = 9;
    assert_eq!(
        active(0, || edges::shadow_early(Some(&value)).ok().map(Iterator::collect::<Vec<_>>)),
        Some(vec![9])
    );
    assert_eq!(
        active(4, || edges::shadow_early(Some(&value)).ok().map(Iterator::collect::<Vec<_>>)),
        Some(vec![7])
    );
    let seen = Cell::new(0);
    assert_eq!(active(5, || edges::initializer_shadow(3, &seen)), 0);
    assert_eq!(seen.get(), 4, "a new let binding is not in scope in its initializer");
    assert!(!active(0, || edges::unknown_borrowed(&[(0, true)])));
    assert!(active(6, || edges::unknown_borrowed(&[(0, true)])));
    assert_eq!(active(7, || *edges::inferred_range().end()).to_bits(), 3.5_f32.to_bits());
    assert_eq!(active(8, || *edges::associated_range(4).end()).to_bits(), 3.0_f64.to_bits());
    assert_eq!(active(9, || edges::nested_sum(&[2, 3])), 6);
    let log = RefCell::new(Vec::new());
    assert_eq!(
        active(10, || {
            let result = edges::shortcircuited(&log, false);
            (result, HITS.get())
        }),
        (false, 0)
    );
    assert!(log.borrow().is_empty(), "the guarded right operand is still lazy");
    assert!(active(10, || edges::shortcircuited(&log, true)));
    assert_eq!(*log.borrow(), ["consume"]);
    assert_eq!(active(0, || edges::pattern_scopes(3)), 3);
    assert_eq!(active(11, || edges::pattern_scopes(3)), 4);
    for (ordinal, expected) in [(0, " a| b"), (12, "a | b"), (13, " a|b ")] {
        log.borrow_mut().clear();
        assert_eq!(active(ordinal, || edges::multiple_temporaries(&log)), expected);
        assert_eq!(*log.borrow(), ["consume"]);
    }
    assert_match_and_callable_boundaries();
}

fn assert_match_and_callable_boundaries() {
    assert_eq!(active(0, || edges::local_methods().collect::<Vec<_>>()), [7]);
    assert_eq!(active(14, || edges::local_methods().collect::<Vec<_>>()), Vec::<u8>::new());
    for (ordinal, expected, arm) in [(0, true, "left"), (15, false, "right")] {
        let log = RefCell::new(Vec::new());
        assert_eq!(active(ordinal, || edges::matched(&log)), expected);
        assert_eq!(*log.borrow(), ["make", "borrow", arm, "drop", "after"]);
    }
    assert!(active(0, edges::match_string));
    assert!(!active(16, edges::match_string));
    let log = RefCell::new(Vec::new());
    assert_eq!(active(17, || (edges::match_arm(&log, false), HITS.get())), (false, 0));
    assert!(
        log.borrow().is_empty(),
        "a scrutinee lift must not hoist mutations out of lazy arms"
    );
    assert!(!active(0, || edges::match_arm(&log, true)));
    assert!(active(17, || edges::match_arm(&log, true)));
    assert_eq!(*log.borrow(), ["consume", "consume"]);
}

#[test]
fn discarded_call_results_drop_before_the_replacement_default_is_constructed() {
    thread_local! {
        static EVENTS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }
    struct Probe(&'static str);
    impl Default for Probe {
        fn default() -> Self {
            EVENTS.with_borrow_mut(|events| events.push("default"));
            Self("drop-default")
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            EVENTS.with_borrow_mut(|events| events.push(self.0));
        }
    }
    let value = default_after(Probe("drop-original"));
    assert_eq!(EVENTS.with_borrow(Clone::clone), ["drop-original", "default"]);
    drop(value);
    assert_eq!(EVENTS.with_borrow(Clone::clone), ["drop-original", "default", "drop-default"]);
}

#[test]
fn unsupported_source_edits_are_rejected_without_losing_viable_neighbors() {
    let (file, found) = candidates(
        include_str!("fixtures/correctness/identities.rs"),
        "expr.increment,expr.decrement,bool_expr.negate,option.none_to_some,call_result.default,call.replace_with_default",
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "numeric" && candidate.replacement.contains("hold((file)"))
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "closure" && file.slice(&candidate.span).trim() == "file")
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.mutator == "bool_expr.negate" && file.slice(&candidate.span).trim() == "binary")
    );
    assert!(
        !found
            .iter()
            .any(|candidate| matches!(candidate.item_path.as_ref(), "cfg_parameter" | "cfg_receiver")
                && candidate.mutator == "bool_expr.negate"
                && file.slice(&candidate.span).contains("String::from"))
    );
    assert!(!found.iter().any(
        |candidate| matches!(candidate.item_path.as_ref(), "shadowed" | "Endpoint::unrelated")
            && candidate.mutator == "option.none_to_some"
    ));
    assert!(
        !found
            .iter()
            .any(|candidate| matches!(candidate.item_path.as_ref(), "future" | "callable") && candidate.mutator.starts_with("call"))
    );
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "sync_call" && candidate.mutator == "call_result.default")
    );
    let (file, found) = candidates(
        include_str!("fixtures/correctness/borrows.rs"),
        "stmt.delete_call,stmt.delete_assign,loop.delete_continue,loop.delete_break,loop.break_to_continue",
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "mandatory" && file.slice(&candidate.span).contains("abort"))
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "ownership" && candidate.mutator == "stmt.delete_assign")
    );
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "guard_pair" && candidate.mutator == "stmt.delete_assign")
    );
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "mandatory_continue" && candidate.mutator == "loop.delete_continue")
    );
    assert!(!found.iter().any(|candidate| candidate.item_path.as_ref() == "partial_move"
        && candidate.mutator == "stmt.delete_assign"
        && file.slice(&candidate.span).contains("state.text =")));
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "mandatory_break" && candidate.mutator == "loop.delete_break")
    );
    assert!(found.iter().any(|candidate| candidate.item_path.as_ref() == "partial_move"
        && candidate.mutator == "stmt.delete_assign"
        && file.slice(&candidate.span).contains("state.count +=")));
    assert!(
        found
            .iter()
            .any(|candidate| candidate.item_path.as_ref() == "neighbor" && candidate.mutator == "stmt.delete_call")
    );
    assert!(!found.iter().any(|candidate| candidate.item_path.as_ref() == "moved_exit"
        && matches!(candidate.mutator, "loop.delete_break" | "loop.break_to_continue")));
}
