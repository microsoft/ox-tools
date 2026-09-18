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
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Output, Stdio};

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

struct FakeCoverageTools {
    _directory: TempDir,
    cargo: PathBuf,
    llvm_cov: PathBuf,
    rustc: PathBuf,
}

impl FakeCoverageTools {
    fn compile() -> Self {
        let directory = TempDir::new().expect("fake tools tempdir");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-coverage-tool.rs");
        let helper = directory.path().join(format!("fake-coverage-tool{}", std::env::consts::EXE_SUFFIX));
        let status = ProcessCommand::new("rustc")
            .args(["--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&helper)
            .status()
            .expect("compile fake coverage tool");
        assert!(status.success(), "fake coverage tool must compile");

        let cargo = directory.path().join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        let rustc = directory.path().join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        let rustlib = directory.path().join("rustlib");
        let llvm_bin = rustlib.join("bin");
        let target_libdir = rustlib.join("lib");
        fs::create_dir_all(&llvm_bin).expect("create fake LLVM bin");
        fs::create_dir_all(&target_libdir).expect("create fake target libdir");
        let llvm_cov = llvm_bin.join(format!("llvm-cov{}", std::env::consts::EXE_SUFFIX));
        fs::copy(&helper, &cargo).expect("copy fake cargo");
        fs::copy(&helper, &rustc).expect("copy fake rustc");
        fs::copy(&helper, &llvm_cov).expect("copy fake llvm-cov");

        Self {
            _directory: directory,
            cargo,
            llvm_cov,
            rustc,
        }
    }
}

fn fake_collection_command(dir: &Path, tools: &FakeCoverageTools, object: &Path) -> Command {
    fake_collection_command_with_coverage_dir(dir, tools, object, &dir.join("coverage"))
}

fn fake_collection_command_with_coverage_dir(dir: &Path, tools: &FakeCoverageTools, object: &Path, coverage_dir: &Path) -> Command {
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let fake_path = std::env::join_paths(
        std::iter::once(
            tools
                .cargo
                .parent()
                .expect("fake cargo path must have a parent directory")
                .to_path_buf(),
        )
        .chain(std::env::split_paths(&inherited_path)),
    )
    .expect("fake tool directory must form a valid PATH");
    let mut command = coverage_gate(dir);
    command
        .arg("run")
        .args(["--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("PATH", fake_path)
        .env("LLVM_COV", &tools.llvm_cov)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", dir.join("tools.log"))
        .env("FAKE_RESPONSE_LOG", dir.join("response.log"))
        .env("FAKE_WORKSPACE_ROOT", dir)
        .env("FAKE_COVERAGE_OBJECT", object);
    command
}

fn run_concurrently(command: &Command) -> std::process::Child {
    let mut process = ProcessCommand::new(command.get_program());
    process.args(command.get_args());
    if let Some(directory) = command.get_current_dir() {
        process.current_dir(directory);
    }
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            process.env(key, value);
        } else {
            process.env_remove(key);
        }
    }
    process
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn concurrent coverage-gate")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
        .stdout(predicate::str::contains("1 package below threshold"))
        .stdout(predicate::str::contains("beta: 60/100 lines covered; 40 uncovered."))
        .stdout(predicate::str::contains("src/lib.rs: 61-100"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn displayed_rounding_does_not_satisfy_full_coverage_threshold() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 2_000, 1_999)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("99.9%"))
        .stdout(predicate::str::contains("100.0%"))
        .stdout(predicate::str::contains("-<0.1pp"))
        .stdout(predicate::str::contains("FAIL"))
        .stdout(predicate::str::contains("1999/2000 lines covered; 1 uncovered."));
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

/// Write a workspace whose members carry an explicit
/// `[package.metadata.coverage-gate]` body. Each entry is
/// `(name, gate_body)`; an empty `gate_body` omits the block entirely.
fn make_workspace_with_gate(dir: &Path, members: &[(&str, &str)]) {
    let members_list = members.iter().map(|(n, _)| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"2\"\nmembers = [{members_list}]\n"),
    )
    .expect("write workspace root Cargo.toml");

    for (name, gate_body) in members {
        let member_dir = dir.join(name);
        fs::create_dir_all(member_dir.join("src")).expect("mkdir member src");
        let metadata = if gate_body.is_empty() {
            String::new()
        } else {
            format!("\n[package.metadata.coverage-gate]\n{gate_body}\n")
        };
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

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn expect_no_coverable_lines_passes_with_no_data() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[("alpha", "min-lines-percent = 80"), ("beta", "expect-no-coverable-lines = true")],
    );
    // Only alpha contributes coverage data; beta legitimately has none.
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("EMPTY"))
        .stdout(predicate::str::contains("all packages meet their threshold"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn expect_no_coverable_lines_fails_when_lines_present_exits_1() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[("alpha", "min-lines-percent = 80"), ("beta", "expect-no-coverable-lines = true")],
    );
    // beta declared no coverable lines but the tracefile attributes some.
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 100, 95), ("beta/src/lib.rs", 5, 0)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("NOT EMPTY"))
        .stdout(predicate::str::contains("unexpected coverable lines"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary as a subprocess")]
fn conflicting_coverage_metadata_exits_2() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(tmp.path(), &[("alpha", "min-lines-percent = 50\nexpect-no-coverable-lines = true")]);
    let lcov_path = write_lcov(tmp.path(), &[("alpha/src/lib.rs", 10, 10)]);

    coverage_gate(tmp.path())
        .args(["--lcov", &lcov_path])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot set both"));
}

#[test]
fn target_zero_threshold_opts_package_out_of_gate() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[
            (
                "alpha",
                "min-lines-percent = 100\n\n\
                 [package.metadata.coverage-gate.target.'cfg(not(windows))']\n\
                min-lines-percent = 0",
            ),
            ("beta", "min-lines-percent = 80"),
        ],
    );
    let lcov_path = write_lcov(tmp.path(), &[("beta/src/lib.rs", 10, 9)]);

