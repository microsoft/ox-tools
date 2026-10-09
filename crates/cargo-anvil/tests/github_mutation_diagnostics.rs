// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(unix, not(miri)))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![expect(clippy::unwrap_used, reason = "integration tests use panic-on-failure assertions")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use tempfile::TempDir;

const ACTION: &str = include_str!("../templates/github/run-group-action.yml");

fn run_group(group: &str, runner_os: &str, exit_code: i32) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let just = bin.join("just");
    fs::write(
        &just,
        r#"#!/usr/bin/env bash
ulimit -c > "$CORE_LOG"
if [[ "$FAKE_JUST_EXIT" != 0 ]]; then
  echo 'error: recipe `anvil-mutants-diff` failed with exit code 23'
fi
sleep 0.1
exit "$FAKE_JUST_EXIT"
"#,
    )
    .unwrap();
    fs::set_permissions(&just, fs::Permissions::from_mode(0o755)).unwrap();
    let run = ACTION
        .split_once("      run: |\n")
        .unwrap()
        .1
        .split_once("\n    - name: Upload mutation diagnostics")
        .unwrap()
        .0
        .lines()
        .map(|line| line.strip_prefix("        ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap();
    let output = Command::new("bash")
        .args(["-e", "-o", "pipefail", "-c", &run])
        .current_dir(root)
        .env("PATH", path)
        .env("ANVIL_GROUP", group)
        .env("RUNNER_OS", runner_os)
        .env("RUNNER_TEMP", root)
        .env("GITHUB_OUTPUT", root.join("outputs"))
        .env("CORE_LOG", root.join("core-limit"))
        .env("FAKE_JUST_EXIT", exit_code.to_string())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit_code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let outputs = fs::read_to_string(root.join("outputs")).unwrap();
    assert!(outputs.contains(&format!("exit_code={exit_code}\n")));
    if exit_code != 0 {
        assert!(outputs.contains("failed_recipe=anvil-mutants-diff\n"));
    }
    let resources = root.join(format!("anvil-{group}-resources.log"));
    if group == "pr-mutants" && runner_os == "Linux" {
        assert_eq!(fs::read_to_string(root.join("core-limit")).unwrap().trim(), "0");
        assert!(fs::read_to_string(resources).unwrap().contains("[anvil-resources]"));
    } else {
        assert!(!resources.exists());
    }
}

#[test]
fn mutation_diagnostics_preserve_success() {
    run_group("pr-mutants", "Linux", 0);
}

#[test]
fn mutation_diagnostics_preserve_recipe_failure() {
    run_group("pr-mutants", "Linux", 23);
}

#[test]
fn other_groups_do_not_start_mutation_diagnostics() {
    run_group("pr-test", "Linux", 0);
}

#[test]
fn non_linux_mutation_groups_do_not_start_resource_sampling() {
    run_group("pr-mutants", "Windows", 23);
}
