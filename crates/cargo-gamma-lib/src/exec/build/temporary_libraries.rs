// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Temporary library targets that make Cargo's combined selectors valid for mixed workspaces.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};

use camino::{Utf8Path, Utf8PathBuf};
use toml_edit::{DocumentMut, Item, Table, value};

use super::super::workspace::Workspace;
use crate::Result;
use crate::discover::Plan;
use crate::error::error;

struct TemporaryLibrary {
    manifest: Utf8PathBuf,
    original: Vec<u8>,
    source: Utf8PathBuf,
}

/// Adds empty library targets only to the copied workspace for one Cargo check.
///
/// The package roots and target selectors stay identical to an all-library workspace. An empty
/// library changes no user source, and restoring the manifests before the code-generating build
/// keeps these targets out of the verdict oracle.
pub(super) struct TemporaryLibraries {
    entries: Vec<TemporaryLibrary>,
    restored: bool,
}

impl TemporaryLibraries {
    pub(super) fn install(work: &Workspace, plan: &Plan, packages: &[String]) -> Result<Self> {
        let mut guard = Self {
            entries: Vec::new(),
            restored: false,
        };

        for package in packages {
            let Some(directory) = plan.directory_of(package) else {
                return Err(error!("no manifest directory was recorded for workspace package `{package}`"));
            };
            let package_root = work.root.join(directory);
            let manifest = package_root.join("Cargo.toml");
            let _inside = crate::paths::require_within(&manifest, &work.root, "a temporary library manifest")?;
            let original = fs::read(manifest.as_std_path())
                .map_err(|cause| error!("could not read `{manifest}` before adding a temporary library").caused_by(cause))?;
            let text = String::from_utf8(original.clone()).map_err(|cause| error!("`{manifest}` is not UTF-8: {cause}"))?;
            let mut document = text
                .parse::<DocumentMut>()
                .map_err(|cause| error!("could not parse `{manifest}` before adding a temporary library: {cause}"))?;
            if document.contains_key("lib") {
                return Err(error!("`{manifest}` already declares a library target"));
            }

            let (source, name) = create_source(&package_root)?;
            guard.entries.push(TemporaryLibrary {
                manifest: manifest.clone(),
                original,
                source,
            });

            let mut library = Table::new();
            library["path"] = value(name);
            library["test"] = value(false);
            library["bench"] = value(false);
            library["doc"] = value(false);
            document["lib"] = Item::Table(library);
            fs::write(manifest.as_std_path(), document.to_string())
                .map_err(|cause| error!("could not add a temporary library target to `{manifest}`").caused_by(cause))?;
        }

        Ok(guard)
    }

    pub(super) fn restore(mut self) -> Result<()> {
        for entry in self.entries.iter().rev() {
            fs::write(entry.manifest.as_std_path(), &entry.original)
                .map_err(|cause| error!("could not restore `{}` after checking temporary libraries", entry.manifest).caused_by(cause))?;
            match fs::remove_file(entry.source.as_std_path()) {
                Ok(()) => {}
                Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
                Err(cause) => {
                    return Err(error!("could not remove temporary library `{}`", entry.source).caused_by(cause));
                }
            }
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for TemporaryLibraries {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        for entry in self.entries.iter().rev() {
            if fs::write(entry.manifest.as_std_path(), &entry.original).is_ok() {
                let _removed = fs::remove_file(entry.source.as_std_path());
            }
        }
    }
}

fn create_source(directory: &Utf8Path) -> Result<(Utf8PathBuf, String)> {
    for attempt in 0..128 {
        let name = format!("__cargo_gamma_check_lib_{}_{}.rs", std::process::id(), attempt);
        let path = directory.join(&name);
        let opened = OpenOptions::new().write(true).create_new(true).open(path.as_std_path());
        match opened {
            Ok(mut file) => {
                if let Err(cause) = file.write_all(b"#![no_std]\n//! Temporary library target for Cargo Gamma's compiler check.\n") {
                    drop(file);
                    let _removed = fs::remove_file(path.as_std_path());
                    return Err(error!("could not write temporary library `{path}`").caused_by(cause));
                }
                return Ok((path, name));
            }
            Err(cause) if cause.kind() == io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(error!("could not create temporary library `{path}`").caused_by(cause)),
        }
    }
    Err(error!("could not find an unused temporary library name in `{directory}`"))
}
