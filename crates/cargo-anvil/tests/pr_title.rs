// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri cannot sandbox the filesystem and process operations used here.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

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

#[test]
fn pr_title_recipe_reports_and_accepts_allowed_patterns() {
    if !tools_available() {
        return;
    }

    let tmp = emitted_workspace();
    let invalid = run_title(tmp.path(), Some("not a conventional title"));
    assert!(!invalid.status.success());

    let diagnostic = combined_output(&invalid);
    for expected in [
        "PR title 'not a conventional title' does not match Conventional Commits.",
        "Allowed patterns:",
        "<type>: <description>",
        "<type>(<scope>): <description>",
        "<type>!: <description>",
        "<type>(<scope>)!: <description>",
        "Allowed types (case-insensitive): feat, fix, chore, docs, refactor, test, build, ci, perf, revert",
    ] {
        assert!(diagnostic.contains(expected), "missing '{expected}' from diagnostic:\n{diagnostic}");
    }

    for title in [
        "feat: add validation",
        "fix(parser): handle invalid input",
        "refactor!: revise the API",
        "perf(runtime)!: change scheduling",
        "FEAT: uppercase type",
    ] {
        let output = run_title(tmp.path(), Some(title));
        assert!(
            output.status.success(),
            "expected '{title}' to be accepted:\n{}",
            combined_output(&output)
        );
    }

    for title in ["wip: unlisted type", "feat(): empty scope"] {
        let output = run_title(tmp.path(), Some(title));
        assert!(
            !output.status.success(),
            "expected '{title}' to be rejected:\n{}",
            combined_output(&output)
        );
    }

    let skipped = run_title(tmp.path(), None);
    assert!(skipped.status.success());
    assert!(combined_output(&skipped).contains("anvil-pr-title: PR_TITLE env var is unset or empty; skipping title validation"));

    // Azure DevOps publishes an empty PR_TITLE on non-PR builds and API failures.
    let empty = run_title(tmp.path(), Some(""));
    assert!(empty.status.success());
    assert!(combined_output(&empty).contains("anvil-pr-title: PR_TITLE env var is unset or empty; skipping title validation"));
}
