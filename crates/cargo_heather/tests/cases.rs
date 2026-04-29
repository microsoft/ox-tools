// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for the public stream API.
//!
//! Each test pulls an `input.<ext>` and `expected.<ext>` pair from
//! `tests/fixtures/<case>/`, runs `check` on the input to assert the
//! reported result, and runs `fix` to assert the rewritten output equals
//! the expected fixture verbatim.

use cargo_heather::{CheckResult, FileKind, check, fix};

const HEADER_MIT: &str = "Licensed under the MIT License.";

fn check_str(input: &str, header: &str, kind: FileKind) -> CheckResult {
    check(input.as_bytes(), header, kind).expect("check should not fail on in-memory input")
}

fn fix_to_string(input: &str, header: &str, kind: FileKind) -> (String, CheckResult) {
    let mut output: Vec<u8> = Vec::new();
    let result = fix(input.as_bytes(), &mut output, header, kind).expect("fix should not fail on in-memory input/output");
    let s = String::from_utf8(output).expect("fix output should be valid UTF-8");
    (s, result)
}

// ---------------------------------------------------------------------------
// Rust (.rs) fixtures
// ---------------------------------------------------------------------------

#[test]
fn rust_ok() {
    const INPUT: &str = include_str!("fixtures/rust_ok/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_ok/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, result) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(result, CheckResult::Ok);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_missing() {
    const INPUT: &str = include_str!("fixtures/rust_missing/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_missing/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, result) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_mismatch() {
    const INPUT: &str = include_str!("fixtures/rust_mismatch/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_mismatch/expected.rs");

    let result = check_str(INPUT, HEADER_MIT, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, result) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }));
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_descriptive() {
    // A file with descriptive comment lines after the license header is
    // still "Ok": the expected header just needs to appear as a prefix
    // of the extracted comment block.
    const INPUT: &str = include_str!("fixtures/rust_descriptive/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_descriptive/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_empty() {
    const INPUT: &str = include_str!("fixtures/rust_empty/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_empty/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_existing_non_license_comment_is_preserved() {
    // A leading `//` comment that is NOT a license/copyright/SPDX header
    // must NOT be treated as a license header. `check` reports `Missing`
    // and `fix` prepends the license while preserving the original comment.
    const INPUT: &str = include_str!("fixtures/rust_existing_comment/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_existing_comment/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_doc_comment_is_preserved() {
    // `//!` doc comments are not header comments. The license must be
    // prepended above the doc comment, leaving it intact.
    const INPUT: &str = include_str!("fixtures/rust_doc_comment/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_doc_comment/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

const HEADER_APACHE: &str = "\
Licensed under the Apache License, Version 2.0 (the \"License\");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an \"AS IS\" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.";

#[test]
fn rust_apache_ok() {
    const INPUT: &str = include_str!("fixtures/rust_apache_ok/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_apache_ok/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_APACHE, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_apache_missing() {
    const INPUT: &str = include_str!("fixtures/rust_apache_missing/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_apache_missing/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_APACHE, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_apache_mismatch() {
    // MIT header present, Apache expected — Mismatch. The MIT block is
    // stripped and the multi-line Apache header is prepended.
    const INPUT: &str = include_str!("fixtures/rust_apache_mismatch/input.rs");
    const EXPECTED: &str = include_str!("fixtures/rust_apache_mismatch/expected.rs");

    let result = check_str(INPUT, HEADER_APACHE, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_APACHE, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

// ---------------------------------------------------------------------------
// TOML (.toml) fixtures
// ---------------------------------------------------------------------------

#[test]
fn toml_ok() {
    const INPUT: &str = include_str!("fixtures/toml_ok/input.toml");
    const EXPECTED: &str = include_str!("fixtures/toml_ok/expected.toml");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Toml), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}

#[test]
fn toml_missing() {
    const INPUT: &str = include_str!("fixtures/toml_missing/input.toml");
    const EXPECTED: &str = include_str!("fixtures/toml_missing/expected.toml");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Toml), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}

#[test]
fn toml_mismatch() {
    const INPUT: &str = include_str!("fixtures/toml_mismatch/input.toml");
    const EXPECTED: &str = include_str!("fixtures/toml_mismatch/expected.toml");

    let result = check_str(INPUT, HEADER_MIT, FileKind::Toml);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Toml);
    assert_eq!(out, EXPECTED);
}

// ---------------------------------------------------------------------------
// Cargo-script (.rs with shebang + `---` frontmatter) fixtures
// ---------------------------------------------------------------------------

#[test]
fn script_ok() {
    const INPUT: &str = include_str!("fixtures/script_ok/input.rs");
    const EXPECTED: &str = include_str!("fixtures/script_ok/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::CargoScript), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}

#[test]
fn script_missing() {
    const INPUT: &str = include_str!("fixtures/script_missing/input.rs");
    const EXPECTED: &str = include_str!("fixtures/script_missing/expected.rs");

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::CargoScript), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}

#[test]
fn script_mismatch() {
    const INPUT: &str = include_str!("fixtures/script_mismatch/input.rs");
    const EXPECTED: &str = include_str!("fixtures/script_mismatch/expected.rs");

    let result = check_str(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::CargoScript);
    assert_eq!(out, EXPECTED);
}
