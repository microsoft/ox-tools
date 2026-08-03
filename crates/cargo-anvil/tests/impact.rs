// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the git/cargo/just subprocesses these tests drive.
#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

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
        let ok = Command::new(tool).arg(args).output().map(|o| o.status.success()).unwrap_or(false);
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

/// Emit the local anvil tree into a fresh git workspace whose `origin/master`
/// remote-tracking ref points at the initial commit, leaving HEAD *exactly at
/// the base* (no post-base commit). The committed diff against the base is
/// therefore empty — cargo-delta sees no change and emits no impact JSON.
fn workspace_at_base() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // A minimal two-crate workspace cargo-delta can snapshot.
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
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
    let args = Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    };
    run_update(&cargo_anvil::Catalog::anvil(), &args, root).unwrap();

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

    tmp
}

/// Emit the local anvil tree into a fresh git workspace whose `origin/master`
/// remote-tracking ref (the base [`workspace_at_base`] sets up) points at the
/// initial commit, then advance HEAD past it with a real change so the impact
/// set is non-empty.
fn workspace() -> TempDir {
    let tmp = workspace_at_base();
    let root = tmp.path();

    // Advance HEAD past the base with a real change, so the impact set is
    // non-empty (cargo-delta emits no JSON when nothing changed).
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\npub fn feature() {}\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "feature on alpha"]);

    tmp
}

/// A `just <args>` Command rooted in the test workspace with every
/// CI-injected environment influence that would otherwise leak into the
/// recipe scrubbed:
///
/// - `ANVIL_IMPACT`: a cloud group job runs its checks under `consume`/`off`,
///   and one of those checks (the coverage/mutants suite) is what drives these
///   tests; inherited, the temp-repo recipe would no-op instead of computing.
/// - `BASE_REF` / `GITHUB_BASE_REF` / `SYSTEM_PULLREQUEST_TARGETBRANCH`: on a
///   PR build these point at the *outer* repo's base (e.g.
///   `GITHUB_BASE_REF=main`); inherited, `_anvil-base-ref` would resolve
///   `origin/main`, which does not exist in the temp repo (whose base is
///   `origin/master`), and the snapshot would fail base-ref resolution.
///
/// Tests that need a specific mode or base set the var back on the returned
/// Command.
fn just_cmd(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile"])
        .args(args)
        .env_remove("ANVIL_IMPACT")
        .env_remove("BASE_REF")
        .env_remove("GITHUB_BASE_REF")
        .env_remove("SYSTEM_PULLREQUEST_TARGETBRANCH")
        .current_dir(root);
    cmd
}

