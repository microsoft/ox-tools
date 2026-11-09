// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cfg::CfgSet;
use crate::ops::collect::collect_in;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

const SOURCE: &str = include_str!("../fixtures/campaign/survey.rs");
const DIFF: &str = include_str!("../fixtures/campaign/diff.rs");

fn cursor_zero_retained(source: &str) -> bool {
    let file = SourceFile::parse("cursor.rs", source.into()).unwrap();
    let at = source.find("frame.edge_at += 1;").unwrap() + "frame.edge_at += ".len();
    collect_in(&file, &Selection::parse("literal.int_decrement").unwrap(), &CfgSet::unconditional())
        .iter()
        .any(|candidate| candidate.span.start == at && candidate.replacement == "0")
}

#[test]
fn stationary_frame_cursor_uses_a_typed_idempotent_visited_edge_backedge() {
    assert!(!cursor_zero_retained(SOURCE));
    let renamed = SOURCE
        .replace("frame", "activation")
        .replace("edge_at", "position")
        .replace("node", "vertex")
        .replace("low_link", "minimum")
        .replace("neighbor", "adjacent");
    let file = SourceFile::parse("renamed.rs", renamed.clone()).unwrap();
    let at = renamed.find("activation.position += 1;").unwrap() + "activation.position += ".len();
    let candidates = collect_in(&file, &Selection::parse("literal.int_decrement").unwrap(), &CfgSet::unconditional());
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.span.start == at && candidate.replacement == "0")
    );
}

#[test]
fn frame_backedges_retain_guards_unknown_effects_other_writes_and_shadows() {
    for (from, to) in [
        ("let node = frame.node;", "opaque(); let node = frame.node;"),
        ("if on_stack[neighbor] {", "if on_stack[neighbor] { opaque();"),
        ("frame.edge_at += 1;", "frame.edge_at += 1; assert!(frame.edge_at > 0);"),
        ("frame.edge_at += 1;", "frame.edge_at += 1; frame.edge_at += 1;"),
        ("low_link[node] = low_link[node].min(neighbor_index);", "index_of[neighbor] = None;"),
        (
            "low_link[node] = low_link[node].min(neighbor_index);",
            "low_link[node] = opaque(neighbor_index);",
        ),
        (
            "low_link[node] = low_link[node].min(neighbor_index);",
            "low_link[node] = low_link[node].min(neighbor_index); break;",
        ),
        ("while let Some(frame)", "let index_of = custom(); while let Some(frame)"),
        ("while let Some(frame)", "let call_stack = custom(); while let Some(frame)"),
        ("while let Some(frame)", "for call_stack in custom() { while let Some(frame)"),
    ] {
        let mut source = SOURCE.replacen(from, to, 1);
        if to.starts_with("for call_stack") {
            source = source.replacen("\n    comp_of", "\n    }\n    comp_of", 1);
        }
        assert!(cursor_zero_retained(&source), "{to}");
    }
}

#[test]
fn frame_backedges_reject_custom_numeric_and_collection_contracts() {
    for prefix in [
        "type u32 = Custom;\n",
        "use custom::u32;\n",
        "struct Vec<T>(T);\n",
        "trait Extension { fn min(self, other: Self) -> Self; }\n",
    ] {
        assert!(cursor_zero_retained(&format!("{prefix}{SOURCE}")), "{prefix}");
    }
}

fn diagonal_progress_retained(source: &str) -> bool {
    let file = SourceFile::parse("diagonal.rs", source.into()).unwrap();
    let at = source.find("k += 2;").unwrap();
    collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional())
        .iter()
        .any(|candidate| candidate.span.start == at)
}

#[test]
fn stationary_diagonal_reuses_the_paired_scan_stopping_point() {
    assert!(!diagonal_progress_retained(DIFF));
    let renamed = DIFF.replace("furthest", "frontier").replace("shift", "slot");
    assert!(!diagonal_progress_retained(&renamed));
    let file = SourceFile::parse("diagonal.rs", DIFF.into()).unwrap();
    let candidates = collect_in(&file, &Selection::parse("stmt.delete_assign").unwrap(), &CfgSet::unconditional());
    for advance in ["x += 1;", "y += 1;"] {
        let at = DIFF.find(advance).unwrap();
        assert!(candidates.iter().any(|candidate| candidate.span.start == at), "{advance}");
    }
}

#[test]
fn tuple_bound_slice_aliases_preserve_diagonal_progress_evidence() {
    let source = DIFF.replacen("let mut k = -d;", "let (old_copy, new_copy) = (old, new); let mut k = -d;", 1);
    assert_ne!(source, DIFF);
    assert!(!diagonal_progress_retained(&source));
}

#[test]
fn diagonal_backedges_retain_progress_guards_effects_and_nonstandard_inputs() {
    for (from, to) in [
        ("k += 2;", "k += 2; assert!(k > -d);"),
        ("k += 2;", "k += 2; opaque();"),
        ("k += 2;", "k += 2; k += 1;"),
        ("x += 1;", "x += 1; opaque();"),
        ("old[xi] == new[yi]", "compare(old[xi], new[yi])"),
        ("let mut k = -d;", "let mut k = d;"),
        ("let mut k = -d;", "let shift = opaque(); let mut k = -d;"),
        ("let mut k = -d;", "let furthest = opaque(); let mut k = -d;"),
        ("let mut k = -d;", "let (shift,) = (opaque(),); let mut k = -d;"),
        ("let mut k = -d;", "let (furthest,) = (opaque(),); let mut k = -d;"),
        ("old: &[&'a str]", "old: &Custom"),
        ("k == -d ||", "k != -d ||"),
        ("x += 1;", "x += 2;"),
        ("furthest[shift(k)] = x;", "furthest[shift(k)] = x + 1;"),
        ("usize::try_from(k +", "custom(k +"),
    ] {
        assert!(diagonal_progress_retained(&DIFF.replacen(from, to, 1)), "{to}");
    }
}

#[test]
fn cursor_context_exhaustion_and_hidden_binding_changes_keep_candidates() {
    let prefix = "let pad = 0;\n".repeat(64);
    let source = SOURCE.replacen("while let Some(frame)", &format!("{prefix}while let Some(frame)"), 1);
    assert!(cursor_zero_retained(&source));
    let source = SOURCE.replacen("while let Some(frame)", "type u32 = Custom; while let Some(frame)", 1);
    assert!(cursor_zero_retained(&source));
    let source = SOURCE.replacen("\n    comp_of", "\n    type u32 = Custom;\n    comp_of", 1);
    assert!(
        cursor_zero_retained(&source),
        "local type items are hoisted even when written after the loop"
    );
    let source = DIFF.replacen("let mut k = -d;", &format!("{prefix}let mut k = -d;"), 1);
    assert!(diagonal_progress_retained(&source));
    let source = DIFF
        .replacen("while k <= d {", "for (shift, _) in opaque() { while k <= d {", 1)
        .replacen("\n    Vec::new()", "\n    }\n    Vec::new()", 1);
    assert!(diagonal_progress_retained(&source));
    let source = DIFF.replacen("\n    Vec::new()", "\n    type isize = Custom;\n    Vec::new()", 1);
    assert!(diagonal_progress_retained(&source));
}
