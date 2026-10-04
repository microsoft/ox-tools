// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Versioned artifacts and internal workspace identities.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::DocumentMut;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidate {
    pub name: String,
    pub version: String,
    pub registry: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Candidates {
    pub schema_version: u32,
    pub base_commit: String,
    pub head_commit: String,
    pub source_root: PathBuf,
    pub source_digest: String,
    pub configuration_digest: String,
    pub artifact_roots: Vec<PathBuf>,
    #[expect(clippy::struct_field_names, reason = "The versioned JSON artifact has an explicit candidates key")]
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug)]
pub(crate) struct Package {
    pub name: String,
    pub version: semver::Version,
    pub manifest: PathBuf,
    pub publish: Option<Vec<String>>,
    pub document: DocumentMut,
}

impl Package {
    pub fn directory(&self) -> &Path {
        self.manifest
            .parent()
            .expect("workspace discovery canonicalizes a Cargo.toml file path, which cannot be a filesystem root")
    }

    pub fn publishable(&self) -> bool {
        self.publish.as_ref().is_none_or(|registries| !registries.is_empty())
    }
}

#[derive(Debug)]
pub(crate) struct Workspace {
    pub root: PathBuf,
    pub document: DocumentMut,
    pub packages: BTreeMap<String, Package>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub schema_version: u32,
    pub operation: String,
    pub status: String,
    pub diagnostics: Vec<String>,
    pub candidates: Vec<Candidate>,
    pub derived_requirements: Vec<String>,
    pub registry_probes: Vec<RegistryProbe>,
    pub commands: Vec<CommandRecord>,
    pub provenance: Vec<Provenance>,
}

impl Report {
    pub fn new(operation: &str) -> Self {
        Self {
            schema_version: 1,
            operation: operation.into(),
            status: "running".into(),
            diagnostics: Vec::new(),
            candidates: Vec::new(),
            derived_requirements: Vec::new(),
            registry_probes: Vec::new(),
            commands: Vec::new(),
            provenance: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RegistryProbe {
    pub name: String,
    pub version: String,
    pub registry: String,
    pub state: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommandRecord {
    pub phase: String,
    pub arguments: Vec<String>,
    pub exit_code: Option<i32>,
    pub diagnostic: String,
    pub output: String,
    pub working_directory: PathBuf,
    pub target_directory: PathBuf,
}

#[derive(Debug, Serialize)]
pub(crate) struct Provenance {
    pub configuration: String,
    pub name: String,
    pub version: String,
    pub package_id: String,
    pub source: Option<String>,
    pub manifest_path: PathBuf,
}
