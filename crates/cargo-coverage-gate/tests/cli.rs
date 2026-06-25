// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end CLI tests for `cargo-coverage-gate`.
//!
//! Each test builds a self-contained workspace under a `TempDir`,
//! drops an lcov tracefile into it, and invokes the binary via
//! [`assert_cmd`]. The binary is run from inside the temp workspace
//! so that `cargo metadata` resolves the right `Cargo.toml`. The
//! `coverage-gate` token is prepended to the argv because that's what
//! cargo's subcommand convention does.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
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

/// Build an lcov tracefile string with the given per-file totals.
/// Files are `(path_relative_to_workspace, lines_count, lines_covered)`.
/// Each file becomes a section with `count` `DA:N,X` records where the
/// first `covered` of them have non-zero hit counts.
fn make_coverage_lcov(dir: &Path, files: &[(&str, u32, u32)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (rel, count, covered) in files {
        let full = dir.join(rel);
        let path = full.to_string_lossy().replace('\\', "/");
        out.push_str("TN:\n");
        writeln!(out, "SF:{path}").expect("write to String never fails");
        for i in 1..=*count {
            let hits = if i <= *covered { 5 } else { 0 };
            writeln!(out, "DA:{i},{hits}").expect("write to String never fails");
        }
        writeln!(out, "LF:{count}").expect("write to String never fails");
        writeln!(out, "LH:{covered}").expect("write to String never fails");
        out.push_str("end_of_record\n");
    }
    out
}

/// Convenience: write the lcov tracefile to `dir/lcov.info` and return
/// the path as a string.
fn write_lcov(dir: &Path, files: &[(&str, u32, u32)]) -> String {
    let path = dir.join("lcov.info");
    fs::write(&path, make_coverage_lcov(dir, files)).expect("write lcov.info");
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
    let lcov_path = write_lcov(
        tmp.path(),
        &[
            ("alpha/src/lib.rs", 100, 95),
            ("beta/src/lib.rs", 100, 90),
            ("gamma/src/lib.rs", 100, 100),
        ],
    );

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("gamma"))
        .stdout(predicate::str::contains("all packages meet their threshold"))
        // Both per-package and workspace-default sources appear in the Source column.
        .stdout(predicate::str::contains("package"))
        .stdout(predicate::str::contains("workspace"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn one_crate_below_threshold_exits_1() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95), ("beta/src/lib.rs", 100, 60)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("FAIL"))
        .stdout(predicate::str::contains("1 package below threshold"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn multiple_lcov_files_merge_at_line_level() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let src = tmp.path().join("alpha/src/lib.rs").to_string_lossy().replace('\\', "/");

    // Two configs of the SAME file, each covering a disjoint half (50%
    // alone, below the 80% threshold), but the union covers all 4 lines
    // (100%). Only a correct line-level merge passes the gate.
    let config_a = format!("TN:\nSF:{src}\nDA:1,5\nDA:2,5\nDA:3,0\nDA:4,0\nend_of_record\n");
    let config_b = format!("TN:\nSF:{src}\nDA:1,0\nDA:2,0\nDA:3,5\nDA:4,5\nend_of_record\n");
    let path_a = tmp.path().join("a.info");
    let path_b = tmp.path().join("b.info");
    fs::write(&path_a, &config_a).expect("write a.info");
    fs::write(&path_b, &config_b).expect("write b.info");
    let a = path_a.to_string_lossy().into_owned();
    let b = path_b.to_string_lossy().into_owned();

    // Either file alone is 50% -> fails.
    coverage_gate(tmp.path())
        .args(["--lcov", &a])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("FAIL"));

    // Both together merge to 100% -> passes.
    coverage_gate(tmp.path())
        .args(["--lcov", &a, "--lcov", &b])
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn gated_crate_with_no_data_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    // Only alpha has data; beta has none.
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("NO DATA"))
        .stdout(predicate::str::contains("no attributed coverage data"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn package_flag_restricts_scope() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80")), ("beta", Some("80"))], None);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95), ("beta/src/lib.rs", 100, 50)]);

    // Only gate alpha; beta would fail but is out of scope.
    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "-p", "alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta").not());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn package_flag_accepts_repeated_short_form() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(
        tmp.path(),
        &[("alpha", Some("80")), ("beta", Some("80")), ("gamma", Some("80"))],
        None,
    );
    let lcov_path = write_lcov(
        tmp.path(),
        &[
            ("alpha/src/lib.rs", 100, 95),
            ("beta/src/lib.rs", 100, 95),
            ("gamma/src/lib.rs", 100, 50),
        ],
    );

    // -p repeated, cargo-style. gamma should be excluded.
    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "-p", "alpha", "-p", "beta"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("gamma").not());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn package_flag_accepts_glob_pattern() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(
        tmp.path(),
        &[("alpha", Some("80")), ("alpha_macros", Some("80")), ("beta", Some("80"))],
        None,
    );
    let lcov_path = write_lcov(
        tmp.path(),
        &[
            ("alpha/src/lib.rs", 100, 95),
            ("alpha_macros/src/lib.rs", 100, 95),
            ("beta/src/lib.rs", 100, 50),
        ],
    );

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "-p", "alpha*"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha "))
        .stdout(predicate::str::contains("alpha_macros"))
        .stdout(predicate::str::contains("beta").not());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn package_flag_with_unknown_name_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 100)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "-p", "typo"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("typo"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn summary_file_flag_writes_markdown() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("summary.md");

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "--summary-file", summary.to_str().expect("utf-8")])
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
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("step-summary.md");

    Command::cargo_bin("cargo-coverage-gate")
        .expect("binary present")
        .current_dir(tmp.path())
        .arg("coverage-gate")
        .env("GITHUB_STEP_SUMMARY", &summary)
        .env_remove("COVERAGE_GATE_SUMMARY")
        .args(["--lcov", &lcov_path])
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
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);
    let summary = tmp.path().join("summary.md");

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path, "--summary-file", summary.to_str().expect("utf-8"), "--quiet"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    assert!(summary.exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn lcov_with_test_name_records_is_parsed() {
    // Real-world lcov files often interleave TN: (test name) records
    // and TN:<empty> sections. Verify the parser handles both.
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    let full = tmp.path().join("alpha/src/lib.rs");
    let path = full.to_string_lossy().replace('\\', "/");
    let body = format!("TN:my_test\nSF:{path}\nDA:1,5\nDA:2,5\nDA:3,5\nDA:4,5\nDA:5,0\nLF:5\nLH:4\nend_of_record\n");
    let lcov_path = tmp.path().join("cov.info");
    fs::write(&lcov_path, body).expect("write lcov");

    coverage_gate(tmp.path())
        .args(["--lcov", lcov_path.to_str().expect("utf-8")])
        .assert()
        .success();
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn malformed_lcov_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    let lcov_path = tmp.path().join("bad.info");
    fs::write(&lcov_path, "this is not lcov\n").expect("write lcov");

    coverage_gate(tmp.path())
        .args(["--lcov", lcov_path.to_str().expect("utf-8")])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("lcov tracefile"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn missing_lcov_file_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);

    coverage_gate(tmp.path())
        .args(["--lcov", "does-not-exist.info"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does-not-exist.info"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn default_threshold_is_100_when_nothing_configured() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", None)], None);
    // 95% < 100% built-in default, so this must fail.
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("100.0%"))
        .stdout(predicate::str::contains("default"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn defaults_to_target_coverage_lcov_when_flag_omitted() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("80"))], None);
    // Write the lcov at the default location and omit --lcov entirely.
    let default_dir = tmp.path().join("target/coverage");
    fs::create_dir_all(&default_dir).expect("create target/coverage");
    fs::write(
        default_dir.join("lcov.info"),
        make_coverage_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 100)]),
    )
    .expect("write default lcov");

    coverage_gate(tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));
}
