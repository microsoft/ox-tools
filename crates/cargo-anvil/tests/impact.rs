// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the git/cargo/just subprocesses these tests drive.
#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

//! Behavioral tests for the `anvil-impact` recipe's two-key cache.
//!
//! These exercise the *runtime* behavior of the emitted `impact.just`
//! recipe (not just its emitted text): the expensive base-ref snapshot
//! (`baseline.json`, keyed on the *composite* of the base commit sha and the
//! effective `.delta.toml` identity) is regenerated when the base moves *or*
//! the cargo-delta config changes, the cheap working-tree snapshot
//! (`current.json`, keyed on the HEAD sha) is regenerated only when HEAD
//! moves, and a no-op invocation reuses both. (The config half of the
//! baseline key is what `baseline_regenerates_when_delta_config_changes_without_moving_the_base`
//! pins -- it stops a warm cache from diffing snapshots taken under different
//! cargo-delta rules.)
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

use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
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
        let ok = Command::new(tool).arg(args).output().is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipping: required tool '{tool}' not available");
            return false;
        }
    }
    true
}

/// Returns true if the tools the *cargo-delta-free* recipe paths need are on
/// PATH (git, just, pwsh). Deliberately omits cargo/cargo-delta: the paths this
/// gates -- `ANVIL_IMPACT=off`, an invalid mode, a dirty-tree widen, and
/// `consume` -- must reach their decision WITHOUT ever invoking cargo-delta.
/// Requiring it here would both wrongly skip these tests on a machine that
/// lacks it and mask a regression that starts shelling out to it on a path that
/// must not. The tests that use this gate run under [`ShimBin::tripwire_cargo`],
/// which turns any cargo invocation into a hard failure + a logged entry.
fn core_tools_available() -> bool {
    for (tool, arg) in [("git", "--version"), ("just", "--version"), ("pwsh", "--version")] {
        let ok = Command::new(tool).arg(arg).output().is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipping: required tool '{tool}' not available");
            return false;
        }
    }
    true
}

/// A scratch directory prepended to `PATH` that holds pwsh command shims
/// (`<name>.ps1`) used to make the recipe's `cargo` / `rustup` calls observable
/// -- or to prove they never happen. pwsh resolves a bare `cargo`/`rustup` to
/// the `.ps1` on PATH cross-platform, so the emitted recipes call the shim
/// instead of the real tool. Kept in its own `TempDir` (not the workspace) so
/// the shims never surface in the workspace's `git status` -- which the recipe's
/// dirty-tree probe would otherwise read as an uncommitted change.
///
/// Every shim logs to the file named by the `ANVIL_TEST_LOG` env var; tests set
/// that var to [`ShimBin::log`] on the command they run.
struct ShimBin {
    _dir: TempDir,
    path: OsString,
    log: PathBuf,
}

impl ShimBin {
    fn new(scripts: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let log = dir.path().join("shim.log");
        for (name, body) in scripts {
            write(&dir.path().join(name), body);
        }
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        let path = env::join_paths(paths).unwrap();
        Self { _dir: dir, path, log }
    }

    /// A `cargo` shim that records every invocation to `ANVIL_TEST_LOG` and then
    /// fails hard (exit 97). Used to assert a recipe path never shells out to
    /// cargo-delta: any call both fails the recipe and leaves a log entry.
    ///
    /// 97 is an arbitrary recognizable test-only sentinel: no assertion checks
    /// for it specifically (callers verify the log is empty / the recipe
    /// succeeded), so any nonzero value would do -- it is chosen to be visually
    /// distinct from the recipe contract exit codes (1/2) so an unexpected
    /// failure surfacing this code is obviously the tripwire firing.
    fn tripwire_cargo() -> Self {
        Self::new(&[(
            "cargo.ps1",
            "if ($env:ANVIL_TEST_LOG) { Add-Content -LiteralPath $env:ANVIL_TEST_LOG -Value ($args -join ' ') }\n\
             [Console]::Error.WriteLine('cargo tripwire: unexpected cargo invocation: ' + ($args -join ' '))\n\
             exit 97\n",
        )])
    }

