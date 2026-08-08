// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::{Context, Result};

/// Validates a single dependency entry and returns an error message if invalid.
fn validate_dependency(name: &str, value: &toml::Value) -> Result<(), String> {
    if value.is_str() {
        return Err(format!(
            "  - '{name}': uses simple version string, should be a table with default-features = false",
        ));
    }

    let Some(dep_table) = value.as_table() else {
        return Err(format!("  - '{name}': dependency is not a table"));
    };

    match dep_table.get("default-features") {
        Some(toml::Value::Boolean(false)) => Ok(()),

        Some(toml::Value::Boolean(true)) => Err(format!("  - '{name}': has default-features = true (must be false)")),

        None => Err(format!("  - '{name}': missing default-features = false")),

        Some(_) => Err(format!(
            "  - '{name}': default-features has unexpected value (must be boolean false)",
        )),
    }
}

/// Validates a dependencies table and collects errors and dependency names.
/// Dependencies with `workspace = true` are skipped since they inherit settings from the workspace.
fn validate_deps_table(
    deps_table: &toml::map::Map<String, toml::Value>,
    exceptions: &[String],
    errors: &mut Vec<String>,
    found_deps: &mut Vec<String>,
) {
    for (name, value) in deps_table {
        found_deps.push(name.clone());
        if exceptions.contains(name) {
            continue;
        }

        // Skip dependencies that use workspace = true
        if let Some(table) = value.as_table()
            && table.get("workspace") == Some(&toml::Value::Boolean(true))
        {
            continue;
        }

        if let Err(err) = validate_dependency(name, value) {
            errors.push(err);
        }
    }
}

