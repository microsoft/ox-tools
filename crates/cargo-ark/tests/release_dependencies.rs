// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline integration test for Ark and Cargo.

#![cfg(not(miri))] // Requires host filesystem access and child processes.

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
fn handles_discovery_input_before_looking_for_a_workspace() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-ark"));
    command.arg("release-dependencies").current_dir(temp.path());
    let empty = run_report(&mut command, "[]")?;
    assert!(empty.status.success(), "{empty:?}");
    assert!(
        String::from_utf8_lossy(&empty.stdout).contains("no release candidates"),
        "{empty:?}"
    );

    let invalid = run_report(&mut command, r#"[{"name":"ark-fixture-peer"}]"#)?;
    assert_error(&invalid, "candidate is missing its version");

    let invalid_encoding = run_report(&mut command, [0xff])?;
    assert_error(&invalid_encoding, "stream did not contain valid UTF-8");

    let no_workspace = run_report(&mut command, BOTH_BUMPED)?;
    assert_error(&no_workspace, "locating the Cargo workspace failed");
    Ok(())
}

#[test]
fn validates_shipped_tests_and_reports_release_failures() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let checkout = temp.path().join("checkout");
    fs::create_dir(&checkout)?;
    let workspace = checkout.join("workspace");
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

    let internal_scratch = run_report(
        ark_command(&workspace, temp.path())?
            .arg("--temp-dir")
            .arg(workspace.join("consumer")),
        BOTH_BUMPED,
    )?;
    assert_error(&internal_scratch, "select a temporary directory outside the source workspace");

    let nonexistent_parent = temp.path().join("nonexistent");
    let mut command = ark_command(&workspace, temp.path())?;
    command.args(["--temp-dir"]).arg(&nonexistent_parent);
    let unavailable_scratch = run_report(&mut command, BOTH_BUMPED)?;
    assert_error(&unavailable_scratch, "preparing release scratch directory in");
    assert!(String::from_utf8_lossy(&unavailable_scratch.stderr).contains("nonexistent"));

    let stale_report = r#"[{"name":"ark-fixture-peer","new_version":"0.1.0"}]"#;
    let stale = ark(&workspace, stale_report, false)?;
    assert_error(&stale, "cargo package for release candidates failed");
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("ark-fixture-peer@0.1.0"),
        "{stale:?}"
    );

    let failure = ark(&workspace, BOTH_BUMPED, true)?;
    let stdout = String::from_utf8_lossy(&failure.stdout);
    assert_error(
        &failure,
        "ark-fixture-consumer: cargo test --no-default-features from package archive failed",
    );
    assert!(stdout.contains("archive_all ... ok"), "{stdout}");
    assert!(stdout.contains("deliberately failing shipped test"), "{stdout}");

    let missing = ark(&workspace, ONLY_CONSUMER_BUMPED, false)?;
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert_error(&missing, "cargo package for release candidates failed");
    assert!(stderr.contains("no matching package named `ark-fixture-peer` found"), "{stderr}");
    assert!(stderr.contains("location searched: crates.io index"), "{stderr}");
    Ok(())
}

fn ark(workspace: &Path, report: &str, fail_shipped_test: bool) -> io::Result<Output> {
    let parent = workspace
        .parent()
        .expect("the fixture workspace is a child of the test temp directory");
    let home = tempfile::tempdir_in(parent)?;
    let relative_home = Path::new("..")
        .join("..")
        .join(home.path().file_name().expect("tempdir_in creates a named subdirectory"));
    let mut command = ark_command(&workspace.join("consumer"), &relative_home)?;
    command
        .arg("--temp-dir")
        .arg(home.path())
        .env("ARK_FIXTURE_CARGO_HOME", home.path());
    if fail_shipped_test {
        command.env("ARK_FIXTURE_FAIL_TEST", "1");
    }
    run_report(&mut command, report)
}

fn ark_command(working_dir: &Path, home: &Path) -> io::Result<Command> {
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
        .args(["ark", "release-dependencies"])
        .current_dir(working_dir)
        .env("PATH", path)
        .env("CARGO_HOME", home)
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never")
        .env_remove("ARK_FIXTURE_FAIL_TEST");
    Ok(command)
}

fn run_report(command: &mut Command, report: impl AsRef<[u8]>) -> io::Result<Output> {
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(report.as_ref())?;
    }
    child.wait_with_output()
}

fn assert_error(output: &Output, message: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(&format!("cargo ark release-dependencies: {message}")), "{stderr}");
}

fn copy_fixture(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir(destination)?;
    for entry in source.read_dir()? {
        let entry = entry?;
        let name = entry.file_name();
        // Cargo skips nested packages when packaging Ark itself.
        let target = if name == "Cargo.fixture.toml" {
            destination.join("Cargo.toml")
        } else {
            destination.join(name)
        };
        if entry.file_type()?.is_dir() {
            copy_fixture(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
