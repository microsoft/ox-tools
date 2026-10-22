// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Collector-level controls for bounded resource proofs.

use super::super::super::collect_in;
use crate::cfg::CfgSet;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

const STATE: &str = include_str!("fixtures/state.rs");
const HELPERS: &str = include_str!("fixtures/helpers.rs");
const RANGES: &str = include_str!("fixtures/ranges.rs");
const GUARDED: &str = include_str!("acceptance/guarded.rs");
const GLOB: &str = include_str!("fixtures/campaign/glob.rs");
const SURVEY: &str = include_str!("fixtures/campaign/survey.rs");
const DIFF: &str = include_str!("fixtures/campaign/diff.rs");
const TEXT: &str = include_str!("fixtures/campaign/report/text.rs");
const DASHBOARD: &str = include_str!("fixtures/campaign/commands/dashboard.rs");
const HINTS: &str = include_str!("fixtures/campaign/discover/hints.rs");
const ISOLATION: &str = include_str!("fixtures/campaign/isolation.rs");
const BASELINE: &str = include_str!("fixtures/campaign/baseline.rs");

#[test]
fn source_tokenizer_question_progress() {
    use syn::spanned::Spanned as _;
    let source = GLOB;
    let file = SourceFile::parse("glob.rs", source.into()).unwrap();
    let function = file
        .ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "tokenize" => Some(function),
            _ => None,
        })
        .unwrap();
    let expression = function
        .block
        .stmts
        .iter()
        .find_map(|stmt| match stmt {
            syn::Stmt::Expr(expression @ syn::Expr::While(_), _) => Some(expression),
            _ => None,
        })
        .unwrap();
    let progress = expression.span().byte_range().start + source[expression.span().byte_range().start..].find("index += 1;").unwrap();
    let proof = super::LoopProof::new(
        expression,
        [
            ("characters".into(), super::Binding::VecChar),
            ("tokens".into(), super::Binding::Vec),
            ("index".into(), super::Binding::Integer),
        ]
        .into_iter()
        .collect(),
        true,
    )
    .unwrap()
    .initializers(&file.ast);
    assert!(proof.omits(source, &(progress..progress + "index += 1;".len()), ""));
    let found = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    assert!(!found.iter().any(|candidate| candidate.span.start == progress));
}

#[test]
fn source_tokenizer_disabled_star_predicate() {
    let source = GLOB;
    let file = SourceFile::parse("glob.rs", source.into()).unwrap();
    let function = file
        .ast
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "tokenize" => Some(function),
            _ => None,
        })
        .unwrap();
    let expression = function
        .block
        .stmts
        .iter()
        .find_map(|statement| match statement {
            syn::Stmt::Expr(expression @ syn::Expr::While(_), _) => Some(expression),
            _ => None,
        })
        .unwrap();
    let start = source.find("characters[index] == '*'").unwrap() + "characters[index] == ".len();
    let proof = super::LoopProof::new(
        expression,
        [
            ("characters".into(), super::Binding::VecChar),
            ("tokens".into(), super::Binding::Vec),
            ("index".into(), super::Binding::Integer),
        ]
        .into_iter()
        .collect(),
        true,
    )
    .unwrap()
    .initializers(&file.ast)
    .stable_calls(
        std::iter::once((
            "Some".into(),
            super::StableCall {
                parameters: vec![super::Binding::CharReference],
                output: super::Binding::MaybeChar,
                char_option: true,
            },
        ))
        .collect(),
    );
    assert!(proof.omits(source, &(start..start + 3), "'#'"));
    let found = collect_in(
        &file,
        &Selection::parse("literal.char_to_distinct").unwrap(),
        &CfgSet::unconditional(),
    );
    assert!(!found.iter().any(|candidate| candidate.span.start == start));
}

mod guarded;

