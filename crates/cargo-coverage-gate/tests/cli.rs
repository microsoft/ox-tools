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
use std::process::Command as ProcessCommand;

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
    llvm_profdata: PathBuf,
    rustc: PathBuf,
    target_libdir: PathBuf,
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
        let rustup = directory.path().join(format!("rustup{}", std::env::consts::EXE_SUFFIX));
        let rustc = directory.path().join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        let rustlib = directory.path().join("rustlib");
        let llvm_bin = rustlib.join("bin");
        let target_libdir = rustlib.join("lib");
        fs::create_dir_all(&llvm_bin).expect("create fake LLVM bin");
        fs::create_dir_all(&target_libdir).expect("create fake target libdir");
        let llvm_cov = llvm_bin.join(format!("llvm-cov{}", std::env::consts::EXE_SUFFIX));
        let llvm_profdata = llvm_bin.join(format!("llvm-profdata{}", std::env::consts::EXE_SUFFIX));
        fs::copy(&helper, &cargo).expect("copy fake cargo");
        fs::copy(&helper, rustup).expect("copy fake rustup");
        fs::copy(&helper, &rustc).expect("copy fake rustc");
        fs::copy(&helper, &llvm_cov).expect("copy fake llvm-cov");
        fs::copy(&helper, &llvm_profdata).expect("copy fake llvm-profdata");

        Self {
            _directory: directory,
            cargo,
            llvm_cov,
            llvm_profdata,
            rustc,
            target_libdir,
        }
    }
}

fn fake_collection_command(dir: &Path, tools: &FakeCoverageTools, object: &Path) -> Command {
    fake_collection_command_inner(dir, tools, object, Some("nightly-test"))
}

fn fake_collection_command_active(dir: &Path, tools: &FakeCoverageTools, object: &Path) -> Command {
    fake_collection_command_inner(dir, tools, object, None)
}

