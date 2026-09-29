// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Percentage and flaky-result gates, pinned through `dispatch`.
//!
//! The gate decides a process exit code, and an exit code is the whole contract a CI job depends
//! on: a run that fell below the bar has to be distinguishable from one that cleared it, and a run
//! whose population never scored has to be distinguishable from both. These reach the gate through
//! the same public entry point CI does — `run`, which parses and dispatches — so the codes are
//! pinned end to end rather than at the command function alone. The `merge` gate is used because it
//! reaches every code without building or running a subject: it reads finished reports and grades
//! them, so a single JSON file is enough to drive each outcome.

use camino::Utf8PathBuf;
use cargo_gamma_lib::testing::{Sink, run};

const EXIT_OK: i32 = 0;
const EXIT_GATE_FAILED: i32 = 2;

/// Writes a report whose single file holds one mutant per status given, and returns its path.
fn report(dir: &Utf8PathBuf, name: &str, statuses: &[&str]) -> Utf8PathBuf {
    let mutants: Vec<_> = statuses
        .iter()
        .enumerate()
        .map(|(index, status)| {
            serde_json::json!({
                "id": format!("m{index}"),
                "mutatorName": "fn_value.one",
                "location": { "start": { "line": index + 1, "column": 1 }, "end": { "line": index + 1, "column": 2 } },
                "status": status,
            })
        })
        .collect();

    let document = serde_json::json!({
        "schemaVersion": "1.0",
        "thresholds": { "high": 80, "low": 60 },
        "framework": { "name": "cargo-gamma", "version": "0.0.0" },
        "config": { "startedAt": 100, "shard": serde_json::Value::Null },
        "files": {
            "src/lib.rs": { "source": "pub fn f() {}\n", "language": "rust", "mutants": mutants }
        }
    });

    let path = dir.join(name);
    std::fs::write(path.as_std_path(), serde_json::to_string(&document).expect("serialize")).expect("write report");
    path
}

/// Dispatches `merge` over one report and returns the exit code and everything it printed.
fn merge(report: &Utf8PathBuf, min_score: &str) -> (i32, String) {
    let mut host = Sink::default();
    let code = run(
        &mut host,
        ["cargo-gamma", "gamma", "merge", report.as_str(), "--min-score", min_score],
    );

    (code, format!("{}{}", host.out(), host.err()))
}

fn tempdir() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
    (dir, root)
}

#[test]
fn a_legacy_subset_cannot_erase_a_survivor_and_make_the_gate_pass() {
    let (_dir, root) = tempdir();
    let full = report(&root, "full.json", &["Killed", "Survived"]);
    let subset = report(&root, "subset.json", &["Killed"]);
    let mut document: serde_json::Value = serde_json::from_slice(&std::fs::read(&subset).unwrap()).unwrap();
    document["config"]["startedAt"] = serde_json::json!(200);
    std::fs::write(&subset, serde_json::to_vec(&document).unwrap()).unwrap();
    let mut host = Sink::default();
    let code = run(
        &mut host,
        ["cargo-gamma", "merge", full.as_str(), subset.as_str(), "--min-score", "100"],
    );
    assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
    assert!(host.err().contains("withdrawals unchecked"), "{}", host.err());
    assert!(host.err().contains("legacy or unsupported"), "{}", host.err());
}

/// A merge that clears the bar exits zero.
#[test]
fn a_merged_score_at_or_above_the_bar_exits_ok() {
    let (_dir, root) = tempdir();
    let path = report(&root, "a.json", &["Killed", "Killed", "Killed", "Survived"]);

    let (code, output) = merge(&path, "70");

    assert_eq!(code, EXIT_OK, "{output}");
}

/// A merge below the bar exits with the gate-failed code, not merely a non-zero one.
#[test]
fn a_merged_score_below_the_bar_exits_gate_failed() {
    let (_dir, root) = tempdir();
    let path = report(&root, "a.json", &["Killed", "Survived", "Survived", "Survived"]);

    let (code, output) = merge(&path, "80");

    assert_eq!(code, EXIT_GATE_FAILED, "{output}");
    assert!(output.contains("below the required"), "{output}");
}

/// A merge whose population never scored exits gate-failed rather than passing a perfect placeholder.
///
/// This is the regression for the two score halves disagreeing on an empty population: the printed
/// score is 100%, so gating on it would pass `--min-score 100` over a merge that graded nothing.
#[test]
fn a_merge_that_scored_nothing_exits_gate_failed() {
    let (_dir, root) = tempdir();
    let path = report(&root, "a.json", &["Ignored", "Ignored"]);

    let (code, output) = merge(&path, "100");

    assert_eq!(code, EXIT_GATE_FAILED, "{output}");
    assert!(output.contains("no mutant counted toward the merged score"), "{output}");
}