    /// The lines logged by the shims so far (empty when nothing was invoked).
    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
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
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.97\"\n",
    );
    write(
        &root.join("Justfile"),
        "set windows-shell := [\"pwsh\", \"-NoProfile\", \"-Command\"]\n",
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
    run_update(&Catalog::anvil(), &args, root).unwrap();

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
        "snapshots/baseline.key",
        "snapshots/current.json",
        "snapshots/current.key",
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
    // (origin/master), so current.key changes while the baseline key does
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
fn baseline_regenerates_when_delta_config_changes_without_moving_the_base() {
    if !tools_available() {
        return;
    }
    // Regression: the baseline cache must be keyed on the *effective cargo-delta
    // config identity* as well as the base sha. cargo-delta snapshots capture
    // WHAT they capture according to `.delta.toml` (file_exclude / [parser] /
    // assume_patterns / ...). If the baseline were keyed on the base sha alone,
    // a committed `.delta.toml` edit -- which moves HEAD but NOT the base ref --
    // would regenerate the *current* snapshot under the new config while the
    // warm baseline stayed under the OLD config, and `cargo delta impact` would
    // then diff two snapshots taken under different rules and silently mis-scope.
    let tmp = workspace();
    let root = tmp.path();

    // Commit an initial `.delta.toml`. It must be committed (not left in the
    // working tree) or the dirty-tree guard would widen and skip snapshotting.
    let config = root.join(".delta.toml");
    write(&config, "file_exclude_patterns = [\"target\", \"*.md\"]\n");
    git(root, &["add", ".delta.toml"]);
    git(root, &["commit", "-q", "-m", "add delta config"]);

    // First run captures the baseline under config v1.
    let first = run_impact(root);
    assert!(
        first.contains("snapshotting baseline"),
        "first run with a config present should snapshot the baseline:\n{first}"
    );

    // Sanity: the base ref is genuinely NOT moving across the config edit, so a
    // sha-only key would treat the stale baseline as fresh.
    let base_before = git_stdout(root, &["rev-parse", "origin/master"]);

    // Change the config and commit it. This advances HEAD (so `current`
    // regenerates) but leaves origin/master where it was.
    write(&config, "file_exclude_patterns = [\"target\", \"*.md\", \"*.png\"]\n");
    git(root, &["add", ".delta.toml"]);
    git(root, &["commit", "-q", "-m", "change delta config"]);
    assert_eq!(
        git_stdout(root, &["rev-parse", "origin/master"]),
        base_before,
        "the base ref must not move across the config edit (that is the whole point)"
    );

    // The config change alone must invalidate the baseline: without the
    // composite key this would report "baseline snapshot up to date".
    let after = run_impact(root);
    assert!(
        after.contains("snapshotting baseline"),
        "a changed `.delta.toml` must regenerate the baseline even though the base ref did not move:\n{after}"
    );
    assert!(
        !after.contains("baseline snapshot up to date"),
        "the stale (old-config) baseline must NOT be reused after a config change:\n{after}"
    );
}

#[test]
fn current_snapshot_ignores_tracked_target_changes_without_moving_head() {
    if !tools_available() {
        return;
    }
    // The current-snapshot key is the HEAD sha ALONE -- deliberately NOT a hash
    // of `git diff HEAD`, so that tracked content under `target/` (where the
    // impact cache itself lives) can never perturb it. This pins that contract:
    // a tracked `target/` file modified WITHOUT moving HEAD must be a full cache
    // hit. A `git diff`-based key would regenerate the current snapshot here;
    // the HEAD-only key must not. (The dirty-tree guard also excludes `target/`,
    // so the modification does not widen either.)
    let tmp = workspace();
    let root = tmp.path();

    // Force-track a file under `target/` (target/ is git-ignored in the emitted
    // workspace, hence `-f`). Committing it advances HEAD so the first run's
    // cache corresponds exactly to this commit.
    let tracked = root.join("target/tracked.marker");
    write(&tracked, "one\n");
    git(root, &["add", "-f", "target/tracked.marker"]);
    git(root, &["commit", "-q", "-m", "track a target/ file"]);

    // First run populates the cache at this HEAD.
    let first = run_impact(root);
    assert!(
        first.contains("snapshotting working tree"),
        "first run should snapshot the working tree:\n{first}"
    );
    let head_before = git_stdout(root, &["rev-parse", "HEAD"]);

    // Modify ONLY the tracked target/ file, without committing: HEAD does not
    // move and the change is confined to target/.
    write(&tracked, "one\ntwo\n");
    assert_eq!(
        git_stdout(root, &["rev-parse", "HEAD"]),
        head_before,
        "modifying a working-tree file must not move HEAD"
    );

    let after = run_impact(root);
    assert!(
        !after.contains("widening"),
        "a tracked target/ change is excluded by the dirty guard, so scoping must not widen:\n{after}"
    );
    assert!(
        after.contains("snapshots up to date"),
        "a tracked target/ change must not invalidate the HEAD-keyed current snapshot:\n{after}"
    );
    assert!(
        !after.contains("snapshotting working tree"),
        "the current snapshot must be reused, NOT regenerated, for a target/-only change:\n{after}"
    );
    assert!(
        after.contains("cache hit"),
        "the impact projection must be a cache hit when nothing outside target/ moved:\n{after}"
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
        fs::read_to_string(impact_dir.join("impact.json")).unwrap().trim(),
        "{}",
        "an empty diff must persist an empty impact object so impact.json always exists"
    );
    for tier in ["modified", "affected", "required"] {
        assert_eq!(
            fs::read_to_string(impact_dir.join(format!("include_{tier}.txt"))).unwrap().trim(),
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
    if !core_tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();

    // ANVIL_IMPACT=off must reach its no-op WITHOUT cargo-delta: run under a
    // tripwire cargo shim so any invocation fails loudly and is logged.
    let shim = ShimBin::tripwire_cargo();
    let out = just_cmd(root, &["anvil-impact"])
        .env("ANVIL_IMPACT", "off")
        .env("PATH", &shim.path)
        .env("ANVIL_TEST_LOG", &shim.log)
        .output()
        .unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "ANVIL_IMPACT=off run failed:\n{combined}");
    // No snapshotting, no projection, and -- crucially -- no artifacts written.
    assert!(!combined.contains("snapshotting"), "off run must not snapshot:\n{combined}");
    assert!(
        !root.join("target/anvil/impact/impact.json").exists(),
        "off run must not write impact artifacts"
    );
    assert!(
        shim.log_lines().is_empty(),
        "off run must not invoke cargo-delta, but did: {:?}",
        shim.log_lines()
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
    let affected_clean = fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap();
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
        fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap().trim(),
        "--workspace"
    );
    assert_eq!(
        fs::read_to_string(impact_dir.join("include_required.txt")).unwrap().trim(),
        "--workspace"
    );
    // modified is empty (not --skip), so its workspace-wide tools still run.
    assert_eq!(fs::read_to_string(impact_dir.join("include_modified.txt")).unwrap().trim(), "");

    // The warning must fire on EVERY dirty invocation, not just the first --
    // running again with the same dirty tree still warns (the dirty check runs
    // before the cache-freshness check).
    let dirty_again = run_impact(root);
    assert!(
        dirty_again.contains("widening all tiers to --workspace"),
        "a repeated dirty run must warn again, not silently reuse a cache:\n{dirty_again}"
    );

    // Committing the change restores impact scoping on the next run.
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "commit beta wip"]);
    let recommitted = run_impact(root);
    assert!(
        !recommitted.contains("widening"),
        "committing the uncommitted change must restore scoping:\n{recommitted}"
    );
    // Scoping is not just un-widened -- the newly committed crate must actually
    // land in the affected tier (guards against a regression that drops it or
    // widens without printing the warning).
    let affected_after = fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap();
    assert!(
        affected_after.contains("--package beta@"),
        "committing the uncommitted change must scope beta into the affected tier, got: {affected_after}"
    );
}

