// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixture-driven integration tests for `.rs` files against the canonical
//! two-line Microsoft MIT header (`Copyright (c) Microsoft Corporation.` +
//! `Licensed under the MIT License.`).

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{HEADER_MS_MIT, check_str, fix_to_string};

#[test]
fn rust_ms_mit_ok() {
    const INPUT: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {}
";
    const EXPECTED: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MS_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_ms_mit_missing() {
    const INPUT: &str = "fn main() {}\n";
    const EXPECTED: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MS_MIT, FileKind::Rust), CheckResult::Missing);
    let (out, _) = fix_to_string(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_ms_mit_mismatch() {
    const INPUT: &str = "\
// Licensed under the Apache License.

fn main() {}
";
    const EXPECTED: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {}
";

    let result = check_str(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_ms_mit_partial_is_mismatch() {
    // File has only the MIT line, missing the Copyright line. The
    // expected header is two lines, so this is a Mismatch — `fix`
    // replaces the partial block with the full two-line header.
    const INPUT: &str = "\
// Licensed under the MIT License.

fn main() {}
";
    const EXPECTED: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn main() {}
";

    let result = check_str(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert!(matches!(result, CheckResult::Mismatch { .. }), "expected Mismatch, got {result:?}");
    let (out, _) = fix_to_string(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}

#[test]
fn rust_ms_mit_with_trailing_descriptive_comment_is_ok() {
    // Two-line header followed by a non-license descriptive comment.
    // The expected header just needs to be a prefix of the extracted
    // comment block, so this is `Ok`.
    const INPUT: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// More info about this file.

fn main() {}
";
    const EXPECTED: &str = "\
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// More info about this file.

fn main() {}
";

    assert_eq!(check_str(INPUT, HEADER_MS_MIT, FileKind::Rust), CheckResult::Ok);
    let (out, _) = fix_to_string(INPUT, HEADER_MS_MIT, FileKind::Rust);
    assert_eq!(out, EXPECTED);
}
