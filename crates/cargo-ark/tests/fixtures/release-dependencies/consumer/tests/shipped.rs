// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn check_archive() {
    assert!(std::path::Path::new("Cargo.toml.orig").is_file());
    assert_eq!(ark_fixture_consumer::value(), 42);
}

#[test]
#[cfg(all(feature = "baseline", feature = "extra"))]
fn archive_all() {
    check_archive();
}

#[test]
#[cfg(not(any(feature = "baseline", feature = "extra")))]
fn archive_none() {
    check_archive();
    assert!(std::env::var_os("ARK_FIXTURE_FAIL_TEST").is_none(), "deliberately failing shipped test");
}
