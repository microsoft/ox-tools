// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline integration test for Ark and Cargo.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::{env, fs};

const BOTH_BUMPED: &str = r#"[
    {"name":"ark-fixture-consumer","old_version":"0.1.0","new_version":"0.1.1"},
    {"name":"ark-fixture-peer","old_version":"0.1.0","new_version":"0.1.1"}
]"#;
const ONLY_CONSUMER_BUMPED: &str = r#"[
    {"name":"ark-fixture-consumer","old_version":"0.1.0","new_version":"0.1.1"}
]"#;

#[test]
fn validates_shipped_tests_and_reports_release_failures() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release-dependencies");
    copy_fixture(&fixture, &workspace)?;

    let success = ark(&workspace, BOTH_BUMPED, false)?;
    let stdout = String::from_utf8_lossy(&success.stdout);
    let stderr = String::from_utf8_lossy(&success.stderr);
    assert!(success.status.success(), "{stderr}");
    assert!(stderr.contains("Packaging ark-fixture-peer v0.1.1"), "{stderr}");
    assert!(stderr.contains("Packaging ark-fixture-consumer v0.1.1"), "{stderr}");
    assert!(!stderr.contains("Packaging ark-fixture-stable"), "{stderr}");
    assert!(stdout.contains("archive_all ... ok"), "{stdout}");
    assert!(stdout.contains("archive_none ... ok"), "{stdout}");

    let failure = ark(&workspace, BOTH_BUMPED, true)?;
    let stdout = String::from_utf8_lossy(&failure.stdout);
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(!failure.status.success(), "{stdout}");
    assert!(stdout.contains("archive_all ... ok"), "{stdout}");
    assert!(stdout.contains("deliberately failing shipped test"), "{stdout}");
    assert!(
        stderr.contains("cargo test --no-default-features from package archive failed"),
        "{stderr}"
    );

    let peer = workspace.join("peer/Cargo.toml");
    let manifest = fs::read_to_string(&peer)?;
    fs::write(&peer, manifest.replace("0.1.1", "0.1.0"))?;
    let missing = ark(&workspace, ONLY_CONSUMER_BUMPED, false)?;
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(!missing.status.success(), "{stderr}");
    assert!(stderr.contains("cargo package for release candidates failed"), "{stderr}");
    assert!(stderr.contains("ark-fixture-peer") && stderr.contains("0.1.1"), "{stderr}");
    Ok(())
}

fn ark(workspace: &Path, report: &str, fail_shipped_test: bool) -> io::Result<Output> {
    let home = tempfile::tempdir()?;
    // Keep using the test's toolchain after leaving its workspace and Cargo home.
    let path = env::join_paths(
        Path::new(env!("CARGO"))
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .map_err(io::Error::other)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-ark"));
    command
        .args(["ark", "release-dependencies", "--temp-dir"])
        .arg(home.path())
        .current_dir(workspace)
        .env("PATH", path)
        .env("CARGO_HOME", home.path())
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never")
        .env_remove("ARK_FIXTURE_FAIL_TEST")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if fail_shipped_test {
        command.env("ARK_FIXTURE_FAIL_TEST", "1");
    }
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(report.as_bytes())?;
    }
    child.wait_with_output()
}

fn copy_fixture(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir(destination)?;
    for entry in source.read_dir()? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_fixture(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
