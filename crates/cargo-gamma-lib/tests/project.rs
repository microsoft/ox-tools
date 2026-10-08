// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guards the isolated, embedded project generator used throughout the gamma test suites.

use std::fs;

use cargo_gamma_lib::testing::{write_fixture, write_project};
use tempfile::TempDir;

#[path = "support/flag_values.rs"]
mod flag_values;
#[path = "support/project_inputs.rs"]
mod project_inputs;

#[test]
#[cfg_attr(
    miri,
    ignore = "materializes real filesystem outputs; Miri isolation does not support host temporary directories"
)]
fn source_bytes_and_explicit_package_metadata_are_preserved() {
    let directory = TempDir::new().expect("could not create a project directory");
    let manifest = "[package]\nname = \"named\"\nversion = \"1.2.3-alpha.1\"\nedition = \"2024\"\n[workspace]\n";
    let source = "\n/// A source-location fixture.\npub fn accepts(value: u32) -> bool {\n    value >= 1\n}\n";
    write_project(directory.path(), &[("Cargo.toml", manifest), ("src/lib.rs", source)]);

    assert_eq!(fs::read_to_string(directory.path().join("Cargo.toml")).expect("manifest"), manifest);
    assert_eq!(fs::read_to_string(directory.path().join("src/lib.rs")).expect("source"), source);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "materializes real filesystem outputs; Miri isolation does not support host temporary directories"
)]
fn legacy_cargo_configuration_is_not_shadowed_by_a_new_file() {
    let directory = TempDir::new().expect("could not create a project directory");
    write_project(
        directory.path(),
        &[
            ("src/lib.rs", ""),
            (".cargo/config", "[build]\nrustflags = [\"--cfg\", \"legacy\"]\n"),
        ],
    );
    let path = directory.path().join(".cargo/config");
    let config: toml_edit::DocumentMut = fs::read_to_string(path).expect("legacy configuration").parse().expect("valid TOML");

    assert!(!directory.path().join(".cargo/config.toml").exists());
    assert_eq!(config["net"]["offline"].as_bool(), Some(true));
    assert_eq!(config["build"]["rustflags"][1].as_str(), Some("legacy"));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "materializes real filesystem outputs; Miri isolation does not support host temporary directories"
)]
fn extensionless_cargo_configuration_takes_precedence_when_both_files_exist() {
    let directory = TempDir::new().expect("could not create a project directory");
    let legacy = "[net]\noffline = false\n[build]\nrustflags = [\"--cfg\", \"legacy\"]\n";
    let modern = "[net]\noffline = false\n[build]\nrustflags = [\"--cfg\", \"modern\"]\n";
    write_project(
        directory.path(),
        &[("src/lib.rs", ""), (".cargo/config", legacy), (".cargo/config.toml", modern)],
    );
    let config: toml_edit::DocumentMut = fs::read_to_string(directory.path().join(".cargo/config"))
        .expect("legacy configuration")
        .parse()
        .expect("valid TOML");

    assert_eq!(config["net"]["offline"].as_bool(), Some(true));
    assert_eq!(config["build"]["rustflags"][1].as_str(), Some("legacy"));
    assert_eq!(
        fs::read_to_string(directory.path().join(".cargo/config.toml")).expect("modern configuration"),
        modern
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "materializes real filesystem outputs; Miri isolation does not support host temporary directories"
)]
fn deliberately_invalid_inputs_are_written_without_being_repaired() {
    let directory = TempDir::new().expect("could not create a fixture directory");
    let malformed = "[build\ntarget =\n";
    write_fixture(directory.path(), &[(".cargo/config.toml", malformed)]);

    assert_eq!(
        fs::read_to_string(directory.path().join(".cargo/config.toml")).expect("malformed fixture"),
        malformed
    );
    assert!(!directory.path().join("Cargo.toml").exists());
}

#[test]
#[cfg_attr(miri, ignore = "uses a host temporary output directory; pure path rejection is tested in process")]
#[should_panic(expected = "embedded fixture paths must stay inside their owned directory")]
fn embedded_files_cannot_escape_the_owned_directory() {
    let directory = TempDir::new().expect("could not create a fixture directory");
    write_fixture(directory.path(), &[("../outside.rs", "")]);
}

#[cfg(windows)]
#[test]
#[cfg_attr(miri, ignore = "uses a host temporary output directory; pure path rejection is tested in process")]
#[should_panic(expected = "embedded fixture paths must stay inside their owned directory")]
fn windows_drive_relative_assets_cannot_escape_the_owned_directory() {
    let directory = TempDir::new().expect("could not create a fixture directory");
    // An invalid Windows file name prevents an outside write if the containment guard regresses.
    write_fixture(directory.path(), &[("C:invalid<fixture>.rs", "")]);
}

#[cfg(windows)]
#[test]
#[cfg_attr(miri, ignore = "uses a host temporary output directory; pure path rejection is tested in process")]
#[should_panic(expected = "embedded fixture paths must stay inside their owned directory")]
fn windows_root_relative_assets_cannot_escape_the_owned_directory() {
    let directory = TempDir::new().expect("could not create a fixture directory");
    write_fixture(directory.path(), &[("\\invalid<fixture>.rs", "")]);
}
