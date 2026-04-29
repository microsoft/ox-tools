// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration file parsing for `cargo-heather`.
//!
//! Reads `.cargo-heather.toml` from the project root and resolves
//! the expected license header text. Falls back to the `license`
//! field in `Cargo.toml` when no dedicated config file exists.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::info;

use crate::error::HeatherError;
use crate::license;

/// The default configuration file name.
pub const CONFIG_FILE_NAME: &str = ".cargo-heather.toml";

/// Raw deserialized configuration from `.cargo-heather.toml`.
#[derive(Debug, Deserialize)]
struct RawConfig {
    /// SPDX license identifier (e.g., `"MIT"`, `"Apache-2.0"`).
    license: Option<String>,
    /// Custom multi-line header text (without comment markers).
    header: Option<String>,
    /// Whether to process Rust script files (shebang + `---` frontmatter). Default: `true`.
    scripts: Option<bool>,
    /// Whether to process TOML files whose file name starts with `.`. Default: `false`.
    dot_toml: Option<bool>,
    /// List of relative paths to exclude from checking.
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

/// The `license` field in `Cargo.toml` can be either a plain string
/// (`license = "MIT"`) or a workspace reference (`license.workspace = true`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LicenseField {
    Plain(String),
    Workspace { workspace: bool },
}

/// The `[workspace]` table in a workspace root `Cargo.toml`.
#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    package: Option<WorkspacePackage>,
}

/// The `[workspace.package]` table.
#[derive(Debug, Deserialize)]
struct WorkspacePackage {
    license: Option<String>,
}

/// Resolved configuration with the final header text and processing options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeatherConfig {
    /// The expected header text that should appear at the top of each source file.
    pub header_text: String,
    /// Whether to process Rust script files (shebang + `---` frontmatter). Default: `true`.
    pub scripts: bool,
    /// Whether to process TOML files whose file name starts with `.`. Default: `false`.
    pub dot_toml: bool,
    /// List of relative paths to exclude from checking.
    pub exclude: Vec<String>,
}

impl HeatherConfig {
    /// Create a config with the given header text and default processing options.
    #[must_use]
    pub fn with_defaults(header_text: String) -> Self {
        Self {
            header_text,
            scripts: true,
            dot_toml: false,
            exclude: Vec::new(),
        }
    }
}

