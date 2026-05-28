// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end CLI tests for `cargo-coverage-gate`.
//!
//! Each test builds a self-contained workspace under a `TempDir`,
//! drops a JSON fixture into it, and invokes the binary via
//! [`assert_cmd`]. The binary is run from inside the temp workspace
//! so that `cargo metadata` resolves the right `Cargo.toml`. The
//! `coverage-gate` token is prepended to the argv because that's what
//! cargo's subcommand convention does.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Write a workspace at `dir` containing the given members. Each
/// member entry is `(name, optional min-lines-percent)`. The workspace root
/// gets a `[workspace.metadata.coverage-gate]` block when
/// `workspace_min_lines_percent` is `Some`.
fn make_workspace(dir: &Path, members: &[(&str, Option<&str>)], workspace_min_lines_percent: Option<&str>) {
    let names: Vec<&&str> = members.iter().map(|(n, _)| n).collect();
    let members_list = names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
    let workspace_meta = workspace_min_lines_percent
        .map(|m| format!("\n[workspace.metadata.coverage-gate]\nmin-lines-percent = {m}\n"))
        .unwrap_or_default();
    fs::write(
        dir.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"2\"\nmembers = [{members_list}]\n{workspace_meta}"),
    )
    .expect("write workspace root Cargo.toml");

    for (name, min_lines_percent) in members {
        let member_dir = dir.join(name);
        fs::create_dir_all(member_dir.join("src")).expect("mkdir member src");
        let metadata = min_lines_percent
            .map(|m| format!("\n[package.metadata.coverage-gate]\nmin-lines-percent = {m}\n"))
            .unwrap_or_default();
        fs::write(
            member_dir.join("Cargo.toml"),
            format!(
                r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
{metadata}"#
            ),
        )
        .expect("write member Cargo.toml");
        fs::write(member_dir.join("src/lib.rs"), "// empty\n").expect("write lib.rs");
    }
}

/// Build a coverage JSON v2 string with the given per-file totals.
/// Files are `(path_relative_to_workspace, lines_count, lines_covered)`.
fn make_coverage_json(dir: &Path, files: &[(&str, u64, u64)]) -> String {
    let files_json: Vec<String> = files
        .iter()
        .map(|(rel, count, covered)| {
            let full = dir.join(rel);
            let s = full.to_string_lossy().replace('\\', "/");
            format!(r#"{{"filename":"{s}","summary":{{"lines":{{"count":{count},"covered":{covered}}}}}}}"#)
        })
        .collect();
    format!(
        r#"{{"type":"llvm.coverage.json.export","version":"2.0.1","data":[{{"files":[{}],"totals":{{}}}}]}}"#,
        files_json.join(",")
    )
}

/// Convenience: write the coverage JSON to `dir/coverage.json` and
/// return the path as a string.
fn write_json(dir: &Path, files: &[(&str, u64, u64)]) -> String {
    let path = dir.join("coverage.json");
    fs::write(&path, make_coverage_json(dir, files)).expect("write coverage.json");
    path.to_string_lossy().into_owned()
}

/// Construct a `cargo coverage-gate` invocation scoped to `dir`.
fn coverage_gate(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cargo-coverage-gate").expect("binary present");
    cmd.current_dir(dir)
        .arg("coverage-gate")
        // The summary-file env vars must not leak in from the host
        // environment — tests that exercise them set them explicitly.
        .env_remove("GITHUB_STEP_SUMMARY")
        .env_remove("COVERAGE_GATE_SUMMARY");
    cmd
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn all_pass_mixed_sources() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(
        tmp.path(),
        &[
            ("alpha", Some("80")), // per-crate source
            ("beta", None),        // workspace source (inherits)
            ("gamma", None),       // workspace source too
        ],
        Some("75"),
    );
    let json = write_json(
        tmp.path(),
        &[
            ("alpha/src/lib.rs", 100, 95),
            ("beta/src/lib.rs", 100, 90),
            ("gamma/src/lib.rs", 100, 100),
        ],
    );

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("gamma"))
        .stdout(predicate::str::contains("all crates meet their threshold"))
        // Both Crate and Workspace sources appear in the table.
        .stdout(predicate::str::contains("crate"))
        .stdout(predicate::str::contains("workspace"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn one_crate_below_threshold_exits_1() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95), ("beta/src/lib.rs", 100, 60)]);

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("FAIL"))
        .stdout(predicate::str::contains("1 package(s) below threshold"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn gated_crate_with_no_data_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    // Only alpha has data; beta has none.
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("NO DATA"))
        .stdout(predicate::str::contains("no attributed coverage data"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn crates_flag_restricts_scope() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95), ("beta/src/lib.rs", 100, 50)]);

    // Only gate alpha; beta would fail but is out of scope.
    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json, "--packages", "alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta").not());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn crates_flag_with_unknown_name_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 100)]);

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json, "--packages", "typo"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("typo"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn summary_file_flag_writes_markdown() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("summary.md");

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json, "--summary-file", summary.to_str().expect("utf-8")])
        .assert()
        .success();

    let body = fs::read_to_string(&summary).expect("summary file written");
    assert!(body.contains("### coverage-gate"), "got:\n{body}");
    assert!(body.contains("| alpha |"), "got:\n{body}");
    assert!(body.contains("✅"), "got:\n{body}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn github_step_summary_env_is_auto_detected() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("step-summary.md");

    Command::cargo_bin("cargo-coverage-gate")
        .expect("binary present")
        .current_dir(tmp.path())
        .arg("coverage-gate")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .env_remove("COVERAGE_GATE_SUMMARY")
        .args(["--llvm-cov-json", &json])
        .assert()
        .success();

    assert!(summary.exists(), "GITHUB_STEP_SUMMARY file must be written");
    let body = fs::read_to_string(&summary).expect("read summary");
    assert!(body.contains("### coverage-gate"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn quiet_suppresses_stdout_but_still_writes_summary() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("summary.md");

    coverage_gate(tmp.path())
        .args([
            "--llvm-cov-json",
            &json,
            "--summary-file",
            summary.to_str().expect("utf-8"),
            "--quiet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    assert!(summary.exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn unknown_version_string_warns_but_continues() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let full = tmp.path().join("alpha/src/lib.rs");
    let path_str = full.to_string_lossy().replace('\\', "/");
    let body = format!(
        r#"{{"type":"llvm.coverage.json.export","version":"3.0.0","data":[{{"files":[{{"filename":"{path_str}","summary":{{"lines":{{"count":100,"covered":95}}}}}}],"totals":{{}}}}]}}"#
    );
    let json_path = tmp.path().join("cov.json");
    fs::write(&json_path, body).expect("write json");

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", json_path.to_str().expect("utf-8")])
        .assert()
        .success();
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn malformed_json_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    let json_path = tmp.path().join("bad.json");
    fs::write(&json_path, "this is not json\n").expect("write json");

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", json_path.to_str().expect("utf-8")])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("coverage JSON"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn missing_json_file_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", "does-not-exist.json"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does-not-exist.json"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn default_threshold_is_100_when_nothing_configured() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    // 95% < 100% built-in default, so this must fail.
    let json = write_json(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);

    coverage_gate(tmp.path())
        .args(["--llvm-cov-json", &json])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("100.0%"))
        .stdout(predicate::str::contains("default"));
}