#[test]
fn impact_include_reads_zero_byte_modified_file_without_throwing() {
    if !tools_available() {
        return;
    }
    // The dirty-tree widen writes include_modified.txt as a 0-byte file
    // (`-Value '' -NoNewline`). `Get-Content -Raw` returns $null for a 0-byte
    // file, so `_anvil-impact-include` must read it null-safely rather than
    // throwing on `$null.Trim()` -- which every modified-tier check hits on any
    // dirty local run.
    let tmp = workspace();
    let root = tmp.path();
    // Uncommitted edit -> anvil-impact widens and writes the 0-byte include.
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn wip() {}\n");
    run_impact(root);
    let modified_file = root.join("target/anvil/impact/include_modified.txt");
    assert_eq!(
        fs::metadata(&modified_file).unwrap().len(),
        0,
        "the dirty widen must write a 0-byte include_modified.txt for this test to be meaningful"
    );

    let out = just_cmd(root, &["_anvil-impact-include", "modified"]).output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "_anvil-impact-include must not throw on a 0-byte include file:\n{combined}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "",
        "an empty modified tier must resolve to '' (run), not crash"
    );
}

#[test]
fn impact_dirty_tree_widens_without_needing_a_resolvable_base() {
    if !core_tools_available() {
        return;
    }
    // Regression: the dirty-tree safety net must win even when the recompute
    // path could NOT run. A first-time or local checkout can have a dirty
    // working tree AND an unresolvable base ref (origin/<base> never fetched);
    // _anvil-impact-snapshot must short-circuit on the dirty tree rather than
    // fail base-ref resolution before anvil-impact's widen runs.
    let tmp = workspace();
    let root = tmp.path();

    // Uncommitted edit -> dirty tree.
    write(&root.join("crates/beta/src/lib.rs"), "pub fn b() {}\npub fn wip() {}\n");

    // BASE_REF points at a ref that does not exist, so the recompute path
    // would hard-fail base-ref resolution. The dirty short-circuit must make
    // that unreachable -- and reach the widen WITHOUT cargo-delta, which a
    // first-time checkout with uncommitted changes may not even have installed.
    // The tripwire cargo shim proves
    // the widen path never shells out to it.
    let shim = ShimBin::tripwire_cargo();
    let out = just_cmd(root, &["anvil-impact"])
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .env("PATH", &shim.path)
        .env("ANVIL_TEST_LOG", &shim.log)
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
    assert!(
        shim.log_lines().is_empty(),
        "a dirty-tree widen must not invoke cargo-delta, but did: {:?}",
        shim.log_lines()
    );
    assert_eq!(
        fs::read_to_string(root.join("target/anvil/impact/include_affected.txt"))
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
    let expected = fs::read_to_string(root.join("target/anvil/impact/include_affected.txt"))
        .unwrap()
        .trim()
        .to_owned();

    // Perturb the tree (uncommitted) so the cache no longer reflects the
    // working tree. In normal mode this dirty tree would widen; consume must
    // still trust the downloaded cache verbatim without re-snapshotting.
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
fn consumer_reuses_cache_with_unresolvable_base_only_under_consume() {
    if !tools_available() {
        return;
    }
    // A downstream cloud-workflow group job downloaded the impact artifact but
    // its checkout neither installs cargo-delta nor fetches the base ref. The
    // supported way to reuse that cache is ANVIL_IMPACT=consume, which trusts
    // the present cache verbatim. In normal (unset) mode an unresolvable base
    // must NOT be trusted: trusting a present-but-possibly-stale baseline is the
    // fail-open direction, so it falls through to recompute and fails fast.
    let tmp = workspace();
    let root = tmp.path();
    run_impact(root); // produce the cache (the "impact job")

    // consume: trusts the present cache with an unresolvable base, no recompute.
    let consume = just_cmd(root, &["anvil-impact"])
        .env("ANVIL_IMPACT", "consume")
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .output()
        .unwrap();
    let consume_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&consume.stdout),
        String::from_utf8_lossy(&consume.stderr)
    );
    assert!(
        consume.status.success(),
        "consume must reuse the present cache with an unresolvable base:\n{consume_combined}"
    );
    assert!(
        !consume_combined.contains("napshotting"),
        "consume must not re-snapshot (cargo-delta must not be needed):\n{consume_combined}"
    );

    // normal (unset) mode: the same unresolvable base must NOT silently trust
    // the present baseline -- it recomputes and fails fast with fetch guidance.
    let normal = just_cmd(root, &["anvil-impact"])
        .env("BASE_REF", "refs/heads/anvil-does-not-exist")
        .output()
        .unwrap();
    let normal_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&normal.stdout),
        String::from_utf8_lossy(&normal.stderr)
    );
    assert_ne!(
        normal.status.code(),
        Some(0),
        "normal mode must not trust an unresolvable base:\n{normal_combined}"
    );
    assert!(
        normal_combined.contains("locally") && normal_combined.contains("origin"),
        "normal-mode failure must give git fetch guidance:\n{normal_combined}"
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
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.97\"\n",
    );
    write(
        &root.join("Justfile"),
        "set unstable\nset windows-shell := [\"pwsh\", \"-NoProfile\", \"-Command\"]\n",
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
    run_update(&Catalog::anvil(), &args, root).unwrap();
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
        fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap().trim(),
        "--workspace"
    );
    assert_eq!(
        fs::read_to_string(impact_dir.join("include_required.txt")).unwrap().trim(),
        "--workspace"
    );
}