fn fake_collection_command_inner(dir: &Path, tools: &FakeCoverageTools, object: &Path, toolchain: Option<&str>) -> Command {
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
    command.arg("run");
    if let Some(toolchain) = toolchain {
        command.args(["--toolchain", toolchain]);
    }
    command
        .args(["--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(dir.join("coverage"))
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("PATH", fake_path)
        .env_remove("RUSTUP")
        .env("LLVM_COV", &tools.llvm_cov)
        .env("LLVM_PROFDATA", &tools.llvm_profdata)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", dir.join("tools.log"))
        .env("FAKE_RESPONSE_LOG", dir.join("response.log"))
        .env("FAKE_WORKSPACE_ROOT", dir)
        .env("FAKE_COVERAGE_OBJECT", object);
    if let Some(toolchain) = toolchain {
        command.env("FAKE_EXPECT_TOOLCHAIN", toolchain);
    }
    command
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
fn run_collects_both_configurations_with_response_files_and_evaluates() {
    let tmp = TempDir::new().expect("tempdir");
    // alpha is deliberately opted out of gating but must still be passed to
    // cargo-llvm-cov because its tests can cover beta.
    make_workspace(tmp.path(), &[("alpha", Some("0")), ("beta", Some("100"))], None);
    let package_file = tmp.path().join("packages.txt");
    fs::write(&package_file, "\nalpha@0.1.0\n").expect("write package file");

    let object_dir = tmp.path().join("objects with spaces");
    fs::create_dir_all(&object_dir).expect("create object directory");
    let object = object_dir.join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");

    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("collected coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    fs::write(coverage_dir.join("lcov-all-features.info"), b"old LCOV").expect("write prior LCOV");
    let tool_log = tmp.path().join("tools.log");
    let response_log = tmp.path().join("response.log");
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    coverage_gate(tmp.path())
        .arg("run")
        .arg("--package-file")
        .arg(&package_file)
        .args(["--package", "beta", "--jobs", "3", "--target", "x86_64-pc-windows-msvc"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("LLVM_COV", &tools.llvm_cov)
        .env("LLVM_PROFDATA", &tools.llvm_profdata)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", &tool_log)
        .env("FAKE_RESPONSE_LOG", &response_log)
        .env("FAKE_WORKSPACE_ROOT", tmp.path())
        .env("FAKE_COVERAGE_OBJECT", &object)
        .env("LLVM_COV_FLAGS", "-fake-cov-flag")
        .env("LLVM_PROFDATA_FLAGS", "-fake-profdata-flag")
        .assert()
        .success()
        .stdout(predicate::str::contains("all packages meet their threshold"));

    assert!(coverage_dir.join("lcov-all-features.info").is_file());
    assert!(coverage_dir.join("lcov-no-default-features.info").is_file());
    assert!(
        fs::read_to_string(coverage_dir.join("lcov-all-features.info"))
            .expect("read replacement LCOV")
            .starts_with("TN:"),
        "successful recollection must atomically replace the prior artifact"
    );

    let log = fs::read_to_string(tool_log).expect("read fake tool log");
    assert_eq!(log.matches("cargo\tllvm-cov\tclean\t--workspace").count(), 2, "{log}");
    assert_eq!(log.matches("cargo\tllvm-cov\tnextest\t--no-report").count(), 2, "{log}");
    assert!(log.contains("--all-features"), "{log}");
    assert!(log.contains("--no-default-features"), "{log}");
    assert!(log.contains("--package\talpha@0.1.0"), "{log}");
    assert!(log.contains("--package\tbeta@0.1.0"), "{log}");
    assert!(log.contains("--jobs\t3"), "{log}");
    assert!(log.contains("--build-jobs\t3"), "{log}");
    assert!(log.contains("--target\tx86_64-pc-windows-msvc"), "{log}");
    assert_eq!(log.matches("\t--locked").count(), 2, "{log}");
    assert!(log.contains("--cargo-message-format=json-render-diagnostics"), "{log}");
    assert!(log.contains("llvm-profdata\tmerge\t-sparse\t-f"), "{log}");
    assert!(log.contains("-fake-profdata-flag"), "{log}");
    assert!(log.contains("llvm-cov\texport\t-format=lcov"), "{log}");
    assert!(log.contains("-fake-cov-flag"), "{log}");

    let response = fs::read_to_string(response_log).expect("read response log");
    assert!(response.contains("-object"), "{response}");
    assert!(response.contains("objects with spaces"), "{response}");
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
        .env("LLVM_PROFDATA", &tools.llvm_profdata)
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
    assert!(!coverage_dir.join("lcov-no-default-features.info").exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_export_failure_preserves_published_lcov_and_cleans_temporary_files() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);

    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");

    let tools = FakeCoverageTools::compile();
    let coverage_dir = tmp.path().join("coverage");
    fs::create_dir(&coverage_dir).expect("create coverage directory");
    let published_lcov = coverage_dir.join("lcov-all-features.info");
    let published_bytes = b"previously published LCOV\n";
    fs::write(&published_lcov, published_bytes).expect("write previously published LCOV");
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    coverage_gate(tmp.path())
        .arg("run")
        .arg("--quiet")
        .args(["--configuration", "all-features"])
        .arg("--coverage-dir")
        .arg(&coverage_dir)
        .env("CARGO", &tools.cargo)
        .env("RUSTC", &tools.rustc)
        .env("LLVM_COV", &tools.llvm_cov)
        .env("LLVM_PROFDATA", &tools.llvm_profdata)
        .env("FAKE_REAL_CARGO", real_cargo)
        .env("FAKE_TOOL_LOG", tmp.path().join("tools.log"))
        .env("FAKE_RESPONSE_LOG", tmp.path().join("response.log"))
        .env("FAKE_WORKSPACE_ROOT", tmp.path())
        .env("FAKE_COVERAGE_OBJECT", &object)
        .env("FAKE_FAIL_COV", "1")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("requested llvm-cov failure"))
        .stderr(predicate::str::contains("llvm-cov export failed"));

    assert_eq!(
        fs::read(&published_lcov).expect("read preserved LCOV"),
        published_bytes,
        "failed recollection must preserve the last completed artifact byte-for-byte"
    );
    assert!(
        fs::read_dir(&coverage_dir).expect("read coverage directory").all(|entry| !entry
            .expect("coverage entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".coverage-gate-")),
        "collection failure must remove every temporary artifact"
    );
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and cargo metadata")]
fn run_with_empty_package_file_is_a_successful_noop() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let package_file = tmp.path().join("packages.txt");
    fs::write(&package_file, "\n \r\n").expect("write empty package file");

    coverage_gate(tmp.path())
        .arg("run")
        .arg("--package-file")
        .arg(&package_file)
        .assert()
        .success()
        .stderr(predicate::str::contains("selection is empty"))
        .stdout(predicate::str::is_empty());

    assert!(!tmp.path().join("target/coverage").exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and cargo metadata")]
fn run_rejects_package_file_entry_without_exact_workspace_version() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let package_file = tmp.path().join("packages.txt");
    fs::write(&package_file, "alpha@9.9.9\n").expect("write package file");

    coverage_gate(tmp.path())
        .arg("run")
        .arg("--package-file")
        .arg(&package_file)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("exact `name@version` workspace member"));
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
fn run_reports_nextest_object_and_profile_failures() {
    for (variable, expected) in [
        ("FAKE_FAIL_CLEAN", "cargo llvm-cov clean failed"),
        ("FAKE_FAIL_NEXTEST", "llvm-cov nextest --no-report"),
        ("FAKE_NO_OBJECT", "no executable object paths"),
        ("FAKE_NO_PROFILE", "no raw profiles"),
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
fn run_propagates_temporary_profile_and_response_file_failures() {
    for variable in ["FAKE_BREAK_DIRECTORY_AFTER_NEXTEST", "FAKE_BREAK_DIRECTORY_AFTER_PROFDATA"] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
        let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
        fs::write(&object, b"object").expect("write fake object");
        let tools = FakeCoverageTools::compile();
        let coverage_dir = tmp.path().join("coverage");

        fake_collection_command(tmp.path(), &tools, &object)
            .env(variable, &coverage_dir)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("failed to create temporary file"));
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_forwards_non_json_nextest_output() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_CLEAN_STDOUT", "1")
        .env("FAKE_NEXTEST_TEXT", "1")
        .env("FAKE_PROFDATA_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("coverage-clean-stdout"))
        .stdout(predicate::str::contains("non-JSON nextest output"))
        .stdout(predicate::str::contains("profdata-stdout"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_rejects_stable_rust_and_old_cargo_llvm_cov() {
    for (variable, value, expected) in [
        ("FAKE_FAIL_CARGO_VERSION", "1", "cargo toolchain validation failed"),
        ("FAKE_STABLE_TOOLCHAIN", "1", "requires a nightly Rust toolchain"),
        ("FAKE_STABLE_RUSTC", "1", "requires nightly rustc"),
        ("FAKE_LLVM_COV_VERSION", "0.6.1", "requires cargo-llvm-cov >= 0.7.0"),
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
#[cfg_attr(miri, ignore = "spawns the binary")]
fn run_rejects_an_empty_toolchain_selection() {
    let tmp = TempDir::new().expect("tempdir");
    coverage_gate(tmp.path())
        .arg("run")
        .args(["--toolchain", ""])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be empty"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake Rustup")]
fn run_reports_rustup_toolchain_resolution_failures() {
    for (variable, expected) in [
        ("FAKE_FAIL_RUSTUP", "exited with"),
        ("FAKE_EMPTY_RUSTUP_OUTPUT", "did not report a program path"),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        let tools = FakeCoverageTools::compile();
        let object = tmp.path().join("unused-object");

        fake_collection_command(tmp.path(), &tools, &object)
            .env(variable, "1")
            .assert()
            .code(2)
            .stderr(predicate::str::contains(expected));
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_honors_coverage_gate_toolchain_environment() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100")), ("beta", Some("100"))], None);
    let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
    fs::write(&object, b"object").expect("write fake object");
    let tools = FakeCoverageTools::compile();

    fake_collection_command_active(tmp.path(), &tools, &object)
        .env("COVERAGE_GATE_TOOLCHAIN", "nightly-env")
        .env("FAKE_EXPECT_TOOLCHAIN", "nightly-env")
        .assert()
        .success();
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn zero_threshold_only_selection_runs_plain_tests_without_a_gate() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("0"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .env("FAKE_PLAIN_NEXTEST_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("plain-nextest-stdout"))
        .stderr(predicate::str::contains("tests passed without coverage collection or gating"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("cargo\tnextest\trun"), "{log}");
    assert!(log.contains("cargo\tnextest\trun\t--workspace\t--all-features\t--locked"), "{log}");
    assert!(!log.contains("llvm-cov\tnextest"), "{log}");
    assert!(!tmp.path().join("coverage/lcov-all-features.info").exists());
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn arm64_windows_target_runs_plain_tests_without_advertising_coverage() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();

    fake_collection_command(tmp.path(), &tools, &object)
        .args(["--target", "aarch64-pc-windows-msvc"])
        .env("FAKE_PLAIN_NEXTEST_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::contains("plain-nextest-stdout"))
        .stderr(predicate::str::contains("does not support cargo-llvm-cov"))
        .stderr(predicate::str::contains("without coverage collection or gating"));

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("cargo\tnextest\trun"), "{log}");
    assert!(log.contains("\t--locked"), "{log}");
    assert!(log.contains("--target\taarch64-pc-windows-msvc"), "{log}");
    assert!(!log.contains("llvm-cov\tnextest"), "{log}");
    assert!(!tmp.path().join("coverage/lcov-all-features.info").exists());
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
        .env("FAKE_CLEAN_STDOUT", "1")
        .env("FAKE_NEXTEST_TEXT", "1")
        .env("FAKE_PROFDATA_STDOUT", "1")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    assert!(fs::read_to_string(summary).expect("read summary").contains("### coverage-gate"));
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn quiet_suppresses_plain_nextest_stdout_but_preserves_skip_diagnostic() {
    let tmp = TempDir::new().expect("tempdir");
    make_workspace(tmp.path(), &[("alpha", Some("0"))], None);
    let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
    let tools = FakeCoverageTools::compile();
    let summary = tmp.path().join("summary.md");

    fake_collection_command(tmp.path(), &tools, &object)
        .arg("--quiet")
        .arg("--summary-file")
        .arg(&summary)
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
fn plain_test_failures_propagate_for_zero_only_and_arm64_paths() {
    for (threshold, target) in [("0", None), ("100", Some("aarch64-pc-windows-msvc"))] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some(threshold))], None);
        let object = tmp.path().join(format!("unused-object{}", std::env::consts::EXE_SUFFIX));
        let tools = FakeCoverageTools::compile();
        let mut command = fake_collection_command(tmp.path(), &tools, &object);
        if let Some(target) = target {
            command.args(["--target", target]);
        }

        command
            .env("FAKE_FAIL_PLAIN_NEXTEST", "1")
            .assert()
            .code(2)
            .stderr(predicate::str::contains("requested plain nextest failure"))
            .stderr(predicate::str::contains("cargo nextest failed"));
    }
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_discovers_llvm_tools_from_rustc() {
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
        .env_remove("LLVM_PROFDATA")
        .env_remove("RUSTC")
        .env("FAKE_TARGET_LIBDIR", &tools.target_libdir)
        .env("FAKE_EXPECT_RUSTC_TOOLCHAIN", "nightly-test")
        .args(["--target", "x86_64-pc-windows-msvc"])
        .assert()
        .success();
}

#[test]
#[cfg_attr(miri, ignore = "spawns the binary and fake coverage tools")]
fn run_reports_rustc_tool_discovery_failures() {
    for (variable, expected) in [
        ("FAKE_FAIL_RUSTC", "exited with"),
        ("FAKE_FAIL_TARGET_LIBDIR", "requested target-libdir failure"),
        ("FAKE_INVALID_RUSTC_OUTPUT", "was not UTF-8"),
        ("FAKE_EMPTY_TARGET_LIBDIR", "had no parent directory"),
    ] {
        let tmp = TempDir::new().expect("tempdir");
        make_workspace(tmp.path(), &[("alpha", Some("100"))], None);
        let object = tmp.path().join(format!("test-object{}", std::env::consts::EXE_SUFFIX));
        fs::write(&object, b"object").expect("write fake object");
        let tools = FakeCoverageTools::compile();

        fake_collection_command_active(tmp.path(), &tools, &object)
            .env_remove("LLVM_COV")
            .env_remove("LLVM_PROFDATA")
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
    fake_collection_command_active(tmp.path(), &tools, &object)
        .env_remove("LLVM_COV")
        .env_remove("LLVM_PROFDATA")
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
    fake_collection_command_active(tmp.path(), &tools, &object)
        .env_remove("LLVM_COV")
        .env_remove("LLVM_PROFDATA")
        .env_remove("RUSTC")
        .env("PATH", fake_path)
        .env("FAKE_TARGET_LIBDIR", &tools.target_libdir)
        .assert()
        .success();

    let log = fs::read_to_string(tmp.path().join("tools.log")).expect("read fake tool log");
    assert!(log.contains("rustc\t--print\ttarget-libdir"), "{log}");
    assert!(log.contains("llvm-profdata\tmerge"), "{log}");
    assert!(log.contains("llvm-cov\texport"), "{log}");
}
