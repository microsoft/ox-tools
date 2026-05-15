// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration file parsing for `cargo-heather`.
//!
//! Reads `.cargo-heather.toml` from the project root and resolves
//! the expected license header text. Falls back to the `license`
//! field in `Cargo.toml` when no dedicated config file exists.

use std::path::{Path, PathBuf};

use cargo_heather::HeatherError;
use cargo_heather::license;
use serde::Deserialize;
use tracing::info;

/// The default configuration file name.
pub(crate) const CONFIG_FILE_NAME: &str = ".cargo-heather.toml";

/// Raw deserialized configuration from `.cargo-heather.toml`.
#[derive(Debug, Deserialize)]
struct RawConfig {
    license: Option<String>,
    header: Option<String>,
    scripts: Option<bool>,
    dot_toml: Option<bool>,
    exclude: Option<Vec<String>>,
}

/// Minimal representation of `Cargo.toml` for license fallback.
#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
    workspace: Option<WorkspaceSection>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    license: Option<LicenseField>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LicenseField {
    Plain(String),
    Workspace { workspace: bool },
}

#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    package: Option<WorkspacePackage>,
}

#[derive(Debug, Deserialize)]
struct WorkspacePackage {
    license: Option<String>,
}

/// Resolved configuration with the final header text and processing options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeatherConfig {
    pub(crate) header_text: String,
    pub(crate) scripts: bool,
    pub(crate) dot_toml: bool,
    pub(crate) exclude: Vec<String>,
}

impl HeatherConfig {
    fn with_defaults(header_text: String) -> Self {
        Self {
            header_text,
            scripts: true,
            dot_toml: false,
            exclude: Vec::new(),
        }
    }
}

/// Load and resolve the configuration.
pub(crate) fn load_config(project_dir: &Path) -> Result<HeatherConfig, HeatherError> {
    let config_path = project_dir.join(CONFIG_FILE_NAME);
    if config_path.exists() {
        return load_config_from_path(&config_path);
    }

    let cargo_toml_path = project_dir.join("Cargo.toml");
    if cargo_toml_path.exists()
        && let Some(config) = try_load_from_cargo_toml(&cargo_toml_path)?
    {
        info!("No {} found, using license from Cargo.toml.", CONFIG_FILE_NAME);
        return Ok(config);
    }

    Err(HeatherError::ConfigNotFound(config_path))
}

/// Load and resolve configuration from a specific file path.
pub(crate) fn load_config_from_path(config_path: &Path) -> Result<HeatherConfig, HeatherError> {
    let content = read_config_file(config_path)?;
    let raw = parse_raw_config(config_path, &content)?;
    resolve_config(raw)
}

fn read_config_file(path: &Path) -> Result<String, HeatherError> {
    if !path.exists() {
        return Err(HeatherError::ConfigNotFound(path.to_path_buf()));
    }
    std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })
}

fn parse_raw_config(path: &Path, content: &str) -> Result<RawConfig, HeatherError> {
    toml::from_str(content).map_err(|e| HeatherError::ConfigParse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

fn resolve_config(raw: RawConfig) -> Result<HeatherConfig, HeatherError> {
    let base = match (raw.license, raw.header) {
        (Some(_), Some(_)) => Err(HeatherError::ConfigInvalid("specify either 'license' or 'header', not both".into())),
        (None, None) => Err(HeatherError::ConfigInvalid(
            "must specify either 'license' (SPDX identifier) or 'header' (custom text)".into(),
        )),
        (Some(spdx_id), None) => {
            let header_text = license::header_for_license(&spdx_id)?;
            Ok(header_text.to_owned())
        }
        (None, Some(header)) => {
            if header.trim().is_empty() {
                return Err(HeatherError::ConfigInvalid("'header' must not be empty".into()));
            }
            Ok(header)
        }
    }?;

    Ok(HeatherConfig {
        header_text: base,
        scripts: raw.scripts.unwrap_or(true),
        dot_toml: raw.dot_toml.unwrap_or(false),
        exclude: raw.exclude.unwrap_or_default(),
    })
}

fn try_load_from_cargo_toml(path: &Path) -> Result<Option<HeatherConfig>, HeatherError> {
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let manifest: CargoManifest = toml::from_str(&content).map_err(|e| HeatherError::ConfigParse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    if let Some(field) = manifest.package.and_then(|p| p.license) {
        return match field {
            LicenseField::Plain(id) if !id.trim().is_empty() => {
                let header_text = license::header_for_license(&id)?;
                Ok(Some(HeatherConfig::with_defaults(header_text.to_owned())))
            }
            LicenseField::Workspace { workspace: true } => resolve_workspace_license(path),
            LicenseField::Plain(_) | LicenseField::Workspace { workspace: false } => Ok(None),
        };
    }

    if let Some(id) = manifest
        .workspace
        .and_then(|w| w.package)
        .and_then(|p| p.license)
        .filter(|id| !id.trim().is_empty())
    {
        let header_text = license::header_for_license(&id)?;
        return Ok(Some(HeatherConfig::with_defaults(header_text.to_owned())));
    }

    Ok(None)
}

fn resolve_workspace_license(package_cargo_toml: &Path) -> Result<Option<HeatherConfig>, HeatherError> {
    let workspace_root = find_workspace_root(package_cargo_toml)?;

    let content = std::fs::read_to_string(&workspace_root).map_err(|e| HeatherError::FileRead {
        path: workspace_root.clone(),
        source: e,
    })?;

    let manifest: CargoManifest = toml::from_str(&content).map_err(|e| HeatherError::ConfigParse {
        path: workspace_root,
        message: e.to_string(),
    })?;

    let spdx_id = manifest
        .workspace
        .and_then(|w| w.package)
        .and_then(|p| p.license)
        .filter(|id| !id.trim().is_empty());

    match spdx_id {
        Some(id) => {
            let header_text = license::header_for_license(&id)?;
            Ok(Some(HeatherConfig::with_defaults(header_text.to_owned())))
        }
        None => Ok(None),
    }
}

fn find_workspace_root(package_cargo_toml: &Path) -> Result<PathBuf, HeatherError> {
    let start_dir = package_cargo_toml
        .parent()
        .ok_or_else(|| HeatherError::ConfigInvalid(format!("cannot determine parent directory of '{}'", package_cargo_toml.display())))?;

    let mut current = start_dir;

    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.exists() && cargo_toml_has_workspace(&candidate)? {
            return Ok(candidate);
        }

        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }

    Err(HeatherError::ConfigInvalid(format!(
        "license.workspace = true in '{}' but no workspace root Cargo.toml found",
        package_cargo_toml.display()
    )))
}

fn cargo_toml_has_workspace(path: &Path) -> Result<bool, HeatherError> {
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let manifest: CargoManifest = toml::from_str(&content).map_err(|e| HeatherError::ConfigParse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    Ok(manifest.workspace.is_some())
}

/// Return the expected config file path for a project directory.
pub(crate) fn config_path_for(project_dir: &Path) -> PathBuf {
    project_dir.join(CONFIG_FILE_NAME)
}
