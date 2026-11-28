// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

#[test]
fn the_tools_own_catalog_has_no_unexpected_findings() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-unused-deps"))
        .args(["unused-deps", "--manifest-path"])
        .arg(manifest)
        .output()
        .expect("Cargo provides the test binary");
    if output.status.success() {
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        std::env::var_os("CARGO_GAMMA").is_some(),
        "this repository's catalog should be clean: {stderr}"
    );
    let dependencies = stderr.lines().filter_map(|line| line.strip_prefix("  - ")).collect();
    assert!(
        only_injected_runtime_dependencies(&dependencies),
        "the instrumented catalog should differ only by cargo-gamma's injected runtime dependencies: {stderr}"
    );
}

fn only_injected_runtime_dependencies(dependencies: &BTreeSet<&str>) -> bool {
    !dependencies.is_empty() && dependencies.iter().all(|&name| matches!(name, "cargo-gamma-rt" | "gamma_rt"))
}

#[test]
fn injected_runtime_catalog_findings_allow_either_alias_but_nothing_else() {
    for (names, expected) in [
        (vec!["gamma_rt"], true),
        (vec!["cargo-gamma-rt"], true),
        (vec!["cargo-gamma-rt", "gamma_rt"], true),
        (vec![], false),
        (vec!["gamma_rt", "unrelated"], false),
        (vec!["unrelated"], false),
    ] {
        assert_eq!(only_injected_runtime_dependencies(&names.into_iter().collect()), expected);
    }
}
