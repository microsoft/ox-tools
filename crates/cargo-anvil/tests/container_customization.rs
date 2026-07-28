// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))] // exercises the real pwsh driver against a fake `wsl`; miri can't sandbox this.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Driver-level verification of the `customize.ps1` runtime contract from
//! [`containers.md`](../docs/design/containers.md#8-container-customization).
//!
//! Generates the real `justfiles/anvil/container/` tree with
//! [`cargo_anvil::test_support::run_update`], then runs the generated
//! `run-in-container.ps1` against a fake `wsl` on `PATH` so the driver's
//! own process, argument construction, and validation execute for real.
//! `anvil-clippy` is used throughout so the GitHub-token path (which would
//! also require a fake `gh`) is never exercised.
//!
//! The Bash mirror lives in `container_customization_bash.rs` and runs on
//! Unix. It cannot run in this Windows test process because `bash` resolves
//! to the WSL launcher, which uses a different filesystem namespace from the
//! generated temporary repository.

use std::path::Path;
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

/// A repository with the public container tree generated and no derived
/// catalog involved, proving the driver loads `customize.ps1` purely by
/// standard path discovery.
fn repo_with_container() -> TempDir {
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
    write(&root.join("rust-toolchain.toml"), "channel = \"1.93\"\n");
    run_update(&Catalog::anvil(), &local(), root).unwrap();
    assert!(
        !root.join("justfiles/anvil/container/customize.ps1").exists(),
        "the public catalog must not emit customize.ps1 by default"
    );
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git must be available");
    assert!(status.success(), "temporary Git repository must initialize");
    tmp
}

