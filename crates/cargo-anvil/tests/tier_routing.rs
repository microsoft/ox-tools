// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const RUNNER: &str = include_str!("../templates/justfiles/anvil/runner.just");
const JUSTFILE: &str = "routing.just";

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

fn fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    write(&tmp.path().join("runner.just"), RUNNER);
    let justfile = r#"
import 'runner.just'

runner := env_var_or_default("ANVIL_RUNNER", "native")

default: (_anvil-run "pr" runner)
failure: (_anvil-run "fail" runner)

[private]
_anvil-pr: first second

[private]
_anvil-fail: first failing

[windows]
[script("pwsh")]
first:
    Write-Output first

[windows]
[script("pwsh")]
second:
    Write-Output second

[windows]
[script("pwsh")]
failing:
    Write-Output failing
    exit 7

[windows]
[script("pwsh")]
anvil-container *recipe:
    Write-Output 'container:{{ recipe }}'

[unix]
first:
    @printf 'first\n'

[unix]
second:
    @printf 'second\n'

[unix]
failing:
    @printf 'failing\n'
    @exit 7

[unix]
anvil-container *recipe:
    @printf 'container:%s\n' '{{ recipe }}'
"#;
    write(&tmp.path().join(JUSTFILE), justfile);
    tmp
}

fn just_available() -> bool {
    Command::new("just").arg("--version").output().is_ok()
}

fn run(root: &Path, recipes: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command.args(["--justfile", root.join(JUSTFILE).to_str().unwrap()]);
    command.args(recipes).current_dir(root);
    command.env_remove("ANVIL_RUNNER").env_remove("ANVIL_IN_CONTAINER");
    command.envs(environment.iter().copied());
    command.output().expect("just is required to verify generated tier routing")
}

#[test]
fn native_routing_preserves_output_and_exit_status() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    let direct = run(tmp.path(), &["_anvil-pr"], &[]);
    let routed = run(tmp.path(), &["default"], &[]);

    assert_eq!(routed.status.code(), direct.status.code());
    assert_eq!(routed.stdout, direct.stdout);
    assert_eq!(routed.stderr, direct.stderr);
}

#[test]
fn native_routing_preserves_failure_output_and_exit_status() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    let direct = run(tmp.path(), &["_anvil-fail"], &[]);
    let routed = run(tmp.path(), &["failure"], &[]);

    assert_eq!(direct.status.code(), Some(7));
    assert_eq!(routed.status.code(), direct.status.code());
    assert_eq!(routed.stdout, direct.stdout);
    assert_eq!(routed.stderr, direct.stderr);
}

#[test]
fn configured_container_routing_uses_the_container_recipe() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    let output = run(tmp.path(), &["default"], &[("ANVIL_RUNNER", "container")]);

    assert!(
        output.status.success(),
        "container route failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "container:_anvil-pr");
}

#[test]
fn in_container_forces_native_execution() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    let output = run(
        tmp.path(),
        &["default"],
        &[("ANVIL_RUNNER", "container"), ("ANVIL_IN_CONTAINER", "1")],
    );

    assert!(
        output.status.success(),
        "native recursion guard failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn invalid_runner_value_fails_instead_of_falling_back_to_native() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    let output = run(tmp.path(), &["default"], &[("ANVIL_RUNNER", "Container")]);

    assert!(
        !output.status.success(),
        "invalid runner unexpectedly succeeded: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected 'native' or 'container'"),
        "invalid runner error must be actionable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
