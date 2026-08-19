// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the `deps` command.
//!
//! Two fixture workspaces drive the dependency walk:
//! - `tests/fixtures/tiny-crate`: single-package workspace with `itoa` + `miniz_oxide` (→ `adler2`)
//!   and an optional `once_cell` behind the default `extra` feature
//! - `tests/fixtures/tiny-virtual-workspace`: virtual workspace with one member depending on `itoa`
//!
//! Every dependency in those fixtures is a path dependency, so `cargo metadata` resolves them
//! without a registry, and the facts for them come from the synthetic crates.io database dump
//! served by `support::MockWorld`. Nothing here touches the network.

#![cfg(not(miri))]

mod support;

use serde_json::Value;
use support::{MockWorld, run_cli};

/// Path to the fixture crate's Cargo.toml, relative to `cargo-aprz-lib/`.
const FIXTURE_MANIFEST: &str = "tests/fixtures/tiny-crate/Cargo.toml";

/// Path to the virtual workspace fixture's Cargo.toml, relative to `cargo-aprz-lib/`.
const VIRTUAL_WS_MANIFEST: &str = "tests/fixtures/tiny-virtual-workspace/Cargo.toml";

/// Reads a JSON report and returns the names of the appraised crates, sorted.
fn report_names(json_path: &std::path::Path) -> Vec<String> {
    let json_content = std::fs::read_to_string(json_path).expect("read JSON");
    let parsed: Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array").clone();
    let mut names: Vec<String> = crates.iter().filter_map(|c| c["name"].as_str().map(ToOwned::to_owned)).collect();
    names.sort();
    names
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_all_report_types() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");
    let csv_path = temp_dir.path().join("report.csv");
    let html_path = temp_dir.path().join("report.html");
    let excel_path = temp_dir.path().join("report.xlsx");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--json",
            json_path.to_str().expect("valid path"),
            "--csv",
            csv_path.to_str().expect("valid path"),
            "--html",
            html_path.to_str().expect("valid path"),
            "--excel",
            excel_path.to_str().expect("valid path"),
            "--console",
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps command failed: {}", host.error_str());

    // JSON report
    assert!(json_path.exists(), "JSON report should be created");
    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array");
    assert_eq!(
        crates.len(),
        4,
        "tiny-crate should have 4 dependencies (itoa, miniz_oxide, adler2, once_cell)"
    );
    let names = report_names(&json_path);
    assert_eq!(names, ["adler2", "itoa", "miniz_oxide", "once_cell"]);
    let entry = crates.iter().find(|c| c["name"].as_str() == Some("itoa")).expect("itoa entry");
    assert_eq!(entry["version"].as_str(), Some("1.0.17"));
    let metrics = entry["metrics"].as_object().expect("metrics object");
    assert!(!metrics.is_empty(), "should have metrics");

    // CSV report
    assert!(csv_path.exists(), "CSV report should be created");
    let csv_content = std::fs::read_to_string(&csv_path).expect("read CSV");
    let csv_lines: Vec<&str> = csv_content.lines().collect();
    assert!(csv_lines.len() >= 2, "CSV should have header + data rows");
    assert!(csv_lines[0].starts_with("Metric"), "CSV header should start with 'Metric'");
    assert!(csv_content.contains("itoa"), "CSV should contain itoa");

    // HTML report
    assert!(html_path.exists(), "HTML report should be created");
    let html_content = std::fs::read_to_string(&html_path).expect("read HTML");
    assert!(html_content.contains("<html"), "HTML report should contain html tag");
    assert!(html_content.contains("itoa"), "HTML report should mention itoa");

    // Excel report
    assert!(excel_path.exists(), "Excel report should be created");
    let excel_size = std::fs::metadata(&excel_path).expect("excel metadata").len();
    assert!(excel_size > 0, "Excel report should not be empty");

    // Console output
    let console_output = host.output_str();
    assert!(console_output.contains("itoa"), "console output should mention itoa");
    assert!(console_output.contains("adler2"), "console output should mention adler2");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_console_output() {
    let world = MockWorld::new().await;
    let host = run_cli(&world, &["deps", "--manifest-path", FIXTURE_MANIFEST, "--console"]).await;

    assert!(host.error_str().is_empty(), "deps command failed: {}", host.error_str());

    let output = host.output_str();
    assert!(output.contains("itoa"), "console output should mention itoa");
    assert!(output.contains("adler2"), "console output should mention adler2 (transitive dep)");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_csv_output() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let csv_path = temp_dir.path().join("report.csv");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--csv",
            csv_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps command failed: {}", host.error_str());
    assert!(csv_path.exists(), "CSV report should be created");

    let csv_content = std::fs::read_to_string(&csv_path).expect("read CSV");
    let lines: Vec<&str> = csv_content.lines().collect();
    assert!(lines.len() >= 2, "CSV should have header + data, got {} lines", lines.len());
    assert!(lines[0].starts_with("Metric"), "header row should start with 'Metric'");
    assert!(csv_content.contains("itoa"), "CSV should contain itoa");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_standard_deps_only() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--dependency-types",
            "standard",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps command failed: {}", host.error_str());

    // itoa, miniz_oxide, adler2, and once_cell are all standard dependencies
    assert_eq!(report_names(&json_path), ["adler2", "itoa", "miniz_oxide", "once_cell"]);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_dev_deps_only() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--dependency-types",
            "dev",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps command failed: {}", host.error_str());

    // tiny-crate has no dev dependencies
    let names = report_names(&json_path);
    assert!(names.is_empty(), "tiny-crate should have no dev dependencies, got {names:?}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_nonexistent_package() {
    let world = MockWorld::new().await;
    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--package",
            "no-such-package",
            "--console",
        ],
    )
    .await;

    let err_msg = host.error_str();
    assert!(!err_msg.is_empty(), "should fail for nonexistent package");
    assert!(
        err_msg.contains("no-such-package"),
        "error should mention the package name, got: {err_msg}"
    );
}