/// Installs a fake `wsl` on `PATH` so the real Windows driver runs against
/// controllable Docker Engine behavior without depending on the host's WSL
/// configuration.
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
        if ($env:FAKE_TOKEN_PATH_LOG -and $commandArgs[-1] -like '*anvil-github-token-*') {
            Add-Content -LiteralPath $env:FAKE_TOKEN_PATH_LOG -Value $commandArgs[-1]
        }
        Write-Output '/mnt/c/fake/path'
        exit 0
    }
    'id' {
        if ($commandArgs[0] -eq '-u') { Write-Output '1000' } else { Write-Output '1000' }
        exit 0
    }
    'docker' {
        $logPath = $env:FAKE_DOCKER_LOG
        if ($logPath) { Add-Content -LiteralPath $logPath -Value ($commandArgs -join ' ') }
        $sub = $commandArgs[0]
        switch ($sub) {
            'version' { Write-Output '26.1.5'; exit 0 }
            'image' {
                if ($env:FAKE_DOCKER_IMAGE_EXISTS -eq '1') { exit 0 } else { exit 1 }
            }
            'build' {
                exit [int]($(if ($env:FAKE_DOCKER_BUILD_EXIT) { $env:FAKE_DOCKER_BUILD_EXIT } else { '0' }))
            }
            'volume' { exit 0 }
            'run' {
                $joined = $commandArgs -join ' '
                if ($env:FAKE_DOCKER_FAIL_MARKER -and $joined.Contains($env:FAKE_DOCKER_FAIL_MARKER)) {
                    exit 1
                }
                exit 0
            }
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
    test_log: String,
    token_paths: Vec<std::path::PathBuf>,
}

fn assert_token_files_removed(run: &DriverRun) {
    assert!(!run.token_paths.is_empty(), "expected a GitHub token path");
    for path in &run.token_paths {
        assert!(!path.exists(), "temporary GitHub token file was not removed: {}", path.display());
    }
}

/// Runs the real generated `run-in-container.ps1` against the fake `wsl`,
/// with `customize.ps1` written from `customize_ps1_body` beforehand.
fn run_driver(root: &Path, customize_ps1_body: &str, recipe: &str, env: &[(&str, &str)]) -> DriverRun {
    run_driver_args(root, customize_ps1_body, &[recipe], env)
}

fn run_driver_args(root: &Path, customize_ps1_body: &str, recipe_args: &[&str], env: &[(&str, &str)]) -> DriverRun {
    let container_dir = root.join("justfiles/anvil/container");
    write(&container_dir.join("customize.ps1"), customize_ps1_body);

    let bin_dir = root.join("fake-bin");
    install_fake_wsl(&bin_dir);

    let docker_log = root.join("docker.log");
    let test_log = root.join("test.log");
    let token_path_log = root.join("token-path.log");
    let path = format!("{};{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let mut command = Command::new("pwsh");
    command
        .args(["-NoProfile", "-File", "justfiles/anvil/container/run-in-container.ps1"])
        .args(recipe_args)
        .current_dir(root)
        .env("PATH", path)
        .env("FAKE_DOCKER_LOG", &docker_log)
        .env("FAKE_TEST_LOG", &test_log)
        .env("FAKE_TOKEN_PATH_LOG", &token_path_log)
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANVIL_CONTAINER_IMAGE")
        .env_remove("ANVIL_CONTAINER_NO_REBUILD");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("pwsh must be available to run the driver");

    DriverRun {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        docker_log: std::fs::read_to_string(&docker_log).unwrap_or_default(),
        test_log: std::fs::read_to_string(&test_log).unwrap_or_default(),
        token_paths: std::fs::read_to_string(&token_path_log)
            .unwrap_or_default()
            .lines()
            .map(Into::into)
            .collect(),
    }
}

#[test]
fn every_requested_recipe_is_checked_for_github_authentication() {
    let tmp = repo_with_container();
    let run = run_driver_args(
        tmp.path(),
        "",
        &["anvil-clippy", "anvil-aprz"],
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1"), ("GITHUB_TOKEN", "test-token")],
    );

    assert!(
        run.status.success(),
        "a later requested recipe must receive GitHub authentication: {}",
        run.stderr
    );
    assert!(
        run.docker_log.lines().any(|line| line.contains("just anvil-clippy anvil-aprz")),
        "all arguments must still be forwarded to the requested recipe: {}",
        run.docker_log
    );
    assert_eq!(
        run.docker_log
            .lines()
            .filter(|line| line.starts_with("run ") && line.contains("just anvil-aprz"))
            .count(),
        1,
        "a later token-requiring recipe must cause one isolated anvil-aprz invocation"
    );
    assert!(
        run.docker_log
            .lines()
            .any(|line| line.contains("--env ANVIL_APRZ_ALREADY_RAN=1") && line.contains("just anvil-clippy anvil-aprz")),
        "the requested recipes must run with APRZ marked complete: {}",
        run.docker_log
    );
}

#[test]
fn customization_can_provide_github_authentication() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "$env:GITHUB_TOKEN = 'custom-token'\n",
        "anvil-aprz",
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1")],
    );

    assert!(run.status.success(), "custom authentication must be accepted: {}", run.stderr);
    assert!(
        run.docker_log.contains("/run/secrets/anvil-github-token"),
        "the customization-provided token must be mounted for APRZ: {}",
        run.docker_log
    );
    assert_token_files_removed(&run);
}

#[test]
fn aggregate_recipe_isolates_the_token_from_the_main_container() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "",
        "_anvil-pr",
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1"), ("GITHUB_TOKEN", "test-token")],
    );

    assert!(run.status.success(), "aggregate recipe failed: {}", run.stderr);
    let aprz = run
        .docker_log
        .lines()
        .find(|line| line.contains("just anvil-aprz"))
        .expect("aggregate recipe must run isolated APRZ");
    let main = run
        .docker_log
        .lines()
        .find(|line| line.contains("just _anvil-pr"))
        .expect("aggregate recipe must run its main container");
    assert!(
        aprz.contains("/run/secrets/anvil-github-token"),
        "APRZ must receive the token mount: {aprz}"
    );
    assert!(
        !main.contains("/run/secrets/anvil-github-token"),
        "main container must not receive the token mount: {main}"
    );
    assert!(
        main.contains("ANVIL_APRZ_ALREADY_RAN=1"),
        "main container must skip the completed APRZ check: {main}"
    );
    assert!(
        !run.docker_log.contains("--env GITHUB_TOKEN"),
        "the token must never be passed through the environment"
    );
    assert_token_files_removed(&run);
}

