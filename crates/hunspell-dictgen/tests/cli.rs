// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::fs;

use assert_cmd::Command;
use tempfile::TempDir;

#[test]
fn cli_generates_the_requested_dictionary() {
    let temporary = TempDir::new().expect("temporary directory creation should succeed");
    let input = temporary.path().join("words.txt");
    let output = temporary.path().join("generated").join("words.dic");
    fs::write(&input, "beta\n7\nalpha\n").expect("input fixture should be writable");

    Command::cargo_bin("hunspell-dictgen")
        .expect("hunspell-dictgen binary should build")
        .arg("--input")
        .arg(&input)
        .arg("--output")
        .arg(&output)
        .assert()
        .success();

    assert_eq!(
        fs::read_to_string(output).expect("generated dictionary should be readable"),
        "2\nalpha\nbeta\n"
    );
}
