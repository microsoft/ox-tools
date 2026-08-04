// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))] // exercises the real pwsh driver against a fake `wsl`; miri can't sandbox this.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Driver-level verification of the declarative container contract from
//! [container-config.md](../docs/design/container-config.md).
//!
//! Generates the real `.anvil/container/` tree, then runs the generated
//! `run-in-container.ps1` against a fake `wsl` whose `wslpath` performs real
//! drive-letter translation, so mount-source translation is exercised rather
//! than stubbed.
//!
//! The Bash mirror lives in `container_config_bash.rs` and runs on Unix.

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

/// A repository whose `.anvil/config.toml` is written *before* generation, so
/// the generated tree and the coherence record agree.
fn repo_with_config(config: &str) -> TempDir {
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
    if !config.is_empty() {
        write(&root.join(".anvil/config.toml"), config);
    }
    run_update(&Catalog::anvil(), &local(), root).unwrap();

    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git must be available");
    assert!(status.success(), "temporary Git repository must initialize");
    tmp
}

/// A fake `wsl` whose `wslpath` performs real drive-letter translation, so the
/// driver's own path handling is exercised end to end.
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
    fn line_containing(&self, needle: &str) -> &str {
        self.docker_log
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("expected a docker line containing {needle}: {}", self.docker_log))
    }

    fn container_count(&self) -> usize {
        self.docker_log.lines().filter(|line| line.starts_with("run ")).count()
    }
}

fn run_driver(root: &Path) -> DriverRun {
    run_driver_args(root, &["anvil-clippy"])
}