fn observation(
    root: &Utf8PathBuf,
    name: &str,
    at: u64,
    producer: &str,
    confirm: Option<bool>,
    outcomes: &[(&str, Option<&str>)],
) -> Utf8PathBuf {
    let statuses: Vec<_> = outcomes.iter().map(|(status, _)| *status).collect();
    let path = report(root, name, &statuses);
    let mut document: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("decode");
    document["config"]["startedAt"] = serde_json::json!(at);
    document["config"]["confirm"] = serde_json::json!(confirm);
    document["config"]["failOnFlaky"] = serde_json::json!(false);
    document["framework"]["name"] = serde_json::json!(producer);
    for (mutant, (_, reason)) in document["files"]["src/lib.rs"]["mutants"]
        .as_array_mut()
        .expect("mutants")
        .iter_mut()
        .zip(outcomes)
    {
        if let Some(reason) = reason {
            mutant["statusReason"] = serde_json::json!(reason);
        }
    }
    std::fs::write(&path, serde_json::to_vec(&document).expect("encode")).expect("write");
    path
}

fn merge_with(paths: &[&Utf8PathBuf], options: &[&str]) -> (i32, String) {
    let mut args = vec!["cargo-gamma", "merge"];
    args.extend(paths.iter().map(|path| path.as_str()));
    args.extend_from_slice(options);
    let mut host = Sink::default();
    let code = run(&mut host, args);
    (code, format!("{}{}", host.out(), host.err()))
}

#[test]
fn flaky_merge_gate_fails_independently_of_a_perfect_score_and_publishes_reports() {
    let (_dir, root) = tempdir();
    let mut outcomes = vec![("Killed", None); 99];
    outcomes.push((
        "Ignored",
        Some("flaky: test `case::unreliable`; mutated failure; unmutated failure"),
    ));
    let path = observation(&root, "campaign.json", 100, "cargo-gamma", Some(true), &outcomes);
    let json = root.join("merged.json");
    let html = root.join("merged.html");
    for options in [
        vec![],
        vec!["--min-score", "100"],
        vec!["--json-report", json.as_str(), "--html-report", html.as_str()],
    ] {
        let (code, output) = merge_with(&[&path], &options);
        assert_eq!(code, EXIT_GATE_FAILED, "{output}");
        assert!(output.contains("99 detected, 0 not detected, score 100"), "{output}");
        assert!(output.contains("flaky, inconclusive mutant `m99`"), "{output}");
        assert!(output.contains("case::unreliable"), "{output}");
    }
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).expect("published JSON")).expect("decode");
    assert_eq!(document["config"]["failOnFlaky"], true);
    assert_eq!(document["config"]["mergeProvenance"]["verdicts"]["m99"]["producer"], "cargo-gamma");
    assert_eq!(document["config"]["mergeProvenance"]["verdicts"]["m99"]["confirm"], true);
    assert!(std::fs::read_to_string(&html).expect("published HTML").contains("case::unreliable"));
    let (code, output) = merge_with(&[&path], &["--no-fail-on-flaky", "--min-score", "100"]);
    assert_eq!(code, EXIT_OK, "{output}");
}

#[test]
fn flaky_merge_opt_out_leaves_percentage_pending_and_ungraded_gates_enabled() {
    let (_dir, root) = tempdir();
    for (other, reason) in [
        ("Survived", None),
        ("Survived", Some("timed out: budget")),
        ("Survived", Some("out of memory: ceiling")),
        ("Pending", None),
        ("Ignored", Some("suppressed by policy")),
    ] {
        let path = observation(
            &root,
            "campaign.json",
            100,
            "cargo-gamma",
            Some(true),
            &[("Ignored", Some("flaky: unreliable")), (other, reason)],
        );
        let (code, output) = merge_with(&[&path], &["--no-fail-on-flaky", "--min-score", "100"]);
        assert_eq!(code, EXIT_GATE_FAILED, "{output}");
        assert!(!output.contains("flaky-result gate failed"), "{output}");
    }
    let path = observation(
        &root,
        "only.json",
        100,
        "cargo-gamma",
        Some(true),
        &[("Ignored", Some("flaky: unreliable"))],
    );
    assert_eq!(merge_with(&[&path], &[]).0, EXIT_GATE_FAILED);
    assert_eq!(merge_with(&[&path], &["--no-fail-on-flaky"]).0, EXIT_OK);
}

#[test]
fn only_gamma_flakes_and_known_unconfirmed_detections_trigger_strict_merging() {
    let (_dir, root) = tempdir();
    for (producer, confirm, status, reason, expected, diagnosis) in [
        ("cargo-gamma", Some(true), "Killed", None, EXIT_OK, ""),
        ("cargo-gamma", None, "Killed", None, EXIT_OK, "unknown confirmation provenance"),
        (
            "cargo-gamma",
            Some(false),
            "Killed",
            Some("failed `case::unchecked`"),
            EXIT_GATE_FAILED,
            "unconfirmed detection",
        ),
        ("cargo-gamma", Some(true), "Ignored", Some("suppressed by policy"), EXIT_OK, ""),
        ("cargo-gamma", Some(true), "Ignored", Some("not built: unavailable"), EXIT_OK, ""),
        ("foreign", Some(false), "Ignored", Some("flaky: arbitrary text"), EXIT_OK, ""),
        ("foreign", Some(false), "Killed", None, EXIT_OK, "unknown confirmation provenance"),
    ] {
        let path = observation(&root, "campaign.json", 100, producer, confirm, &[(status, reason)]);
        let (code, output) = merge_with(&[&path], &[]);
        assert_eq!(code, expected, "{producer} {confirm:?} {status}: {output}");
        assert!(output.contains(diagnosis), "{output}");
        assert!(!output.contains("flaky, inconclusive"), "{output}");
        assert_eq!(merge_with(&[&path], &["--no-fail-on-flaky"]).0, EXIT_OK);
    }
}