#[test]
fn guarded_cursor_deletion_retains_its_compiled_finite_result_difference() {
    assert_eq!(guarded::scan(vec!['x'], 0), 2);
    assert_eq!(guarded::deleted(vec!['x'], 0), 1);
    assert_eq!(guarded::scan(vec!['y'], 0), 1);
    assert_eq!(guarded::deleted(vec!['y'], 0), 1);
    let file = SourceFile::parse("guarded.rs", GUARDED.into()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    let guard_end = GUARDED.find("true").unwrap() + "true".len();
    let progress = guard_end + GUARDED[guard_end..].find("index += 1;").unwrap();
    assert!(candidates.iter().any(|candidate| {
        &*candidate.item_path == "scan" && candidate.mutator == "stmt.delete_assign" && candidate.span.start == progress
    }));
}

#[test]
fn growing_guard_replay_retains_a_compiled_finite_escape() {
    assert_eq!(guarded::finite_growth(1), 1);
    assert_eq!(guarded::growth_deleted(1), 0);
    let file = SourceFile::parse("guarded.rs", GUARDED.into()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    assert!(
        candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "finite_growth" && file.slice(&candidate.span) == "cursor += 1;")
    );
}

#[test]
fn negated_padding_retains_its_compiled_shorter_panel_behavior() {
    let mut original = vec!["short".into()];
    let mut mutant = original.clone();
    guarded::pad_right(&mut original, 2);
    guarded::negated_padding(&mut mutant, 2);
    assert_eq!(original, ["", "short"]);
    assert_eq!(mutant, ["short"]);
}

#[test]
fn resource_candidates_are_filtered_in_process_with_stable_retained_spans() {
    let file = SourceFile::parse("state.rs", STATE.into()).unwrap();
    let selection =
        Selection::parse("stmt.delete_call,stmt.delete_assign,cond.negate,shift.shl_to_shr,arith.add_to_mul,arith.add_to_sub,assign.add_to_sub,loop.delete_break").unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());
    for candidate in &candidates {
        assert_eq!(STATE.get(candidate.span.clone()).unwrap(), file.slice(&candidate.span));
    }
    let identity = candidates
        .iter()
        .filter(|candidate| &*candidate.item_path == "retained_identity" && candidate.mutator == "stmt.delete_call")
        .collect::<Vec<_>>();
    assert_eq!(identity.len(), 1);
    assert_eq!(identity[0].identity_occurrence, Some(1));
    for (item, mutator, retained) in [
        ("projected", "stmt.delete_assign", false),
        ("projected", "cond.negate", true),
        ("owned_pad", "stmt.delete_call", false),
        ("pad", "stmt.delete_call", false),
        ("finite_negated_padding", "cond.negate", true),
        ("suffixes", "loop.delete_break", true),
        ("suffixes", "cond.negate", true),
        ("scalar", "shift.shl_to_shr", false),
        ("scalar", "cond.negate", true),
        ("boundary", "arith.add_to_mul", false),
        ("boundary", "arith.add_to_sub", true),
        ("immutable_boundary", "arith.add_to_mul", false),
        ("immutable_boundary", "stmt.delete_assign", false),
        ("mutable_boundary", "arith.add_to_mul", true),
        ("finite_offset", "arith.add_to_mul", true),
        ("immutable_boundary", "arith.add_to_sub", true),
    ] {
        assert_eq!(
            candidates
                .iter()
                .any(|candidate| &*candidate.item_path == item && candidate.mutator == mutator),
            retained,
            "{item}: {mutator}",
        );
    }
    let question_progress = STATE.find("index += 1;").unwrap();
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.span.contains(&question_progress) && candidate.mutator == "stmt.delete_assign")
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.span.contains(&question_progress) && candidate.mutator == "assign.add_to_sub")
    );
}

#[test]
fn local_scalar_helper_effects_are_resolved_without_inventing_progress() {
    let file = SourceFile::parse("helpers.rs", HELPERS.into()).unwrap();
    let candidates = collect_in(
        &file,
        &Selection::parse("cond.always_true,cond.negate").unwrap(),
        &CfgSet::unconditional(),
    );
    assert!(
        !candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "retry" && candidate.mutator == "cond.always_true")
    );
    assert!(
        !candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "warm_retry" && candidate.mutator == "cond.always_true")
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "warm_retry" && candidate.mutator == "cond.negate")
    );
    for item in ["budget", "fresh_retry", "changed_population"] {
        assert!(
            candidates
                .iter()
                .any(|candidate| &*candidate.item_path == item && candidate.mutator == "cond.always_true"),
            "{item}"
        );
    }
    assert!(
        candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "budget_before_cache" && candidate.mutator == "cond.always_true")
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| &*candidate.item_path == "error_retry" && candidate.mutator == "cond.always_true")
    );
}

