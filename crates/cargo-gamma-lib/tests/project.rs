// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Guards the isolated, embedded project generator used throughout the gamma test suites.

use std::fs;
use std::process::Command;

use cargo_gamma_lib::testing::{write_fixture, write_project};
use tempfile::TempDir;

fn builds_offline(directory: &TempDir) {
    let home = TempDir::new().expect("could not create an empty Cargo home");
    let built = Command::new(env!("CARGO"))
        .current_dir(directory.path())
        .env("CARGO_HOME", home.path())
        .args(["test", "--offline", "--no-run", "--target-dir"])
        .arg(directory.path().join("target"))
        .output()
        .expect("Cargo must be installed to build this test suite");
    assert!(
        built.status.success(),
        "the embedded project must build with an empty registry cache:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

#[test]
fn library_and_binary_samples_build_without_a_registry_cache() {
    for (path, source) in [
        ("src/lib.rs", "pub fn accepts(value: u32) -> bool { value >= 1 }\n"),
        ("src/main.rs", "fn main() { assert!(1_u32 >= 1); }\n"),
    ] {
        let directory = TempDir::new().expect("could not create a project directory");
        write_project(directory.path(), &[(path, source)]);

        builds_offline(&directory);
    }
}

#[test]
fn local_package_graphs_and_custom_rustflags_still_build_offline() {
    let directory = TempDir::new().expect("could not create a project directory");
    write_project(
        directory.path(),
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"subject\", \"oracle\"]\nresolver = \"2\"\n"),
            (
                "subject/Cargo.toml",
                "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
            ),
            (
                "subject/src/lib.rs",
                "#[cfg(not(embedded_fixture))]\ncompile_error!(\"fixture rustflags were lost\");\n\
                 pub fn accepts(value: u32) -> bool { value >= 1 }\n",
            ),
            (
                "oracle/Cargo.toml",
                "[package]\nname = \"oracle\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 [dependencies]\nsubject = { path = \"../subject\" }\n",
            ),
            (
                "oracle/src/lib.rs",
                "#[test]\nfn the_boundary_is_pinned() { assert!(subject::accepts(1)); }\n",
            ),
            (
                ".cargo/config.toml",
                "[build]\nrustflags = [\"--cfg\", \"embedded_fixture\", \"--check-cfg\", \"cfg(embedded_fixture)\"]\n",
            ),
        ],
    );

    builds_offline(&directory);
}

#[test]
fn source_bytes_and_explicit_package_metadata_are_preserved() {
    let directory = TempDir::new().expect("could not create a project directory");
    let manifest = "[package]\nname = \"named\"\nversion = \"1.2.3-alpha.1\"\nedition = \"2024\"\n[workspace]\n";
    let source = "\n/// A source-location fixture.\npub fn accepts(value: u32) -> bool {\n    value >= 1\n}\n";
    write_project(directory.path(), &[("Cargo.toml", manifest), ("src/lib.rs", source)]);

    assert_eq!(fs::read_to_string(directory.path().join("Cargo.toml")).expect("manifest"), manifest);
    assert_eq!(fs::read_to_string(directory.path().join("src/lib.rs")).expect("source"), source);
}

#[test]
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
#[should_panic(expected = "embedded fixture paths must stay inside their owned directory")]
fn embedded_files_cannot_escape_the_owned_directory() {
    let directory = TempDir::new().expect("could not create a fixture directory");
    write_fixture(directory.path(), &[("../outside.rs", "")]);
}