/// Validates all dependencies in the given Cargo.toml content.
///
/// Checks `[workspace.dependencies]` if a `[workspace]` section exists,
/// and `[dependencies]` if a `[package]` section exists. If both are present,
/// both are checked.
///
/// # Returns
///
/// A tuple containing:
/// * A vector of error messages for invalid dependencies
/// * A vector of all dependency names found
/// * A vector of section labels that were checked (e.g. `"[workspace.dependencies]"`, `"[dependencies]"`)
pub fn validate_dependencies(content: &str, exceptions: &[String]) -> Result<(Vec<String>, Vec<String>, Vec<&'static str>)> {
    let parsed: toml::Value = toml::from_str(content).context("Failed to parse Cargo.toml")?;

    let has_workspace = parsed.get("workspace").is_some();
    let has_package = parsed.get("package").is_some();

    let mut errors = Vec::new();
    let mut found_deps = Vec::new();
    let mut checked_sections = Vec::new();

    // Check [workspace.dependencies] if [workspace] exists
    if has_workspace && let Some(deps) = parsed.get("workspace").and_then(|w| w.get("dependencies")) {
        let deps_table = deps.as_table().context("[workspace.dependencies] is not a table")?;
        checked_sections.push("[workspace.dependencies]");
        validate_deps_table(deps_table, exceptions, &mut errors, &mut found_deps);
    }

    // Check [dependencies] if [package] exists (plain crate)
    if has_package && let Some(deps) = parsed.get("dependencies") {
        let deps_table = deps.as_table().context("[dependencies] is not a table")?;
        checked_sections.push("[dependencies]");
        validate_deps_table(deps_table, exceptions, &mut errors, &mut found_deps);
    }

    if checked_sections.is_empty() {
        anyhow::bail!("No [workspace.dependencies] or [dependencies] section found");
    }

    Ok((errors, found_deps, checked_sections))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_dependency_with_default_features_false() {
        let toml_str = r#"
version = "1.0"
default-features = false
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid when default-features = false");
    }

    #[test]
    fn test_validate_dependency_with_default_features_false_and_features() {
        let toml_str = r#"
version = "1.0"
default-features = false
features = ["feature1", "feature2"]
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid with default-features = false and features");
    }

    #[test]
    fn test_validate_dependency_simple_version_string() {
        let value = toml::Value::String("1.0".to_string());

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("test-crate"));
        assert!(error.contains("uses simple version string"));
    }

    #[test]
    fn test_validate_dependency_not_a_table() {
        // Test with an array value (not a string or table)
        let value = toml::Value::Array(vec![toml::Value::String("1.0".to_string())]);

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("test-crate"));
        assert!(error.contains("dependency is not a table"));
    }

    #[test]
    fn test_validate_dependency_missing_default_features() {
        let toml_str = r#"
version = "1.0"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("test-crate"));
        assert!(error.contains("missing default-features = false"));
    }

    #[test]
    fn test_validate_dependency_default_features_true() {
        let toml_str = r#"
version = "1.0"
default-features = true
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("test-crate"));
        assert!(error.contains("has default-features = true"));
    }

    #[test]
    fn test_validate_dependency_with_git_source() {
        let toml_str = r#"
git = "https://github.com/example/repo"
default-features = false
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid with git source and default-features = false");
    }

    #[test]
    fn test_validate_dependency_with_path_source() {
        let toml_str = r#"
path = "../local-crate"
default-features = false
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid with path source and default-features = false");
    }

    #[test]
    fn test_validate_dependency_with_optional_flag() {
        let toml_str = r#"
version = "1.0"
default-features = false
optional = true
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid with optional flag and default-features = false");
    }

    #[test]
    fn test_validate_dependency_default_features_string() {
        let toml_str = r#"
version = "1.0"
default-features = "false"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("test-crate"));
        assert!(error.contains("unexpected value"));
    }

    #[test]
    fn test_validate_dependency_complex_configuration() {
        let toml_str = r#"
version = "1.0"
default-features = false
features = ["feat1", "feat2"]
optional = true
package = "other-name"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_ok(), "Should be valid with complex configuration");
    }

    #[test]
    fn test_validate_dependency_git_without_default_features() {
        let toml_str = r#"
git = "https://github.com/example/repo"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("missing default-features = false"));
    }

    #[test]
    fn test_validate_dependency_path_without_default_features() {
        let toml_str = r#"
path = "../local-crate"
"#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();

        let result = validate_dependency("test-crate", &value);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.contains("missing default-features = false"));
    }

    #[test]
    fn test_validate_workspace_dependencies_all_valid() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = { version = "1.0", default-features = false }
tokio = { version = "1.0", default-features = false, features = ["rt"] }
"#;

        let (errors, _, sections) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "Should have no errors with all valid dependencies");
        assert_eq!(sections, vec!["[workspace.dependencies]"]);
    }

    #[test]
    fn test_validate_workspace_dependencies_with_errors() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.0" }
"#;

        let (errors, _, _) = validate_dependencies(content, &[]).unwrap();
        assert_eq!(errors.len(), 2, "Should have 2 errors");
    }

    #[test]
    fn test_validate_no_workspace_no_package() {
        let content = r#"
[some-other-section]
key = "value"
"#;

        let result = validate_dependencies(content, &[]);
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("No [workspace.dependencies] or [dependencies] section found"));
    }

    #[test]
    fn test_validate_workspace_no_dependencies() {
        let content = r#"
[workspace]
members = ["crate1"]
"#;

        let result = validate_dependencies(content, &[]);
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("No [workspace.dependencies] or [dependencies] section found"));
    }

    #[test]
    fn test_validate_workspace_dependencies_empty_dependencies() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
"#;

        let (errors, _, _) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "Should have no errors with empty dependencies");
    }

    #[test]
    fn test_validate_workspace_dependencies_with_exceptions() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = { version = "1.0", default-features = false }
tokio = { version = "1.0", default-features = false, features = ["rt"] }
"#;

        let exceptions = vec!["tokio".to_string()];
        let (errors, found_deps, _) = validate_dependencies(content, &exceptions).unwrap();
        assert!(errors.is_empty(), "Should have no errors with valid dependencies");
        assert_eq!(found_deps.len(), 2, "Should find 2 dependencies");
        assert!(found_deps.contains(&"serde".to_string()));
        assert!(found_deps.contains(&"tokio".to_string()));
    }

    #[test]
    fn test_validate_workspace_dependencies_with_exceptions_and_errors() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = "1.0"
