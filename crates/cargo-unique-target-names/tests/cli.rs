// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The process contract: exit codes and the text written to stdout and stderr.
//!
//! These are the only tests that run the built binary, because the exit code
//! and the streams are the tool's interface to a CI pipeline and cannot be
//! observed by calling the library. The collision rule itself is tested through
//! the library in `collisions.rs`.

// miri cannot sandbox the subprocess and filesystem work these tests do.
#![cfg(not(miri))]

mod common;

use std::path::PathBuf;
use std::process::{Command, Output};
use std::{env, fs};

use common::{Member, workspace};

/// The binary under test, as cargo built it next to the integration test.
fn binary() -> PathBuf {
    let mut path = env::current_exe().expect("the test binary must have a path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("cargo-unique-target-names{}", env::consts::EXE_SUFFIX))
}

fn run(manifest: &std::path::Path) -> Output {
    Command::new(binary())
        .args(["unique-target-names", "--manifest-path"])
        .arg(manifest)
        .current_dir(env::temp_dir())
        .output()
        .expect("the built binary must be runnable")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_clean_workspace_exits_zero_and_says_so() {
    let temp = workspace(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example"),
    ]);
    let output = run(&temp.path().join("Cargo.toml"));
    let diagnostic = combined(&output);
    assert_eq!(output.status.code(), Some(0), "{diagnostic}");
    assert!(
        diagnostic.contains("All workspace targets uplift to their own path"),
        "{diagnostic}"
    );
}

#[test]
fn a_contended_workspace_exits_one_and_names_the_remedy() {
    let temp = workspace(&[
        Member::new("alpha", "shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "shared", "beta_tool", "beta_lib_example"),
    ]);
    let output = run(&temp.path().join("Cargo.toml"));
    let diagnostic = combined(&output);
    assert_eq!(output.status.code(), Some(1), "{diagnostic}");
    assert!(
        diagnostic.contains("cargo-unique-target-names: target 'shared' is declared by 2 targets"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("Rename the reported targets so each one uplifts to its own path."),
        "{diagnostic}"
    );
}

/// A workspace that cannot be read is neither clean nor contended, and must be
/// distinguishable from both.
#[test]
fn an_unreadable_workspace_exits_with_its_own_code() {
    let temp = tempfile::tempdir().expect("temporary directory must be creatable");
    let manifest = temp.path().join("Cargo.toml");
    fs::write(&manifest, "this is not a manifest\n").expect("manifest must be writable");
    let output = run(&manifest);
    let diagnostic = combined(&output);
    assert_eq!(
        output.status.code(),
        Some(i32::from(cargo_unique_target_names::EXIT_UNREADABLE_WORKSPACE)),
        "a broken workspace must not share an exit code with a finding: {diagnostic}"
    );
    assert!(diagnostic.contains("failed to read workspace metadata from cargo"), "{diagnostic}");
}
