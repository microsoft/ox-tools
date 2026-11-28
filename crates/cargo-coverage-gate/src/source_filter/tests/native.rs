// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exercise the native source reader with fixed Cargo metadata and immutable source files.

use super::*;
use crate::error::MergedSourceChecksumError;

fn root() -> PathBuf {
    fs::canonicalize(source_index_fixture_root()).expect("the packaged source-index fixture exists")
}

fn with_lib<T>(source: &Path, operation: impl FnOnce() -> T) -> T {
    let mut metadata = source_index_fixture_metadata(&root());
    let targets = &mut metadata.packages[0].targets;
    targets.truncate(1);
    targets[0].src_path = source
        .to_path_buf()
        .try_into()
        .expect("fixture paths originate in Cargo's UTF-8 manifest path");
    crate::workspace::metadata_loader_test_seam::with_loader(
        move |manifest| {
            assert_eq!(manifest, None);
            Ok(metadata.clone())
        },
        operation,
    )
}

#[test]
#[cfg_attr(miri, ignore = "canonicalizes immutable source fixtures")]
fn survivor_filtered_named_tests_propagate_cross_section_checksum_conflicts() {
    let source = root().join("member/src/lib.rs");
    let input = format!(
        "TN:first-test\nSF:{0}\nDA:4,1,first\nend_of_record\n\
         TN:second-test\nSF:{0}\nDA:4,1,second\nend_of_record\n",
        source.display()
    );
    let error = with_lib(&source, || crate::evaluate(&input, None, &[])).unwrap_err();
    let cause = error_cause::<MergedSourceChecksumError>(&error);
    assert_eq!(cause.line, 4);
    assert_eq!(Path::new(&cause.path), source);
}

#[test]
#[cfg_attr(miri, ignore = "runs the native Rust compiler to locate its sources")]
fn survivor_missing_sources_return_errors_without_panicking() {
    let root = root();
    let missing = missing_source_path();
    let error = with_lib(&root.join("member/src/lib.rs"), || {
        crate::filter_lcov(&lcov_section(&missing, &[(1, 0)]), None)
    })
    .unwrap_err();
    assert_eq!(error_cause::<ResolveCoverageSourceError>(&error).path, diagnostic_path(&missing));

    let present = root.join("external-lib.rs");
    let error = with_lib(&root.join("member/src/lib.rs"), || {
        crate::filter_lcov(&lcov_section(&present, &[(1, 0)]), None)
    })
    .unwrap_err();
    assert_eq!(error_cause::<UnownedCoverageSourceError>(&error).path, diagnostic_path(&present));
}

#[test]
#[cfg_attr(miri, ignore = "canonicalizes immutable source fixtures")]
fn survivor_metadata_resolution_failures_are_returned_and_empty_reports_do_not_resolve() {
    let missing = missing_source_path();
    let empty = crate::workspace::metadata_loader_test_seam::with_loader(
        |_| panic!("empty reports must not resolve metadata"),
        || crate::filter_lcov_many(&[], None),
    );
    assert_eq!(empty.unwrap(), [] as [String; 0]);
    let error = with_lib(&missing, || crate::filter_lcov("", None)).unwrap_err();
    assert_eq!(error_cause::<ResolveCoverageSourceError>(&error).path, diagnostic_path(&missing));
}

#[test]
#[cfg_attr(miri, ignore = "reads immutable source fixtures")]
fn survivor_native_source_reads_return_errors_for_unreadable_roots_and_included_sources() {
    let src = root().join("member/src");
    let directory = src.join("implementation");
    for lib in [&directory, &src.join("unreadable-include.rs")] {
        let error = with_lib(lib, || crate::filter_lcov("", None)).unwrap_err();
        assert_eq!(error_cause::<ReadCoverageSourceError>(&error).path, diagnostic_path(&directory));
    }
}

#[test]
#[cfg_attr(miri, ignore = "reads immutable source fixtures")]
fn survivor_native_included_sources_preserve_line_bounds_and_promote_to_released_analysis() {
    let src = root().join("member/src");
    let included = src.join("included-test.rs");
    let last = lcov_section(&included, &[(4, 0)]);
    with_lib(&src.join("test-include-root.rs"), || {
        assert_eq!(crate::filter_lcov(&last, None).unwrap(), "");
        let error = crate::filter_lcov(&lcov_section(&included, &[(5, 0)]), None).unwrap_err();
        let cause = error_cause::<CoverageLineOutOfRangeError>(&error);
        assert_eq!((cause.line, cause.line_count), (5, 4));
        assert_eq!(cause.path, diagnostic_path(&included));
    });

    let filtered = with_lib(&src.join("promoted-include-root.rs"), || crate::filter_lcov(&last, None)).unwrap();
    assert_eq!(parse_lcov(&filtered), parse_lcov(&last));
}

#[test]
#[cfg_attr(miri, ignore = "reads immutable source fixtures")]
fn survivor_metadata_returns_conditional_module_errors_from_an_immutable_source() {
    let source = root().with_file_name("conditional-module.rs");
    let error = with_lib(&source, || crate::filter_lcov("", None)).unwrap_err();
    let cause = error_cause::<ConditionalModulePathError>(&error);
    assert_eq!(cause.module, "model");
    assert_eq!(cause.attribute, r#"#[cfg_attr(all(), path = "selected.rs")]"#);
    assert_eq!(cause.declaring, diagnostic_path(&source));
}