/// Run `just anvil-impact` and return combined stdout+stderr. Asserts success.
fn run_impact(root: &Path) -> String {
    let out = just_cmd(root, &["anvil-impact"]).output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
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
    assert!(
        first.contains("snapshotting baseline"),
        "first run should snapshot the baseline:\n{first}"
    );
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
    // A clean, unchanged repeat run takes the consumer fast path: both
    // snapshots are recognized as current WITHOUT re-invoking cargo-delta, and
    // the impact projection is a cache hit.
    assert!(
        noop.contains("snapshots up to date"),
        "no-op run should reuse both snapshots via the fast path:\n{noop}"
    );
    assert!(noop.contains("cache hit"), "no-op run should report an impact cache hit:\n{noop}");

    // --- 3. HEAD moves (a new commit): only `current.json` is regenerated. ---
    // A committed change advances HEAD without moving the base ref
    // (origin/master), so current.state changes while the baseline key does
    // not. The tree stays clean, so scoping is NOT widened.
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\npub fn a2() {}\n");
    git(root, &["add", "crates/alpha/src/lib.rs"]);
    git(root, &["commit", "-q", "-m", "edit alpha"]);
    let edited = run_impact(root);
    assert!(
        !edited.contains("widening"),
        "a committed change keeps the tree clean, so scoping must not widen:\n{edited}"
    );
    assert!(
        edited.contains("baseline snapshot up to date"),
        "a new commit must not move the base, so baseline is reused:\n{edited}"
    );
    assert!(
        edited.contains("snapshotting working tree"),
        "a new commit moves HEAD, so the current snapshot is regenerated:\n{edited}"
    );

    // --- 4. Base ref moves: only `baseline.json` is regenerated. ---
    // Advance origin/master to a NEW commit without moving HEAD (commit on a
    // throwaway branch, repoint the ref, return to main). The tree stays
    // clean, so `current` is untouched and only the baseline regenerates.
    let head_before = git_stdout(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "-b", "base-advance"]);
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn b2() {}\n");
    git(root, &["add", "crates/beta/src/lib.rs"]);
    git(root, &["commit", "-q", "-m", "advance base"]);
    let advanced = git_stdout(root, &["rev-parse", "HEAD"]);
    git(root, &["update-ref", "refs/remotes/origin/master", &advanced]);
    git(root, &["checkout", "-q", "main"]);
    // Sanity: HEAD is unchanged and the tree is clean.
    assert_eq!(git_stdout(root, &["rev-parse", "HEAD"]), head_before);

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
fn impact_empty_output_when_head_equals_base() {
    if !tools_available() {
        return;
    }
    // A clean checkout whose HEAD is exactly the base ref: cargo-delta sees no
    // committed diff and emits no impact JSON. The recipe must still write a
    // durable, EMPTY impact set -- `{}` to impact.json and the `--skip`
    // sentinel for every tier -- and treat an unchanged repeat run as a cache
    // hit. (The shared `workspace()` fixture always advances HEAD past the
    // base, so this empty-output path is otherwise never exercised.)
    let tmp = workspace_at_base();
    let root = tmp.path();
    let impact_dir = root.join("target/anvil/impact");

    let first = run_impact(root);
    // HEAD == base with a clean tree scopes by impact (empty), never widens.
    assert!(!first.contains("widening"), "a clean HEAD==base tree must not widen:\n{first}");
    assert_eq!(
        std::fs::read_to_string(impact_dir.join("impact.json")).unwrap().trim(),
        "{}",
        "an empty diff must persist an empty impact object so impact.json always exists"
    );
    for tier in ["modified", "affected", "required"] {
        assert_eq!(
            std::fs::read_to_string(impact_dir.join(format!("include_{tier}.txt")))
                .unwrap()
                .trim(),
            "--skip",
            "an empty impact set must project tier '{tier}' to the --skip sentinel"
        );
    }

    // Unchanged repeat run: both snapshots and the projection are a cache hit.
    let noop = run_impact(root);
    assert!(
        noop.contains("snapshots up to date"),
        "an unchanged HEAD==base rerun must reuse both snapshots via the fast path:\n{noop}"
    );
    assert!(
        noop.contains("cache hit"),
        "an unchanged rerun must report an impact cache hit:\n{noop}"
    );
}

#[test]
fn impact_off_short_circuits_without_computing() {
    if !tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();

    let out = just_cmd(root, &["anvil-impact"]).env("ANVIL_IMPACT", "off").output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "ANVIL_IMPACT=off run failed:\n{combined}");
    // No snapshotting, no projection, and -- crucially -- no artifacts written.
    assert!(!combined.contains("snapshotting"), "off run must not snapshot:\n{combined}");
    assert!(
        !root.join("target/anvil/impact/impact.json").exists(),
        "off run must not write impact artifacts"
    );
}