/// Write a fake `cargo` into `dir` that appends its argv (one invocation per
/// line) to `log` and exits 0, so a check recipe's tool invocation can be
/// observed without running real cargo. `dir` is meant to be prepended to
/// PATH via [`path_with_prefix`].
fn fake_cargo(dir: &Path, log: &Path) {
    fs::create_dir_all(dir).unwrap();
    #[cfg(windows)]
    {
        // Script shims receive the inline argument-array expression as one
        // nested array, whereas native Cargo flattens it. Normalize the shim
        // input before recording the native-process argv contract.
        let script = format!(
            "$effectiveArgs = @()\n\
             foreach ($argument in $args) {{\n\
             \x20   if ($argument -is [Array]) {{ $effectiveArgs += @($argument) }} else {{ $effectiveArgs += $argument }}\n\
             }}\n\
             Add-Content -LiteralPath '{}' -Value ($effectiveArgs -join ' ')\n\
             exit 0\n",
            log.display()
        );
        fs::write(dir.join("cargo.ps1"), script).unwrap();
    }
    #[cfg(unix)]
    {
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n", log.display());
        let path = dir.join("cargo");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The process `PATH` with `dir` prepended, so an executable in `dir` shadows
/// the same-named tool elsewhere on PATH.
fn path_with_prefix(dir: &Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap()
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
    let affected = fs::read_to_string(impact_dir.join("include_affected.txt")).unwrap();
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
    let argv = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        argv.contains("build") && argv.contains(&affected) && argv.contains("--examples"),
        "the cached --package list must reach the tool; captured argv:\n{argv}\nexpected to contain: build ... {affected} ... --examples"
    );

    // The `--skip` sentinel must short-circuit: the tool is never invoked.
    fs::write(impact_dir.join("include_affected.txt"), "--skip").unwrap();
    fs::write(&log, "").unwrap();
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
    let argv_skip = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !argv_skip.contains("build"),
        "the --skip sentinel must short-circuit the tool (no cargo build); captured argv:\n{argv_skip}"
    );
}

