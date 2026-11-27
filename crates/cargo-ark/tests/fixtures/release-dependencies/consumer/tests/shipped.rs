// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

fn check_archive() {
    assert!(std::path::Path::new("Cargo.toml.orig").is_file());
    let cargo_home = std::path::PathBuf::from(std::env::var_os("CARGO_HOME").unwrap());
    assert!(cargo_home.is_absolute(), "Cargo home must still resolve from the archive");
    let expected_home = std::env::var_os("ARK_FIXTURE_CARGO_HOME").unwrap();
    assert_eq!(
        cargo_home.canonicalize().unwrap(),
        std::fs::canonicalize(expected_home).unwrap(),
        "Cargo home must resolve relative to Ark's invocation directory"
    );
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
}

#[test]
#[cfg(all(feature = "baseline", not(feature = "extra")))]
fn archive_default() {
    check_archive();
    assert!(std::env::var_os("ARK_FIXTURE_FAIL_TEST").is_none(), "deliberately failing shipped test");
}