#[test]
fn impact_widens_to_full_workspace_when_working_tree_is_dirty() {
    if !tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();
    let impact_dir = root.join("target/anvil/impact");

    // workspace() leaves a clean tree whose only change vs the base is a
    // committed feature on `alpha`. A clean run therefore scopes by impact
    // (not the whole workspace) -- and anvil's own target/ artifacts must not
    // be mistaken for a dirty tree.
    let clean = run_impact(root);
    assert!(!clean.contains("widening"), "a clean tree must not widen:\n{clean}");
    let affected_clean = std::fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap();
    assert!(
        affected_clean.contains("alpha") && !affected_clean.contains("--workspace"),
        "clean run should scope the affected tier to the committed crate, got: {affected_clean}"
    );

    // Dirty the tree with an *uncommitted* edit to a DIFFERENT crate (beta).
    // cargo-delta only sees the committed alpha change, so without the safety
    // net beta would be silently scoped out. The recipe must widen instead.
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn wip() {}\n");
    let dirty = run_impact(root);
    assert!(
        dirty.contains("widening all tiers to --workspace"),
        "a dirty tree must widen every tier to the full workspace:\n{dirty}"
    );
    assert_eq!(
        std::fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap().trim(),
        "--workspace"
    );
    assert_eq!(
        std::fs::read_to_string(impact_dir.join("include_required.txt")).unwrap().trim(),
        "--workspace"
    );
    // modified is empty (not --skip), so its workspace-wide tools still run.
    assert_eq!(std::fs::read_to_string(impact_dir.join("include_modified.txt")).unwrap().trim(), "");

    // The warning must fire on EVERY dirty invocation, not just the first --
    // running again with the same dirty tree still warns (the dirty check runs
    // before the cache-freshness check).
    let dirty_again = run_impact(root);
    assert!(
        dirty_again.contains("widening all tiers to --workspace"),
        "a repeated dirty run must warn again, not silently reuse a cache:\n{dirty_again}"
    );

    // Committing the WIP restores impact scoping on the next run.
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "commit beta wip"]);
    let recommitted = run_impact(root);
    assert!(
        !recommitted.contains("widening"),
        "committing the WIP must restore scoping:\n{recommitted}"
    );
}

#[test]
fn impact_dirty_tree_widens_without_needing_a_resolvable_base() {
    if !tools_available() {
        return;
    }
    // Regression: the dirty-tree safety net must win even when the recompute
    // path could NOT run. A first-time / local WIP checkout can have a dirty
    // tree AND an unresolvable base ref (origin/<base> never fetched);
    // _anvil-impact-snapshot must short-circuit on the dirty tree rather than
    // fail base-ref resolution before anvil-impact's widen runs.
    let tmp = workspace();
    let root = tmp.path();

    // Uncommitted edit -> dirty tree.
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn wip() {}\n");

    // BASE_REF points at a ref that does not exist, so the recompute path
    // would hard-fail base-ref resolution. The dirty short-circuit must make
    // that unreachable.
    let out = just_cmd(root, &["anvil-impact"])
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "dirty run with an unresolvable base must still succeed:\n{combined}"
    );
    assert!(
        combined.contains("widening all tiers to --workspace"),
        "a dirty tree must widen even when the base ref is unresolvable:\n{combined}"
    );
    // The snapshot short-circuited -> cargo-delta / base ref were never needed.
    assert!(
        !combined.contains("napshotting"),
        "dirty tree must short-circuit the snapshot before recompute:\n{combined}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("target/anvil/impact/include_affected.txt"))
            .unwrap()
            .trim(),
        "--workspace"
    );
}