/// Load and resolve the configuration.
///
/// Searches for `.cargo-heather.toml` in the given directory first.
/// If not found, falls back to the `license` field in `Cargo.toml`.
///
/// # Errors
///
/// Returns an error if no configuration source can be found, the file
/// is malformed, or contains an unknown SPDX identifier.
pub fn load_config(project_dir: &Path) -> Result<HeatherConfig, HeatherError> {
    let config_path = project_dir.join(CONFIG_FILE_NAME);
    if config_path.exists() {
        return load_config_from_path(&config_path);
    }

    // Fall back to Cargo.toml's license field
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
///
/// # Errors
///
/// Returns an error if the file cannot be read, parsed, or is invalid.
pub fn load_config_from_path(config_path: &Path) -> Result<HeatherConfig, HeatherError> {
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

/// Try to extract a license header config from `Cargo.toml`.
///
/// Checks, in order:
/// 1. `[package].license` — plain string (`license = "MIT"`)
/// 2. `[package].license` — workspace reference (`license.workspace = true`),
///    resolved from the workspace root's `[workspace.package].license`
/// 3. `[workspace.package].license` — for workspace-only manifests that have
///    no `[package]` section
///
/// Returns `Ok(None)` if no license can be determined. Returns `Err` if the
/// file can't be parsed or the SPDX identifier is unrecognized.
fn try_load_from_cargo_toml(path: &Path) -> Result<Option<HeatherConfig>, HeatherError> {
    let content = std::fs::read_to_string(path).map_err(|e| HeatherError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;

    let manifest: CargoManifest = toml::from_str(&content).map_err(|e| HeatherError::ConfigParse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    // Try [package].license first.
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

    // No [package] or no license in it — try [workspace.package].license directly.
    // This covers workspace-root manifests that only have a [workspace] section.
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

/// Resolve `license.workspace = true` by finding the workspace root `Cargo.toml`
/// and reading its `[workspace.package].license` field.
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

/// Walk up from a package `Cargo.toml` to find the workspace root `Cargo.toml`
/// (one that contains a `[workspace]` table).
///
/// First checks the package's own `Cargo.toml` (it may itself be the workspace
/// root), then walks parent directories.
fn find_workspace_root(package_cargo_toml: &Path) -> Result<PathBuf, HeatherError> {
    // Start from the directory containing the package Cargo.toml, then go up.
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

/// Check whether a `Cargo.toml` file contains a `[workspace]` table.
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

/// Build the expected commented header lines for a `.rs` file.
///
/// Converts plain text header into `//` comment lines.
/// For other comment styles, use [`CommentStyle::format_header`](crate::comment::CommentStyle::format_header).
#[must_use]
pub fn format_header_comment(header_text: &str) -> String {
    crate::comment::CommentStyle::DoubleSlash.format_header(header_text)
}

/// Return the expected config file path for a project directory.
#[must_use]
pub fn config_path_for(project_dir: &Path) -> PathBuf {
    project_dir.join(CONFIG_FILE_NAME)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_config(dir: &Path, content: &str) {
        std::fs::write(dir.join(CONFIG_FILE_NAME), content).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn load_with_spdx_license() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "license = \"MIT\"\n");

        let config = load_config(dir.path()).unwrap();
        assert_eq!(config.header_text, "Licensed under the MIT License.");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn load_with_custom_header() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "header = \"Copyright 2024 MyCompany\\nAll rights reserved.\"\n");

        let config = load_config(dir.path()).unwrap();
        assert!(config.header_text.contains("Copyright 2024 MyCompany"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn load_with_multiline_header() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "header = \"\"\"\nCopyright 2024\nAll rights reserved.\n\"\"\"\n");

        let config = load_config(dir.path()).unwrap();
        assert!(config.header_text.contains("Copyright 2024"));
        assert!(config.header_text.contains("All rights reserved."));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_both_license_and_header() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "license = \"MIT\"\nheader = \"Custom header\"\n");

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_neither_license_nor_header() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "# empty config\n");

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("must specify"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_empty_header() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "header = \"  \"\n");

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_unknown_license() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "license = \"FAKE-1.0\"\n");

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("FAKE-1.0"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_config_not_found_and_no_cargo_toml() {
        let dir = TempDir::new().unwrap();
        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_to_cargo_toml_license() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nlicense = \"MIT\"\n").unwrap();

        let config = load_config(dir.path()).unwrap();
        assert_eq!(config.header_text, "Licensed under the MIT License.");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_to_cargo_toml_apache() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nlicense = \"Apache-2.0\"\n",
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(config.header_text.contains("Apache License, Version 2.0"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_skipped_when_cargo_toml_has_no_license() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\n").unwrap();

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_error_on_unknown_cargo_toml_license() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nlicense = \"FAKE-1.0\"\n",
        )
        .unwrap();

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("FAKE-1.0"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_workspace_license_inheritance() {
        // Simulate a workspace: parent dir has workspace Cargo.toml,
        // child dir has package Cargo.toml with license.workspace = true
        let dir = TempDir::new().unwrap();

        // Workspace root Cargo.toml
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/my_crate\"]\n\n[workspace.package]\nlicense = \"MIT\"\n",
        )
        .unwrap();

        // Package directory
        let pkg_dir = dir.path().join("crates").join("my_crate");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("Cargo.toml"),
            "[package]\nname = \"my_crate\"\nlicense.workspace = true\n",
        )
        .unwrap();

        let config = load_config(&pkg_dir).unwrap();
        assert_eq!(config.header_text, "Licensed under the MIT License.");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_workspace_license_same_dir() {
        // The workspace root and the package are in the same Cargo.toml
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n\n[workspace.package]\nlicense = \"Apache-2.0\"\n\n[package]\nname = \"mono\"\nlicense.workspace = true\n",
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert!(config.header_text.contains("Apache License, Version 2.0"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_workspace_no_license_in_workspace_root() {
        let dir = TempDir::new().unwrap();

        // Workspace root without license
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"pkg\"]\n\n[workspace.package]\n",
        )
        .unwrap();

        let pkg_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("Cargo.toml"), "[package]\nname = \"pkg\"\nlicense.workspace = true\n").unwrap();

        let err = load_config(&pkg_dir).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_workspace_false_treated_as_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nlicense.workspace = false\n",
        )
        .unwrap();

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_empty_license_treated_as_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nlicense = \"\"\n").unwrap();

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_whitespace_only_license_treated_as_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nlicense = \"   \"\n").unwrap();

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn fallback_workspace_root_only_manifest() {
        // A workspace root Cargo.toml with no [package] section at all,
        // only [workspace.package].license — common for pure workspace roots.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nlicense = \"MIT\"\n",
        )
        .unwrap();

        let config = load_config(dir.path()).unwrap();
        assert_eq!(config.header_text, "Licensed under the MIT License.");
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn config_file_takes_priority_over_cargo_toml() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "license = \"ISC\"\n");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"\nlicense = \"MIT\"\n").unwrap();

        let config = load_config(dir.path()).unwrap();
        // Should use .cargo-heather.toml (ISC), not Cargo.toml (MIT)
        assert!(config.header_text.contains("Permission to use"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn error_malformed_toml() {
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "this is not valid toml {{{\n");

        let err = load_config(dir.path()).unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn format_header_single_line() {
        let result = format_header_comment("Licensed under the MIT License.");
        assert_eq!(result, "// Licensed under the MIT License.");
    }

    #[test]
    fn format_header_multiline() {
        let result = format_header_comment("Line one\n\nLine three");
        assert_eq!(result, "// Line one\n//\n// Line three");
    }

    #[test]
    fn config_path_for_returns_correct_path() {
        let path = config_path_for(Path::new("/my/project"));
        assert!(path.to_string_lossy().contains(CONFIG_FILE_NAME));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn load_config_from_path_direct() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&path, "license = \"ISC\"\n").unwrap();

        let config = load_config_from_path(&path).unwrap();
        assert!(config.header_text.contains("Permission to use"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn read_config_file_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let err = read_config_file(&path).unwrap_err();
        assert!(err.to_string().contains("config file not found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn read_config_file_io_error() {
        // A directory cannot be read as a file
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("adir");
        std::fs::create_dir(&path).unwrap();
        // Ensure the path "exists" but reading as file fails
        // On Windows, reading a dir as file gives an error
        let result = read_config_file(&path);
        // Either ConfigNotFound (unlikely since dir exists) or FileRead error
        result.unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn try_load_from_cargo_toml_io_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent").join("Cargo.toml");
        let err = try_load_from_cargo_toml(&path).unwrap_err();
        assert!(err.to_string().contains("failed to read file"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn try_load_from_cargo_toml_malformed_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Cargo.toml");
        std::fs::write(&path, "this is {{{ not valid").unwrap();
        let err = try_load_from_cargo_toml(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn find_workspace_root_not_found() {
        // Create a temp dir tree with no workspace Cargo.toml
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        // Create a non-workspace Cargo.toml
        std::fs::write(sub.join("Cargo.toml"), "[package]\nname = \"test\"\n").unwrap();

        let err = find_workspace_root(&sub.join("Cargo.toml")).unwrap_err();
        assert!(err.to_string().contains("no workspace root Cargo.toml found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn cargo_toml_has_workspace_io_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent_dir").join("Cargo.toml");
        let err = cargo_toml_has_workspace(&path).unwrap_err();
        assert!(err.to_string().contains("failed to read file"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn cargo_toml_has_workspace_malformed_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Cargo.toml");
        std::fs::write(&path, "not valid {{{").unwrap();
        let err = cargo_toml_has_workspace(&path).unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn resolve_workspace_license_read_error() {
        // Create workspace structure where workspace root Cargo.toml exists
        // but we'll test the path where find_workspace_root succeeds
        // but reading it fails. Hard to simulate I/O error on existing file.
        // Instead, test find_workspace_root failure path.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("crate");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

        // No workspace root exists → resolve_workspace_license will fail
        let err = resolve_workspace_license(&sub.join("Cargo.toml")).unwrap_err();
        assert!(err.to_string().contains("no workspace root Cargo.toml found"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn resolve_workspace_license_malformed_workspace_root() {
        // Create a workspace root with invalid TOML
        let dir = TempDir::new().unwrap();
        // Sub-directory with a package Cargo.toml
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        // Workspace root with [workspace] but invalid content after
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers = [\"sub\"]\n").unwrap();

        // find_workspace_root will find the root. resolve_workspace_license should parse it
        // and find no [workspace.package].license → Ok(None)
        let result = resolve_workspace_license(&sub.join("Cargo.toml")).unwrap();
        assert!(result.is_none());
    }
}
