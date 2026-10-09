// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(unix, not(miri)))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![expect(clippy::unwrap_used, reason = "integration tests use panic-on-failure assertions")]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

const ACTION: &str = include_str!("../templates/github/run-group-action.yml");

fn run_script() -> String {
    ACTION
        .split_once("      run: |\n")
        .unwrap()
        .1
        .split_once("\n    - name: Upload mutation diagnostics")
        .unwrap()
        .0
        .lines()
        .map(|line| line.strip_prefix("        ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

// Scripted probes and a FIFO-driven clock avoid host statistics and sampler races.
const FIXTURE: &str = r#"
exec 3<>sample-ready
exec 4<>sample-continue
date() { printf 'fixture-time\n'; }
free() {
  if [[ "$FAKE_FAILURE" == "probe" ]]; then
    printf 'fixture memory probe failed\n' >&2
    return 23
  fi
  printf 'fixture memory\n'
}
df() { printf 'fixture disk\n'; }
ps() { printf 'fixture process\n'; }
sysctl() { printf '|fixture-crash-handler\n'; }
sudo() {
  printf '%s\n' "$*" >> "$SYSCTL_LOG"
  if [[ "$FAKE_FAILURE" == "core-setup" && "$*" == "-n sysctl -w kernel.core_pattern=core" ]]; then
    return 23
  fi
  if [[ "$FAKE_FAILURE" == "core-restore" && "$*" != "-n sysctl -w kernel.core_pattern=core" ]]; then
    return 23
  fi
}
sleep() {
  printf 'sampled\n' >&3
  read -r -t 10 -u 4
}
tee() {
  if [[ "$1" == "-a" && "$FAKE_FAILURE" == "log" && ! -f log-failed ]]; then
    local line
    while IFS= read -r line; do printf '%s\n' "$line"; done
    : > log-failed
    return 23
  fi
  command tee "$@"
}
just() {
  ulimit -c > "$CORE_LOG"
  if [[ "$ANVIL_GROUP" == "pr-mutants" && "$RUNNER_OS" == "Linux" ]]; then
    local sample
    for ((sample = 0; sample < REQUIRED_SAMPLES; sample++)); do
      if ! read -r -t 10 -u 3; then
        printf 'fixture: sampler did not finish the required sample\n' >&2
        return 90
      fi
      if ((sample + 1 < REQUIRED_SAMPLES)); then
        printf 'next\n' >&4
      fi
    done
  fi
  if [[ "$FAKE_JUST_EXIT" != 0 ]]; then
    echo 'error: recipe `anvil-mutants-diff` failed with exit code 23'
  fi
  return "$FAKE_JUST_EXIT"
}
"#;

fn run_group(group: &str, runner_os: &str, runner_environment: &str, exit_code: i32, failure: &str) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    assert!(
        Command::new("mkfifo")
            .args([root.join("sample-ready"), root.join("sample-continue")])
            .status()
            .unwrap()
            .success()
    );
    let required_samples = if failure.is_empty() { 1 } else { 2 };
    let run = format!("{FIXTURE}\n{}", run_script());
    let output = Command::new("bash")
        .args(["-e", "-o", "pipefail", "-c", &run])
        .current_dir(root)
        .env("ANVIL_GROUP", group)
        .env("RUNNER_OS", runner_os)
        .env("RUNNER_ENVIRONMENT", runner_environment)
        .env("RUNNER_TEMP", root)
        .env("GITHUB_OUTPUT", root.join("outputs"))
        .env("CORE_LOG", root.join("core-limit"))
        .env("SYSCTL_LOG", root.join("core-routing"))
        .env("FAKE_JUST_EXIT", exit_code.to_string())
        .env("FAKE_FAILURE", failure)
        .env("REQUIRED_SAMPLES", required_samples.to_string())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit_code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let resources = root.join(format!("anvil-{group}-resources.log"));
    let core_routing = root.join("core-routing");
    if group == "pr-mutants" && runner_os == "Linux" && runner_environment == "github-hosted" {
        assert_eq!(
            fs::read_to_string(core_routing).unwrap(),
            "-n sysctl -w kernel.core_pattern=core\n-n sysctl -w kernel.core_pattern=|fixture-crash-handler\n"
        );
    } else {
        assert!(!core_routing.exists());
    }
    if failure == "core-setup" {
        assert!(!root.join("core-limit").exists(), "recipe must not run after setup fails");
        assert!(!root.join("outputs").exists());
        return;
    }
    let outputs = fs::read_to_string(root.join("outputs")).unwrap();
    assert!(outputs.contains(&format!("exit_code={exit_code}\n")));
    if exit_code != 0 {
        assert!(outputs.contains("failed_recipe=anvil-mutants-diff\n"));
    }
    if group == "pr-mutants" && runner_os == "Linux" {
        assert_eq!(fs::read_to_string(root.join("core-limit")).unwrap().trim(), "0");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.matches("[anvil-resources]").count(), required_samples);
        let stored_samples = if failure == "log" { required_samples - 1 } else { required_samples };
        assert_eq!(
            fs::read_to_string(resources).unwrap().matches("[anvil-resources]").count(),
            stored_samples
        );
        if !failure.is_empty() {
            assert!(stdout.contains("::warning::"));
        }
    } else {
        assert!(!resources.exists());
    }
}

#[test]
fn mutation_diagnostics_preserve_success() {
    run_group("pr-mutants", "Linux", "github-hosted", 0, "");
}

#[test]
fn mutation_diagnostics_preserve_recipe_failure() {
    run_group("pr-mutants", "Linux", "github-hosted", 23, "");
}

#[test]
fn other_groups_do_not_start_mutation_diagnostics() {
    run_group("pr-test", "Linux", "github-hosted", 0, "");
}

#[test]
fn non_linux_mutation_groups_do_not_start_resource_sampling() {
    run_group("pr-mutants", "Windows", "github-hosted", 23, "");
}

#[test]
fn failed_probe_does_not_stop_later_samples() {
    run_group("pr-mutants", "Linux", "github-hosted", 0, "probe");
}

#[test]
fn failed_log_write_does_not_stop_later_samples() {
    run_group("pr-mutants", "Linux", "github-hosted", 0, "log");
}

#[test]
fn self_hosted_mutation_groups_do_not_change_host_core_routing() {
    run_group("pr-mutants", "Linux", "self-hosted", 0, "");
}

#[test]
fn failed_core_routing_restore_preserves_recipe_failure() {
    run_group("pr-mutants", "Linux", "github-hosted", 23, "core-restore");
}

#[test]
fn failed_core_routing_setup_does_not_start_recipe() {
    run_group("pr-mutants", "Linux", "github-hosted", 23, "core-setup");
}
