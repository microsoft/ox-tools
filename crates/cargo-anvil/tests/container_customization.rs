// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))] // exercises the real pwsh driver against a fake `podman`; miri can't sandbox this.
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
//! `run-in-container.ps1` against a fake `podman` on `PATH` so the driver's
//! own process, argument construction, and validation execute for real.
//! `anvil-clippy` is used throughout so the GitHub-token path (which would
//! also require a fake `gh`) is never exercised.
//!
//! A Bash/`podman.sh` mirror of these tests is impractical in this
//! environment: on this Windows host, `bash` resolves to the WSL launcher,
//! which runs in a different filesystem namespace than the generated
//! tempdir, so it cannot see the fake `podman` stub or the generated tree.
//! Bash/PowerShell parity for the contract itself is instead pinned by the
//! literal-string assertions in
//! `src/anvil/artifacts/container.rs::drivers_implement_the_versioned_customization_contract`,
//! which checks both scripts contain matching contract markers.

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

/// Installs a fake `podman` on `PATH` (as `podman.cmd` + `podman.ps1`, since
/// `PowerShell`'s bare-word command resolution needs a `PATHEXT` extension) so
/// the real driver runs against controllable, observable behavior instead of
/// a real container engine.
fn install_fake_podman(bin_dir: &Path) {
    std::fs::create_dir_all(bin_dir).unwrap();
    write(
        &bin_dir.join("podman.cmd"),
        "@echo off\r\npwsh -NoProfile -File \"%~dp0podman.ps1\" %*\r\nexit /b %ERRORLEVEL%\r\n",
    );
    write(
        &bin_dir.join("podman.ps1"),
        r"
$logPath = $env:FAKE_PODMAN_LOG
if ($logPath) { Add-Content -LiteralPath $logPath -Value ($args -join ' ') }
$sub = $args[0]
switch ($sub) {
    'version' { Write-Output '5.0.0'; exit 0 }
    'image' {
        if ($env:FAKE_PODMAN_IMAGE_EXISTS -eq '1') { exit 0 } else { exit 1 }
    }
    'build' {
        exit [int]($(if ($env:FAKE_PODMAN_BUILD_EXIT) { $env:FAKE_PODMAN_BUILD_EXIT } else { '0' }))
    }
    'run' {
        $joined = $args -join ' '
        if ($env:FAKE_PODMAN_FAIL_MARKER -and $joined.Contains($env:FAKE_PODMAN_FAIL_MARKER)) {
            exit 1
        }
        exit 0
    }
    default { exit 0 }
}
",
    );
}

struct DriverRun {
    status: std::process::ExitStatus,
    stderr: String,
    podman_log: String,
    test_log: String,
}

/// Runs the real generated `run-in-container.ps1` against the fake `podman`,
/// with `customize.ps1` written from `customize_ps1_body` beforehand.
fn run_driver(root: &Path, customize_ps1_body: &str, recipe: &str, env: &[(&str, &str)]) -> DriverRun {
    run_driver_args(root, customize_ps1_body, &[recipe], env)
}

fn run_driver_args(root: &Path, customize_ps1_body: &str, recipe_args: &[&str], env: &[(&str, &str)]) -> DriverRun {
    let container_dir = root.join("justfiles/anvil/container");
    write(&container_dir.join("customize.ps1"), customize_ps1_body);

    let bin_dir = root.join("fake-bin");
    install_fake_podman(&bin_dir);

    let podman_log = root.join("podman.log");
    let test_log = root.join("test.log");
    let path = format!("{};{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let mut command = Command::new("pwsh");
    command
        .args(["-NoProfile", "-File", "justfiles/anvil/container/run-in-container.ps1"])
        .args(recipe_args)
        .current_dir(root)
        .env("PATH", path)
        .env("FAKE_PODMAN_LOG", &podman_log)
        .env("FAKE_TEST_LOG", &test_log)
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
        podman_log: std::fs::read_to_string(&podman_log).unwrap_or_default(),
        test_log: std::fs::read_to_string(&test_log).unwrap_or_default(),
    }
}