    coverage_gate(tmp.path())
        .args(["--target", "x86_64-unknown-linux-gnu", "--lcov", &lcov_path])
        .assert()
        .success()
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("(no data)"))
        .stdout(predicate::str::contains("0.0%"));
}

#[test]
fn empty_lcov_passes_when_all_effective_policies_allow_no_data() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[
            (
                "alpha",
                "min-lines-percent = 100\n\n\
                 [package.metadata.coverage-gate.target.'cfg(not(windows))']\n\
                min-lines-percent = 0",
            ),
            ("beta", "expect-no-coverable-lines = true"),
        ],
    );
    let empty_lcov = write_lcov(tmp.path(), &[]);

    coverage_gate(tmp.path())
        .args(["--target", "x86_64-unknown-linux-gnu", "--lcov", &empty_lcov])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("all packages meet their threshold"));
}

#[test]
fn target_zero_threshold_package_remains_gated_when_override_does_not_match() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[(
            "alpha",
            "min-lines-percent = 100\n\n\
             [package.metadata.coverage-gate.target.'cfg(not(windows))']\n\
            min-lines-percent = 0",
        )],
    );
    let empty_lcov = write_lcov(tmp.path(), &[]);

    coverage_gate(tmp.path())
        .args(["--target", "x86_64-pc-windows-msvc", "--lcov", &empty_lcov])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("NO DATA"));
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

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_delegates_both_configurations_to_cargo_llvm_cov_report_and_evaluates() {
    let tmp = TempDir::new().expect("tempdir");
    // alpha is deliberately opted out of gating but must still be passed to
    // cargo-llvm-cov because its tests can cover beta.
    make_workspace(tmp.path(), &[("alpha", Some("0")), ("beta", Some("100"))], None);

    let object_dir = tmp.path().join("objects with spaces");
    fs::create_dir_all(&object_dir).expect("create object directory");
    let object = object_dir.join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");

    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("collected coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    fs::write(coverage_dir.join("lcov-all-features.info"), b"old LCOV").expect("write prior LCOV");
    let tool_log = tmp.path().join("tools.log");
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    coverage_gate(tmp.path())
        .arg("run")
        .args([
            "--package",
            "alpha",
            "--package",
            "beta",
            "--jobs",
            "3",
            "--target",
            "x86_64-pc-windows-msvc",
        ])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("LLVM_COV", &tools.llvm_cov)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", &tool_log)
        .env("FAKE_WORKSPACE_ROOT", tmp.path())
        .env("FAKE_COVERAGE_OBJECT", &object)
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    assert!(coverage_dir.join("lcov-all-features.info").is_file());
    assert!(coverage_dir.join("lcov-no-default.info").is_file());
    assert!(
        fs::read_to_string(coverage_dir.join("lcov-all-features.info"))
            .expect("read replacement LCOV")
            .starts_with("TN:"),
        "successful collection must write the requested stable artifact"
    );

    let log = fs::read_to_string(tool_log).expect("read fake tool log");
    assert!(!log.contains("cargo\tllvm-cov\tclean"), "{log}");
    assert_eq!(log.matches("cargo\tllvm-cov\tnextest\t--no-report").count(), 2, "{log}");
    let configuration_targets = log
        .lines()
        .filter(|line| line.contains("llvm-cov\tnextest"))
        .filter_map(|line| line.split('\t').find_map(|field| field.strip_prefix("COVERAGE_TARGET=")))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        configuration_targets.len(),
        2,
        "each configuration needs fresh profile state:\n{log}"
    );
    assert!(configuration_targets.iter().any(|target| target.ends_with("all-features")), "{log}");
    assert!(configuration_targets.iter().any(|target| target.ends_with("no-default")), "{log}");
    assert!(log.contains("--all-features"), "{log}");
    assert!(log.contains("--no-default-features"), "{log}");
    assert!(log.contains("--package\talpha@0.1.0"), "{log}");
    assert!(log.contains("--package\tbeta@0.1.0"), "{log}");
    assert!(log.contains("--jobs\t3"), "{log}");
    assert!(log.contains("--build-jobs\t3"), "{log}");
    assert!(log.contains("--target\tx86_64-pc-windows-msvc"), "{log}");
    assert_eq!(log.matches("\t--locked").count(), 2, "{log}");
    assert_eq!(log.matches("cargo\tllvm-cov\treport\t--lcov").count(), 2, "{log}");
    assert!(!log.contains("--cargo-message-format"), "{log}");
    assert!(!log.contains("llvm-profdata\tmerge"), "{log}");
    assert!(!log.contains("llvm-cov\texport"), "{log}");
    assert!(
        fs::read_dir(&coverage_dir).expect("read coverage directory").all(|entry| !entry
            .expect("coverage entry")
            .file_name()
            .to_string_lossy()
            .contains(".rsp")),
        "temporary response files must be removed"
    );
}

