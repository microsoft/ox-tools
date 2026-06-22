// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration file parsing for `cargo-heather`.
//!
//! Reads `.cargo-heather.toml` from the project root and resolves
//! the expected license header text. Falls back to the `license`
//! field in `Cargo.toml` when no dedicated config file exists.

use std::path::{Path, PathBuf};

use cargo_heather::{HeatherError, license};
use serde::Deserialize;

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
        println!("No {CONFIG_FILE_NAME} found, using license from Cargo.toml.");
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn read_config_file_missing_path_is_config_not_found() {
        let err = read_config_file(Path::new("/definitely/missing/.cargo-heather.toml")).unwrap_err();
        assert!(matches!(err, HeatherError::ConfigNotFound(_)), "{err}");
    }

    #[test]
    fn read_config_file_on_directory_is_file_read_error() {
        let tmp = TempDir::new().unwrap();
        // A directory exists() but read_to_string fails on it.
        let err = read_config_file(tmp.path()).unwrap_err();
        assert!(matches!(err, HeatherError::FileRead { .. }), "{err}");
    }

    #[test]
    fn parse_raw_config_rejects_malformed_toml() {
        let err = parse_raw_config(Path::new("x.toml"), "this = = not toml").unwrap_err();
        assert!(matches!(err, HeatherError::ConfigParse { .. }), "{err}");
    }

    #[test]
    fn resolve_config_rejects_both_license_and_header() {
        let raw = RawConfig {
            license: Some("MIT".into()),
            header: Some("// H".into()),
            scripts: None,
            dot_toml: None,
            exclude: None,
        };
        assert!(matches!(resolve_config(raw), Err(HeatherError::ConfigInvalid(_))));
    }

    #[test]
    fn resolve_config_rejects_neither_license_nor_header() {
        let raw = RawConfig {
            license: None,
            header: None,
            scripts: None,
            dot_toml: None,
            exclude: None,
        };
        assert!(matches!(resolve_config(raw), Err(HeatherError::ConfigInvalid(_))));
    }

    #[test]
    fn resolve_config_rejects_empty_header() {
        let raw = RawConfig {
            license: None,
            header: Some("   ".into()),
            scripts: None,
            dot_toml: None,
            exclude: None,
        };
        assert!(matches!(resolve_config(raw), Err(HeatherError::ConfigInvalid(_))));
    }

    #[test]
    fn resolve_config_accepts_spdx_license_and_applies_option_defaults() {
        let raw = RawConfig {
            license: Some("MIT".into()),
            header: None,
            scripts: None,
            dot_toml: None,
            exclude: None,
        };
        let cfg = resolve_config(raw).unwrap();
        assert!(!cfg.header_text.is_empty());
        assert!(cfg.scripts); // default true
        assert!(!cfg.dot_toml); // default false
        assert!(cfg.exclude.is_empty());
    }

    #[test]
    fn resolve_config_accepts_custom_header_and_explicit_options() {
        let raw = RawConfig {
            license: None,
            header: Some("// Custom".into()),
            scripts: Some(false),
            dot_toml: Some(true),
            exclude: Some(vec!["target".into()]),
        };
        let cfg = resolve_config(raw).unwrap();
        assert_eq!(cfg.header_text, "// Custom");
        assert!(!cfg.scripts);
        assert!(cfg.dot_toml);
        assert_eq!(cfg.exclude, vec!["target".to_owned()]);
    }

    #[test]
    fn load_config_errors_when_neither_config_nor_cargo_toml_present() {
        let tmp = TempDir::new().unwrap();
        let err = load_config(tmp.path()).unwrap_err();
        assert!(matches!(err, HeatherError::ConfigNotFound(_)), "{err}");
    }

    #[test]
    fn load_config_falls_back_to_cargo_toml_license() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\nlicense = \"MIT\"\n");
        let cfg = load_config(tmp.path()).unwrap();
        assert!(!cfg.header_text.is_empty());
    }

    #[test]
    fn try_load_from_cargo_toml_rejects_malformed() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), "Cargo.toml", "not = = toml");
        assert!(matches!(try_load_from_cargo_toml(&p), Err(HeatherError::ConfigParse { .. })));
    }

    #[test]
    fn try_load_from_cargo_toml_without_license_is_none() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        assert!(try_load_from_cargo_toml(&p).unwrap().is_none());
    }

    #[test]
    fn try_load_from_cargo_toml_empty_or_false_workspace_license_is_none() {
        let tmp = TempDir::new().unwrap();
        // Plain but empty license string → None.
        let p = write(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\nlicense = \"\"\n");
        assert!(try_load_from_cargo_toml(&p).unwrap().is_none());
    }

    #[test]
    fn try_load_from_cargo_toml_uses_workspace_package_license() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), "Cargo.toml", "[workspace.package]\nlicense = \"MIT\"\n");
        let cfg = try_load_from_cargo_toml(&p).unwrap().unwrap();
        assert!(!cfg.header_text.is_empty());
    }

    #[test]
    fn try_load_from_cargo_toml_resolves_workspace_inherited_license() {
        // package license = { workspace = true } walks up to the workspace root.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"member\"]\n[workspace.package]\nlicense = \"MIT\"\n",
        );
        let member = write(
            tmp.path(),
            "member/Cargo.toml",
            "[package]\nname = \"m\"\nlicense.workspace = true\n",
        );
        let cfg = try_load_from_cargo_toml(&member).unwrap().unwrap();
        assert!(!cfg.header_text.is_empty());
    }

    #[test]
    fn find_workspace_root_errors_when_no_workspace_ancestor() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let err = find_workspace_root(&p).unwrap_err();
        assert!(matches!(err, HeatherError::ConfigInvalid(_)), "{err}");
    }

    #[test]
    fn cargo_toml_has_workspace_distinguishes_tables() {
        let tmp = TempDir::new().unwrap();
        let ws = write(tmp.path(), "ws/Cargo.toml", "[workspace]\nmembers = []\n");
        let pkg = write(tmp.path(), "pkg/Cargo.toml", "[package]\nname = \"x\"\n");
        assert!(cargo_toml_has_workspace(&ws).unwrap());
        assert!(!cargo_toml_has_workspace(&pkg).unwrap());
    }

    #[test]
    fn try_load_from_cargo_toml_on_directory_is_file_read_error() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(try_load_from_cargo_toml(tmp.path()), Err(HeatherError::FileRead { .. })));
    }

    #[test]
    fn workspace_inherited_license_absent_resolves_to_none() {
        // member inherits license, but the workspace root declares no
        // [workspace.package].license -> resolve_workspace_license None arm.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Cargo.toml", "[workspace]\nmembers = [\"member\"]\n");
        let member = write(
            tmp.path(),
            "member/Cargo.toml",
            "[package]\nname = \"m\"\nlicense.workspace = true\n",
        );
        assert!(try_load_from_cargo_toml(&member).unwrap().is_none());
    }

    #[test]
    fn cargo_toml_has_workspace_on_directory_is_file_read_error() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(cargo_toml_has_workspace(tmp.path()), Err(HeatherError::FileRead { .. })));
    }

    #[test]
    fn cargo_toml_has_workspace_on_malformed_is_parse_error() {
        let tmp = TempDir::new().unwrap();
        let p = write(tmp.path(), "Cargo.toml", "x = = bad");
        assert!(matches!(cargo_toml_has_workspace(&p), Err(HeatherError::ConfigParse { .. })));
    }
}
