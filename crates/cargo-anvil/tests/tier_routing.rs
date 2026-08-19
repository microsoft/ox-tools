// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
offtier: (_anvil-run "offtier" runner "off")

[private]
_anvil-pr: first second

[private]
_anvil-fail: first failing

[private]
_anvil-offtier: require-impact-off second

[windows]
[script("pwsh")]
require-impact-off:
    if ($env:ANVIL_IMPACT -ne 'off') { Write-Output "impact=$($env:ANVIL_IMPACT)"; exit 9 }
    Write-Output 'impact-off'

[unix]
require-impact-off:
    @if [ "${ANVIL_IMPACT:-}" = off ]; then printf 'impact-off\n'; else printf 'impact=%s\n' "${ANVIL_IMPACT:-}"; exit 9; fi

[windows]
[script("pwsh", "-NoProfile")]
first:
    Write-Output first

[windows]
[script("pwsh", "-NoProfile")]
second:
    Write-Output second

[windows]
[script("pwsh", "-NoProfile")]
failing:
    Write-Output failing
    exit 7

[windows]
[script("pwsh", "-NoProfile")]
anvil-container *recipe:
    Write-Output 'container:{{ recipe }}'

[script("pwsh", "-NoProfile")]
profile-independent:
    Write-Output profile-safe

[script("pwsh")]
profile-dependent:
    Write-Output profile-noisy

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

fn profile_fixture() -> TempDir {
    let tmp = fixture();
    install_profile_noise_wrapper(tmp.path());
    tmp
}

fn just_available() -> bool {
    Command::new("just").arg("--version").output().is_ok()
}

fn pwsh_available() -> bool {
    Command::new("pwsh").arg("--version").output().is_ok()
}

fn pwsh_path() -> PathBuf {
    let output = Command::new("pwsh")
        .args(["-NoProfile", "-Command", "(Get-Command pwsh).Source"])
        .output()
        .expect("pwsh availability is checked before creating the fixture");
    assert!(output.status.success(), "failed to resolve pwsh path");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn install_profile_noise_wrapper(root: &Path) {
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let real_pwsh = pwsh_path();

    #[cfg(windows)]
    {
        let source = bin.join("pwsh.rs");
        let real_pwsh = format!("{:?}", real_pwsh.to_string_lossy());
        write(
            &source,
            &format!(
                r#"use std::io::Write as _;
use std::process::{{Command, exit}};

fn main() {{
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !args.iter().any(|arg| arg.to_string_lossy().eq_ignore_ascii_case("-NoProfile")) {{
        println!("PROFILE_OUTPUT");
        std::io::stdout().flush().expect("stdout must flush");
    }}
    let status = Command::new({real_pwsh})
        .args(&args)
        .status()
        .expect("real pwsh must start");
    exit(status.code().unwrap_or(1));
}}
"#
            ),
        );
        let status = Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(bin.join("pwsh.exe"))
            .status()
            .expect("rustc is available while running cargo tests");
        assert!(status.success(), "failed to compile the Windows pwsh test shim");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let escaped = real_pwsh.to_string_lossy().replace('\'', "'\\''");
        let wrapper = format!(
            "#!/usr/bin/env sh\ncase \" $* \" in *\" -NoProfile \"*) ;; *) printf 'PROFILE_OUTPUT\\n' ;; esac\nexec '{escaped}' \"$@\"\n"
        );
        let path = bin.join("pwsh");
        write(&path, &wrapper);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

fn path_with_profile_wrapper(root: &Path) -> OsString {
    let mut paths = vec![root.join("fake-bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(paths).unwrap()
}

fn run(root: &Path, recipes: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command.args(["--justfile", root.join(JUSTFILE).to_str().unwrap()]);
    command.args(recipes).current_dir(root);
    command
        .env_remove("ANVIL_RUNNER")
        .env_remove("ANVIL_IN_CONTAINER")
        .env_remove("ANVIL_IMPACT");
    command.env("PATH", path_with_profile_wrapper(root));
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

#[test]
fn off_impact_argument_exports_anvil_impact_before_dependencies_run() {
    if !just_available() {
        return;
    }
    let tmp = fixture();
    // Routed through `_anvil-run` with impact "off": the router must export
    // ANVIL_IMPACT=off in the shell *before* re-invoking the private tier, so
    // that tier's own dependency (`require-impact-off`, which runs before any
    // recipe body) observes it. This is the scheduled/full full-workspace
    // backstop -- the guarantee that impact scoping is off for those tiers.
    let routed = run(tmp.path(), &["offtier"], &[]);
    assert!(
        routed.status.success(),
        "off-routed tier failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&routed.stdout),
        String::from_utf8_lossy(&routed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&routed.stdout).lines().collect::<Vec<_>>(),
        ["impact-off", "second"],
        "the off-mode dependency must observe ANVIL_IMPACT=off and run before the rest of the tier"
    );

    // Sanity: invoking the private tier directly (no `_anvil-run`) does NOT set
    // the mode, so the same dependency fails with its sentinel exit code --
    // proving the router's shell export is what makes the routed run pass, not
    // some ambient default.
    let direct = run(tmp.path(), &["_anvil-offtier"], &[]);
    assert_eq!(
        direct.status.code(),
        Some(9),
        "direct tier must fail without ANVIL_IMPACT=off: stdout={}; stderr={}",
        String::from_utf8_lossy(&direct.stdout),
        String::from_utf8_lossy(&direct.stderr)
    );
}

#[test]
fn powershell_recipe_output_is_independent_of_profiles() {
    if !just_available() || !pwsh_available() {
        return;
    }
    let tmp = profile_fixture();
    let noisy = run(tmp.path(), &["profile-dependent"], &[]);
    assert!(
        noisy.status.success(),
        "profile-dependent negative control failed: {}",
        String::from_utf8_lossy(&noisy.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&noisy.stdout).lines().collect::<Vec<_>>(),
        ["PROFILE_OUTPUT", "profile-noisy"],
        "the negative control must prove the fake pwsh shim was invoked"
    );

    let output = run(tmp.path(), &["profile-independent"], &[]);
    assert!(
        output.status.success(),
        "profile-independent recipe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().collect::<Vec<_>>(),
        ["profile-safe"]
    );
}
