// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for `PowerShell` files (`.ps1`) against the
//! "Copyright (c) Microsoft Corporation." single-line header.
//!
//! These tests cover scenarios where descriptive comments exist
//! but no license header is present — the fixer must *prepend* the
//! license header, not strip and replace the existing comments.

mod common;

use cargo_heather::{CheckResult, FileKind};
use common::{check_str, fix_to_string};

const HEADER: &str = "Copyright (c) Microsoft Corporation.";

// ── Missing header with descriptive comment block ────────────────────

/// Regression: a file whose only leading comments are descriptive (no
/// license keywords) must be classified as Missing. The fix must
/// prepend the license header and preserve the descriptive comments.
///
/// Repro from ox-sdk `scripts/docs-clean-branches.ps1`.
#[test]
fn ps1_missing_with_descriptive_block() {
    const INPUT: &str = "\
#
# Clean up old branches with generated documentation.
# After branch is merged the master build pipeline shall remove the branch documentation.
#
Write-Output \"done\"
";
    const EXPECTED: &str = "\
# Copyright (c) Microsoft Corporation.

#
# Clean up old branches with generated documentation.
# After branch is merged the master build pipeline shall remove the branch documentation.
#
Write-Output \"done\"
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Missing
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED, "descriptive comments must be preserved");
}

/// Repro from ox-sdk `scripts/docs-make-map.ps1`.
#[test]
fn ps1_missing_with_short_descriptive_block() {
    const INPUT: &str = "\
#
# Create a map index of existing branches' documentation.
#
Write-Output \"done\"
";
    const EXPECTED: &str = "\
# Copyright (c) Microsoft Corporation.

#
# Create a map index of existing branches' documentation.
#
Write-Output \"done\"
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Missing
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED, "descriptive comments must be preserved");
}

/// Repro from ox-sdk `load_testing/scenarios/scenario_workers.ps1` —
/// a multi-line descriptive header followed by a `param(` block.
#[test]
fn ps1_missing_with_banner_comment() {
    const INPUT: &str = "\
# ============================================================================
# Script: scenario_workers.ps1
#
# Benchmarks throughput as the number of worker processes increases, keeping
# concurrency fixed.
# ============================================================================

param(
    [Parameter(Mandatory=$true)]
    $ServerAddress
)
";
    const EXPECTED: &str = "\
# Copyright (c) Microsoft Corporation.

# ============================================================================
# Script: scenario_workers.ps1
#
# Benchmarks throughput as the number of worker processes increases, keeping
# concurrency fixed.
# ============================================================================

param(
    [Parameter(Mandatory=$true)]
    $ServerAddress
)
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Missing
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED, "banner comment must be preserved");
}

// ── Missing header with no comments at all ───────────────────────────

#[test]
fn ps1_missing_no_comments() {
    const INPUT: &str = "\
param($Name)
Write-Output $Name
";
    const EXPECTED: &str = "\
# Copyright (c) Microsoft Corporation.

param($Name)
Write-Output $Name
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Missing
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED);
}

// ── Correct header present ───────────────────────────────────────────

#[test]
fn ps1_ok() {
    const INPUT: &str = "\
# Copyright (c) Microsoft Corporation.

Write-Output \"hello\"
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Ok
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Ok);
    assert_eq!(out, INPUT);
}

/// Header present with trailing descriptive comments — still OK
/// because `header_matches` does prefix matching.
#[test]
fn ps1_ok_with_trailing_comments() {
    const INPUT: &str = "\
# Copyright (c) Microsoft Corporation.
#
# Some descriptive comment.

Write-Output \"hello\"
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Ok
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Ok);
    assert_eq!(out, INPUT);
}

// ── Mismatch — wrong license header ──────────────────────────────────

#[test]
fn ps1_mismatch_wrong_license() {
    const INPUT: &str = "\
# Licensed under the Apache License.

Write-Output \"hello\"
";
    const EXPECTED: &str = "\
# Copyright (c) Microsoft Corporation.

Write-Output \"hello\"
";

    assert!(matches!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Mismatch { .. }
    ));
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert!(matches!(result, CheckResult::Mismatch { .. }));
    assert_eq!(out, EXPECTED);
}

// ── Shebang file (missing header) ────────────────────────────────────

#[test]
fn ps1_shebang_missing_with_descriptive_comments() {
    const INPUT: &str = "\
#!/usr/bin/env pwsh
#
# Descriptive comment.
#
param($Name)
";
    const EXPECTED: &str = "\
#!/usr/bin/env pwsh
# Copyright (c) Microsoft Corporation.

#
# Descriptive comment.
#
param($Name)
";

    assert_eq!(
        check_str(INPUT, HEADER, FileKind::PowerShell),
        CheckResult::Missing
    );
    let (out, result) = fix_to_string(INPUT, HEADER, FileKind::PowerShell);
    assert_eq!(result, CheckResult::Missing);
    assert_eq!(out, EXPECTED, "shebang file: descriptive comments must be preserved");
}
