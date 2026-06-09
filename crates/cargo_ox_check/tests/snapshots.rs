// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Snapshot tests for the full emitted file tree.
//!
//! For a small set of representative input combinations, run
//! `cargo ox-check update` against a bare-workspace tempdir, collect
//! every file ox-check produced (sorted by path), and snapshot the
//! whole tree as one string. Snapshots live under `tests/snapshots/`
//! and are reviewed via `cargo insta review`.
//!
//! Coverage rationale: the imperative tests in `src/run.rs` pin the
//! algorithm (which decisions are taken, which paths exist); these
//! snapshot tests pin the *byte-exact emitted content* so template
//! edits surface as reviewable diffs. The two layers are
//! complementary — neither subsumes the other.

#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

use std::path::{Path, PathBuf};

use cargo_ox_check::cli::UpdateArgs;
use cargo_ox_check::manifest::MANIFEST_FILE_NAME;
use cargo_ox_check::run::run_update;
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Bare workspace fixture: one root manifest + one member crate, with
/// nothing else in the tree. Everything ox-check produces is therefore
/// strictly its own output.
fn bare_workspace() -> TempDir {
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

/// Walk the workspace, collect every file produced or modified by
/// ox-check, and render them into a single deterministic string.
///
/// The manifest (`.ox-check.lock`) is filtered out: it carries the
/// `rendered_by` version which would churn on every crate-version bump,
/// drowning the actual content review in noise. The schema-validation
/// test suite already asserts the manifest is valid TOML.
fn render_tree(root: &Path) -> String {
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some(MANIFEST_FILE_NAME))
        .collect();
    paths.sort();

    let mut out = String::new();
    for path in paths {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let body = std::fs::read_to_string(&path).unwrap();
        out.push_str("=== ");
        out.push_str(&rel);
        out.push_str(" ===\n");
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn run(args: &UpdateArgs, tmp: &TempDir) {
    run_update(args, tmp.path()).unwrap();
}

#[test]
fn local_only_tree() {
    let tmp = bare_workspace();
    run(
        &UpdateArgs {
            backends: vec![],
            no_backends: true,
            dry_run: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("local_only", render_tree(tmp.path()));
}

#[test]
fn github_backend_tree() {
    let tmp = bare_workspace();
    run(
        &UpdateArgs {
            backends: vec!["github".to_owned()],
            no_backends: false,
            dry_run: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("github_backend", render_tree(tmp.path()));
}

#[test]
fn ado_backend_tree() {
    let tmp = bare_workspace();
    run(
        &UpdateArgs {
            backends: vec!["ado".to_owned()],
            no_backends: false,
            dry_run: false,
        },
        &tmp,
    );
    insta::assert_snapshot!("ado_backend", render_tree(tmp.path()));
}
