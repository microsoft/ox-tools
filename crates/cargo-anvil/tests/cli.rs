// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't spawn subprocesses or do the FS ops these tests need.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! End-to-end tests that spawn the real `cargo-anvil` binary via
//! `assert_cmd`. These exercise the process-boundary glue (`main`,
//! `run`, argv parsing, backend autodetection from a real `git`
//! remote) that the in-process `run_update` tests deliberately bypass.
//!
//! Exploratory: added to measure what additional coverage spawn-based
//! integration tests contribute over the in-process suite.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

/// Write `contents` to `path`, creating parent directories as needed.
fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// A minimal two-file workspace: a virtual `[workspace]` root plus one
/// member crate. anvil synthesizes everything else.
fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    tmp
}

/// Spawn `cargo-anvil anvil <args…>` in `dir`. The leading `anvil`
/// token mimics how cargo invokes the subcommand.
fn anvil(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("cargo-anvil").expect("cargo-anvil binary should build");
    cmd.current_dir(dir).arg("anvil").args(args);
    cmd
}

/// Initialize a git repo in `dir` with the given `origin` remote URL.
fn git_init_with_origin(dir: &Path, origin: &str) {
    for args in [vec!["init"], vec!["remote", "add", "origin", origin]] {
        let status = StdCommand::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git should be on PATH");
        assert!(status.success(), "git {args:?} failed");
    }
}

#[test]
fn version_flag_prints_version() {
    let tmp = TempDir::new().unwrap();
    anvil(tmp.path(), &["--version"])
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag_succeeds_and_describes_backends() {
    let tmp = TempDir::new().unwrap();
    anvil(tmp.path(), &["--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--backend"))
        .stdout(predicates::str::contains("--no-backends"));
}

#[test]
fn invalid_backend_name_is_rejected() {
    let ws = workspace();
    anvil(ws.path(), &["--backend", "gitlab"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("gitlab"));
}

#[test]
fn dry_run_on_fresh_workspace_reports_changes_and_exits_1() {
    let ws = workspace();
    anvil(ws.path(), &["--no-backends", "--dry-run"]).assert().failure().code(1);
    // Dry-run must not have written anything.
    assert!(!ws.path().join("justfiles/anvil/mod.just").exists());
}

#[test]
fn apply_writes_files_then_dry_run_is_clean() {
    let ws = workspace();
    // First apply: writes the managed tree, exits 0.
    anvil(ws.path(), &["--no-backends"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Will create"));
    assert!(ws.path().join("justfiles/anvil/mod.just").is_file());
    assert!(ws.path().join(".anvil.lock").is_file());

    // Second dry-run against the now-up-to-date tree: no changes, exit 0.
    anvil(ws.path(), &["--no-backends", "--dry-run"]).assert().success();
}

#[test]
fn autodetect_github_backend_from_git_origin() {
    let ws = workspace();
    git_init_with_origin(ws.path(), "https://github.com/example/repo.git");
    // No --backend / --no-backends: anvil autodetects github from origin.
    // Fresh workspace ⇒ changes ⇒ dry-run exits 1, and the github
    // workflow files appear in the plan.
    anvil(ws.path(), &["--dry-run"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicates::str::contains(".github/workflows/anvil-pr.yml"));
}

#[test]
fn apply_with_autodetected_github_backend_writes_workflows() {
    let ws = workspace();
    git_init_with_origin(ws.path(), "https://github.com/example/repo.git");
    // Apply (not dry-run) so the process exits 0 via a normal `main`
    // return: the autodetect-success path is exercised AND its coverage
    // is flushed (a `--dry-run` run would `process::exit(1)` and drop the
    // profile on Windows).
    anvil(ws.path(), &[]).assert().success();
    assert!(ws.path().join(".github/workflows/anvil-pr.yml").is_file());
}

#[test]
fn autodetect_fails_for_unrecognized_origin_host() {
    let ws = workspace();
    git_init_with_origin(ws.path(), "https://gitlab.com/example/repo.git");
    anvil(ws.path(), &["--dry-run"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("autodetect"));
}

#[test]
fn autodetect_fails_without_origin_remote() {
    let ws = workspace();
    let status = StdCommand::new("git")
        .arg("init")
        .current_dir(ws.path())
        .status()
        .expect("git should be on PATH");
    assert!(status.success());
    anvil(ws.path(), &["--dry-run"]).assert().failure();
}
