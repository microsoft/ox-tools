// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(unix, not(miri)))] // exercises the real Bash driver against a fake `docker`; miri can't sandbox this.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]
#![expect(
    clippy::literal_string_with_formatting_args,
    reason = "the Bash fixture intentionally contains shell parameter expansions"
)]

//! Driver-level verification of the linked-worktree contract from
//! [`containers.md`](../docs/design/containers.md#61-linked-git-worktrees).
//!
//! Real Git repositories and real linked worktrees are created on disk, then
//! the generated `run-in-container.sh` runs against a fake `docker` on `PATH`
//! so the driver's own Git discovery, path handling, and validation execute
//! for real. `anvil-clippy` is used throughout so the GitHub-token path is
//! never exercised.
//!
//! The `PowerShell` mirror lives in `container_worktrees.rs` and runs on
//! Windows.

use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    write(path, contents);
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn local() -> Cli {
    Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    }
}

fn git(current_dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .expect("git must be available");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A committed repository with the public container tree generated, so a
/// linked worktree checked out from it also contains `.anvil/container/`.
fn committed_repo(root: &Path) {
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    write(&root.join("rust-toolchain.toml"), "channel = \"1.93\"\n");
    run_update(&Catalog::anvil(), &local(), root).unwrap();

    git(root, &["init", "--quiet", "--initial-branch=main"]);
    git(root, &["config", "user.email", "anvil@example.invalid"]);
    git(root, &["config", "user.name", "Anvil Test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.safecrlf", "false"]);
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "--message", "initial"]);
}

/// A main repository plus a linked worktree checked out from it.
struct Worktrees {
    _tmp: TempDir,
    main: PathBuf,
    linked: PathBuf,
}

fn repo_with_linked_worktree() -> Worktrees {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    let linked = tmp.path().join("linked");
    std::fs::create_dir_all(&main).unwrap();
    committed_repo(&main);
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            linked.to_str().expect("temporary path must be UTF-8"),
        ],
    );
    Worktrees { _tmp: tmp, main, linked }
}

/// Installs a fake `docker` on `PATH` so the real driver runs against
/// controllable, observable behavior instead of a real container engine.
fn install_fake_docker(bin_dir: &Path) {
    write_executable(
        &bin_dir.join("docker"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${FAKE_DOCKER_LOG:-}" ]]; then
    printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
fi
case "${1:-}" in
    version) echo '26.1.5'; exit 0 ;;
    image) exit 1 ;;
    build) exit 0 ;;
    volume) exit 0 ;;
    run) exit 0 ;;
    *) exit 0 ;;
esac
"#,
    );
}

struct DriverRun {
    status: std::process::ExitStatus,
    stderr: String,
    docker_log: String,
}

impl DriverRun {
    /// The arguments of the final `docker run` that executes the recipe.
    fn recipe_run(&self) -> &str {
        self.docker_log
            .lines()
            .rfind(|line| line.starts_with("run "))
            .unwrap_or_else(|| panic!("expected a docker run invocation: {}", self.docker_log))
    }
}

/// Runs the real generated `run-in-container.sh` from `working_dir`, with a
/// fake `docker` and any extra fake tools from `bin_dir` ahead of `PATH`.
fn run_driver(working_dir: &Path, bin_dir: &Path) -> DriverRun {
    install_fake_docker(bin_dir);
    let docker_log = bin_dir.join("docker.log");
    let _ = std::fs::remove_file(&docker_log);
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let output = Command::new("bash")
        .arg(".anvil/container/run-in-container.sh")
        .arg("anvil-clippy")
        .current_dir(working_dir)
        .env("PATH", path)
        .env("FAKE_DOCKER_LOG", &docker_log)
        .env_remove("ANVIL_IN_CONTAINER")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANVIL_CONTAINER_BASE_IMAGE")
        .env_remove("ANVIL_CONTAINER_IMAGE")
        .env_remove("ANVIL_CONTAINER_NO_REBUILD")
        .output()
        .expect("bash must be available to run the driver");

    DriverRun {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        docker_log: std::fs::read_to_string(&docker_log).unwrap_or_default(),
    }
}

const GIT_METADATA_TARGET: &str = "target=/anvil-git";

#[test]
fn ordinary_repository_runs_without_a_git_metadata_mount() {
    let repos = repo_with_linked_worktree();
    let bin_dir = repos.main.join("fake-bin");
    let run = run_driver(&repos.main, &bin_dir);

    assert!(run.status.success(), "driver failed: {}", run.stderr);
    assert!(
        !run.docker_log.contains(GIT_METADATA_TARGET),
        "an ordinary repository must not mount Git metadata: {}",
        run.docker_log
    );
    assert!(
        !run.docker_log.contains("GIT_WORK_TREE"),
        "an ordinary repository must not override the Git work tree: {}",
        run.docker_log
    );
}