#[test]
fn msrv_test_uses_affected_packages_for_both_feature_modes_and_skips_without_msrv() {
    if !tools_available() {
        return;
    }
    let tmp = workspace();
    let root = tmp.path();
    run_impact(root);
    let affected = fs::read_to_string(root.join("target/anvil/impact/include_affected.txt"))
        .unwrap()
        .trim()
        .to_owned();

    let bin = root.join(".fakebin");
    let log = root.join("cargo-argv.log");
    fake_cargo(&bin, &log);
    let path = path_with_prefix(&bin);
    let run_msrv = || {
        just_cmd(root, &["anvil-msrv-test"])
            .env("ANVIL_IMPACT", "consume")
            .env("RUSTUP_TOOLCHAIN", "test-stable")
            .env("PATH", &path)
            .output()
            .unwrap()
    };

    let output = run_msrv();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "anvil-msrv-test failed:\n{combined}");
    let argv = fs::read_to_string(&log).unwrap();
    for expected in [
        format!("+1.97 test {affected} --all-targets --all-features --locked"),
        format!("+1.97 test {affected} --all-targets --locked"),
    ] {
        assert!(argv.contains(&expected), "missing MSRV invocation '{expected}' in:\n{argv}");
    }

    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        manifest.replace("\n[workspace.package]\nrust-version = \"1.97\"\n", "\n"),
    )
    .unwrap();
    fs::write(&log, "").unwrap();

    let skipped = run_msrv();
    let skip_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&skipped.stdout),
        String::from_utf8_lossy(&skipped.stderr)
    );
    assert!(skipped.status.success(), "no-MSRV invocation failed:\n{skip_combined}");
    assert!(
        fs::read_to_string(&log).unwrap().is_empty(),
        "no-MSRV invocation must skip before calling cargo"
    );
}