#[test]
fn impact_consume_mode_trusts_cache_without_recompute() {
    if !tools_available() {
        return;
    }
    // A cloud-workflow group job downloads the impact artifact, then runs its
    // checks with ANVIL_IMPACT=consume. anvil-impact must be a pure no-op that
    // trusts the present cache -- even when the fast path would NOT apply
    // (working tree changed since the cache was produced) and the base ref is
    // unresolvable and cargo-delta is unavailable, none of which a group job
    // can satisfy.
    let tmp = workspace();
    let root = tmp.path();
    run_impact(root); // the "impact job" produces the cache
    let expected = std::fs::read_to_string(root.join("target/anvil/impact/include_affected.txt"))
        .unwrap()
        .trim()
        .to_owned();

    // Perturb the tree so the fast path's current.state would NOT match.
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn later() {}\n");

    let out = just_cmd(root, &["anvil-impact"])
        .env("ANVIL_IMPACT", "consume")
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "consume run failed:\n{combined}");
    assert!(
        !combined.contains("napshotting"),
        "consume must not (re)snapshot -- cargo-delta must not be needed:\n{combined}"
    );

    // A scoped check resolves its tier scope from the downloaded cache.
    let inc = just_cmd(root, &["_anvil-impact-include", "affected"])
        .env("ANVIL_IMPACT", "consume")
        .output()
        .unwrap();
    assert!(inc.status.success());
    assert_eq!(String::from_utf8_lossy(&inc.stdout).trim(), expected);
}

#[test]
fn impact_consumer_reuses_cache_with_unresolvable_base() {
    if !tools_available() {
        return;
    }
    // Simulate a downstream cloud-workflow group job: it DOWNLOADED the impact
    // artifact (target/anvil/impact/) produced by the impact job, but its own
    // checkout installs neither cargo-delta nor fetches the base ref. The
    // consumer fast path must trust the present snapshots and no-op, rather
    // than trying to recompute (which needs cargo-delta + a resolvable base).
    let tmp = workspace();
    let root = tmp.path();
    run_impact(root); // produce the cache (the "impact job")

    // BASE_REF points at a ref that does not exist locally, standing in for a
    // shallow consumer checkout where origin/<base> was never fetched.
    let out = just_cmd(root, &["anvil-impact"])
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "consumer run with unresolvable base failed:\n{combined}");
    assert!(
        combined.contains("snapshots up to date"),
        "consumer must reuse the present snapshots:\n{combined}"
    );
    // No (re)snapshot happened -> cargo-delta was never invoked. "Snapshotting
    // workspace.." (cargo-delta) and "snapshotting ..." (the recipe) both carry
    // the substring "napshotting".
    assert!(
        !combined.contains("napshotting"),
        "consumer must not re-snapshot (cargo-delta must not be needed):\n{combined}"
    );
}

#[test]
fn impact_falls_back_to_full_workspace_when_base_has_no_workspace() {
    if !tools_available() {
        return;
    }
    // First-time anvil adoption: the base commit predates the cargo workspace
    // (no root Cargo.toml), so there is nothing for cargo-delta to snapshot at
    // the baseline. The recipe must fall back to full-workspace validation
    // rather than failing.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "anvil@example.com"]);
    git(root, &["config", "user.name", "anvil test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.safecrlf", "false"]);

    // Base commit: a repo with no cargo workspace at all.
    write(&root.join("README.md"), "pre-anvil repo\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "before anvil (no workspace)"]);
    let base = git_stdout(root, &["rev-parse", "HEAD"]);
    git(root, &["update-ref", "refs/remotes/origin/master", &base]);

    // The introducing commit: add the cargo workspace + emit the anvil tree.
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "pub fn a() {}\n");
    let args = Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    };
    run_update(&cargo_anvil::Catalog::anvil(), &args, root).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "introduce anvil"]);

    let out = run_impact(root);
    assert!(
        out.contains("baseline has no workspace") || out.contains("no root Cargo.toml"),
        "first-time-adoption run should detect the workspace-less baseline:\n{out}"
    );
    // The affected/required tiers default to --workspace (run everything),
    // and the impact set is still produced (no failure).
    let impact_dir = root.join("target/anvil/impact");
    assert_eq!(
        std::fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap().trim(),
        "--workspace"
    );
    assert_eq!(
        std::fs::read_to_string(impact_dir.join("include_required.txt")).unwrap().trim(),
        "--workspace"
    );
}

