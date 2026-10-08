// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Materializes embedded test assets without using the repository as a sample project.
//!
//! Kept under `tests/` so `cargo-llvm-cov`'s default report exclusions apply.

use std::fs;
use std::path::Path;

#[path = "project_inputs.rs"]
mod inputs;

const SUBJECT_MANIFEST: &str = "[package]\nname = \"subject\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\n[workspace]\n";

/// Writes embedded files into an owned test directory without changing their contents.
///
/// Use this for deliberately incomplete or malformed inputs, including configuration tests.
pub fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for &(relative, contents) in files {
        let relative = Path::new(relative);
        assert!(
            inputs::safe_relative_path(relative),
            "embedded fixture paths must stay inside their owned directory: {}",
            relative.display()
        );
        let path = root.join(relative);
        let parent = path.parent().expect("a file joined to the fixture root has a parent");
        fs::create_dir_all(parent).unwrap_or_else(|cause| panic!("could not create fixture directory {}: {cause}", parent.display()));
        fs::write(&path, contents).unwrap_or_else(|cause| panic!("could not write embedded fixture {}: {cause}", path.display()));
    }
}

/// Writes an isolated Cargo project from embedded files and disables Cargo networking.
///
/// An explicit manifest preserves specialized package and workspace layouts. Otherwise the
/// project receives a standalone, dependency-free `subject` manifest. Sources are written exactly
/// as supplied, so source-location assertions keep their original line and column numbers.
/// If both Cargo configuration files exist, the file without an extension takes precedence.
pub fn write_project(root: &Path, files: &[(&str, &str)]) {
    write_fixture(root, files);
    if !root.join("Cargo.toml").exists() {
        write_fixture(root, &[("Cargo.toml", SUBJECT_MANIFEST)]);
    }

    let configuration = if root.join(".cargo/config").exists() {
        ".cargo/config"
    } else {
        ".cargo/config.toml"
    };
    let path = root.join(configuration);
    let text = if path.exists() {
        fs::read_to_string(&path).unwrap_or_else(|cause| panic!("could not read embedded Cargo configuration {}: {cause}", path.display()))
    } else {
        String::new()
    };
    let document = inputs::offline_configuration(&text)
        .unwrap_or_else(|cause| panic!("project configuration must be valid TOML; use write_fixture for malformed inputs: {cause}"));
    write_fixture(root, &[(configuration, &document.to_string())]);
}
