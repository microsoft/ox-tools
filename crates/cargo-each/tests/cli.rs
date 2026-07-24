// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end tests for the `cargo-each` binary, driven through a temporary
//! fixture workspace so selection / filtering / execution are exercised
//! against real `cargo metadata`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Write a fixture workspace with three members:
/// - `alpha` (lib),
/// - `beta` (bin only),
/// - `gamma` (lib, carrying `[package.metadata.role] = "script-only"`).
fn fixture() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\", \"beta\", \"gamma\"]\n",
    )
    .expect("write workspace root");

    write_lib(root, "alpha", "");
    write_bin(root, "beta");
    write_lib(root, "gamma", "\n[package.metadata]\nrole = \"script-only\"\n");

    let manifest = root.join("Cargo.toml");
    (tmp, manifest)
}

fn write_lib(root: &Path, name: &str, extra: &str) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"),
    )
    .expect("write member Cargo.toml");
    fs::write(dir.join("src/lib.rs"), "// fixture\n").expect("write lib.rs");
}

fn write_bin(root: &Path, name: &str) {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src")).expect("mkdir src");
    fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .expect("write member Cargo.toml");
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write main.rs");
}

/// Build a `cargo-each each` invocation against the fixture at `manifest`.
fn each(manifest: &Path) -> Command {
    let mut cmd = Command::cargo_bin("cargo-each").expect("binary");
    cmd.arg("each").arg("--manifest-path").arg(manifest);
    cmd
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_whole_workspace_expands_to_workspace_flag() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "cargo", "clippy", "{packages}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cargo clippy --workspace"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn per_package_runs_once_per_selected_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "-p", "gamma", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo alpha").and(predicate::str::contains("echo gamma")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn none_is_a_successful_noop() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--none", "--once", "--dry-run", "--", "cargo", "test", "{packages}"])
        .assert()
        .success()
        .stderr(predicate::str::contains("nothing to do"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn filter_lib_drops_bin_only_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--filter", "lib", "--dry-run", "--", "tool", "{name}"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("tool alpha")
                .and(predicate::str::contains("tool gamma"))
                .and(predicate::str::contains("tool beta").not()),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn exclude_filter_metadata_drops_matching_member() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--exclude-filter",
            "metadata:role=script-only",
            "--dry-run",
            "--",
            "tool",
            "{name}",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("tool gamma")
                .not()
                .and(predicate::str::contains("tool alpha")),
        );
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_shows_crate_root_in_dry_run() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--chdir", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(cd ").and(predicate::str::contains("alpha")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_with_once_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--chdir", "--once", "--dry-run", "--", "echo", "{packages}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--chdir").and(predicate::str::contains("--once")));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn unknown_selector_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "does-not-exist", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("did not match"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn bad_predicate_is_a_usage_error() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--filter", "nonsense", "--dry-run", "--", "echo", "{name}"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("invalid filter predicate"));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn executes_command_and_propagates_success() {
    // Actually spawns a command (not --dry-run) to cover the execution path.
    // `cargo --version` is available on every CI runner and is a no-op.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--", "cargo", "--version"])
        .assert()
        .success();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn executes_command_and_propagates_failure() {
    // A failing child command's exit code propagates (fail-fast).
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--", "cargo", "this-subcommand-does-not-exist"])
        .assert()
        .failure();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn chdir_executes_from_crate_root() {
    // Non-dry-run --chdir so the child's working directory is actually set.
    // `cargo --version` succeeds regardless of directory.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "--chdir", "--", "cargo", "--version"])
        .assert()
        .success();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn keep_going_runs_all_members_then_fails() {
    // With --keep-going every member runs even though the command fails for
    // each; the overall exit is non-zero.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "-p",
            "alpha",
            "-p",
            "gamma",
            "--keep-going",
            "--",
            "cargo",
            "this-subcommand-does-not-exist",
        ])
        .assert()
        .failure();
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn dry_run_quotes_arguments_with_whitespace() {
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["--workspace", "--once", "--dry-run", "--", "echo", "a b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"a b\""));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_with_filter_uses_explicit_packages_not_workspace() {
    // A `--filter` narrows the whole workspace, so `{packages}` must expand to
    // an explicit `--package` list rather than a bare `--workspace`.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--filter",
            "lib",
            "--once",
            "--dry-run",
            "--",
            "cargo",
            "x",
            "{packages}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--package alpha").and(predicate::str::contains("--workspace").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn once_with_exclude_filter_uses_explicit_packages_not_workspace() {
    // An `--exclude-filter` also narrows the set, so `{packages}` must expand
    // to an explicit `--package` list, not `--workspace`.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args([
            "--workspace",
            "--exclude-filter",
            "metadata:role=script-only",
            "--once",
            "--dry-run",
            "--",
            "cargo",
            "x",
            "{packages}",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("--package").and(predicate::str::contains("--workspace").not()));
}

#[cfg_attr(miri, ignore = "spawns the cargo-each binary and cargo subprocesses; miri supports neither")]
#[test]
fn fail_fast_stops_before_running_later_members() {
    // Without --keep-going, a failure on the first member must stop the run:
    // cargo-each prints a per-member label to stderr before each command, so a
    // stopped run never prints the second member's label.
    let (_tmp, manifest) = fixture();
    each(&manifest)
        .args(["-p", "alpha", "-p", "gamma", "--", "cargo", "this-subcommand-does-not-exist"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cargo each: alpha").and(predicate::str::contains("cargo each: gamma").not()));
}