#[test]
fn token_file_is_removed_after_aprz_or_main_failure() {
    for marker in ["just anvil-aprz", "just _anvil-pr"] {
        let tmp = repo_with_container();
        let run = run_driver(
            tmp.path(),
            "",
            "_anvil-pr",
            &[
                ("FAKE_DOCKER_IMAGE_EXISTS", "1"),
                ("GITHUB_TOKEN", "test-token"),
                ("FAKE_DOCKER_FAIL_MARKER", marker),
            ],
        );

        assert!(!run.status.success(), "failure marker must fail the driver: {marker}");
        assert_token_files_removed(&run);
    }
}

#[test]
fn powershell_just_dispatch_treats_interpolated_values_as_data() {
    if Command::new("just").arg("--version").output().is_err() {
        return;
    }

    let tmp = repo_with_container();
    let root = tmp.path();

    let runner_output = Command::new("just")
        .args(["_anvil-run", "missing", "x') { Write-Output RUNNER_INJECTED } elseif ('a"])
        .current_dir(root)
        .output()
        .expect("just must be available");
    assert!(!runner_output.status.success(), "the missing native tier must fail");
    assert!(
        !String::from_utf8_lossy(&runner_output.stdout).contains("RUNNER_INJECTED"),
        "the runner parameter must not execute as PowerShell source"
    );

    write(
        &root.join("justfiles/anvil/container/run-in-container.ps1"),
        "param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Recipe)\nWrite-Output 'DRIVER_OK'\n",
    );
    let recipe_output = Command::new("just")
        .args(["anvil-container", "x'); Write-Output RECIPE_INJECTED; @('a"])
        .current_dir(root)
        .output()
        .expect("just must be available");
    assert!(
        recipe_output.status.success(),
        "the escaped container recipe must reach the driver: {}",
        String::from_utf8_lossy(&recipe_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&recipe_output.stdout);
    assert!(stdout.contains("DRIVER_OK"), "the container driver must run");
    assert!(
        !stdout.contains("RECIPE_INJECTED"),
        "the recipe parameter must not execute as PowerShell source"
    );
}

#[test]
fn cold_run_exposes_contract_inputs_scopes_phases_and_runs_cleanup() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r#"
if ($AnvilContainerCustomizationApiVersion -ne 1) { throw 'unsupported customization API version' }
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "exists=$AnvilContainerImageExists"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "recipes=$($AnvilContainerRequestedRecipes -join ',')"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "windows=$AnvilContainerHostIsWindows"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "repo-is-dir=$(Test-Path -LiteralPath $AnvilContainerRepoRoot -PathType Container)"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "dir-is-container-dir=$($AnvilContainerDir -eq (Join-Path $AnvilContainerRepoRoot 'justfiles/anvil/container'))"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "repo-wsl=$AnvilContainerRepoRootWsl"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "dir-wsl=$AnvilContainerDirWsl"
$AnvilContainerBuildArgs = @('--secret', 'id=build-marker,src=fake')
$AnvilContainerRunArgs = @('--label', 'run-marker=1')
$AnvilContainerCleanup = { Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value 'cleanup-ran' }
"#;
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(
        run.status.success(),
        "cold run must succeed: stderr={}\ndocker.log={}",
        run.stderr,
        run.docker_log
    );
    assert!(run.test_log.contains("exists=False"), "log: {}", run.test_log);
    assert!(run.test_log.contains("recipes=anvil-clippy"), "log: {}", run.test_log);
    assert!(run.test_log.contains("windows=True"), "log: {}", run.test_log);
    assert!(run.test_log.contains("repo-is-dir=True"), "log: {}", run.test_log);
    assert!(run.test_log.contains("dir-is-container-dir=True"), "log: {}", run.test_log);
    assert!(run.test_log.contains("repo-wsl=/mnt/c/fake/path"), "log: {}", run.test_log);
    assert!(
        run.test_log.contains("dir-wsl=/mnt/c/fake/path/justfiles/anvil/container"),
        "log: {}",
        run.test_log
    );
    // Build-phase arguments must only appear on the `build` invocation, and
    // run-phase arguments only on the `run` invocation: phases stay isolated.
    let build_line = run
        .docker_log
        .lines()
        .find(|line| line.starts_with("build "))
        .unwrap_or_else(|| panic!("expected a docker build invocation, got: {}", run.docker_log));
    assert!(build_line.contains("id=build-marker,src=fake"), "line: {build_line}");
    assert!(!build_line.contains("run-marker=1"), "line: {build_line}");
    let run_line = run
        .docker_log
        .lines()
        .find(|line| line.starts_with("run ") && line.contains("just anvil-clippy"))
        .unwrap_or_else(|| panic!("expected a docker run invocation, got: {}", run.docker_log));
    assert!(run_line.contains("run-marker=1"), "line: {run_line}");
    assert!(!run_line.contains("id=build-marker,src=fake"), "line: {run_line}");
    assert!(
        run_line.contains("--user 1000:1000"),
        "recipe execution must use the WSL user: {run_line}"
    );
    assert_eq!(
        run.docker_log.lines().filter(|line| line.starts_with("volume create ")).count(),
        3,
        "the driver must create all named cache volumes: {}",
        run.docker_log
    );
    assert!(
        run.test_log.contains("cleanup-ran"),
        "cleanup must run after an ordinary successful invocation: {}",
        run.test_log
    );
}

