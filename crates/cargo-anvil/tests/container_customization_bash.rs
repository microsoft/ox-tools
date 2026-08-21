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

//! Driver-level verification of the `customize.sh` runtime contract from
//! [`containers.md`](../docs/design/containers.md#8-container-customization).
//!
//! This is the Bash mirror of `container_customization.rs`'s `PowerShell`
//! driver tests. It generates the real `.anvil/container/` tree
//! with [`cargo_anvil::test_support::run_update`], then runs the generated
//! `run-in-container.sh` against a fake `docker` on `PATH` so the driver's
//! own process, argument construction, and validation execute for real —
//! including with the default (customize.sh-empty) arrays, which is the
//! condition that regressed under Bash 3.2 / Bash <4.4 `set -u` semantics.
//! `anvil-clippy` is used throughout so the GitHub-token path (which would
//! also require a fake `gh`) is never exercised.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
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

fn write_executable(path: &Path, contents: &str) {
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

/// A repository with the public container tree generated and no derived
/// catalog involved, proving the driver loads `customize.sh` purely by
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
        !root.join(".anvil/container/customize.sh").exists(),
        "the public catalog must not emit customize.sh by default"
    );
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git must be available");
    assert!(status.success(), "temporary Git repository must initialize");
    tmp
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
    version)
        echo '26.1.5'
        exit 0
        ;;
    image)
        if [[ "${FAKE_DOCKER_IMAGE_EXISTS:-}" == "1" ]]; then exit 0; else exit 1; fi
        ;;
    build)
        exit "${FAKE_DOCKER_BUILD_EXIT:-0}"
        ;;
    volume)
        exit 0
        ;;
    run)
        joined="$*"
        if [[ -n "${FAKE_DOCKER_FAIL_MARKER:-}" && "$joined" == *"$FAKE_DOCKER_FAIL_MARKER"* ]]; then
            exit 1
        fi
        exit 0
        ;;
    *)
        exit 0
        ;;
esac
"#,
    );
}

struct DriverRun {
    status: std::process::ExitStatus,
    stderr: String,
    docker_log: String,
    test_log: String,
}

fn token_source_paths(docker_log: &str) -> Vec<std::path::PathBuf> {
    docker_log
        .lines()
        .flat_map(str::split_whitespace)
        .filter_map(|argument| {
            argument.strip_prefix("type=bind,source=").and_then(|mount| {
                mount
                    .split_once(",target=/run/secrets/anvil-github-token")
                    .map(|(source, _)| source)
            })
        })
        .map(Into::into)
        .collect()
}

fn assert_token_files_removed(run: &DriverRun) {
    let paths = token_source_paths(&run.docker_log);
    assert!(!paths.is_empty(), "expected a GitHub token mount: {}", run.docker_log);
    for path in paths {
        assert!(!path.exists(), "temporary GitHub token file was not removed: {}", path.display());
    }
}

fn created_volumes(docker_log: &str) -> BTreeSet<String> {
    docker_log
        .lines()
        .filter_map(|line| line.strip_prefix("volume create "))
        .map(str::to_owned)
        .collect()
}

/// Runs the real generated `run-in-container.sh` against the fake `docker`,
/// with `customize.sh` written from `customize_sh_body` beforehand.
fn run_driver(root: &Path, customize_sh_body: &str, recipe: &str, env: &[(&str, &str)]) -> DriverRun {
    run_driver_args(root, customize_sh_body, &[recipe], env)
}

fn run_driver_args(root: &Path, customize_sh_body: &str, recipe_args: &[&str], env: &[(&str, &str)]) -> DriverRun {
    run_driver_maybe_customized(root, Some(customize_sh_body), recipe_args, env)
}

/// Runs the driver with no `customize.sh` at the current location, so the
/// stranded-legacy-file detection is observable.
fn run_driver_without_customization(root: &Path, recipe: &str, env: &[(&str, &str)]) -> DriverRun {
    run_driver_maybe_customized(root, None, &[recipe], env)
}

