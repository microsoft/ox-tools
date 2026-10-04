// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Isolated test data for directly exercising validation boundaries.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::model::{Package, Workspace};

pub(crate) fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("case-unit-")
        .tempdir()
        .unwrap()
}

pub(crate) fn workspace(root: &Path, packages: &[(&str, &str)]) -> Workspace {
    fs::write(root.join("Cargo.toml"), "[workspace]\nresolver='2'\nmembers=['crates/*']\n").unwrap();
    let mut members = BTreeMap::new();
    for (name, extra) in packages {
        let directory = root.join("crates").join(name);
        fs::create_dir_all(directory.join("src")).unwrap();
        fs::write(directory.join("src").join("lib.rs"), "pub fn fixture() {}").unwrap();
        let manifest = directory.join("Cargo.toml");
        let text = format!("[package]\nname='{name}'\nversion='1.0.0'\nedition='2021'\n{extra}");
        fs::write(&manifest, &text).unwrap();
        members.insert(
            (*name).to_owned(),
            Package {
                name: (*name).to_owned(),
                version: "1.0.0".parse().unwrap(),
                manifest: manifest.canonicalize().unwrap(),
                document: text.parse().unwrap(),
                publish: extra.contains("publish=false").then(Vec::new),
            },
        );
    }
    Workspace {
        root: root.canonicalize().unwrap(),
        document: "[workspace]\nresolver='2'\n".parse().unwrap(),
        packages: members,
    }
}
