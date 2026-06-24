// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the git/cargo/just subprocesses these tests drive.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    reason = "integration tests favor concise assertions over Result plumbing"
)]

//! Behavioral tests for the `anvil-impact` recipe's two-key cache.
//!
//! These exercise the *runtime* behavior of the emitted `impact.just`
//! recipe (not just its emitted text): the expensive base-ref snapshot
//! (`baseline.json`, keyed on the base commit sha) is regenerated only
//! when the base moves, the cheap working-tree snapshot (`current.json`,
//! keyed on `HEAD + worktree-diff`) is regenerated only when the tree
//! changes, and a no-op invocation reuses both.
//!
//! The recipe prints a distinct line for each path -- "snapshotting
//! baseline" / "baseline snapshot up to date" and "snapshotting working
//! tree" / "current snapshot up to date" -- so the tests assert on those
//! markers rather than on file mtimes (which artifact upload/download
//! doesn't preserve, and which the cache deliberately does not key on).
//!
//! The test drives real `git`, `cargo`, `cargo-delta`, `just`, and `pwsh`
//! subprocesses; if any is missing it is skipped, never failed (matching
//! the schema-validation tests). See `docs/verification.md`.

use std::path::Path;
use std::process::Command;

use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Returns true if every external tool the recipe needs is on PATH.
fn tools_available() -> bool {
    for (tool, args) in [
        ("git", "--version"),
        ("cargo", "--version"),
        ("cargo-delta", "--version"),
        ("just", "--version"),
        ("pwsh", "--version"),
    ] {
        let ok = Command::new(tool)
            .arg(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping: required tool '{tool}' not available");
            return false;
        }
    }
    true
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Emit the local anvil tree into a fresh git workspace whose `origin/main`
/// remote-tracking ref points at the initial commit.
fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // A minimal two-crate workspace cargo-delta can snapshot.
    write(&root.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n");
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\n");
    write(
        &root.join("crates/beta/Cargo.toml"),
        "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\n");

    // Emit the anvil recipe tree (Justfile import region + justfiles/anvil/*).
    let args = Cli { backends: vec![], no_backends: true, dry_run: false, force: false };
    run_update(&cargo_anvil::Catalog::anvil(), &args, root).unwrap();

    // Initialize git and create the base ref the recipe resolves to
    // (origin/main) as a remote-tracking ref pointing at the first commit.
    // Initialize git, commit the base, and record it as origin/master (the
    // base ref the recipe and cargo-delta both resolve to in this bare repo).
    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "anvil@example.com"]);
    git(root, &["config", "user.name", "anvil test"]);
    // Neutralize any inherited global autocrlf/safecrlf so `git add` doesn't
    // reject the LF-normalized emitted files on Windows dev machines.
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.safecrlf", "false"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git_stdout(root, &["rev-parse", "HEAD"]);
    // Use origin/master as the base ref: cargo-delta's `impact` subcommand
    // defaults to origin/master when no `origin` remote is configured (as in
    // this bare test repo), and the recipe's base-ref resolution falls through
    // origin/main -> origin/master, so both agree on the same base.
    git(root, &["update-ref", "refs/remotes/origin/master", &base]);

    // Advance HEAD past the base with a real change, so the impact set is
    // non-empty (cargo-delta emits no JSON when nothing changed).
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\npub fn feature() {}\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "feature on alpha"]);

    tmp
}

/// Run `just anvil-impact` and return combined stdout+stderr. Asserts success.
fn run_impact(root: &Path) -> String {
    let out = Command::new("just")
        .args(["--justfile", "Justfile", "anvil-impact"])
        .current_dir(root)
        .output()
        .unwrap();
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "just anvil-impact failed:\n{combined}");
    combined
}

#[test]
fn impact_cache_regenerates_per_key_and_reuses_when_unchanged() {
    if !tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();
    let impact_dir = root.join("target/anvil/impact");

    // --- 1. First run: both snapshots are produced from scratch. ---
    let first = run_impact(root);
    assert!(first.contains("snapshotting baseline"), "first run should snapshot the baseline:\n{first}");
    assert!(
        first.contains("snapshotting working tree"),
        "first run should snapshot the working tree:\n{first}"
    );
    // The durable artifacts exist.
    for f in [
        "snapshots/baseline.json",
        "snapshots/baseline.sha",
        "snapshots/current.json",
        "snapshots/current.state",
        "impact.json",
        "include_modified.txt",
        "include_affected.txt",
        "include_required.txt",
    ] {
        assert!(impact_dir.join(f).exists(), "missing impact artifact: {f}");
    }

    // --- 2. No change: both snapshots are reused (cache hit). ---
    let noop = run_impact(root);
    assert!(noop.contains("baseline snapshot up to date"), "no-op run should reuse baseline:\n{noop}");
    assert!(noop.contains("current snapshot up to date"), "no-op run should reuse current:\n{noop}");
    assert!(noop.contains("cache hit"), "no-op run should report an impact cache hit:\n{noop}");

    // --- 3. Working tree changes: only `current.json` is regenerated. ---
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\npub fn a2() {}\n");
    let edited = run_impact(root);
    assert!(
        edited.contains("baseline snapshot up to date"),
        "an uncommitted edit must not move the base, so baseline is reused:\n{edited}"
    );
    assert!(
        edited.contains("snapshotting working tree"),
        "an uncommitted edit must regenerate the current snapshot:\n{edited}"
    );

    // --- 4. Base ref moves: only `baseline.json` is regenerated. ---
    // Advance origin/master to a NEW commit without moving HEAD: commit on a
    // throwaway branch, repoint origin/master, then restore the
    // working tree to its prior (edited) state so `current` is unaffected.
    let edited_lib = std::fs::read_to_string(root.join("crates/alpha/src/lib.rs")).unwrap();
    let head_before = git_stdout(root, &["rev-parse", "HEAD"]);
    git(root, &["stash", "push", "-q", "--include-untracked"]);
    git(root, &["checkout", "-q", "-b", "base-advance"]);
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn b2() {}\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "advance base"]);
    let advanced = git_stdout(root, &["rev-parse", "HEAD"]);
    git(root, &["update-ref", "refs/remotes/origin/master", &advanced]);
    git(root, &["checkout", "-q", "main"]);
    git(root, &["stash", "pop", "-q"]);
    // Sanity: HEAD is unchanged; the working-tree edit is restored.
    assert_eq!(git_stdout(root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(std::fs::read_to_string(root.join("crates/alpha/src/lib.rs")).unwrap(), edited_lib);

    let base_moved = run_impact(root);
    assert!(
        base_moved.contains("snapshotting baseline"),
        "moving origin/master must regenerate the baseline snapshot:\n{base_moved}"
    );
    assert!(
        base_moved.contains("current snapshot up to date"),
        "moving the base must not touch the (unchanged) working-tree snapshot:\n{base_moved}"
    );
}

#[test]
fn impact_off_short_circuits_without_computing() {
    if !tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();

    let out = Command::new("just")
        .args(["--justfile", "Justfile", "anvil-impact"])
        .env("ANVIL_IMPACT", "off")
        .current_dir(root)
        .output()
        .unwrap();
    let combined =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "ANVIL_IMPACT=off run failed:\n{combined}");
    // No snapshotting, no projection, and -- crucially -- no artifacts written.
    assert!(!combined.contains("snapshotting"), "off run must not snapshot:\n{combined}");
    assert!(
        !root.join("target/anvil/impact/impact.json").exists(),
        "off run must not write impact artifacts"
    );
}
