// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! The `--min-score` gate's exit codes, pinned through `dispatch`.
//!
//! The gate decides a process exit code, and an exit code is the whole contract a CI job depends
//! on: a run that fell below the bar has to be distinguishable from one that cleared it, and a run
//! whose population never scored has to be distinguishable from both. These reach the gate through
//! the same public entry point CI does — `run`, which parses and dispatches — so the codes are
//! pinned end to end rather than at the command function alone. The `merge` gate is used because it
//! reaches every code without building or running a subject: it reads finished reports and grades
//! them, so a single JSON file is enough to drive each outcome.

use camino::Utf8PathBuf;
use cargo_gamma_lib::run;
use cargo_gamma_lib::testing::Sink;

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