#[test]
fn linked_worktree_mounts_the_common_git_directory_read_only() {
    let repos = repo_with_linked_worktree();
    let bin_dir = repos.linked.join("fake-bin");
    let common_dir = git(&repos.linked, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let git_dir = git(&repos.linked, &["rev-parse", "--absolute-git-dir"]);
    let relative = git_dir
        .strip_prefix(&format!("{common_dir}/"))
        .expect("a linked worktree keeps its Git directory inside the common Git directory");

    let run = run_driver(&repos.linked, &bin_dir);
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let recipe_run = run.recipe_run();
    for expected in [
        format!("type=bind,source={common_dir},target=/anvil-git,readonly"),
        format!("GIT_DIR=/anvil-git/{relative}"),
        "GIT_WORK_TREE=/workspace".to_owned(),
        "GIT_OPTIONAL_LOCKS=0".to_owned(),
        "GIT_CONFIG_KEY_0=lfs.storage".to_owned(),
        "GIT_CONFIG_VALUE_0=/tmp/anvil-lfs".to_owned(),
    ] {
        assert!(recipe_run.contains(&expected), "missing {expected} in: {recipe_run}");
    }
    let ownership_container = run
        .docker_log
        .lines()
        .find(|line| line.contains("chown"))
        .unwrap_or_else(|| panic!("expected an ownership container: {}", run.docker_log));
    assert!(
        !ownership_container.contains(GIT_METADATA_TARGET),
        "the root ownership container must not receive the Git metadata mount: {ownership_container}"
    );
}

#[test]
fn windows_gitdir_pointer_is_translated_before_git_discovery() {
    let repos = repo_with_linked_worktree();
    let bin_dir = repos.linked.join("fake-bin");
    let git_dir = git(&repos.linked, &["rev-parse", "--absolute-git-dir"]);
    write(&repos.linked.join(".git"), "gitdir: C:\\worktrees\\linked\r\n");
    write_executable(
        &bin_dir.join("wslpath"),
        &format!("#!/usr/bin/env bash\nprintf '%s\\n' '{git_dir}'\n"),
    );

    let run = run_driver(&repos.linked, &bin_dir);
    assert!(run.status.success(), "driver failed: {}", run.stderr);
    assert!(
        run.recipe_run().contains(GIT_METADATA_TARGET),
        "a translated Windows worktree must still mount Git metadata: {}",
        run.docker_log
    );
}

#[test]
fn windows_gitdir_pointer_to_missing_metadata_fails_before_docker_runs() {
    let repos = repo_with_linked_worktree();
    let bin_dir = repos.linked.join("fake-bin");
    let missing = repos.linked.join("absent-metadata");
    write(&repos.linked.join(".git"), "gitdir: C:\\worktrees\\linked\r\n");
    write_executable(
        &bin_dir.join("wslpath"),
        &format!("#!/usr/bin/env bash\nprintf '%s\\n' '{}'\n", missing.display()),
    );

    let run = run_driver(&repos.linked, &bin_dir);
    assert!(!run.status.success(), "the driver must reject unresolvable Git metadata");
    assert!(
        run.stderr.contains("could not resolve the Windows linked-worktree gitdir pointer"),
        "unexpected diagnostic: {}",
        run.stderr
    );
    assert!(
        run.docker_log.is_empty() || !run.docker_log.contains("run "),
        "no container may start once Git metadata is unresolvable: {}",
        run.docker_log
    );
}

#[test]
fn git_directory_outside_its_common_directory_is_rejected() {
    let repos = repo_with_linked_worktree();
    let bin_dir = repos.linked.join("fake-bin");
    let common_dir = git(&repos.linked, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let git_dir = git(&repos.linked, &["rev-parse", "--absolute-git-dir"]);

    // Relocate the worktree's Git directory outside the common Git directory.
    // Mounting it would require exposing an unrelated host path, so the driver
    // must refuse rather than widen the mount.
    let detached = repos.linked.join("detached-gitdir");
    let status = Command::new("cp")
        .args(["-r", &git_dir, detached.to_str().expect("temporary path must be UTF-8")])
        .status()
        .expect("cp must be available");
    assert!(status.success(), "worktree metadata must be copyable");
    write(&detached.join("commondir"), &format!("{common_dir}\n"));
    write(&detached.join("gitdir"), &format!("{}/.git\n", repos.linked.display()));
    write(&repos.linked.join(".git"), &format!("gitdir: {}\n", detached.display()));

    let run = run_driver(&repos.linked, &bin_dir);
    assert!(!run.status.success(), "an escaping Git directory must be rejected");
    assert!(
        run.stderr.contains("outside its common Git directory"),
        "unexpected diagnostic: {}",
        run.stderr
    );
    assert!(
        !run.docker_log.contains("run "),
        "no container may start once the Git layout is rejected: {}",
        run.docker_log
    );
}