/// Write a fake `cargo` into `dir` that appends its argv (one invocation per
/// line) to `log` and exits 0, so a check recipe's tool invocation can be
/// observed without running real cargo. `dir` is meant to be prepended to
/// PATH via [`path_with_prefix`].
fn fake_cargo(dir: &Path, log: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    #[cfg(windows)]
    {
        // `.cmd` so pwsh's `& cargo` resolves it via PATHEXT before any real
        // `cargo.exe` on a later PATH entry.
        let script = format!("@echo off\r\n>>\"{}\" echo %*\r\nexit /b 0\r\n", log.display());
        std::fs::write(dir.join("cargo.cmd"), script).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n", log.display());
        let path = dir.join("cargo");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The process `PATH` with `dir` prepended, so an executable in `dir` shadows
/// the same-named tool elsewhere on PATH.
fn path_with_prefix(dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

#[test]
fn scoped_check_consumes_cached_package_list_and_skips_on_sentinel() {
    if !tools_available() {
        return;
    }
    // End-to-end proof of the shared check contract that the 25 rewritten
    // checks all use: a scoped check resolves its tier from the downloaded
    // target/anvil/impact cache, splits the `--package name@version` list, and
    // passes it to its cargo tool -- and short-circuits (never invoking the
    // tool) on the `--skip` sentinel. `anvil-examples` stands in for the
    // family; a fake `cargo` on PATH captures the argv the recipe builds. This
    // guards the PowerShell capture/splitting/short-circuit path that static
    // text-presence assertions on the emitted recipe cannot.
    let tmp = workspace();
    let root = tmp.path();
    let impact_dir = root.join("target/anvil/impact");

    // The "impact job" produces the cache; affected scopes to the committed crate.
    run_impact(root);
    let affected = std::fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap();
    let affected = affected.trim().to_owned();
    assert!(
        affected.contains("--package alpha@"),
        "precondition: the affected tier should be a scoped --package list, got: {affected}"
    );

    let bin = root.join(".fakebin");
    let log = root.join("cargo-argv.log");
    fake_cargo(&bin, &log);
    let path = path_with_prefix(&bin);

    // consume mode: anvil-impact no-ops (no snapshot / cargo-delta), so the
    // ONLY cargo invocation is the recipe's own `cargo build` -- captured by
    // the shim.
    let out = just_cmd(root, &["anvil-examples"])
        .env("ANVIL_IMPACT", "consume")
        .env("PATH", &path)
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "scoped anvil-examples run failed:\n{combined}");
    let argv = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        argv.contains("build") && argv.contains(&affected) && argv.contains("--examples"),
        "the cached --package list must reach the tool; captured argv:\n{argv}\nexpected to contain: build ... {affected} ... --examples"
    );

    // The `--skip` sentinel must short-circuit: the tool is never invoked.
    std::fs::write(impact_dir.join("include_affected.txt"), "--skip").unwrap();
    std::fs::write(&log, "").unwrap();
    let skipped = just_cmd(root, &["anvil-examples"])
        .env("ANVIL_IMPACT", "consume")
        .env("PATH", &path)
        .output()
        .unwrap();
    let skip_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&skipped.stdout),
        String::from_utf8_lossy(&skipped.stderr)
    );
    assert!(skipped.status.success(), "skipped anvil-examples run failed:\n{skip_combined}");
    let argv_skip = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !argv_skip.contains("build"),
        "the --skip sentinel must short-circuit the tool (no cargo build); captured argv:\n{argv_skip}"
    );
}

