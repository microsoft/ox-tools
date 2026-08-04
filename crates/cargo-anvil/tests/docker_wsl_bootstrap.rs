// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(all(windows, not(miri)))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

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

fn generated_repo() -> TempDir {
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
    run_update(
        &Catalog::anvil(),
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        root,
    )
    .unwrap();
    tmp
}

fn ready_facts() -> String {
    r#"{
  "windowsBuild": 26100,
  "wslVersion": "2.5.7",
  "distro": "Ubuntu-24.04",
  "wslVersionMode": 2,
  "osId": "ubuntu",
  "osVersion": "24.04",
  "systemdConfigured": true,
  "systemdRunning": true,
  "dockerPath": "/usr/bin/docker",
  "dockerRealPath": "/usr/bin/docker",
  "dockerClientVersion": "26.1.5",
  "dockerServerVersion": "26.1.5",
  "conflictingPackages": "",
  "dockerServiceInstalled": true,
  "dockerServiceEnabled": true,
  "dockerServiceActive": true,
  "user": "anvil",
  "userInDockerGroup": true,
  "dockerSocketAccess": true,
  "windowsBridge": true,
  "dockerDesktopDistros": []
}"#
    .to_owned()
}

fn run_doctor_with_shell(root: &Path, shell: &str, name: &str, facts: &str) -> (bool, String) {
    let facts_path = root.join(format!("{name}.json"));
    write(&facts_path, facts);
    let output = Command::new(shell)
        .args([
            "-NoProfile",
            "-File",
            ".anvil/container/setup-docker-in-wsl.ps1",
            "-Doctor",
            "-FactsPath",
        ])
        .arg(&facts_path)
        .current_dir(root)
        .output()
        .expect("pwsh must be available");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn run_doctor(root: &Path, name: &str, facts: &str) -> (bool, String) {
    run_doctor_with_shell(root, "pwsh", name, facts)
}

#[test]
fn doctor_simulates_supported_missing_partial_and_conflicting_hosts() {
    let tmp = generated_repo();

    let (ready, output) = run_doctor(tmp.path(), "ready", &ready_facts());
    assert!(ready, "supported facts must pass: {output}");
    assert!(output.contains("[READY]"));

    let (ready, output) = run_doctor_with_shell(tmp.path(), "powershell", "ready-windows-powershell", &ready_facts());
    assert!(ready, "Windows PowerShell 5.1 must pass: {output}");

    let missing = ready_facts()
        .replace(r#""dockerPath": "/usr/bin/docker""#, r#""dockerPath": """#)
        .replace(r#""dockerRealPath": "/usr/bin/docker""#, r#""dockerRealPath": """#)
        .replace(r#""dockerClientVersion": "26.1.5""#, r#""dockerClientVersion": """#)
        .replace(r#""dockerServerVersion": "26.1.5""#, r#""dockerServerVersion": """#)
        .replace(r#""dockerServiceInstalled": true"#, r#""dockerServiceInstalled": false"#)
        .replace(r#""dockerServiceEnabled": true"#, r#""dockerServiceEnabled": false"#)
        .replace(r#""dockerServiceActive": true"#, r#""dockerServiceActive": false"#)
        .replace(r#""userInDockerGroup": true"#, r#""userInDockerGroup": false"#)
        .replace(r#""dockerSocketAccess": true"#, r#""dockerSocketAccess": false"#)
        .replace(r#""windowsBridge": true"#, r#""windowsBridge": false"#);
    let (ready, output) = run_doctor(tmp.path(), "missing", &missing);
    assert!(!ready);
    assert!(output.contains("not installed"));

    let partial = ready_facts()
        .replace(r#""systemdRunning": true"#, r#""systemdRunning": false"#)
        .replace(r#""userInDockerGroup": true"#, r#""userInDockerGroup": false"#)
        .replace(r#""dockerSocketAccess": true"#, r#""dockerSocketAccess": false"#)
        .replace(r#""windowsBridge": true"#, r#""windowsBridge": false"#);
    let (ready, output) = run_doctor(tmp.path(), "partial", &partial);
    assert!(!ready);
    assert!(output.contains("[FAIL] systemd runtime"));
    assert!(output.contains("[FAIL] Docker group"));

    let conflict = ready_facts()
        .replace(
            r#""dockerRealPath": "/usr/bin/docker""#,
            r#""dockerRealPath": "/mnt/wsl/docker-desktop/cli-tools/usr/bin/docker""#,
        )
        .replace(r#""dockerDesktopDistros": []"#, r#""dockerDesktopDistros": ["docker-desktop"]"#);
    let (ready, output) = run_doctor(tmp.path(), "conflict", &conflict);
    assert!(!ready);
    assert!(output.contains("resolves through Docker Desktop"));
    assert!(output.contains("never removed"));
}