#[test]
fn direct_and_staged_merges_gate_winning_evidence_not_input_history() {
    let (_dir, root) = tempdir();
    for (old, recent, expected) in [
        (("Ignored", Some("flaky: old")), ("Killed", None), EXIT_OK),
        (("Killed", None), ("Ignored", Some("flaky: newer")), EXIT_GATE_FAILED),
        (("Ignored", Some("flaky: old")), ("Pending", None), EXIT_GATE_FAILED),
    ] {
        let older = observation(&root, "older.json", 100, "cargo-gamma", Some(true), &[old]);
        let newer = observation(&root, "newer.json", 200, "cargo-gamma", Some(true), &[recent]);
        let stage = root.join("stage.json");
        let (direct, output) = merge_with(&[&older, &newer], &["--json-report", stage.as_str()]);
        assert_eq!(direct, expected, "{output}");
        assert_eq!(merge_with(&[&newer, &older], &[]).0, expected);
        let (staged, output) = merge_with(&[&stage, &older], &[]);
        assert_eq!(staged, expected, "{output}");
    }
}

#[test]
fn staged_mixed_producers_preserve_reason_protocol_and_confirmation_policy() {
    let (_dir, root) = tempdir();
    let foreign = observation(
        &root,
        "foreign.json",
        100,
        "foreign",
        Some(false),
        &[("Ignored", Some("flaky: foreign free text")), ("Killed", None)],
    );
    let gamma = observation(&root, "gamma.json", 200, "cargo-gamma", Some(true), &[("Pending", None)]);
    let stage = root.join("stage.json");
    assert_eq!(merge_with(&[&foreign, &gamma], &["--json-report", stage.as_str()]).0, EXIT_OK);
    assert_eq!(merge_with(&[&stage], &[]).0, EXIT_OK);

    let unconfirmed = observation(&root, "unconfirmed.json", 300, "cargo-gamma", Some(false), &[("Killed", None)]);
    let (code, output) = merge_with(&[&unconfirmed], &["--no-fail-on-flaky", "--json-report", stage.as_str()]);
    assert_eq!(code, EXIT_OK, "{output}");
    let (code, output) = merge_with(&[&stage], &[]);
    assert_eq!(code, EXIT_GATE_FAILED, "{output}");
    assert!(output.contains("unconfirmed detection"), "{output}");
    let confirmed = observation(&root, "confirmed.json", 400, "cargo-gamma", Some(true), &[("Killed", None)]);
    assert_eq!(merge_with(&[&stage, &confirmed], &[]).0, EXIT_OK);
    let (code, output) = merge_with(&[&stage, &foreign], &[]);
    assert_eq!(code, EXIT_GATE_FAILED, "{output}");
}

#[test]
fn no_confirm_is_rejected_before_discovery_unless_effective_policy_opts_out() {
    let (_dir, root) = tempdir();
    std::fs::write(root.join("gamma.toml"), "no-confirm = true\n").expect("config");
    let mut host = Sink::default();
    let code = run(&mut host, ["cargo-gamma", "run", "--dir", root.as_str()]);
    assert_eq!(code, 1, "{}", host.err());
    assert!(host.err().contains("--no-fail-on-flaky"), "{}", host.err());
    assert!(!host.err().contains("Cargo.toml"), "{}", host.err());

    std::fs::write(root.join("gamma.toml"), "no-fail-on-flaky = true\n").expect("config");
    let mut host = Sink::default();
    let code = run(
        &mut host,
        ["cargo-gamma", "run", "--dir", root.as_str(), "--no-confirm", "--no-config"],
    );
    assert_eq!(code, 1, "{}", host.err());
    assert!(host.err().contains("--no-fail-on-flaky"), "{}", host.err());
}

#[test]
fn large_unconfirmed_campaigns_have_bounded_diagnostics_and_complete_reports() {
    let (_dir, root) = tempdir();
    let path = observation(&root, "campaign.json", 100, "cargo-gamma", Some(false), &[("Killed", None); 25]);
    let json = root.join("merged.json");
    let (code, output) = merge_with(&[&path], &["--json-report", json.as_str()]);
    assert_eq!(code, EXIT_GATE_FAILED, "{output}");
    assert!(output.contains("5 more flaky or unconfirmed findings"), "{output}");
    assert!(!output.contains("mutant `m24`"), "{output}");
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(json).expect("read")).expect("decode");
    assert_eq!(document["files"]["src/lib.rs"]["mutants"].as_array().expect("mutants").len(), 25);
}
