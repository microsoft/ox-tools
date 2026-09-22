// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Checks each exported macro's diagnostic identity and essential reason.
//!
//! A `compile_fail` doctest only proves that *some* error occurred; it accepts a diagnostic from
//! any cause, including an unrelated one a regression introduced by accident. These tests compile
//! through Cargo against this checkout's path dependency, then check the macro prefix and message
//! fragment that distinguish the intended rejection.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{self, Command};
use std::{env, fs};

fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// Compiles `source` against this checkout's `gamma` crate and returns Cargo's stderr.
///
/// Every failure is reported as a failure. A skip here would be indistinguishable from a passing
/// diagnostic check, so a lookup that stopped finding the artifact, or a host without a usable
/// `rustc`, would retire every assertion below while the suite stayed green — which is the one
/// outcome a test pinning diagnostics must not produce. Panics if the fixture unexpectedly
/// compiles too: every fixture this file hands to it is deliberately malformed, and a clean
/// compile means the validation this test exists to pin has stopped rejecting it.
#[track_caller]
fn diagnostic_for(name: &str, source: &str) -> String {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let directory = scratch.join(format!("diagnostics-{}-{name}", process::id()));
    let target = scratch.join(format!("diagnostics-{}-target", process::id()));
    let source_dir = directory.join("src");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let manifest = format!(
        "[package]\nname = {name:?}\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n\
         [workspace]\n\n[dependencies]\ngamma = {{ package = \"cargo-gamma-attrs\", path = {manifest_dir:?} }}\n"
    );

    fs::create_dir_all(&source_dir).expect("the scratch directory must be creatable");
    fs::write(directory.join("Cargo.toml"), manifest).expect("the fixture manifest must be writable");
    fs::write(source_dir.join("lib.rs"), source).expect("the fixture source must be writable");

    let compiler = cargo();
    let output = Command::new(&compiler)
        .args(["check", "--quiet", "--manifest-path"])
        .arg(directory.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(target)
        .output()
        .unwrap_or_else(|error| panic!("Cargo must be runnable to check what it reports for {name}: {error}"));

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
