// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native source-filter regressions through the released library API.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::fs;
use std::path::{Path, PathBuf};

use cargo_coverage_gate::{evaluate, filter_lcov, filter_lcov_many};

fn fixture_root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source-index"))
        .expect("the checked-in source-index fixture directory must exist")
}

fn missing_source_path() -> PathBuf {
    // A regular file cannot also contain this source's parent directory.
    fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .expect("Cargo provides the existing crate directory")
        .join("Cargo.toml")
        .join("survivor-absent-parent")
        .join("lib.rs")
}

fn lcov_section(path: &Path, line: usize) -> String {
    format!("TN:\nSF:{}\nDA:{line},0\nend_of_record\n", path.display())
}

fn manifest(scenario: &str) -> PathBuf {
    fixture_root().join("native-scenarios").join(scenario).join("Cargo.toml")
}

#[test]
fn survivor_filtered_named_tests_propagate_cross_section_checksum_conflicts() {
    let root = fixture_root();
    let source = root.join("member/src/lib.rs");
    let source = source.display();
    let input = format!(
        "TN:first-test\nSF:{source}\nDA:4,1,first\nend_of_record\n\
         TN:second-test\nSF:{source}\nDA:4,1,second\nend_of_record\n"
    );
    let error = evaluate(&input, Some(&root.join("Cargo.toml")), &[]).unwrap_err().to_string();
    assert!(error.contains("conflicting checksums for line 4"), "{error}");
    assert!(error.contains("lib.rs"), "{error}");
}

#[test]
fn survivor_missing_sources_return_errors_without_panicking() {
    let missing = missing_source_path();
    assert!(!missing.exists());
    let manifest = fixture_root().join("Cargo.toml");
    let error = filter_lcov(&lcov_section(&missing, 1), Some(&manifest)).unwrap_err().to_string();
    assert!(error.contains("failed to resolve Rust source path"), "{error}");
    assert!(error.contains(&missing.display().to_string()), "{error}");

    let present = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    assert!(present.is_absolute());
    let error = filter_lcov(&lcov_section(&present, 1), Some(&manifest)).unwrap_err().to_string();
    assert!(error.contains(&present.display().to_string()), "{error}");
    assert!(error.contains("not reachable"), "{error}");
}

#[test]
fn survivor_metadata_resolution_failures_are_returned_and_empty_reports_do_not_resolve() {
    let manifest = manifest("missing");
    let missing = manifest
        .parent()
        .unwrap()
        .join("Cargo.toml")
        .join("survivor-absent-parent")
        .join("lib.rs");
    assert!(filter_lcov_many(&[], Some(&manifest)).unwrap().is_empty());
    let error = filter_lcov("", Some(&manifest)).unwrap_err().to_string();
    assert!(error.contains("failed to resolve Rust source path"), "{error}");
    assert!(error.contains(&missing.display().to_string()), "{error}");
}

#[test]
fn survivor_native_source_reads_return_errors_for_unreadable_roots_and_included_sources() {
    let root_manifest = manifest("unreadable-root");
    let directory = fs::canonicalize(root_manifest.parent().unwrap()).unwrap();
    let error = filter_lcov("", Some(&root_manifest)).unwrap_err().to_string();
    assert!(error.contains("failed to read Rust source"), "{error}");
    assert!(error.contains(&directory.display().to_string()), "{error}");
    let included = directory;
    let error = filter_lcov("", Some(&manifest("unreadable-include"))).unwrap_err().to_string();
    assert!(error.contains("failed to read Rust source"), "{error}");
    assert!(error.contains(&included.display().to_string()), "{error}");
}

#[test]
fn survivor_native_included_sources_preserve_line_bounds_and_promote_to_released_analysis() {
    let included = fs::canonicalize(fixture_root().join("native-scenarios/included/included-test.rs")).unwrap();
    let text = include_str!("fixtures/source-index/native-scenarios/included/included-test.rs");
    let manifest = manifest("included");
    assert_eq!(
        filter_lcov(&lcov_section(&included, text.lines().count()), Some(&manifest)).unwrap(),
        ""
    );
    let error = filter_lcov(&lcov_section(&included, text.lines().count() + 1), Some(&manifest))
        .unwrap_err()
        .to_string();
    assert!(error.contains("line 5"), "{error}");
    assert!(error.contains("source has 4 lines"), "{error}");
    assert!(error.contains(&included.display().to_string()), "{error}");

    let promoted = fixture_root().join("native-scenarios/promoted/Cargo.toml");
    let filtered = filter_lcov(&lcov_section(&included, 4), Some(&promoted)).unwrap();
    assert!(filtered.contains("DA:4,0"), "{filtered}");
    assert!(filtered.contains("included-test.rs"), "{filtered}");
}

#[test]
fn survivor_metadata_returns_conditional_module_errors_from_an_immutable_source() {
    let error = filter_lcov("", Some(&manifest("conditional"))).unwrap_err().to_string();
    assert!(error.contains("model"), "{error}");
    assert!(error.contains("cfg_attr"), "{error}");
    assert!(error.contains("selected.rs"), "{error}");
}