#[test]
fn warm_run_skips_the_build_and_still_reports_image_exists() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r#"
Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value "exists=$AnvilContainerImageExists"
"#;
    let run = run_driver(root, customize, "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);

    assert!(
        run.status.success(),
        "warm run must succeed: stderr={}\ndocker.log={}",
        run.stderr,
        run.docker_log
    );
    assert!(run.test_log.contains("exists=True"), "log: {}", run.test_log);
    assert!(
        !run.docker_log.lines().any(|line| line.starts_with("build ")),
        "a warm run (matching image already present) must not invoke docker build: {}",
        run.docker_log
    );
}

#[test]
fn prepare_args_without_a_prepare_command_are_rejected_before_docker_runs() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r"
$AnvilContainerPrepareArgs = @('--label', 'prepare-marker=1')
";
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(!run.status.success(), "prepare args without a prepare command must fail validation");
    assert!(
        run.stderr.contains("AnvilContainerPrepareArgs requires") && run.stderr.contains("AnvilContainerPrepareCommand"),
        "stderr must name the invalid output: {}",
        run.stderr
    );
    assert!(
        !run.docker_log
            .lines()
            .any(|line| line.starts_with("build ") || line.starts_with("run ")),
        "validation must fail before any Docker build or run invocation \
         (version/image-exists checks happen earlier and are expected): {}",
        run.docker_log
    );
}

#[test]
fn null_array_output_is_rejected_before_docker_runs() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r"
$AnvilContainerRunArgs = $null
";
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(!run.status.success(), "a null array output must fail validation");
    assert!(
        run.stderr.contains("AnvilContainerRunArgs must be a string array"),
        "stderr must name the invalid output: {}",
        run.stderr
    );
    assert!(
        !run.docker_log
            .lines()
            .any(|line| line.starts_with("build ") || line.starts_with("run ")),
        "validation must fail before any Docker build or run invocation \
         (version/image-exists checks happen earlier and are expected): {}",
        run.docker_log
    );
}

#[test]
fn content_changing_build_arguments_are_rejected() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "$AnvilContainerBuildArgs = @('--build-arg', 'BASE_IMAGE=example.invalid/base')\n",
        "anvil-clippy",
        &[],
    );

    assert!(!run.status.success(), "content-changing build arguments must fail validation");
    assert!(
        run.stderr
            .contains("AnvilContainerBuildArgs accepts only BuildKit --secret arguments"),
        "stderr must explain the image-identity restriction: {}",
        run.stderr
    );
}
#[test]
fn cleanup_still_runs_after_the_main_recipe_container_fails() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r"
$AnvilContainerRunArgs = @('--label', 'run-marker=1')
$AnvilContainerCleanup = { Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value 'cleanup-ran' }
";
    let run = run_driver(
        root,
        customize,
        "anvil-clippy",
        &[
            ("FAKE_DOCKER_IMAGE_EXISTS", "1"), // warm run: only the main recipe container executes.
            ("FAKE_DOCKER_FAIL_MARKER", "run-marker=1"),
        ],
    );

    assert!(!run.status.success(), "the driver must surface the recipe failure");
    assert!(
        run.test_log.contains("cleanup-ran"),
        "cleanup must still run after an ordinary recipe failure: {}",
        run.test_log
    );
}
