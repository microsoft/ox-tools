// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

use std::process::Command;

#[gamma::resource("cargo-aprz-cargo-subprocess")]
mod cargo_subprocess_resource {}

fn cargo_aprz() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cargo-aprz"))
}

#[test]
fn binary_delegates_to_the_command_dispatcher() {
    let output = cargo_aprz()
        .args(["aprz", "--help"])
        .output()
        .expect("the built cargo-aprz binary must be executable");

    assert!(output.status.success(), "stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Appraise the quality of Rust dependencies"),
        "stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn binary_exits_nonzero_when_command_execution_fails() {
    let output = cargo_aprz()
        .args(["aprz", "validate", "--manifest-path", "this-manifest-does-not-exist/Cargo.toml"])
        .output()
        .expect("the built cargo-aprz binary must be executable");

    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("ERROR:"), "{output:?}");
}