/// `--package <name>` filters to a specific package in the workspace.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_with_package_flag() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--package",
            "tiny-crate",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(
        host.error_str().is_empty(),
        "deps --package tiny-crate should succeed: {}",
        host.error_str()
    );

    assert_eq!(
        report_names(&json_path),
        ["adler2", "itoa", "miniz_oxide", "once_cell"],
        "should still resolve all 4 deps for tiny-crate"
    );
}

/// `--package <name>` on a leaf package resolves only that package's own dependencies.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_package_flag_on_leaf_member() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--package",
            "miniz_oxide",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps --package miniz_oxide: {}", host.error_str());
    assert_eq!(report_names(&json_path), ["adler2"], "miniz_oxide only depends on adler2");
}

/// `--workspace` processes all workspace members.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_with_workspace_flag() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--workspace",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps --workspace should succeed: {}", host.error_str());

    assert_eq!(
        report_names(&json_path),
        ["adler2", "itoa", "miniz_oxide", "once_cell"],
        "workspace should resolve all 4 deps"
    );
}

/// A virtual workspace (no root package, no `--package`, no `--workspace`) defaults to
/// processing all workspace members.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_virtual_workspace() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            VIRTUAL_WS_MANIFEST,
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(
        host.error_str().is_empty(),
        "deps on virtual workspace should succeed: {}",
        host.error_str()
    );

    // The virtual workspace member depends only on itoa
    assert_eq!(report_names(&json_path), ["itoa"]);
}

/// `--all-features` activates all features.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_all_features() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--all-features",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(
        host.error_str().is_empty(),
        "deps --all-features should succeed: {}",
        host.error_str()
    );

    // --all-features should include once_cell (behind the "extra" feature)
    assert_eq!(report_names(&json_path), ["adler2", "itoa", "miniz_oxide", "once_cell"]);
}

/// `--no-default-features` disables the default feature set.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_no_default_features() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--no-default-features",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(
        host.error_str().is_empty(),
        "deps --no-default-features should succeed: {}",
        host.error_str()
    );

    // --no-default-features should exclude once_cell (only in the "extra" default feature)
    assert_eq!(report_names(&json_path), ["adler2", "itoa", "miniz_oxide"]);
}

/// `--features <list>` activates specific features.
#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_deps_command_explicit_features() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "deps",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--no-default-features",
            "-F",
            "extra",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "deps -F extra should succeed: {}", host.error_str());

    // --no-default-features -F extra should re-enable once_cell
    assert_eq!(report_names(&json_path), ["adler2", "itoa", "miniz_oxide", "once_cell"]);
}
