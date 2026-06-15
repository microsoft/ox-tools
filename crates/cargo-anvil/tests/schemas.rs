// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Schema validation for emitted files.
//!
//! Each test generates the full output of `cargo anvil` for a
//! particular backend into a tempdir, then runs an external schema
//! validator (`actionlint`, `taplo`, `just`) over the relevant files.
//!
//! If the validator isn't installed, the test is skipped — never failed.
//! in cloud workflows we enforce installation via the `anvil-tools-install` recipe;
//! locally the test suite degrades gracefully.
//!
//! See `crates/cargo-anvil/docs/verification.md` for the
//! schema-validation strategy.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests favor concise assertions over Result plumbing"
)]

use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::cli::Cli;
use cargo_anvil::run::run_update;
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn empty_workspace() -> TempDir {
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

fn run_with_backend(backend: &str) -> TempDir {
    let tmp = empty_workspace();
    let args = Cli {
        backends: vec![backend.to_owned()],
        no_backends: false,
        dry_run: false,
    };
    run_update(&args, tmp.path()).unwrap();
    tmp
}

fn try_run(cmd: &mut Command) -> Option<Output> {
    match cmd.output() {
        Ok(o) => Some(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("unexpected error spawning validator: {e}"),
    }
}

#[test]
fn taplo_validates_emitted_toml_files() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("taplo");
    cmd.args(["check", "Cargo.toml", "deny.toml", "rustfmt.toml", ".delta.toml", ".anvil.lock"])
        .current_dir(tmp.path());

    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: taplo not installed");
        return;
    };
    assert!(
        out.status.success(),
        "taplo rejected emitted TOML:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn actionlint_validates_emitted_workflows() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("actionlint");
    cmd.current_dir(tmp.path());
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: actionlint not installed");
        return;
    };
    assert!(
        out.status.success(),
        "actionlint rejected emitted workflows:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn just_lists_emitted_recipes() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "--list"]).current_dir(tmp.path());
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "just --list failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    for expected in ["anvil", "anvil-pr", "anvil-pr-fast", "anvil-scheduled", "anvil-clippy"] {
        assert!(
            listing.contains(expected),
            "`just --list` did not contain recipe '{expected}':\n{listing}"
        );
    }
}

#[test]
fn ado_yaml_emitted_files_have_consistent_indent() {
    let tmp = run_with_backend("ado");
    let root = tmp.path().join(".pipelines");
    let mut count = 0_usize;
    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|s| s == "yml"))
    {
        count += 1;
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(!text.contains('\t'), "tab indentation in {}", entry.path().display());
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start_matches(' ').len();
            assert_eq!(
                indent % 2,
                0,
                "non-aligned indent ({indent} spaces) in {}: {line}",
                entry.path().display()
            );
        }
    }
    assert!(count >= 11, "expected at least 11 emitted .pipelines yml files, got {count}");
}