tokio = { version = "1.0" }
"#;

        let exceptions = vec!["tokio".to_string()];
        let (errors, found_deps, _) = validate_dependencies(content, &exceptions).unwrap();
        assert_eq!(errors.len(), 1, "Should have 1 error");
        assert_eq!(found_deps.len(), 2, "Should find 2 dependencies");
        assert!(found_deps.contains(&"serde".to_string()));
        assert!(found_deps.contains(&"tokio".to_string()));
    }

    // Tests for plain package (non-workspace) support

    #[test]
    fn test_validate_package_dependencies_all_valid() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = { version = "1.0", default-features = false }
tokio = { version = "1.0", default-features = false, features = ["rt"] }
"#;

        let (errors, _, sections) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "Should have no errors with all valid dependencies");
        assert_eq!(sections, vec!["[dependencies]"]);
    }

    #[test]
    fn test_validate_package_dependencies_with_errors() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1.0" }
"#;

        let (errors, _, sections) = validate_dependencies(content, &[]).unwrap();
        assert_eq!(errors.len(), 2, "Should have 2 errors");
        assert_eq!(sections, vec!["[dependencies]"]);
    }

    #[test]
    fn test_validate_package_no_dependencies() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"
"#;

        let result = validate_dependencies(content, &[]);
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("No [workspace.dependencies] or [dependencies] section found"));
    }

    #[test]
    fn test_validate_package_dependencies_with_exceptions() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1.0", default-features = false }
"#;

        let exceptions = vec!["serde".to_string()];
        let (errors, found_deps, _) = validate_dependencies(content, &exceptions).unwrap();
        assert!(errors.is_empty(), "Should have no errors with exception");
        assert_eq!(found_deps.len(), 2);
    }

    #[test]
    fn test_validate_both_workspace_and_package_dependencies() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = { version = "1.0", default-features = false }

[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
tokio = { version = "1.0", default-features = false }
"#;

        let (errors, found_deps, sections) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "Should have no errors");
        assert_eq!(found_deps.len(), 2);
        assert!(found_deps.contains(&"serde".to_string()));
        assert!(found_deps.contains(&"tokio".to_string()));
        assert_eq!(sections, vec!["[workspace.dependencies]", "[dependencies]"]);
    }

    #[test]
    fn test_validate_both_workspace_and_package_dependencies_with_errors() {
        let content = r#"
[workspace]
members = ["crate1"]

[workspace.dependencies]
serde = "1.0"

[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
tokio = { version = "1.0" }
"#;

        let (errors, _, _) = validate_dependencies(content, &[]).unwrap();
        assert_eq!(errors.len(), 2, "Should have errors from both sections");
    }

    #[test]
    fn test_validate_package_deps_skip_workspace_true() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
tokio = { version = "1.0", default-features = false }
"#;

        let (errors, found_deps, _) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "workspace = true deps should be skipped");
        assert_eq!(found_deps.len(), 2);
    }

    #[test]
    fn test_validate_package_deps_workspace_true_mixed() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
tokio = { version = "1.0" }
"#;

        let (errors, found_deps, _) = validate_dependencies(content, &[]).unwrap();
        assert_eq!(errors.len(), 1, "Only non-workspace dep should be checked");
        assert_eq!(found_deps.len(), 2);
        assert!(errors[0].contains("tokio"));
    }

    #[test]
    fn test_validate_package_deps_workspace_true_with_features() {
        let content = r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = { workspace = true, features = ["derive"] }
"#;

        let (errors, _, _) = validate_dependencies(content, &[]).unwrap();
        assert!(errors.is_empty(), "workspace = true with features should be skipped");
    }
}
