// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline integration test for Ark and Cargo.

#![cfg(not(miri))] // Requires host filesystem access and child processes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::{env, fs, io};

use cargo_ark::release_dependencies::{Candidates, CargoInvoker, check};

const BOTH_BUMPED: &str = r#"[
    {"name":"ark-fixture-consumer","old_version":"0.1.0","new_version":"0.1.1"},
    {"name":"ark-fixture-peer","old_version":"0.1.0","new_version":"0.1.1"}
]"#;
const ONLY_CONSUMER_BUMPED: &str = r#"[
    {"name":"ark-fixture-consumer","old_version":"0.1.0","new_version":"0.1.1"}
]"#;

#[test]
fn validates_candidate_input() -> Result<(), Box<dyn std::error::Error>> {
    assert!(Candidates::from_report("[]")?.is_empty());
    let invalid = Candidates::from_report(r#"[{"name":"ark-fixture-peer"}]"#).unwrap_err();
    assert_eq!(invalid.to_string(), "candidate is missing its version");
    Ok(())
}

#[test]
fn validates_shipped_tests_with_both_candidates() -> Result<(), Box<dyn std::error::Error>> {
    run_check(BOTH_BUMPED, false)
}

#[test]
fn rejects_a_stale_candidate_version() {
    let stale_report = r#"[{"name":"ark-fixture-peer","new_version":"0.1.0"}]"#;
    let stale = run_check(stale_report, false).expect_err("the stale candidate must not package");
    assert_error(stale.as_ref(), "cargo package for release candidates failed");
}

#[test]
fn reports_a_failing_default_feature_test() {
    let failure = run_check(BOTH_BUMPED, true).expect_err("the default-mode test must fail");
    assert_error(
        failure.as_ref(),
        "ark-fixture-consumer: cargo test (default features) from package archive failed",
    );
}

#[test]
fn fails_to_package_without_the_unpublished_peer() {
    let missing = run_check(ONLY_CONSUMER_BUMPED, false).expect_err("the unpublished peer must not resolve");
    assert_error(missing.as_ref(), "cargo package for release candidates failed");
}

fn run_check(report: &str, fail_shipped_test: bool) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release-dependencies");
    copy_fixture(&fixture, &workspace)?;

    let home = tempfile::tempdir_in(temp.path())?;
    let home_path = std::path::absolute(home.path())?.into_os_string();
    // Use the toolchain that built this integration test.
    let path = env::join_paths(
        Path::new(env!("CARGO"))
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .map_err(io::Error::other)?;
    let child_env = BTreeMap::<OsString, OsString>::from([
        ("PATH".into(), path),
        ("CARGO_HOME".into(), home_path.clone()),
        ("CARGO_NET_OFFLINE".into(), "true".into()),
        ("CARGO_TERM_COLOR".into(), "never".into()),
        ("ARK_FIXTURE_CARGO_HOME".into(), home_path),
        (
            "ARK_FIXTURE_FAIL_TEST".into(),
            if fail_shipped_test { "1".into() } else { "0".into() },
        ),
    ]);
    let cargo = CargoInvoker::new(&workspace, child_env, Some(home.path()))?;
    check(Candidates::from_report(report)?, cargo)
}

fn assert_error(error: &dyn std::error::Error, message: &str) {
    assert!(error.to_string().contains(message), "expected `{message}` in `{error}`");
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