fn run_driver_maybe_customized(root: &Path, customize_sh_body: Option<&str>, recipe_args: &[&str], env: &[(&str, &str)]) -> DriverRun {
    let customize = root.join(".anvil/container/customize.sh");
    match customize_sh_body {
        Some(body) => write(&customize, body),
        None => drop(std::fs::remove_file(&customize)),
    }

    let bin_dir = root.join("fake-bin");
    install_fake_docker(&bin_dir);

    let docker_log = root.join("docker.log");
    let test_log = root.join("test.log");
    let _ = std::fs::remove_file(&docker_log);
    let _ = std::fs::remove_file(&test_log);
    let path = format!("{}:{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let mut command = Command::new("bash");
    command
        .arg(".anvil/container/run-in-container.sh")
        .args(recipe_args)
        .current_dir(root)
        .env("PATH", path)
        .env("FAKE_DOCKER_LOG", &docker_log)
        .env("FAKE_TEST_LOG", &test_log)
        .env_remove("ANVIL_IN_CONTAINER")
        .env_remove("GITHUB_TOKEN")
        .env_remove("ANVIL_CONTAINER_BASE_IMAGE")
        .env_remove("ANVIL_CONTAINER_IMAGE")
        .env_remove("ANVIL_CONTAINER_NO_REBUILD");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("bash must be available to run the driver");

    DriverRun {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        docker_log: std::fs::read_to_string(&docker_log).unwrap_or_default(),
        test_log: std::fs::read_to_string(&test_log).unwrap_or_default(),
    }
}

#[test]
fn a_customization_file_stranded_at_the_pre_move_path_is_reported_and_not_sourced() {
    // Container assets moved from justfiles/anvil/container/ to
    // .anvil/container/. A hand-authored customization file is not
    // catalog-tracked, so `cargo anvil` cannot relocate it; the driver must
    // say so instead of silently running without it.
    let tmp = repo_with_container();
    write(
        &tmp.path().join("justfiles/anvil/container/customize.sh"),
        "ANVIL_CONTAINER_RUN_ARGS=(--label stranded=1)\n",
    );

    let run = run_driver_without_customization(tmp.path(), "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);

    assert!(run.status.success(), "the run must still proceed: stderr={}", run.stderr);
    assert!(
        run.stderr.contains("justfiles/anvil/container/customize.sh") && run.stderr.contains(".anvil/container/customize.sh"),
        "the stranded file and its new home must both be named: stderr={}",
        run.stderr
    );
    assert!(
        !run.docker_log.contains("stranded=1"),
        "the stranded file must not be sourced: docker.log={}",
        run.docker_log
    );
}

#[test]
fn a_customization_file_at_the_current_path_wins_without_a_migration_warning() {
    let tmp = repo_with_container();
    // A stale copy at the old path must be inert, not a second source.
    write(
        &tmp.path().join("justfiles/anvil/container/customize.sh"),
        "echo 'the pre-move path must never be sourced' >&2\nexit 1\n",
    );

    let run = run_driver(
        tmp.path(),
        "ANVIL_CONTAINER_RUN_ARGS=(--label current=1)\n",
        "anvil-clippy",
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1")],
    );

    assert!(run.status.success(), "the run must succeed: stderr={}", run.stderr);
    assert!(
        !run.stderr.contains("justfiles/anvil/container/customize.sh"),
        "no migration warning is due when the current path is populated: stderr={}",
        run.stderr
    );
    assert!(
        run.docker_log.contains("current=1"),
        "the current customization must take effect: docker.log={}",
        run.docker_log
    );
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
        "GITHUB_TOKEN=custom-token\n",
        "anvil-aprz",
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1")],
    );

    assert!(run.status.success(), "custom authentication must be accepted: {}", run.stderr);
    assert!(
        run.docker_log.contains("/run/secrets/anvil-github-token"),
        "the customization-provided token must be mounted for APRZ: {}",
        run.docker_log
    );
    assert_eq!(
        run.docker_log
            .lines()
            .filter(|line| line.starts_with("run ") && line.contains("just anvil-aprz"))
            .count(),
        1,
        "direct anvil-aprz must run exactly one recipe container: {}",
        run.docker_log
    );
    assert_token_files_removed(&run);
}

#[test]
fn customization_can_extend_aprz_classification() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "ANVIL_CONTAINER_NEEDS_GITHUB_TOKEN=true\n",
        "anvil-clippy",
        &[("FAKE_DOCKER_IMAGE_EXISTS", "1"), ("GITHUB_TOKEN", "test-token")],
    );

    assert!(run.status.success(), "custom APRZ classification failed: {}", run.stderr);
    assert!(
        run.docker_log.lines().any(|line| line.contains("just anvil-aprz")),
        "custom classification must trigger isolated APRZ: {}",
        run.docker_log
    );
    assert!(
        run.docker_log
            .lines()
            .any(|line| line.contains("ANVIL_APRZ_ALREADY_RAN=1") && line.contains("just anvil-clippy")),
        "the requested recipe must run after APRZ completion: {}",
        run.docker_log
    );
}

#[test]
fn every_requested_argument_must_be_an_anvil_recipe() {
    let tmp = repo_with_container();
    let run = run_driver_args(tmp.path(), "", &["anvil-clippy", "not-anvil"], &[]);

    assert!(!run.status.success(), "invalid later recipe must fail");
    assert!(
        run.stderr.contains("expected each argument to be an anvil-* recipe"),
        "stderr must explain the command contract: {}",
        run.stderr
    );
    assert!(
        run.docker_log.is_empty(),
        "validation must happen before Docker: {}",
        run.docker_log
    );
}

