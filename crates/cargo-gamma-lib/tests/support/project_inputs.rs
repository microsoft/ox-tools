// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure validation and configuration transforms used by the materializer.

use std::path::{Component, Path};

pub fn safe_relative_path(path: &Path) -> bool {
    !path
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir | Component::ParentDir))
}

pub fn offline_configuration(text: &str) -> Result<toml_edit::DocumentMut, toml_edit::TomlError> {
    let mut document: toml_edit::DocumentMut = text.parse()?;
    document["net"]["offline"] = toml_edit::value(true);
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_relative_assets_are_valid() {
        assert!(safe_relative_path(Path::new("src/lib.rs")));
        assert!(!safe_relative_path(Path::new("../outside.rs")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefixes_and_roots_are_rejected_in_process() {
        assert!(!safe_relative_path(Path::new("C:outside.rs")));
        assert!(!safe_relative_path(Path::new("\\outside.rs")));
    }

    #[test]
    fn offline_configuration_preserves_build_flags() {
        let document = offline_configuration("[net]\noffline = false\n[build]\nrustflags = [\"--cfg\", \"fixture\"]\n").unwrap();
        assert_eq!(document["net"]["offline"].as_bool(), Some(true));
        assert_eq!(document["build"]["rustflags"][1].as_str(), Some("fixture"));
    }
}