/// Write a fake `cargo` into `dir` that answers `cargo metadata …` by printing
/// the contents of `metadata_json` and captures every other invocation's argv
/// (one per line) to `log`, exiting 0. Lets `anvil-loom` be driven with a
/// synthetic workspace shape without a real loom crate.
fn fake_cargo_with_metadata(dir: &Path, log: &Path, metadata_json: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    #[cfg(windows)]
    {
        let script = format!(
            "@echo off\r\nif \"%1\"==\"metadata\" (\r\n  type \"{meta}\"\r\n  exit /b 0\r\n)\r\n>>\"{log}\" echo %*\r\nexit /b 0\r\n",
            meta = metadata_json.display(),
            log = log.display()
        );
        std::fs::write(dir.join("cargo.cmd"), script).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then cat '{meta}'; exit 0; fi\nprintf '%s\\n' \"$*\" >> '{log}'\nexit 0\n",
            meta = metadata_json.display(),
            log = log.display()
        );
        let path = dir.join("cargo");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn loom_runs_declared_targets_in_full_workspace_mode() {
    if !tools_available() {
        return;
    }
    // Regression guard for the loom `--workspace` path: `anvil-loom` parses its
    // own package set from `cargo metadata`, and a prior bug turned the
    // unscoped `--workspace` value into an empty affected set that silently
    // skipped every loom target. The representative anvil-examples/`--skip`
    // test can't catch this bespoke parsing. Here a fake `cargo metadata`
    // reports a crate with a `required-features = ["loom"]` test target; run
    // under ANVIL_IMPACT=off (so the tier resolves to --workspace), loom must
    // detect that target and invoke `cargo test -p <pkg> --test <target>`.
    let tmp = workspace_at_base();
    let root = tmp.path();

    // A synthetic single-crate workspace whose only test target requires the
    // `loom` feature -- the shape `anvil-loom` looks for.
    let metadata = root.join("metadata.json");
    std::fs::write(
        &metadata,
        r#"{"packages":[{"name":"gamma","version":"0.1.0","features":{"loom":[]},"dependencies":[],"targets":[{"kind":["test"],"name":"loomtest","required-features":["loom"]}]}]}"#,
    )
    .unwrap();

    let bin = root.join(".fakebin");
    let log = root.join("cargo-argv.log");
    fake_cargo_with_metadata(&bin, &log, &metadata);
    let path = path_with_prefix(&bin);

    // ANVIL_IMPACT=off -> anvil-impact no-ops and `_anvil-impact-include
    // affected` returns --workspace, so loom runs over the full (faked)
    // metadata rather than a scoped package list.
    let out = just_cmd(root, &["anvil-loom"])
        .env("ANVIL_IMPACT", "off")
        .env("PATH", &path)
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "anvil-loom (full-workspace) run failed:\n{combined}");
    let argv = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        argv.contains("test") && argv.contains("-p gamma") && argv.contains("--test loomtest"),
        "loom must run the declared loom target in full-workspace mode; captured argv:\n{argv}"
    );
}

#[test]
fn invalid_anvil_impact_value_fails_loudly_without_computing() {
    if !tools_available() {
        return;
    }
    // ANVIL_IMPACT is a strict tri-state (off / consume / unset). A typo like
    // `on` must fail closed at every read site with exit 2 and an actionable
    // error -- never silently fall through to "scoping on" (which could skip
    // checks) or compute a scope.
    let tmp = workspace();
    let root = tmp.path();
    for recipe in [&["anvil-impact"][..], &["_anvil-impact-include", "affected"][..]] {
        let out = just_cmd(root, recipe).env("ANVIL_IMPACT", "on").output().unwrap();
        let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            out.status.code(),
            Some(2),
            "ANVIL_IMPACT=on must exit 2 for {recipe:?}:\n{combined}"
        );
        assert!(
            combined.contains("recognized") && combined.contains("consume"),
            "the error must be actionable for {recipe:?}:\n{combined}"
        );
    }
    assert!(
        !root.join("target/anvil/impact/impact.json").exists(),
        "an invalid mode must not compute or write impact artifacts"
    );
}