const RESOURCE_ACCEPTANCE_GAPS: &[(&str, &str, &str, &str)] = &[
    (GLOB, "tokenize", "stmt.delete_assign", "index += 1"),
    (GLOB, "tokenize", "literal.int_decrement", "1"),
    (SURVEY, "strongly_connected_components", "literal.int_decrement", "1"),
    (DIFF, "script", "stmt.delete_assign", "k += 2;"),
    (TEXT, "fit", "stmt.delete_assign", "at += next.len_utf8();"),
    (DASHBOARD, "equalize_panels", "stmt.delete_call", "left.insert"),
    (DASHBOARD, "equalize_panels", "cond.negate", "right.len() < target"),
    (HINTS, "from", "arith.add_to_mul", "start + offset"),
    (HINTS, "from", "stmt.delete_assign", "start = end;"),
    (ISOLATION, "minimize_interaction", "cond.always_true", "reduced"),
    (
        ISOLATION,
        "minimize_interaction",
        "cond.always_false",
        "granularity >= active.len()",
    ),
    (
        ISOLATION,
        "minimize_interaction",
        "relational.ge_to_gt",
        "granularity >= active.len()",
    ),
    (
        ISOLATION,
        "minimize_interaction",
        "stmt.delete_assign",
        "granularity = granularity.saturating_mul(2).min(active.len());",
    ),
    (ISOLATION, "minimize_interaction", "loop.delete_break", "break"),
    (ISOLATION, "minimize_interaction", "loop.break_to_continue", "break"),
    (ISOLATION, "minimize_interaction", "literal.int_decrement", "2"),
    (ISOLATION, "minimize_interaction", "cond.negate", "reduced"),
    (ISOLATION, "minimize_interaction", "stmt.delete_assign", "active = complement;"),
    (BASELINE, "disambiguate_artifact_directories", "loop.delete_break", "break"),
    (BASELINE, "disambiguate_artifact_directories", "loop.break_to_continue", "break"),
    (
        BASELINE,
        "disambiguate_artifact_directories",
        "cond.negate",
        "used.insert(candidate.to_ascii_lowercase())",
    ),
];

#[test]
fn actual_cursor_group_and_retry_acceptance_gaps_keep_the_compiler_oracle() {
    for &(source, item, selector, original) in RESOURCE_ACCEPTANCE_GAPS {
        let file = SourceFile::parse("campaign.rs", source.into()).unwrap();
        let found = collect_in(&file, &Selection::parse(selector).unwrap(), &CfgSet::unconditional());
        let matching = found
            .iter()
            .filter(|candidate| candidate.item_path.contains(item) && file.slice(&candidate.span).contains(original))
            .filter(|candidate| {
                let prefix = source[..candidate.span.start].trim_end();
                if item == "tokenize" {
                    return match original {
                        "1" => prefix.ends_with("index +="),
                        "'*'" => prefix.ends_with("characters[index] =="),
                        _ => true,
                    };
                }
                if item == "strongly_connected_components" {
                    return prefix.ends_with("frame.edge_at +=");
                }
                if item == "fit" {
                    return prefix.ends_with("if next.width().unwrap_or(0) == 0 {");
                }
                if item != "minimize_interaction" {
                    return true;
                }
                match original {
                    "break" => prefix.ends_with("if granularity >= active.len() {"),
                    "2" => prefix.ends_with("granularity.saturating_mul("),
                    _ => true,
                }
            })
            .count();
        assert!(matching > 0, "{item}: source candidate unexpectedly disappeared");
        eprintln!("{item}: {matching} retained {selector} sites containing {original:?}");
    }
}

