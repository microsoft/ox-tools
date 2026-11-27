// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The live ox-tools workspace must not contain dependency cycles.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;

#[test]
#[cfg_attr(miri, ignore = "executes the tool on the live repository")]
fn repository_has_no_dependency_cycles() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cargo-ensure-no-cyclic-deps"));
    cmd.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    cmd.assert().success().stdout("No cyclic dependencies found.\n");
}