#[test]
fn missing_base_ref_on_clean_tree_fails_with_fetch_guidance() {
    if !tools_available() {
        return;
    }
    // A clean tree does NOT hit the dirty-tree widen short-circuit, so a first
    // run must resolve the base ref. When it is unresolvable (origin/<base>
    // never fetched), the recompute path must fail loudly with actionable
    // `git fetch` guidance rather than continue on an invalid baseline or
    // mutate git state.
    let tmp = workspace();
    let root = tmp.path();
    let out = just_cmd(root, &["anvil-impact"])
        .env("BASE_REF", "refs/heads/anvil-nonexistent-base")
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_ne!(
        out.status.code(),
        Some(0),
        "a clean tree with an unresolvable base must fail:\n{combined}"
    );
    assert!(
        combined.contains("locally") && combined.contains("origin"),
        "the error must point at `git fetch origin`:\n{combined}"
    );
    assert!(
        !root.join("target/anvil/impact/impact.json").exists(),
        "a failed base resolution must not write an impact set"
    );
}

#[test]
fn shallow_clone_fails_with_unshallow_guidance() {
    if !tools_available() {
        return;
    }
    // cargo-delta needs full history for the base ref, so a shallow checkout
    // must fail loudly with `git fetch --unshallow` guidance rather than
    // compute against a truncated history.
    let tmp = workspace();
    let root = tmp.path();
    // git treats the presence of `.git/shallow` (listing a boundary commit) as
    // a shallow clone, which is what `git rev-parse --is-shallow-repository`
    // keys on.
    let head = git_stdout(root, &["rev-parse", "HEAD"]);
    write(&root.join(".git/shallow"), &format!("{head}\n"));

    let out = just_cmd(root, &["anvil-impact"]).output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_ne!(out.status.code(), Some(0), "a shallow clone must fail:\n{combined}");
    assert!(
        combined.contains("shallow") && combined.contains("--unshallow"),
        "the error must point at `git fetch --unshallow`:\n{combined}"
    );
}

#[test]
fn consume_without_downloaded_cache_fails_loudly() {
    if !tools_available() {
        return;
    }
    // consume trusts a downloaded cache verbatim, so a missing cache is a real
    // pipeline defect (renamed artifact, a missing download step, or a group
    // action run directly). It must fail loudly rather than let every scoped
    // check silently widen to --workspace while the pipeline stays green.
    let tmp = workspace();
    let root = tmp.path();
    // No target/anvil/impact/ cache has been downloaded.
    let out = just_cmd(root, &["anvil-impact"]).env("ANVIL_IMPACT", "consume").output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_ne!(
        out.status.code(),
        Some(0),
        "consume with no downloaded cache must fail:\n{combined}"
    );
    assert!(
        combined.contains("consume") && combined.contains("missing"),
        "the error must explain the missing consume cache:\n{combined}"
    );

    // With the cache present (as after a real artifact download), consume is a
    // successful no-op.
    run_impact(root);
    let ok = just_cmd(root, &["anvil-impact"]).env("ANVIL_IMPACT", "consume").output().unwrap();
    let ok_combined = format!("{}{}", String::from_utf8_lossy(&ok.stdout), String::from_utf8_lossy(&ok.stderr));
    assert!(
        ok.status.success(),
        "consume with a present cache must succeed as a no-op:\n{ok_combined}"
    );

    // A partially downloaded cache -- one tier's include file missing -- must
    // also fail loudly and name the missing tier. This guards the
    // all-three-files contract against regressing to a directory or
    // representative-file check, which would let one tier silently fall back to
    // its default.
    std::fs::remove_file(root.join("target/anvil/impact/include_affected.txt")).unwrap();
    let partial = just_cmd(root, &["anvil-impact"]).env("ANVIL_IMPACT", "consume").output().unwrap();
    let partial_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&partial.stdout),
        String::from_utf8_lossy(&partial.stderr)
    );
    assert_ne!(
        partial.status.code(),
        Some(0),
        "consume with a partial cache must fail:\n{partial_combined}"
    );
    assert!(
        partial_combined.contains("affected"),
        "the error must name the missing tier (affected):\n{partial_combined}"
    );
}
