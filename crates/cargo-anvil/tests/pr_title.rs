// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Coverage for the `anvil-pr-title` recipe.
//!
//! Every case here carries `#[serial]`. Each one runs the recipe, which starts a
//! `PowerShell` process, and `PowerShell` intermittently aborts while parsing an
//! assembly name during startup (PowerShell/PowerShell#26940). Concurrent
//! interpreter startups on constrained runners hit that abort often enough to
//! fail the suite, so a new case must be serialized alongside the rest.

#![cfg(not(miri))] // miri cannot sandbox the filesystem and process operations used here.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::test_support::{Cli, run_update};
use serial_test::serial;
use tempfile::TempDir;

/// Emitted by the recipe for every value that defers validation.
const SKIP_MESSAGE: &str = "anvil-pr-title: PR_TITLE env var is unset or empty; skipping title validation";

/// Opens the diagnostic the recipe emits for a title it rejects.
const REJECTION_PREFIX: &str = "PR title '";

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn emitted_workspace() -> TempDir {
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
    run_update(
        &cargo_anvil::Catalog::anvil(),
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        root,
    )
    .unwrap();
    tmp
}

fn tools_available() -> bool {
    ["just", "pwsh"]
        .iter()
        .all(|tool| Command::new(tool).arg("--version").output().is_ok())
}

fn run_title(root: &Path, title: Option<&str>) -> Output {
    let mut command = Command::new("just");
    command
        .args(["--justfile", "Justfile", "--color", "never", "anvil-pr-title"])
        .current_dir(root)
        .env_remove("PR_TITLE");
    if let Some(title) = title {
        command.env("PR_TITLE", title);
    }
    command.output().expect("guarded by tools_available() above")
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_accepted(title: &str) {
    let tmp = emitted_workspace();
    let output = run_title(tmp.path(), Some(title));
    assert!(
        output.status.success(),
        "expected '{title}' to be accepted:\n{}",
        combined_output(&output)
    );
}

fn assert_rejected(title: &str) -> String {
    let tmp = emitted_workspace();
    let output = run_title(tmp.path(), Some(title));
    let combined = combined_output(&output);
    assert!(!output.status.success(), "expected '{title}' to be rejected:\n{combined}");
    // An interpreter that fails to start also exits nonzero, so require the
    // validation diagnostic to confirm the rejection came from the recipe.
    assert!(
        combined.contains(REJECTION_PREFIX),
        "expected '{title}' to be rejected by the recipe:\n{combined}"
    );
    combined
}

fn assert_skipped(title: Option<&str>) {
    let tmp = emitted_workspace();
    let output = run_title(tmp.path(), title);
    let combined = combined_output(&output);
    assert!(output.status.success(), "expected the check to be skipped:\n{combined}");
    assert!(combined.contains(SKIP_MESSAGE), "missing skip diagnostic:\n{combined}");
}

#[test]
#[serial]
fn accepts_type_and_description() {
    if !tools_available() {
        return;
    }

    assert_accepted("feat: add validation");
}

#[test]
#[serial]
fn accepts_scope() {
    if !tools_available() {
        return;
    }

    assert_accepted("fix(parser): handle invalid input");
}

#[test]
#[serial]
fn accepts_breaking_change_marker() {
    if !tools_available() {
        return;
    }

    assert_accepted("refactor!: revise the API");
}

#[test]
#[serial]
fn accepts_scope_with_breaking_change_marker() {
    if !tools_available() {
        return;
    }

    assert_accepted("perf(runtime)!: change scheduling");
}

#[test]
#[serial]
fn accepts_uppercase_type() {
    if !tools_available() {
        return;
    }

    assert_accepted("FEAT: uppercase type");
}

#[test]
#[serial]
fn rejects_title_without_type_prefix() {
    if !tools_available() {
        return;
    }

    assert_rejected("not a conventional title");
}

#[test]
#[serial]
fn rejects_unlisted_type() {
    if !tools_available() {
        return;
    }

    assert_rejected("wip: unlisted type");
}

#[test]
#[serial]
fn rejects_empty_scope() {
    if !tools_available() {
        return;
    }

    assert_rejected("feat(): empty scope");
}

#[test]
#[serial]
fn rejection_reports_accepted_patterns_and_types() {
    if !tools_available() {
        return;
    }

    let diagnostic = assert_rejected("not a conventional title");
    for expected in [
        "PR title 'not a conventional title' does not match the accepted Conventional Commits subset.",
        "Allowed patterns:",
        "<type>: <description>",
        "<type>(<scope>): <description>",
        "<type>!: <description>",
        "<type>(<scope>)!: <description>",
        "Allowed types (case-insensitive): feat, fix, chore, docs, refactor, test, build, ci, perf, revert",
    ] {
        assert!(diagnostic.contains(expected), "missing '{expected}' from diagnostic:\n{diagnostic}");
    }
}

#[test]
#[serial]
fn skips_when_title_is_unset() {
    if !tools_available() {
        return;
    }

    assert_skipped(None);
}

#[test]
#[serial]
fn skips_when_title_is_empty() {
    if !tools_available() {
        return;
    }

    // Cloud backends publish an empty title outside a pull request context.
    assert_skipped(Some(""));
}