#[test]
fn base_image_override_is_digest_pinned_and_passed_to_build() {
    let tmp = repo_with_container();
    let base_image = "example.invalid/bullseye@sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let run = run_driver(tmp.path(), "", "anvil-clippy", &[("ANVIL_CONTAINER_BASE_IMAGE", base_image)]);

    assert!(run.status.success(), "digest-pinned override failed: {}", run.stderr);
    assert!(
        run.docker_log
            .lines()
            .any(|line| line.starts_with("build ") && line.contains(&format!("BASE_IMAGE={base_image}"))),
        "Docker build must receive the selected base image: {}",
        run.docker_log
    );

    let invalid = run_driver(
        tmp.path(),
        "",
        "anvil-clippy",
        &[("ANVIL_CONTAINER_BASE_IMAGE", "debian:bullseye-slim")],
    );
    assert!(!invalid.status.success(), "an unpinned base image must fail");
    assert!(invalid.stderr.contains("must be pinned by sha256 digest"));
}

#[test]
fn aggregate_recipe_isolates_the_token_from_the_main_container() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "",
        "_anvil-scheduled",
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
        .find(|line| line.contains("just _anvil-scheduled"))
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
    for marker in ["just anvil-aprz", "just _anvil-scheduled"] {
        let tmp = repo_with_container();
        let run = run_driver(
            tmp.path(),
            "",
            "_anvil-scheduled",
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
fn cold_run_with_empty_default_arrays_exposes_contract_inputs_scopes_phases_and_runs_cleanup() {
    let tmp = repo_with_container();
    let root = tmp.path();
    // Deliberately leaves ANVIL_CONTAINER_BUILD_ARGS/PREPARE_ARGS/PREPARE_COMMAND/RUN_ARGS
    // at their script-provided empty defaults, which is exactly the state
    // that broke under Bash 3.2 / Bash <4.4 `set -u` semantics.
    let customize = r#"
printf 'exists=%s\n' "$ANVIL_CONTAINER_IMAGE_EXISTS" >> "$FAKE_TEST_LOG"
printf 'recipes=%s\n' "${ANVIL_CONTAINER_REQUESTED_RECIPES[*]}" >> "$FAKE_TEST_LOG"
printf 'repo-is-dir=%s\n' "$([[ -d "$ANVIL_CONTAINER_REPO_ROOT" ]] && echo true || echo false)" >> "$FAKE_TEST_LOG"
printf 'dir-is-container-dir=%s\n' "$([[ "$ANVIL_CONTAINER_DIR" == "$ANVIL_CONTAINER_REPO_ROOT/.anvil/container" ]] && echo true || echo false)" >> "$FAKE_TEST_LOG"
anvil_test_cleanup() { printf 'cleanup-ran\n' >> "$FAKE_TEST_LOG"; }
ANVIL_CONTAINER_CLEANUP=anvil_test_cleanup
"#;
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(
        run.status.success(),
        "cold run with empty default arrays must succeed: stderr={}\ndocker.log={}",
        run.stderr,
        run.docker_log
    );
    assert!(run.test_log.contains("exists=false"), "log: {}", run.test_log);
    assert!(run.test_log.contains("recipes=anvil-clippy"), "log: {}", run.test_log);
    assert!(run.test_log.contains("repo-is-dir=true"), "log: {}", run.test_log);
    assert!(run.test_log.contains("dir-is-container-dir=true"), "log: {}", run.test_log);
    assert!(
        run.docker_log.lines().any(|line| line.starts_with("build ")),
        "a cold run must invoke docker build: {}",
        run.docker_log
    );
    assert!(
        run.docker_log
            .lines()
            .any(|line| line.starts_with("run ") && line.contains("just anvil-clippy")),
        "expected a docker run invocation, got: {}",
        run.docker_log
    );
    assert_eq!(
        run.docker_log.lines().filter(|line| line.starts_with("volume create ")).count(),
        3,
        "the driver must create all named cache volumes: {}",
        run.docker_log
    );
    assert!(
        run.docker_log.contains("volume create anvil-cargo-registry-") && run.docker_log.contains("volume create anvil-cargo-git-"),
        "Cargo caches must be repository-scoped: {}",
        run.docker_log
    );
    let recipe_line = run
        .docker_log
        .lines()
        .find(|line| line.starts_with("run ") && line.contains("just anvil-clippy"))
        .expect("the recipe run is asserted present above");
    assert!(
        recipe_line.contains("--user ") && !recipe_line.contains("--user 0:0"),
        "recipe execution must use the WSL/Linux user identity: {recipe_line}"
    );
    assert!(
        run.test_log.contains("cleanup-ran"),
        "cleanup must run after an ordinary successful invocation: {}",
        run.test_log
    );
}

#[test]
fn cargo_caches_are_repository_scoped_but_stable_across_image_ids() {
    let first = repo_with_container();
    let second = repo_with_container();
    let first_run = run_driver(first.path(), "", "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);
    let second_run = run_driver(second.path(), "", "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);

    let first_volumes = created_volumes(&first_run.docker_log);
    let second_volumes = created_volumes(&second_run.docker_log);
    let first_registry = first_volumes
        .iter()
        .find(|name| name.starts_with("anvil-cargo-registry-"))
        .expect("first repository must create a registry cache");
    let second_registry = second_volumes
        .iter()
        .find(|name| name.starts_with("anvil-cargo-registry-"))
        .expect("second repository must create a registry cache");
    let first_git = first_volumes
        .iter()
        .find(|name| name.starts_with("anvil-cargo-git-"))
        .expect("first repository must create a Git cache");
    let second_git = second_volumes
        .iter()
        .find(|name| name.starts_with("anvil-cargo-git-"))
        .expect("second repository must create a Git cache");
    assert_ne!(first_registry, second_registry);
    assert_ne!(first_git, second_git);

    write(&first.path().join("justfiles/anvil/versions.just"), "changed := \"1\"\n");
    let changed_run = run_driver(first.path(), "", "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);
    let changed_volumes = created_volumes(&changed_run.docker_log);
    assert!(changed_volumes.contains(first_registry));
    assert!(changed_volumes.contains(first_git));
    assert_ne!(
        first_volumes.iter().find(|name| name.starts_with("anvil-target-")),
        changed_volumes.iter().find(|name| name.starts_with("anvil-target-"))
    );
}

#[test]
fn warm_run_skips_the_build_and_still_reports_image_exists() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r#"
printf 'exists=%s\n' "$ANVIL_CONTAINER_IMAGE_EXISTS" >> "$FAKE_TEST_LOG"
"#;
    let run = run_driver(root, customize, "anvil-clippy", &[("FAKE_DOCKER_IMAGE_EXISTS", "1")]);

    assert!(
        run.status.success(),
        "warm run must succeed: stderr={}\ndocker.log={}",
        run.stderr,
        run.docker_log
    );
    assert!(run.test_log.contains("exists=true"), "log: {}", run.test_log);
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
ANVIL_CONTAINER_PREPARE_ARGS=(--label 'prepare-marker=1')
";
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(!run.status.success(), "prepare args without a prepare command must fail validation");
    assert!(
        run.stderr
            .contains("ANVIL_CONTAINER_PREPARE_ARGS requires ANVIL_CONTAINER_PREPARE_COMMAND"),
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
fn scalar_output_redeclaration_is_rejected_before_docker_runs() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "unset ANVIL_CONTAINER_RUN_ARGS\nANVIL_CONTAINER_RUN_ARGS=--label\n",
        "anvil-clippy",
        &[],
    );

    assert!(!run.status.success(), "a scalar output must fail validation");
    assert!(
        run.stderr.contains("ANVIL_CONTAINER_RUN_ARGS must be a string array"),
        "stderr must name the invalid output: {}",
        run.stderr
    );
}

#[test]
fn content_changing_build_arguments_are_rejected() {
    let tmp = repo_with_container();
    let run = run_driver(
        tmp.path(),
        "ANVIL_CONTAINER_BUILD_ARGS=(--build-arg BASE_IMAGE=example.invalid/base)\n",
        "anvil-clippy",
        &[],
    );

    assert!(!run.status.success(), "content-changing build arguments must fail validation");
    assert!(
        run.stderr
            .contains("ANVIL_CONTAINER_BUILD_ARGS accepts only BuildKit --secret arguments"),
        "stderr must explain the image-identity restriction: {}",
        run.stderr
    );
}

#[test]
fn cleanup_still_runs_after_the_main_recipe_container_fails() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r#"
ANVIL_CONTAINER_RUN_ARGS=(--label 'run-marker=1')
anvil_test_cleanup() { printf 'cleanup-ran\n' >> "$FAKE_TEST_LOG"; }
ANVIL_CONTAINER_CLEANUP=anvil_test_cleanup
"#;
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

#[test]
fn build_and_run_phase_arguments_stay_isolated() {
    let tmp = repo_with_container();
    let root = tmp.path();
    let customize = r"
ANVIL_CONTAINER_BUILD_ARGS=(--secret 'id=build-marker,src=fake')
ANVIL_CONTAINER_RUN_ARGS=(--label 'run-marker=1')
";
    let run = run_driver(root, customize, "anvil-clippy", &[]);

    assert!(
        run.status.success(),
        "cold run must succeed: stderr={}\ndocker.log={}",
        run.stderr,
        run.docker_log
    );
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
}