#[test]
fn forwarded_parameter_does_not_trigger_github_authentication() {
    let tmp = repo_with_container();
    let run = run_driver_args(
        tmp.path(),
        "",
        &["anvil-clippy", "anvil-aprz"],
        &[("FAKE_PODMAN_IMAGE_EXISTS", "1")],
    );

    assert!(
        run.status.success(),
        "a forwarded parameter must not trigger GitHub authentication: {}",
        run.stderr
    );
    assert!(
        run.podman_log.lines().any(|line| line.contains("just anvil-clippy anvil-aprz")),
        "all arguments must still be forwarded to the requested recipe: {}",
        run.podman_log
    );
    assert_eq!(
        run.podman_log.lines().filter(|line| line.starts_with("run ")).count(),
        1,
        "a forwarded parameter must not cause an isolated anvil-aprz invocation"
    );
}

#[test]
fn powershell_just_dispatch_treats_interpolated_values_as_data() {
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
$AnvilContainerBuildArgs = @('--label', 'build-marker=1')
$AnvilContainerRunArgs = @('--label', 'run-marker=1')
$AnvilContainerCleanup = { Add-Content -LiteralPath $env:FAKE_TEST_LOG -Value 'cleanup-ran' }
"#;
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(
        run.status.success(),
        "cold run must succeed: stderr={}\npodman.log={}",
        run.stderr,
        run.podman_log
    );
    assert!(run.test_log.contains("exists=False"), "log: {}", run.test_log);
    assert!(run.test_log.contains("recipes=anvil-clippy"), "log: {}", run.test_log);
    assert!(run.test_log.contains("windows=True"), "log: {}", run.test_log);
    assert!(run.test_log.contains("repo-is-dir=True"), "log: {}", run.test_log);
    assert!(run.test_log.contains("dir-is-container-dir=True"), "log: {}", run.test_log);
    // Build-phase arguments must only appear on the `build` invocation, and
    // run-phase arguments only on the `run` invocation: phases stay isolated.
    let build_line = run
        .podman_log
        .lines()
        .find(|line| line.starts_with("build "))
        .unwrap_or_else(|| panic!("expected a podman build invocation, got: {}", run.podman_log));
    assert!(build_line.contains("build-marker=1"), "line: {build_line}");
    assert!(!build_line.contains("run-marker=1"), "line: {build_line}");
    let run_line = run
        .podman_log
        .lines()
        .find(|line| line.starts_with("run ") && line.contains("just anvil-clippy"))
        .unwrap_or_else(|| panic!("expected a podman run invocation, got: {}", run.podman_log));
    assert!(run_line.contains("run-marker=1"), "line: {run_line}");
    assert!(!run_line.contains("build-marker=1"), "line: {run_line}");
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
    let run = run_driver(root, customize, "anvil-clippy", &[("FAKE_PODMAN_IMAGE_EXISTS", "1")]);

    assert!(
        run.status.success(),
        "warm run must succeed: stderr={}\npodman.log={}",
        run.stderr,
        run.podman_log
    );
    assert!(run.test_log.contains("exists=True"), "log: {}", run.test_log);
    assert!(
        !run.podman_log.lines().any(|line| line.starts_with("build ")),
        "a warm run (matching image already present) must not invoke podman build: {}",
        run.podman_log
    );
}

#[test]
fn prepare_args_without_a_prepare_command_are_rejected_before_podman_runs() {
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
        !run.podman_log
            .lines()
            .any(|line| line.starts_with("build ") || line.starts_with("run ")),
        "validation must fail before any Podman build or run invocation \
         (version/image-exists checks happen earlier and are expected): {}",
        run.podman_log
    );
}

#[test]
fn null_array_output_is_rejected_before_podman_runs() {
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
        !run.podman_log
            .lines()
            .any(|line| line.starts_with("build ") || line.starts_with("run ")),
        "validation must fail before any Podman build or run invocation \
         (version/image-exists checks happen earlier and are expected): {}",
        run.podman_log
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
            ("FAKE_PODMAN_IMAGE_EXISTS", "1"), // warm run: only the main recipe container executes.
            ("FAKE_PODMAN_FAIL_MARKER", "run-marker=1"),
        ],
    );

    assert!(!run.status.success(), "the driver must surface the recipe failure");
    assert!(
        run.test_log.contains("cleanup-ran"),
        "cleanup must still run after an ordinary recipe failure: {}",
        run.test_log
    );
}
