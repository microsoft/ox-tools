// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for `.toml` files against an MIT-style
//! single-line license header.

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{HEADER_MIT, check_str, fix_to_string};

#[test]
fn toml_ok() {
    const INPUT: &str = "\
# Licensed under the MIT License.

[package]
name = \"foo\"
";
    const EXPECTED: &str = "\
# Licensed under the MIT License.

[package]
name = \"foo\"
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Toml), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}

#[test]
fn toml_missing() {
    const INPUT: &str = "\
[package]
name = \"foo\"
";
    const EXPECTED: &str = "\
# Licensed under the MIT License.

[package]
name = \"foo\"
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Toml), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}

#[test]
fn toml_mismatch() {
    const INPUT: &str = "\
# Licensed under the Apache License.

[package]
name = \"foo\"
";
    const EXPECTED: &str = "\
# Licensed under the MIT License.

[package]
name = \"foo\"
";

    let result = check_str(INPUT, HEADER_MIT, FileKind::Toml);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}
