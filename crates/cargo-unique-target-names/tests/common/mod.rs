// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synthetic cargo workspaces shared by the test suites.

#![allow(dead_code, reason = "each test suite uses a different part of the builder")]

use std::fs;
use std::path::Path;

use tempfile::{Builder, TempDir};

/// A workspace member: its directory and package name, plus the names of the
/// uplifted targets it declares.
pub struct Member {
    directory: String,
    package: String,
    example: String,
    binary: String,
    library_example: String,
    library: Option<(String, String)>,
    extra_manifest: Option<String>,
}

impl Member {
    /// A member whose uplifted target names are all distinct, so nothing
    /// collides unless a test asks for it. Its library keeps the default
    /// (unique) package target name and the default `lib` crate type.
    pub fn new(package: &str, example: &str, binary: &str, library_example: &str) -> Self {
        Self {
            directory: package.to_owned(),
            package: package.to_owned(),
            example: example.to_owned(),
            binary: binary.to_owned(),
            library_example: library_example.to_owned(),
            library: None,
            extra_manifest: None,
        }
    }

    /// The same, with an explicit library name and crate type so a
    /// library-level collision can be provoked.
    #[must_use]
    pub fn with_library(mut self, name: &str, crate_type: &str) -> Self {
        self.library = Some((name.to_owned(), crate_type.to_owned()));
        self
    }

    /// The same, in a directory that does not match the package name. Needed to
    /// give two members package names differing only in case, which their
    /// directories cannot do on a case-insensitive filesystem.
    #[must_use]
    pub fn in_directory(mut self, directory: &str) -> Self {
        directory.clone_into(&mut self.directory);
        self
    }

    /// The same, with extra manifest text appended -- an additional target
    /// declaration a test needs that the standard shape does not provide.
    #[must_use]
    pub fn with_extra_manifest(mut self, extra: &str) -> Self {
        self.extra_manifest = Some(extra.to_owned());
        self
    }
}

/// Builds a workspace whose members all declare the same duplicated `test` and
/// `bench` names -- the only kinds Cargo never uplifts -- plus the per-member
/// uplifted names given by `members`.
#[must_use]
pub fn workspace(members: &[Member]) -> TempDir {
    let temp = Builder::new()
        .prefix("unique target names repo's [copy] (fork) ")
        .tempdir()
        .expect("temporary repository must be creatable");
    write_workspace(temp.path(), members);
    temp
}

fn write_workspace(root: &Path, members: &[Member]) {
    let list = members
        .iter()
        .map(|member| format!("\"{}\"", member.directory))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"3\"\nmembers = [{list}]\n"),
    )
    .expect("workspace manifest must be writable");

    for member in members {
        let package = root.join(&member.directory);
        for directory in ["src", "src/bin", "examples", "tests", "benches"] {
            fs::create_dir_all(package.join(directory)).expect("member directories must be creatable");
        }
        let library = match &member.library {
            Some((name, crate_type)) => {
                format!("[lib]\nname = \"{name}\"\npath = \"src/lib.rs\"\ncrate-type = [\"{crate_type}\"]\n")
            }
            None => "[lib]\npath = \"src/lib.rs\"\n".to_owned(),
        };
        fs::write(
            package.join("Cargo.toml"),
            format!(
                concat!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n",
                    "{library}\n",
                    "[[example]]\nname = \"{library_example}\"\npath = \"examples/{library_example}.rs\"\ncrate-type = [\"lib\"]\n"
                ),
                name = member.package,
                library = library,
                library_example = member.library_example
            ),
        )
        .expect("member manifest must be writable");
        if let Some(extra) = &member.extra_manifest {
            let mut manifest = fs::read_to_string(package.join("Cargo.toml")).expect("member manifest must be readable");
            manifest.push('\n');
            manifest.push_str(extra);
            fs::write(package.join("Cargo.toml"), manifest).expect("member manifest must be writable");
        }
        fs::write(package.join("src/lib.rs"), "").expect("member library must be writable");
        fs::write(package.join(format!("src/bin/{}.rs", member.binary)), "fn main() {}\n").expect("member binary must be writable");
        fs::write(package.join(format!("examples/{}.rs", member.example)), "fn main() {}\n").expect("member example must be writable");
        fs::write(package.join(format!("examples/{}.rs", member.library_example)), "").expect("library example must be writable");
        // Duplicated across members on purpose: Cargo keeps a metadata hash on
        // these, so they must never be reported.
        fs::write(package.join("tests/shared.rs"), "").expect("member test must be writable");
        fs::write(package.join("benches/shared.rs"), "fn main() {}\n").expect("member bench must be writable");
    }
}