fn run_driver_args(root: &Path, args: &[&str]) -> DriverRun {
    let bin_dir = root.join("fake-bin");
    install_fake_wsl(&bin_dir);
    let docker_log = bin_dir.join("docker.log");
    let _ = std::fs::remove_file(&docker_log);
    let path = format!("{};{}", bin_dir.display(), std::env::var("PATH").unwrap_or_default());

    let output = Command::new("pwsh")
        .args(["-NoProfile", "-File", ".anvil/container/run-in-container.ps1"])
        .args(args)
        .current_dir(root)
        .env("PATH", path)
        .env("FAKE_DOCKER_LOG", &docker_log)
        .env_remove("ANVIL_IN_CONTAINER")
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

const CACHE_CONFIG: &str = r#"
[[container.cache]]
name = "pip"
target = "/tmp/anvil-user/.cache/pip"
scope = "worktree"

[[container.cache]]
name = "tools"
target = "/tmp/anvil-user/.cache/tools"
scope = "global"
"#;

#[test]
fn a_repository_declaring_nothing_creates_exactly_the_anvil_owned_volumes() {
    let tmp = repo_with_config("");
    let run = run_driver(tmp.path());
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    assert!(
        !tmp.path().join(".anvil/container/runtime.conf").exists(),
        "no configuration means no runtime file"
    );
    let volumes = run.docker_log.lines().filter(|line| line.starts_with("volume create")).count();
    assert_eq!(volumes, 3, "only the anvil-owned volumes: {}", run.docker_log);
    assert!(!run.docker_log.contains("anvil-cache-"), "no declared caches: {}", run.docker_log);
}

#[test]
fn cache_scope_decides_what_the_volume_name_encodes() {
    let tmp = repo_with_config(CACHE_CONFIG);
    let run = run_driver(tmp.path());
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let worktree = run.line_containing("io.github.cargo-anvil.cache=pip");
    let global = run.line_containing("io.github.cargo-anvil.cache=tools");

    assert!(
        global.trim_end().ends_with("anvil-cache-tools"),
        "a global cache name must be bare: {global}"
    );
    let name = worktree.split_whitespace().next_back().expect("volume name is the last argument");
    assert!(name.starts_with("anvil-cache-pip-"), "got: {name}");
    assert_eq!(name.matches('-').count(), 3, "no image segment in a worktree cache: {name}");

    for line in [worktree, global] {
        assert!(
            line.contains("io.github.cargo-anvil.scope="),
            "labels let lifecycle commands prune: {line}"
        );
        assert!(line.contains("io.github.cargo-anvil.worktree="), "got: {line}");
    }
}

#[test]
fn declared_cache_targets_are_ownership_initialized_without_a_shell() {
    let tmp = repo_with_config(CACHE_CONFIG);
    let run = run_driver(tmp.path());
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let ownership = run.line_containing("chown");
    assert!(!ownership.contains("sh -c"), "no shell string: {ownership}");
    assert!(
        !ownership.contains("type=bind"),
        "the root container mounts no host path: {ownership}"
    );
    for target in [
        "/usr/local/cargo/registry",
        "/tmp/anvil-user/.cache/pip",
        "/tmp/anvil-user/.cache/tools",
    ] {
        assert!(ownership.contains(target), "missing {target} in: {ownership}");
    }
}

/// The Windows driver must translate a resolved host path into WSL before
/// handing it to Docker, exactly as it already does for the repository root.
#[test]
fn host_mount_sources_are_translated_into_wsl_paths() {
    let tmp =
        repo_with_config("[[container.mount]]\nname = \"fixtures\"\nsource = { repository = \"crates/alpha\" }\ntarget = \"/fixtures\"\n");
    let run = run_driver(tmp.path());
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let recipe = run.line_containing("just anvil-clippy");
    assert!(recipe.contains("target=/fixtures,readonly"), "read-only is the default: {recipe}");
    assert!(
        recipe.contains("source=/mnt/"),
        "a Windows path must be translated for Docker: {recipe}"
    );
    assert!(
        !recipe.contains("source=C:"),
        "an untranslated Windows path must never reach Docker: {recipe}"
    );

    let ownership = run.line_containing("chown");
    assert!(
        !ownership.contains("/fixtures"),
        "host mounts never reach the root container: {ownership}"
    );
}

#[test]
fn a_read_write_mount_is_honored_when_explicitly_declared() {
    let tmp = repo_with_config(
        "[[container.mount]]\nname = \"out\"\nsource = { repository = \"crates\" }\ntarget = \"/out\"\nmode = \"read-write\"\n",
    );
    let run = run_driver(tmp.path());
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let recipe = run.line_containing("just anvil-clippy");
    assert!(recipe.contains("target=/out"), "got: {recipe}");
    assert!(
        !recipe.contains("target=/out,readonly"),
        "read-write must not be downgraded: {recipe}"
    );
}

#[test]
fn a_missing_mount_source_fails_before_any_container_starts() {
    let tmp = repo_with_config("[[container.mount]]\nname = \"absent\"\nsource = { sibling = \"not-there\" }\ntarget = \"/absent\"\n");
    let run = run_driver(tmp.path());
    assert!(!run.status.success(), "a missing source must be refused");
    // PowerShell wraps long error text across lines, so match a fragment that
    // survives wrapping rather than the whole sentence.
    assert!(run.stderr.contains("does not exist"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

#[test]
fn an_edited_configuration_that_was_not_regenerated_is_refused() {
    let tmp = repo_with_config(CACHE_CONFIG);
    let config = tmp.path().join(".anvil/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[[container.cache]]\nname = \"extra\"\ntarget = \"/tmp/extra\"\n");
    write(&config, &text);

    let run = run_driver(tmp.path());
    assert!(!run.status.success(), "a stale runtime file must be refused");
    assert!(run.stderr.contains("out of date"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

/// The coherence record covers derived artifacts, not just the input. A
/// `Containerfile` anvil left alone because the user modified it would
/// otherwise pass an input-only guard while missing a declared package.
#[test]
fn a_locally_modified_containerfile_is_refused() {
    let tmp = repo_with_config(CACHE_CONFIG);
    let containerfile = tmp.path().join(".anvil/container/Containerfile");
    let mut text = std::fs::read_to_string(&containerfile).unwrap();
    text.push_str("RUN echo local-edit\n");
    write(&containerfile, &text);

    let run = run_driver(tmp.path());
    assert!(!run.status.success(), "a stale Containerfile must be refused");
    assert!(run.stderr.contains("out of date"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

#[test]
fn a_configuration_added_without_regenerating_is_refused() {
    let tmp = repo_with_config("");
    write(
        &tmp.path().join(".anvil/config.toml"),
        "[[container.cache]]\nname = \"pip\"\ntarget = \"/tmp/pip\"\n",
    );

    let run = run_driver(tmp.path());
    assert!(!run.status.success(), "adding a configuration must be detected");
    assert!(run.stderr.contains("out of date"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

#[test]
fn a_deleted_configuration_leaving_an_orphaned_runtime_file_is_refused() {
    let tmp = repo_with_config(CACHE_CONFIG);
    std::fs::remove_file(tmp.path().join(".anvil/config.toml")).unwrap();

    let run = run_driver(tmp.path());
    assert!(!run.status.success(), "an orphaned runtime file must be detected");
    assert!(run.stderr.contains("out of date"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

const COMMAND_CONFIG: &str = r#"
[[container.command]]
name = "build-image"
recipe = "build-service-image"
workdir = "crates/alpha"

[[container.command.arg]]
name = "tag"
type = "token"

[[container.command.arg]]
name = "mode"
type = "enum"
values = ["fast", "slow"]
required = false
"#;

#[test]
fn anvil_recipes_still_dispatch_unchanged_when_commands_are_registered() {
    let tmp = repo_with_config(COMMAND_CONFIG);
    let run = run_driver_args(tmp.path(), &["anvil-clippy", "anvil-fmt"]);
    assert!(run.status.success(), "driver failed: {}", run.stderr);
    assert!(
        run.line_containing("just anvil-clippy").contains("just anvil-clippy anvil-fmt"),
        "generated recipes keep today's behavior: {}",
        run.docker_log
    );
}

#[test]
fn a_registered_command_resolves_to_its_recipe_and_working_directory() {
    let tmp = repo_with_config(COMMAND_CONFIG);
    let run = run_driver_args(tmp.path(), &["build-image", "v1.2.3"]);
    assert!(run.status.success(), "driver failed: {}", run.stderr);

    let invocation = run.line_containing("build-service-image");
    // `--` stops a value ever being read as a just option.
    assert!(invocation.contains("just build-service-image -- v1.2.3"), "got: {invocation}");
    assert!(
        invocation.contains("--workdir /workspace/crates/alpha"),
        "the declared workdir must apply: {invocation}"
    );
}

#[test]
fn an_optional_argument_may_be_omitted_or_supplied() {
    let tmp = repo_with_config(COMMAND_CONFIG);
    for (args, expected) in [
        (vec!["build-image", "v1"], "just build-service-image -- v1"),
        (vec!["build-image", "v1", "fast"], "just build-service-image -- v1 fast"),
    ] {
        let run = run_driver_args(tmp.path(), &args);
        assert!(run.status.success(), "driver failed for {args:?}: {}", run.stderr);
        assert!(
            run.line_containing("build-service-image").contains(expected),
            "got: {}",
            run.docker_log
        );
    }
}

#[test]
fn an_unregistered_name_is_refused_before_any_container_starts() {
    let tmp = repo_with_config(COMMAND_CONFIG);
    let run = run_driver_args(tmp.path(), &["deploy-thing"]);
    assert!(!run.status.success(), "an unregistered name must be refused");
    assert!(run.stderr.contains("registered command"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}

#[test]
fn argument_count_mismatches_are_refused_before_any_container_starts() {
    let tmp = repo_with_config(COMMAND_CONFIG);
    for args in [vec!["build-image"], vec!["build-image", "v1", "fast", "extra"]] {
        let run = run_driver_args(tmp.path(), &args);
        assert!(!run.status.success(), "{args:?} must be refused");
        assert!(run.stderr.contains("argument"), "got: {}", run.stderr);
        assert_eq!(run.container_count(), 0, "no container may start for {args:?}");
    }
}

/// The four argument types must accept and reject identically on both hosts,
/// which is why they are a closed set rather than author-supplied patterns.
#[test]
fn argument_types_are_enforced() {
    let tmp = repo_with_config(COMMAND_CONFIG);

    let run = run_driver_args(tmp.path(), &["build-image", "v1", "medium"]);
    assert!(!run.status.success(), "an out-of-set enum value must be refused");
    assert!(run.stderr.contains("must be one of"), "got: {}", run.stderr);
    assert_eq!(run.container_count(), 0);

    let run = run_driver_args(tmp.path(), &["build-image", "-oops"]);
    assert!(!run.status.success(), "a token may not start with a hyphen");
    assert_eq!(run.container_count(), 0);
}

#[test]
fn a_path_argument_may_not_escape_the_worktree() {
    let tmp = repo_with_config(
        "[[container.command]]\nname = \"pack\"\nrecipe = \"pack\"\n\n[[container.command.arg]]\nname = \"dir\"\ntype = \"path\"\n",
    );
    let run = run_driver_args(tmp.path(), &["pack", "crates/alpha"]);
    assert!(run.status.success(), "an in-worktree path is accepted: {}", run.stderr);

    let run = run_driver_args(tmp.path(), &["pack", "../outside"]);
    assert!(!run.status.success(), "an escaping path must be refused");
    assert_eq!(run.container_count(), 0, "no container may start: {}", run.docker_log);
}