#[test]
#[cfg(windows)]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn windows_report_overflow_reparses_windows_arguments_with_msystem_set() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = PathBuf::from(r#"C:\coverage objects\say "quoted"\test-object.exe"#);
    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    let lcov_path = coverage_dir.join("lcov-all-features.info");
    fs::write(&lcov_path, b"previous LCOV").expect("write previous LCOV");

    fake_collection_command(tmp.path(), &tools, &object)
        .env("MSYSTEM", "MINGW64")
        .env("FAKE_EXPECT_REPORT_MSYSTEM_REMOVED", "1")
        .env("FAKE_REPORT_COMMAND_TOO_LONG", "1")
        .assert()
        .success()
        .stderr(predicate::str::contains("retrying its export through an LLVM response file"));

    let response = fs::read_to_string(tmp.path().join("response.log")).expect("read response log");
    assert!(
        response.contains(r#""C:\coverage objects\say \"quoted\"\test-object.exe""#),
        "{response}"
    );
    assert!(response.contains("\"-ignore-filename-regex\"\n\"UPSTREAM_DEFAULTS\""), "{response}");
    assert!(
        fs::read_to_string(lcov_path).expect("read published LCOV").starts_with("TN:"),
        "successful fallback must replace the stable artifact"
    );
}

#[test]
#[cfg(windows)]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn windows_report_overflow_converts_paired_no_data_before_publication() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(tmp.path(), &[("alpha", "expect-no-coverable-lines = true")]);
    let object = PathBuf::from(r"C:\coverage objects\empty.exe");
    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    let lcov_path = coverage_dir.join("lcov-all-features.info");
    fs::write(&lcov_path, b"previous LCOV").expect("write previous LCOV");

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_REPORT_COMMAND_TOO_LONG", "1")
        .env("FAKE_FALLBACK_NO_COVERAGE_DATA", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("EMPTY"))
        .stderr(predicate::str::contains("evaluating an empty LCOV report"))
        .stderr(predicate::str::contains("could not load coverage information").not());

    assert_eq!(fs::read(lcov_path).expect("read empty LCOV"), b"");
}

