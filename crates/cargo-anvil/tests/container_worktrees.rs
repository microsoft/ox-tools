// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))] // exercises the real pwsh driver against a fake `wsl`; miri can't sandbox this.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Driver-level verification of the linked-worktree contract from
//! [`containers.md`](../docs/design/containers.md#61-linked-git-worktrees).
//!
//! Real Git repositories and real linked worktrees are created on disk, then
//! the generated `run-in-container.ps1` runs against a fake `wsl` on `PATH`
//! whose `wslpath` performs the same drive-letter translation the real one
//! does. `anvil-clippy` is used throughout so the GitHub-token path is never
//! exercised.
//!
//! The Bash mirror lives in `container_worktrees_bash.rs` and runs on Unix.

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

/// Installs a fake `wsl` on `PATH` whose `wslpath` performs real drive-letter
/// translation, so the driver's own path handling is exercised end to end.
fn install_fake_wsl(bin_dir: &Path) {
    std::fs::create_dir_all(bin_dir).unwrap();
    write(
        &bin_dir.join("wsl.cmd"),
        "@echo off\r\npwsh -NoProfile -File \"%~dp0wsl.ps1\" %*\r\nexit /b %ERRORLEVEL%\r\n",
    );
    write(
        &bin_dir.join("wsl.ps1"),
        r"
$command = $args[1]
$commandArgs = @($args | Select-Object -Skip 2)
switch ($command) {
    'wslpath' {
        $target = ($commandArgs[-1] -replace '\\', '/')
        if ($target -match '^([A-Za-z]):/(.*)$') {
            $target = '/mnt/' + $Matches[1].ToLowerInvariant() + '/' + $Matches[2]
        }
        Write-Output $target
        exit 0
    }
    'id' { Write-Output '1000'; exit 0 }
    'uname' { Write-Output 'x86_64'; exit 0 }
    'docker' {
        if ($env:FAKE_DOCKER_LOG) { Add-Content -LiteralPath $env:FAKE_DOCKER_LOG -Value ($commandArgs -join ' ') }
        switch ($commandArgs[0]) {
            'version' { Write-Output '26.1.5'; exit 0 }
            'image' { exit 1 }
            default { exit 0 }
        }
    }
    default { exit 0 }
}
",
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

/// Runs the real generated `run-in-container.ps1` from `working_dir` against
/// the fake `wsl`.
fn run_driver(working_dir: &Path) -> DriverRun {
    let bin_dir = working_dir.join("fake-bin");
    install_fake_wsl(&bin_dir);
    let docker_log = bin_dir.join("docker.log");
    let _ = std::fs::remove_file(&docker_log);
    let path = format!("{};{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let output = Command::new("pwsh")
        .args(["-NoProfile", "-File", ".anvil/container/run-in-container.ps1", "anvil-clippy"])
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
        .expect("pwsh must be available to run the driver");

    DriverRun {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        docker_log: std::fs::read_to_string(&docker_log).unwrap_or_default(),
    }
}

/// Mirrors the fake `wslpath` translation so expectations stay independent of
/// the driver implementation.
fn to_wsl_path(windows_path: &str) -> String {
    let normalized = windows_path.replace('\\', "/");
    match normalized.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => {
            format!("/mnt/{}/{rest}", drive.to_lowercase())
        }
        _ => normalized,
    }
}

const GIT_METADATA_TARGET: &str = "target=/anvil-git";

#[test]
fn ordinary_repository_runs_without_a_git_metadata_mount() {
    let repos = repo_with_linked_worktree();
    let run = run_driver(&repos.main);

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
    let common_dir = git(&repos.linked, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let git_dir = git(&repos.linked, &["rev-parse", "--absolute-git-dir"]);
    let relative = git_dir
        .strip_prefix(&format!("{common_dir}/"))
        .expect("a linked worktree keeps its Git directory inside the common Git directory");

    let run = run_driver(&repos.linked);
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let recipe_run = run.recipe_run();
    for expected in [
        format!("type=bind,source={},target=/anvil-git,readonly", to_wsl_path(&common_dir)),
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
        .find(|line| line.contains("sh -c chown"))
        .unwrap_or_else(|| panic!("expected an ownership container: {}", run.docker_log));
    assert!(
        !ownership_container.contains(GIT_METADATA_TARGET),
        "the root ownership container must not receive the Git metadata mount: {ownership_container}"
    );
}
