// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Integration test

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

#[test]
#[cfg_attr(miri, ignore)]
fn missing_manifest_reports_metadata_context() {
    let missing_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml/does-not-exist/Cargo.toml");
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cargo-ensure-no-cyclic-deps"));
    cmd.arg("ensure-no-cyclic-deps").arg("--manifest-path").arg(missing_manifest);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Failed to load cargo metadata"));
}