#[test]
#[cfg(windows)]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn windows_report_overflow_failure_preserves_stable_artifact() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = PathBuf::from(r"C:\coverage objects\failing.exe");
    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    let lcov_path = coverage_dir.join("lcov-all-features.info");
    fs::write(&lcov_path, b"completed LCOV").expect("write completed LCOV");

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_REPORT_COMMAND_TOO_LONG", "1")
        .env("FAKE_FAIL_FALLBACK", "1")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("response-file fallback failed"))
        .stderr(predicate::str::contains("requested response-file export failure"));

    assert_eq!(
        fs::read(lcov_path).expect("read preserved LCOV"),
        b"completed LCOV",
        "failed fallback must not publish partial stdout"
    );
}

#[test]
#[cfg_attr(miri, ignore = "spawns concurrent binaries and fake coverage tools")]
fn concurrent_runs_use_isolated_coverage_targets_and_clean_them() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    let first_coverage = tmp.path().join("coverage-first");
    let second_coverage = tmp.path().join("coverage-second");
    let mut first = fake_collection_command_with_coverage_dir(tmp.path(), &tools, &object, &first_coverage);
    first.env("FAKE_NEXTEST_DELAY_MS", "250");
    let mut second = fake_collection_command_with_coverage_dir(tmp.path(), &tools, &object, &second_coverage);
    second.env("FAKE_NEXTEST_DELAY_MS", "250");

    let first = run_concurrently(&first);
    let second = run_concurrently(&second);
    let first = first.wait_with_output().expect("wait for first collection");
    let second = second.wait_with_output().expect("wait for second collection");
    assert_success(&first);
    assert_success(&second);

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    let targets = log
        .lines()
        .filter(|line| line.contains("llvm-cov\tnextest"))
        .filter_map(|line| line.split('\t').find_map(|field| field.strip_prefix("COVERAGE_TARGET=")))
        .map(PathBuf::from)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(targets.len(), 2, "concurrent commands must use distinct targets:\n{log}");
    assert!(targets.iter().all(|target| !target.exists()), "isolated targets must be cleaned");
    assert!(first_coverage.join("lcov-all-features.info").is_file());
    assert!(second_coverage.join("lcov-all-features.info").is_file());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn collection_preserves_shared_coverage_and_test_artifacts() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let sentinels = [
        tmp.path().join("target/llvm-cov/html/sentinel"),
        tmp.path().join("target/llvm-cov/text/sentinel"),
        tmp.path().join("target/tests/trybuild/sentinel"),
        tmp.path().join("tests/target/sentinel"),
        tmp.path().join("target/ui/sentinel"),
    ];
    for sentinel in &sentinels {
        fs::create_dir_all(sentinel.parent().expect("sentinel has a parent")).expect("create shared artifact directory");
        fs::write(sentinel, b"keep").expect("write shared artifact sentinel");
    }

    let tools = FakeCoverageTools::compile();
    fake_collection_command(tmp.path(), &tools, &object).assert().success();

    for sentinel in sentinels {
        assert_eq!(fs::read(&sentinel).expect("read shared artifact sentinel"), b"keep");
    }
    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(!log.contains("cargo\tllvm-cov\tclean"), "{log}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_passes_a_successful_empty_lcov_export_to_evaluation() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(tmp.path(), &[("alpha", "expect-no-coverable-lines = true")]);

    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");

    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("coverage");
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("LLVM_COV", &tools.llvm_cov)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", tmp.path().join("tools.log"))
        .env("FAKE_RESPONSE_LOG", tmp.path().join("response.log"))
        .env("FAKE_WORKSPACE_ROOT", tmp.path())
        .env("FAKE_COVERAGE_OBJECT", &object)
        .env("FAKE_EMPTY_LCOV", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("EMPTY"));

    let lcov = coverage_dir.join("lcov-all-features.info");
    assert!(lcov.is_file());
    assert_eq!(fs::metadata(lcov).expect("LCOV metadata").len(), 0);
    assert!(!coverage_dir.join("lcov-no-default.info").exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary")]
fn run_rejects_external_lcov_input() {
    let tmp = TempDir::new().expect("tempdir");

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--lcov", "external.info"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and cargo metadata")]
fn run_rejects_unknown_package_selector() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--package", "unknown"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("did not match any workspace member"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and cargo metadata")]
fn run_reports_an_unusable_coverage_directory() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let coverage_dir = tmp.path().join("not-a-directory");
    fs::write(&coverage_dir, b"file").expect("write conflicting file");
    let tools = FakeCoverageTools::compile();
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    coverage_gate(tmp.path())
        .arg("run")
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("FAKE_REAL_CARGO", real_cargo)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("failed to create coverage directory"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_reports_collection_and_upstream_report_failures() {
    for (variable, expected) in [
        ("FAKE_FAIL_NEXTEST", "llvm-cov nextest --no-report"),
        ("FAKE_NO_PROFILE", "cargo llvm-cov report failed"),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
        let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
        fs::write(&object, b"object").expect("write fake object");
        let tools = FakeCoverageTools::compile();

        fake_collection_command(tmp.path(), &tools, &object)
            .env(variable, "1")
            .assert()
            .code(2)
            .stderr(predicate::str::contains(expected));
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_preserves_nextest_output_and_rendered_compiler_diagnostics() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_NEXTEST_TEXT", "1")
        .env("FAKE_COMPILER_MESSAGE", "1")
        .env("FAKE_REPORT_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("non-JSON nextest output"))
        .stdout(predicate::str::contains("fake compiler diagnostic"))
        .stdout(predicate::str::contains("report-stdout"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(!log.contains("--cargo-message-format"), "{log}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_rejects_stable_rust_and_old_cargo_llvm_cov() {
    for (variable, value, expected) in [
        ("FAKE_FAIL_CARGO_VERSION", "1", "cargo toolchain validation failed"),
        ("FAKE_STABLE_CARGO", "1", "requires nightly Cargo"),
        ("FAKE_STABLE_RUSTC", "1", "requires nightly rustc"),
        ("FAKE_LLVM_COV_VERSION", "0.8.7", "requires cargo-llvm-cov >= 0.9.0"),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
        let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
        fs::write(&object, b"object").expect("write fake object");
        let tools = FakeCoverageTools::compile();

        fake_collection_command(tmp.path(), &tools, &object)
            .env(variable, value)
            .assert()
            .code(2)
            .stderr(predicate::str::contains(expected));

        let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
        assert!(!log.contains("llvm-cov\tnextest"), "{log}");
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_preserves_inherited_rustup_toolchain() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("RUSTUP_TOOLCHAIN", "inherited-nightly")
        .env("FAKE_EXPECT_RUSTUP_TOOLCHAIN", "inherited-nightly")
        .assert()
        .success();
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn zero_threshold_only_selection_is_instrumented_and_accepts_empty_lcov() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("0"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_NO_COVERAGE_DATA", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"))
        .stderr(predicate::str::contains("evaluating an empty LCOV report"))
        .stderr(predicate::str::contains("could not load coverage information").not());

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("cargo\tllvm-cov\tnextest"), "{log}");
    assert!(log.contains("--no-tests=pass"), "{log}");
    assert!(log.contains("cargo\tllvm-cov\treport"), "{log}");
    assert!(!log.contains("cargo\tnextest\trun"), "{log}");
    let lcov = tmp.path().join("coverage/lcov-all-features.info");
    assert!(lcov.is_file());
    assert_eq!(fs::metadata(lcov).expect("LCOV metadata").len(), 0);
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn explicit_target_is_propagated_to_collection_and_evaluation() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[(
            "alpha",
            "min-lines-percent = 100\n\n\
             [package.metadata.coverage-gate.target.'aarch64-pc-windows-msvc']\n\
             min-lines-percent = 0",
        )],
    );
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .args(["--target", "aarch64-pc-windows-msvc"])
        .env("FAKE_LCOV_HITS", "0")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    let collection_commands = log
        .lines()
        .filter(|line| line.contains("cargo\tllvm-cov\tnextest") || line.contains("cargo\tllvm-cov\treport"))
        .collect::<Vec<_>>();
    assert_eq!(collection_commands.len(), 2, "{log}");
    assert!(
        collection_commands
            .iter()
            .all(|line| line.contains("--target\taarch64-pc-windows-msvc")),
        "{log}"
    );
    assert!(
        log.contains("rustc\t--print\tcfg\t--target\taarch64-pc-windows-msvc"),
        "evaluation must use the explicit collection target:\n{log}"
    );
    assert!(!log.contains("cargo\tnextest\trun"), "{log}");
    assert!(tmp.path().join("coverage/lcov-all-features.info").is_file());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn omitted_target_uses_one_host_for_collection_and_evaluation() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[
            (
                "alpha",
                "min-lines-percent = 100\n\n\
                 [package.metadata.coverage-gate.target.'x86_64-pc-windows-msvc']\n\
                 min-lines-percent = 0",
            ),
            ("beta", "min-lines-percent = 0"),
        ],
    );
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("CARGO_BUILD_TARGET", "aarch64-pc-windows-msvc")
        .env("FAKE_LCOV_HITS", "0")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert_eq!(
        log.matches("rustc\t-vV").count(),
        1,
        "host discovery must be reused for validation:\n{log}"
    );
    let collection_commands = log
        .lines()
        .filter(|line| line.contains("cargo\tllvm-cov\tnextest") || line.contains("cargo\tllvm-cov\treport"))
        .collect::<Vec<_>>();
    assert_eq!(collection_commands.len(), 2, "{log}");
    assert!(
        collection_commands
            .iter()
            .all(|line| line.contains("--target\tx86_64-pc-windows-msvc")),
        "{log}"
    );
    assert!(
        log.contains("rustc\t--print\tcfg\t--target\tx86_64-pc-windows-msvc"),
        "evaluation must use the same resolved host target:\n{log}"
    );
    assert!(!log.contains("--target\taarch64-pc-windows-msvc"), "{log}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn configured_no_coverage_target_runs_plain_tests_without_advertising_coverage() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .args([
            "--target",
            "aarch64-pc-windows-msvc",
            "--configuration",
            "no-default-features",
            "--no-coverage-target",
            "x86_64-pc-windows-msvc",
            "--no-coverage-target",
            "aarch64-pc-windows-msvc",
        ])
        .env("FAKE_PLAIN_NEXTEST_STDOUT", "1")
        .env("FAKE_FAIL_CARGO_VERSION", "1")
        .env("FAKE_FAIL_RUSTC", "1")
        .env("FAKE_LLVM_COV_VERSION", "0.8.7")
        .assert()
        .success()
        .stdout(predicate::str::contains("plain-nextest-stdout"))
        .stderr(predicate::str::contains("configured for no coverage"))
        .stderr(predicate::str::contains("without coverage collection or gating"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert_eq!(log.matches("cargo\tnextest\trun").count(), 2, "{log}");
    assert!(log.contains("--all-features"), "{log}");
    assert!(log.contains("--no-default-features"), "{log}");
    assert!(log.contains("\t--locked"), "{log}");
    assert!(log.contains("--target\taarch64-pc-windows-msvc"), "{log}");
    assert!(!log.contains("llvm-cov\tnextest"), "{log}");
    assert!(!log.contains("cargo\t--version\t--verbose"), "{log}");
    assert!(!log.contains("cargo\tllvm-cov\t--version"), "{log}");
    assert!(!log.contains("rustc\t-vV"), "{log}");
    assert!(!tmp.path().join("coverage/lcov-all-features.info").exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn configured_no_coverage_host_target_is_resolved_and_propagated_without_tool_validation() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .args(["--no-coverage-target", "x86_64-pc-windows-msvc"])
        .env("FAKE_FAIL_CARGO_VERSION", "1")
        .env("FAKE_STABLE_RUSTC", "1")
        .env("FAKE_LLVM_COV_VERSION", "0.8.7")
        .assert()
        .success()
        .stderr(predicate::str::contains("configured for no coverage"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert_eq!(log.matches("rustc\t-vV").count(), 1, "{log}");
    let plain_nextest = log
        .lines()
        .find(|line| line.contains("cargo\tnextest\trun"))
        .expect("plain nextest command must be logged");
    assert!(plain_nextest.contains("--target\tx86_64-pc-windows-msvc"), "{log}");
    assert!(!log.contains("llvm-cov\tnextest"), "{log}");
    assert!(!log.contains("cargo\t--version\t--verbose"), "{log}");
    assert!(!log.contains("cargo\tllvm-cov\t--version"), "{log}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn quiet_suppresses_all_collection_stdout_and_still_writes_summary() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();
    let summary = tmp.path().join("summary.md");

    fake_collection_command(tmp.path(), &tools, &object)
        .arg("--quiet")
        .arg("--summary-file")
        .arg(&summary)
        .env("FAKE_NEXTEST_TEXT", "1")
        .env("FAKE_COMPILER_MESSAGE", "1")
        .env("FAKE_REPORT_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    assert!(fs::read_to_string(summary).expect("read summary").contains("### coverage-gate"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn quiet_suppresses_plain_nextest_stdout_but_preserves_skip_diagnostic() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();
    let summary = tmp.path().join("summary.md");

    fake_collection_command(tmp.path(), &tools, &object)
        .arg("--quiet")
        .arg("--summary-file")
        .arg(&summary)
        .args([
            "--target",
            "aarch64-pc-windows-msvc",
            "--no-coverage-target",
            "aarch64-pc-windows-msvc",
        ])
        .env("FAKE_PLAIN_NEXTEST_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("tests passed without coverage collection or gating"));

    assert!(
        fs::read_to_string(summary)
            .expect("read no-gate summary")
            .contains("without coverage collection or gating")
    );
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn explicitly_configured_plain_test_failures_propagate() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .args([
            "--target",
            "aarch64-pc-windows-msvc",
            "--no-coverage-target",
            "aarch64-pc-windows-msvc",
        ])
        .env("FAKE_FAIL_PLAIN_NEXTEST", "1")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("requested plain nextest failure"))
        .stderr(predicate::str::contains("cargo nextest failed"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_delegates_object_discovery_and_export_to_cargo_llvm_cov_report() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace_with_gate(
        tmp.path(),
        &[
            (
                "alpha",
                "min-lines-percent = 100\n\n\
                 [package.metadata.coverage-gate.target.'cfg(windows)']\n\
                 min-lines-percent = 100",
            ),
            ("beta", "min-lines-percent = 100"),
        ],
    );
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env_remove("LLVM_COV")
        .env("RUSTUP_TOOLCHAIN", "inherited-nightly")
        .env("FAKE_EXPECT_RUSTUP_TOOLCHAIN", "inherited-nightly")
        .args(["--target", "x86_64-pc-windows-msvc"])
        .assert()
        .success();

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("cargo\tllvm-cov\treport\t--lcov"), "{log}");
    assert!(!log.contains("rustc\t--print\ttarget-libdir"), "{log}");
    assert!(!log.contains("llvm-cov\texport"), "{log}");
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_reports_rustc_validation_failures() {
    for (variable, expected) in [("FAKE_FAIL_RUSTC", "exited with"), ("FAKE_INVALID_RUSTC_OUTPUT", "was not UTF-8")] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
        let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
        fs::write(&object, b"object").expect("write fake object");
        let tools = FakeCoverageTools::compile();

        fake_collection_command(tmp.path(), &tools, &object)
            .env_remove("LLVM_COV")
            .env("RUSTC", &tools.rustc)
            .env(variable, "1")
            .assert()
            .code(2)
            .stderr(predicate::str::contains(expected));
    }

    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();
    fake_collection_command(tmp.path(), &tools, &object)
        .env_remove("LLVM_COV")
        .env("RUSTC", tmp.path().join("missing-rustc"))
        .assert()
        .code(2)
        .stderr(predicate::str::contains("failed to execute"));

    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();
    let fake_rustc_dir = tools.rustc.parent().expect("fake rustc path must have a parent directory");
    let fake_path = std::env::join_paths([fake_rustc_dir]).expect("fake tool directory must form a valid PATH");
    fake_collection_command(tmp.path(), &tools, &object)
        .env_remove("LLVM_COV")
        .env_remove("RUSTC")
        .env("PATH", fake_path)
        .assert()
        .success();

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("rustc\t-vV"), "{log}");
    assert!(log.contains("cargo\tllvm-cov\treport\t--lcov"), "{log}");
}

#[test]
#[ignore = "requires the pinned nightly, cargo-llvm-cov, nextest, and LLVM tools"]
fn real_collection_smoke_produces_lcov_and_a_passing_verdict() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("smoke", Some("100"))], None);
    fs::write(
        tmp.path().join("smoke/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             #[test]\n\
             fn answer_is_covered() { assert_eq!(super::answer(), 42); }\n\
         }\n",
    )
    .expect("write covered smoke crate");
    let status = ProcessCommand::new("cargo")
        .current_dir(tmp.path())
        .arg("generate-lockfile")
        .status()
        .expect("generate smoke lockfile");
    assert!(status.success(), "smoke lockfile generation must succeed");
    let coverage_dir = tmp.path().join("coverage");

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--package", "smoke"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("RUSTUP_TOOLCHAIN", "nightly-2026-05-30")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("LLVM_COV")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    let lcov = fs::read_to_string(coverage_dir.join("lcov-all-features.info")).expect("read real LCOV output");
    assert!(lcov.contains("smoke/src/lib.rs") || lcov.contains(r"smoke\src\lib.rs"), "{lcov}");
    assert!(lcov.lines().any(|line| line.starts_with("DA:")), "{lcov}");
    assert!(coverage_dir.join("lcov-no-default.info").is_file());
}

#[test]
#[ignore = "requires the pinned nightly, cargo-llvm-cov, nextest, and LLVM tools"]
fn real_collection_accepts_an_empty_lcov_for_an_all_zero_selection() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("empty", Some("0"))], None);
    let status = ProcessCommand::new("cargo")
        .current_dir(tmp.path())
        .arg("generate-lockfile")
        .status()
        .expect("generate empty-workspace lockfile");
    assert!(status.success(), "empty-workspace lockfile generation must succeed");
    let coverage_dir = tmp.path().join("coverage");

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--package", "empty", "--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("RUSTUP_TOOLCHAIN", "nightly-2026-05-30")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("LLVM_COV")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    let lcov = fs::read_to_string(coverage_dir.join("lcov-all-features.info")).expect("read empty LCOV output");
    assert!(!lcov.lines().any(|line| line.starts_with("DA:")), "{lcov}");
}

#[test]
#[ignore = "requires the pinned nightly, cargo-llvm-cov, nextest, and LLVM tools"]
fn real_collection_handles_mixed_coverable_and_no_data_objects() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("empty", Some("0")), ("smoke", Some("100"))], None);
    fs::write(
        tmp.path().join("smoke/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             #[test]\n\
             fn answer_is_covered() { assert_eq!(super::answer(), 42); }\n\
         }\n",
    )
    .expect("write covered smoke crate");
    let status = ProcessCommand::new("cargo")
        .current_dir(tmp.path())
        .arg("generate-lockfile")
        .status()
        .expect("generate mixed-workspace lockfile");
    assert!(status.success(), "mixed-workspace lockfile generation must succeed");
    let coverage_dir = tmp.path().join("coverage");

    coverage_gate(tmp.path())
        .arg("run")
        .args(["--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("RUSTUP_TOOLCHAIN", "nightly-2026-05-30")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("LLVM_COV")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    let lcov = fs::read_to_string(coverage_dir.join("lcov-all-features.info")).expect("read mixed LCOV output");
    assert!(lcov.contains("smoke/src/lib.rs") || lcov.contains(r"smoke\src\lib.rs"), "{lcov}");
    assert!(lcov.lines().any(|line| line.starts_with("DA:")), "{lcov}");
}
