// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for `.rs` files against an MIT-style
//! single-line license header. Each test inlines its `INPUT` and
//! `EXPECTED` content as raw string literals.

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{HEADER_MIT, check_str, fix_to_string};

#[test]
fn rust_ok() {
    const INPUT: &str = "\
// Licensed under the MIT License.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, result) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(result, CheckResult::Ok);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_missing() {
    const INPUT: &str = "\
fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, result) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_mismatch() {
    const INPUT: &str = "\
// Licensed under the Apache License.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.

fn main() {}
";

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
    const INPUT: &str = "\
// Licensed under the MIT License.
// More info about this file.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.
// More info about this file.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_empty() {
    const INPUT: &str = "";
    const EXPECTED: &str = "// Licensed under the MIT License.\n";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_existing_non_license_comment_is_preserved() {
    // A leading `//` comment that is NOT a license/copyright/SPDX header
    // must NOT be treated as a license header. `check` reports `Missing`
    // and `fix` prepends the license while preserving the original comment.
    const INPUT: &str = "\
// pre-existing comment

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.

// pre-existing comment

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_doc_comment_is_preserved() {
    // `//!` doc comments are not header comments. The license must be
    // prepended above the doc comment, leaving it intact.
    const INPUT: &str = "\
//! Crate-level docs.

fn main() {}
";
    const EXPECTED: &str = "\
// Licensed under the MIT License.

//! Crate-level docs.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}