#[test]
fn actual_jump_hash_zero_states_are_filtered_without_removing_advancing_variants() {
    const SOURCE: &str = include_str!("fixtures/campaign/shard.rs");
    let file = SourceFile::parse("shard.rs", SOURCE.into()).unwrap();
    let selection = Selection::parse(
            "stmt.delete_assign,assign_value.default,arith.add_to_mul,literal.int_decrement,literal.int_increment,literal.int_to_one,literal.int_to_zero,shift.shl_to_shr,shift.shr_to_shl,arith.add_to_sub,arith.mul_to_add,arith.mul_to_div,arith.div_to_mul,arith.div_to_rem",
        )
        .unwrap();
    let candidates = collect_in(&file, &selection, &CfgSet::unconditional());
    for (mutator, original, replacement, retained) in [
        ("stmt.delete_assign", "candidate = next;", "", false),
        ("arith.add_to_mul", "candidate + 1", "(candidate) * (1)", false),
        ("literal.int_decrement", "1", "0", false),
        ("literal.int_decrement", "1_u32", "0", false),
        ("literal.int_increment", "1_u32", "2", false),
        ("shift.shl_to_shr", "1_u32 << 31", "(1_u32) >> (31)", false),
        ("stmt.delete_assign", "next = scaled as i64;", "", false),
        ("assign_value.default", "scaled as i64", "Default::default()", false),
        ("arith.add_to_sub", "candidate + 1", "(candidate) - (1)", true),
        ("literal.int_increment", "1", "2", true),
        (
            "arith.mul_to_add",
            "((candidate + 1) as f64) * (f64::from(1_u32 << 31) / divisor)",
            "(((candidate + 1) as f64)) + ((f64::from(1_u32 << 31) / divisor))",
            true,
        ),
        (
            "arith.div_to_mul",
            "f64::from(1_u32 << 31) / divisor",
            "(f64::from(1_u32 << 31)) * (divisor)",
            true,
        ),
        ("literal.int_decrement", "31", "30", true),
        ("literal.int_decrement", "33", "32", true),
        ("literal.int_to_one", "33", "1", true),
        ("literal.int_to_zero", "33", "0", true),
        ("shift.shr_to_shl", "key >> 33", "(key) << (33)", true),
        (
            "arith.mul_to_div",
            "((candidate + 1) as f64) * (f64::from(1_u32 << 31) / divisor)",
            "(((candidate + 1) as f64)) / ((f64::from(1_u32 << 31) / divisor))",
            true,
        ),
        (
            "arith.div_to_rem",
            "f64::from(1_u32 << 31) / divisor",
            "(f64::from(1_u32 << 31)) % (divisor)",
            true,
        ),
        (
            "stmt.delete_assign",
            "key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);",
            "",
            true,
        ),
    ] {
        let candidate = candidates.iter().any(|candidate| {
            &*candidate.item_path == "shard_of"
                && candidate.mutator == mutator
                && file.slice(&candidate.span) == original
                && &*candidate.replacement == replacement
                && (original != "1" || SOURCE[..candidate.span.start].ends_with("candidate + "))
        });
        assert_eq!(candidate, retained, "{mutator}: {original} -> {replacement}");
    }
}

#[test]
fn primitive_interval_proofs_retain_changing_guards_unknown_floats_and_custom_types() {
    let file = SourceFile::parse("ranges.rs", RANGES.into()).unwrap();
    let candidates = collect_in(
        &file,
        &Selection::parse("shift.shl_to_shr,assign_value.default").unwrap(),
        &CfgSet::unconditional(),
    );
    for (item, retained) in [
        ("positive", false),
        ("changing_guard", true),
        ("unknown_float", true),
        ("opaque_configuration", true),
        ("typed_shadow", true),
        ("tuple_shadow", true),
        ("foreign_return", true),
    ] {
        assert_eq!(
            candidates.iter().any(|candidate| {
                &*candidate.item_path == item && candidate.mutator == "shift.shl_to_shr" && file.slice(&candidate.span) == "1_u32 << 31"
            }),
            retained,
            "{item}",
        );
    }
    for item in ["shadowed_default", "generic_default", "module_default"] {
        assert!(
            candidates.iter().any(|candidate| {
                &*candidate.item_path == item
                    && candidate.mutator == "assign_value.default"
                    && file.slice(&candidate.span) == "scaled as i64"
            }),
            "{item}"
        );
    }
}