/// Write a fake `cargo` into `dir` that answers `cargo metadata …` by printing
/// the contents of `metadata_json` and captures every other invocation's argv
/// (one per line) to `log`, exiting 0. Lets `anvil-loom` be driven with a
/// synthetic workspace shape without a real loom crate.
fn fake_cargo_with_metadata(dir: &Path, log: &Path, metadata_json: &Path) {
    fs::create_dir_all(dir).unwrap();
    #[cfg(windows)]
    {
        let script = format!(
            "$effectiveArgs = @()\n\
             foreach ($argument in $args) {{\n\
             \x20   if ($argument -is [Array]) {{ $effectiveArgs += @($argument) }} else {{ $effectiveArgs += $argument }}\n\
             }}\n\
             if ($effectiveArgs -contains 'metadata') {{ Get-Content -Raw -LiteralPath '{meta}'; exit 0 }}\n\
             Add-Content -LiteralPath '{log}' -Value ($effectiveArgs -join ' ')\n\
             exit 0\n",
            meta = metadata_json.display(),
            log = log.display()
        );
        fs::write(dir.join("cargo.ps1"), script).unwrap();
    }
    #[cfg(unix)]
    {
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then cat '{meta}'; exit 0; fi\nprintf '%s\\n' \"$*\" >> '{log}'\nexit 0\n",
            meta = metadata_json.display(),
            log = log.display()
        );
        let path = dir.join("cargo");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
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
    fs::write(
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
    let argv = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        argv.contains("test") && argv.contains("-p gamma") && argv.contains("--test loomtest"),
        "loom must run the declared loom target in full-workspace mode; captured argv:\n{argv}"
    );
}

