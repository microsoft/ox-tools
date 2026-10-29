// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native tests of the binary's private report-file boundary.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

#[cfg(windows)]
use std::path::Path;
use std::{fs, io};

#[path = "../src/package_glob.rs"]
mod package_glob;
#[path = "../src/bin/cargo-coverage-gate/report_file.rs"]
mod report_file;

#[test]
fn filtered_report_publication_returns_rename_errors_and_removes_staging_files() {
    let output = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let blocked = tempfile::Builder::new().prefix("blocked-").tempdir_in(output.path()).unwrap();
    let error = report_file::publish_reports([(blocked.path(), "all-features-filtered.info".to_owned(), "filtered")]).unwrap_err();
    let cause = error.find_source::<io::Error>().unwrap();
    #[cfg(windows)]
    assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);
    #[cfg(not(windows))]
    assert_eq!(cause.kind(), io::ErrorKind::IsADirectory);
    let remaining = fs::read_dir(output.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(remaining, [blocked.path().to_path_buf()]);
}

#[test]
#[cfg(windows)]
fn survivor_native_sync_preserves_read_only_handle_failure_without_writing() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let file = fs::File::open(path).unwrap();
    let native_error = file.sync_all().unwrap_err();
    let delegated_error = report_file::SynchronizedWrite::sync_all(&file).unwrap_err();
    assert_eq!(delegated_error.kind(), native_error.kind());
    assert_eq!(delegated_error.raw_os_error(), native_error.raw_os_error());
}
