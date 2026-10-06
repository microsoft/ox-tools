// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Checks each exported macro's diagnostic identity and essential reason.
//!
//! A `compile_fail` doctest only proves that *some* error occurred; it accepts a diagnostic from
//! any cause, including an unrelated one a regression introduced by accident. These tests compile
//! embedded consumers using the real macro library, then check the macro prefix and message
//! fragment that distinguish the intended rejection. The consumers have no Cargo dependencies
//! and build with an empty Cargo home; the already-built macro is copied into each fixture.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::{env, fs};

use cargo_metadata::{Message, TargetKind};
use tempfile::TempDir;

#[path = "../../cargo-gamma-lib/tests/support/project.rs"]
mod project;

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// Asks Cargo for the real macro artifact instead of guessing hashes or compiler variants.
fn macro_artifact() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();

    BUILT
        .get_or_init(|| {
            let built = Command::new(cargo())
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .args([
                    "build",
                    "--offline",
                    "--locked",
                    "--lib",
                    "--message-format=json",
                    "--manifest-path",
                ])
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
                .output()
                .expect("Cargo must be runnable to locate the real macro library");
            assert!(
                built.status.success(),
                "the macro library must be available before checking consumers:\n{}",
                String::from_utf8_lossy(&built.stderr)
            );

            Message::parse_stream(built.stdout.as_slice())
                .find_map(|message| match message.expect("Cargo artifact messages must be readable") {
                    Message::CompilerArtifact(artifact)
                        if artifact.target.name == "gamma" && artifact.target.kind.contains(&TargetKind::ProcMacro) =>
                    {
                        artifact
                            .filenames
                            .into_iter()
                            .find(|path| path.as_str().ends_with(env::consts::DLL_SUFFIX))
                            .map(PathBuf::from)
                    }
                    _ => None,
                })
                .expect("Cargo must report the compiled gamma proc-macro library")
        })
        .as_path()
}

/// Compiles embedded `source` in a standalone project using the actual macro entry points.
fn compile_consumer(name: &str, source: &str) -> Output {
    let artifact = macro_artifact();
    let directory = TempDir::new().expect("could not create a consumer project directory");
    let home = TempDir::new().expect("could not create an empty Cargo home");
    let manifest = format!("[package]\nname = {name:?}\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[workspace]\n");
    project::write_project(directory.path(), &[("Cargo.toml", &manifest), ("src/lib.rs", source)]);
    let local_macro = directory
        .path()
        .join(artifact.file_name().expect("Cargo reports a file name for its proc-macro artifact"));
    fs::copy(artifact, &local_macro).expect("the real macro library must be copyable into the consumer");

    Command::new(cargo())
        .current_dir(directory.path())
        .env("CARGO_HOME", home.path())
        .args(["rustc", "--quiet", "--offline", "--lib", "--target-dir"])
        .arg(directory.path().join("target"))
        .args(["--", "--extern"])
        .arg(format!("gamma={}", local_macro.display()))
        .output()
        .unwrap_or_else(|error| panic!("Cargo must be runnable to check what it reports for {name}: {error}"))
}

/// Returns the macro diagnostic produced by deliberately malformed embedded `source`.
///
/// Every failure is reported as a failure. A skip here would be indistinguishable from a passing
/// diagnostic check, so a lookup that stopped finding the artifact, or a host without a usable
/// `rustc`, would retire every assertion below while the suite stayed green — which is the one
/// outcome a test pinning diagnostics must not produce. Panics if the fixture unexpectedly
/// compiles too: every fixture this file hands to it is deliberately malformed, and a clean
/// compile means the validation this test exists to pin has stopped rejecting it.
#[track_caller]
fn diagnostic_for(name: &str, source: &str) -> String {
    let output = compile_consumer(name, source);

    assert!(
        !output.status.success(),
        "a deliberately malformed fixture compiled cleanly\n--- source ---\n{source}"
    );

    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_diagnostic(name: &str, source: &str, fragments: &[&str]) {
    let reported = diagnostic_for(name, source);

    for fragment in fragments {
        assert!(reported.contains(fragment), "expected `{fragment}` in:\n{reported}");
    }
}

#[test]
fn a_valid_macro_consumer_builds_without_a_registry_cache() {
    let built = compile_consumer(
        "valid_consumer",
        "#[gamma::skip]\npub fn accepts(value: u32) -> bool { value >= 1 }\n",
    );

    assert!(
        built.status.success(),
        "the real macro must compile a valid standalone consumer offline:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

#[test]
fn skip_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "skip_malformed",
        "#[gamma::skip(reason = performance)]\nfn scaled(a: i64) -> i64 { a * 2 }\n",
        &["#[gamma::skip]:", "`reason` must be a string literal"],
    );
}

#[test]
fn expect_survived_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "expect_survived_malformed",
        "#[gamma::expect_survived(tag = 7)]\nfn describe(n: usize) -> usize { n }\n",
        &["#[gamma::expect_survived]:", "`tag` must be a string literal"],
    );
}

#[test]
fn expect_killed_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "expect_killed_malformed",
        "#[gamma::expect_killed(reason = 5)]\nfn checksum(bytes: &[u8]) -> usize { bytes.len() }\n",
        &["#[gamma::expect_killed]:", "`reason` must be a string literal"],
    );
}

#[test]
fn value_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "value_malformed",
        "#[gamma::value()]\nfn budget() -> u32 { 512 }\n",
        &["#[gamma::value]:", "expected one expression"],
    );
}

#[test]
fn test_timeout_multiplier_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "test_timeout_multiplier_malformed",
        "#[gamma::test_timeout_multiplier(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n",
        &["#[gamma::test_timeout_multiplier]:", "timeout multiplier must be a positive number"],
    );
}

#[test]
fn timeout_multiplier_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "timeout_multiplier_malformed",
        "#[gamma::timeout_multiplier(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n",
        &["#[gamma::timeout_multiplier]:", "timeout multiplier must be a positive number"],
    );
}

#[test]
fn gamma_reports_its_own_name_in_a_malformed_diagnostic() {
    assert_diagnostic(
        "gamma_malformed",
        "#[gamma::gamma(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n",
        &["#[gamma::gamma]:", "timeout multiplier must be a positive number"],
    );
}
