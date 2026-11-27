// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use walkdir::WalkDir;

/// Stage the checked-in workspace unchanged except for its manifest filenames:
/// Cargo would omit the entire subtree from the package if they were named `Cargo.toml`.
pub fn stage(name: &str) -> TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let temp = tempfile::tempdir().expect("the fixture needs a private workspace");
    for entry in WalkDir::new(&source) {
        let entry = entry.expect("fixture entries are readable");
        let mut dest = temp
            .path()
            .join(entry.path().strip_prefix(&source).expect("walk starts at the fixture root"));
        if entry.file_type().is_dir() {
            fs::create_dir_all(dest).expect("the test owns its workspace");
        } else {
            if entry.file_name() == "Cargo.toml.in" {
                dest.set_file_name("Cargo.toml");
            }
            fs::copy(entry.path(), dest).expect("fixture files are readable and the test owns its workspace");
        }
    }
    temp
}