#[test]
fn invalid_anvil_impact_value_fails_loudly_without_computing() {
    if !core_tools_available() {
        return;
    }
    // ANVIL_IMPACT is a strict tri-state (off / consume / unset). A typo like
    // `on` must fail closed at every read site with exit 2 and an actionable
    // error -- never silently fall through to "scoping on" (which could skip
    // checks) or compute a scope. The rejection must also happen WITHOUT
    // cargo-delta, so run under the tripwire shim.
    let tmp = workspace();
    let root = tmp.path();
    let shim = ShimBin::tripwire_cargo();
    for recipe in [&["anvil-impact"][..], &["_anvil-impact-include", "affected"][..]] {
        let out = just_cmd(root, recipe)
            .env("ANVIL_IMPACT", "on")
            .env("PATH", &shim.path)
            .env("ANVIL_TEST_LOG", &shim.log)
            .output()
            .unwrap();
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
    assert!(
        shim.log_lines().is_empty(),
        "an invalid mode must not invoke cargo-delta, but did: {:?}",
        shim.log_lines()
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
    if !core_tools_available() {
        return;
    }
    // consume trusts a downloaded cache verbatim, so a missing cache is a real
    // pipeline defect (renamed artifact, a missing download step, or a group
    // action run directly). It must fail loudly rather than let every scoped
    // check silently widen to --workspace while the pipeline stays green.
    //
    // consume is a pure cache-presence check: it must never invoke cargo-delta
    // (a downstream group job has neither it nor a base ref), so the whole test
    // runs under a tripwire cargo shim and seeds the cache directly rather than
    // producing it via a real snapshot.
    let tmp = workspace();
    let root = tmp.path();
    let shim = ShimBin::tripwire_cargo();
    let consume = |args: &[&str]| {
        just_cmd(root, args)
            .env("ANVIL_IMPACT", "consume")
            .env("PATH", &shim.path)
            .env("ANVIL_TEST_LOG", &shim.log)
            .output()
            .unwrap()
    };

    // No target/anvil/impact/ cache has been downloaded.
    let out = consume(&["anvil-impact"]);
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
    // successful no-op. Seed the three include files directly -- consume only
    // checks their presence, never recomputes.
    let cache = root.join("target/anvil/impact");
    for (file, spec) in [
        ("include_modified.txt", ""),
        ("include_affected.txt", "--package alpha@0.1.0"),
        ("include_required.txt", "--workspace"),
    ] {
        write(&cache.join(file), spec);
    }
    let ok = consume(&["anvil-impact"]);
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
    fs::remove_file(cache.join("include_affected.txt")).unwrap();
    let partial = consume(&["anvil-impact"]);
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

    // Across every consume path above -- missing, present, and partial -- the
    // recipe must never have shelled out to cargo-delta.
    assert!(
        shim.log_lines().is_empty(),
        "consume must not invoke cargo-delta on any path, but did: {:?}",
        shim.log_lines()
    );
}

/// Invoke the emitted `_anvil-impact-format` helper directly against a
/// hand-written impact JSON file, returning (trimmed stdout, stderr, success).
/// This exercises the formatter's mapping logic -- name/lib/proc-macro
/// resolution and fail-hard on unmappable names -- in isolation from the
/// snapshot/cache machinery, which the snapshot tests only pin as emitted *text*.
fn run_format(root: &Path, tier: &str, impact_json_rel: &str) -> (String, String, bool) {
    let out = just_cmd(root, &["_anvil-impact-format", tier, impact_json_rel]).output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn impact_format_fails_hard_on_unmappable_name() {
    if !tools_available() {
        return;
    }
    // A cargo-delta report naming a package that resolves to no workspace
    // member must fail the recipe (non-zero exit), NOT emit the mapped subset
    // or --skip -- either of which would silently UNDER-scope and skip a check
    // that should run. cargo-delta reports (non-unique) library identifiers, so
    // an unmappable name signals a gap in our reverse-mapping that must surface.
    let tmp = workspace();
    let root = tmp.path();
    // A valid member (`alpha`) plus a name that maps to nothing here.
    let fixture = "impact-fixture.json";
    write(&root.join(fixture), "{\"Affected\":[\"alpha\",\"ghostpkg\"]}\n");

    let (stdout, stderr, ok) = run_format(root, "affected", fixture);
    assert!(!ok, "an unmappable name must fail the recipe:\nstdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("ghostpkg"),
        "the diagnostic must name the unmappable package:\n{stderr}"
    );
}

#[test]
fn impact_format_maps_proc_macro_target_name_to_its_package() {
    if !tools_available() {
        return;
    }
    // cargo-delta reports *target* names (snake_case). A proc-macro crate whose
    // package name differs from its target name (`my-macro` vs `my_macro`) must
    // still resolve to `--package my-macro@<version>` via the proc-macro branch
    // of the lib-target lookup. An unmappable target name no longer widens to
    // --workspace -- `_anvil-impact-format` fails the whole run (see
    // `impact_format_fails_hard_on_unmappable_name`) -- so a mapping miss here
    // would abort scoping rather than silently cost a full-workspace run.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.97\"\n",
    );
    write(
        &root.join("Justfile"),
        "set unstable\nset windows-shell := [\"pwsh\", \"-NoProfile\", \"-Command\"]\n",
    );
    write(
        &root.join("crates/my-macro/Cargo.toml"),
        "[package]\nname = \"my-macro\"\nversion = \"0.3.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n",
    );
    write(&root.join("crates/my-macro/src/lib.rs"), "");
    let args = Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    };
    run_update(&Catalog::anvil(), &args, root).unwrap();

    let fixture = "impact-fixture.json";
    write(&root.join(fixture), "{\"Affected\":[\"my_macro\"]}\n");

    let (stdout, stderr, ok) = run_format(root, "affected", fixture);
    assert!(ok, "the formatter must exit 0:\nstderr: {stderr}");
    assert_eq!(
        stdout, "--package my-macro@0.3.0",
        "the proc-macro target `my_macro` must map back to its package `my-macro`:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

/// Drive `just _anvil-impact-snapshot` under a `cargo` shim that records the
/// `RUSTUP_TOOLCHAIN` in effect at each `cargo delta snapshot`, returning the
/// two logged values in order: `[baseline, current]`. `caller_toolchain` is the
/// input override the caller exports (or `None` to use repository selection).
fn snapshot_toolchain_probe(caller_toolchain: Option<&str>, toolchain_file: bool) -> Vec<String> {
    let tmp = workspace();
    let root = tmp.path();
    if toolchain_file {
        write(&root.join("rust-toolchain.toml"), "[toolchain]\npath = \"toolchains/local\"\n");
        git(root, &["add", "rust-toolchain.toml"]);
        git(root, &["commit", "-q", "-m", "select local toolchain"]);
    }
    // cargo shim: log the effective explicit argument or RUSTUP_TOOLCHAIN on
    // each `delta snapshot` (emitting `{}` as the snapshot), satisfy the
    // cargo-delta prereq probe (`cargo install --list`), and no-op everything
    // else.
    let shim = ShimBin::new(&[
        (
            "cargo.ps1",
            "if (($args -contains 'delta') -and ($args -contains 'snapshot')) {\n\
            \x20   $explicit = @($args | Where-Object { $_ -like '+*' } | Select-Object -First 1)\n\
            \x20   $tc = if ($explicit.Count -eq 1) { $explicit[0].Substring(1) } elseif (Test-Path Env:\\RUSTUP_TOOLCHAIN) { $env:RUSTUP_TOOLCHAIN } else { '<unset>' }\n\
            \x20   Add-Content -LiteralPath $env:ANVIL_TEST_LOG -Value $tc\n\
            \x20   Write-Output '{}'\n\
            \x20   exit 0\n\
            }\n\
            if (($args -contains 'install') -and ($args -contains '--list')) {\n\
            \x20   Write-Output 'cargo-delta v9.9.9:'\n\
            \x20   exit 0\n\
            }\n\
            exit 0\n",
        ),
        (
            "rustup.ps1",
            "if (($args -contains 'show') -and ($args -contains 'active-toolchain')) {\n\
            \x20   Write-Output 'file-active-toolchain (overridden by rust-toolchain.toml)'\n\
            \x20   exit 0\n\
            }\n\
            exit 0\n",
        ),
    ]);

    let mut cmd = just_cmd(root, &["_anvil-impact-snapshot"]);
    cmd.env("PATH", &shim.path).env("ANVIL_TEST_LOG", &shim.log);
    match caller_toolchain {
        Some(tc) => {
            cmd.env("RUSTUP_TOOLCHAIN", tc);
        }
        None => {
            cmd.env_remove("RUSTUP_TOOLCHAIN");
        }
    }
    let out = cmd.output().unwrap();
    let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "_anvil-impact-snapshot failed:\n{combined}");
    shim.log_lines()
}

#[test]
fn impact_snapshot_uses_current_checkout_toolchain_for_both_trees() {
    if !core_tools_available() {
        return;
    }
    // The baseline snapshot is taken inside a worktree checked out at the merge
    // target, whose rust-toolchain.toml may pin a toolchain that is NOT
    // installed here (any PR that bumps rust-toolchain.toml). Both snapshots
    // must therefore use the compiler selected from the current checkout so
    // cargo-delta compares metadata produced under one compiler policy.

    // Case A: the caller override is the selected compiler for both trees.
    let with_caller = snapshot_toolchain_probe(Some("caller-pin-toolchain"), false);
    assert_eq!(
        with_caller,
        vec!["caller-pin-toolchain".to_owned(), "caller-pin-toolchain".to_owned()],
        "baseline and current snapshots must both use the caller's selected compiler"
    );

    // Case B: without an override, the workspace MSRV selects both snapshots.
    let without_caller = snapshot_toolchain_probe(None, false);
    assert_eq!(
        without_caller,
        vec!["1.97".to_owned(), "1.97".to_owned()],
        "baseline and current snapshots must both use Anvil's selected MSRV"
    );

    // Case C: a toolchain file is selected natively in the current checkout.
    // The baseline needs rustup's concrete active identifier because the file
    // may contain a path that cannot be assigned directly to RUSTUP_TOOLCHAIN.
    let with_file = snapshot_toolchain_probe(None, true);
    assert_eq!(
        with_file,
        vec!["file-active-toolchain".to_owned(), "<unset>".to_owned()],
        "baseline must use the current file's concrete active toolchain while the current snapshot lets rustup process that file natively"
    );
}
