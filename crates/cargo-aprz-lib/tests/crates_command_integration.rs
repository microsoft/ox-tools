// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the `crates` command.
//!
//! These exercise the full end-to-end `crates` workflow — collect facts, flatten to metrics,
//! generate reports — against the mocked world set up by `support::MockWorld`: a synthetic
//! crates.io database dump served over HTTP, a seeded advisory database, and mock endpoints for
//! every other service. Nothing here touches the network.

#![cfg(not(miri))]

mod support;

use support::{MockWorld, run_cli};

/// Config with no gates at all, so nothing can flag a crate.
const EMPTY_CONFIG: &str = r"
medium_risk_threshold = 30.0
low_risk_threshold = 70.0
";

/// Config with an always-failing `high_risk` gate so every crate is flagged high risk.
const ALWAYS_FAIL_CONFIG: &str = r#"
medium_risk_threshold = 30.0
low_risk_threshold = 70.0

[[high_risk]]
name = "Always Fail"
description = "Always flags crate as high risk"
expression = "false"
"#;

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_all_report_types() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");
    let csv_path = temp_dir.path().join("report.csv");
    let html_path = temp_dir.path().join("report.html");
    let excel_path = temp_dir.path().join("report.xlsx");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
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

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    // JSON report
    assert!(json_path.exists(), "JSON report file should be created");
    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array");
    assert_eq!(crates.len(), 1);
    let entry = &crates[0];
    assert_eq!(entry["name"].as_str(), Some("serde"));
    assert_eq!(entry["version"].as_str(), Some("1.0.200"));
    let metrics = entry["metrics"].as_object().expect("metrics object");
    assert!(!metrics.is_empty(), "should have metrics");

    // CSV report
    assert!(csv_path.exists(), "CSV report file should be created");
    let csv_content = std::fs::read_to_string(&csv_path).expect("read CSV");
    let csv_lines: Vec<&str> = csv_content.lines().collect();
    assert!(csv_lines.len() >= 2, "CSV should have header + data rows");
    assert!(csv_lines[0].starts_with("Metric"), "CSV header should start with 'Metric'");
    assert!(csv_content.contains("serde"), "CSV should contain crate name");

    // HTML report
    assert!(html_path.exists(), "HTML report file should be created");
    let html_content = std::fs::read_to_string(&html_path).expect("read HTML");
    assert!(html_content.contains("<html"), "HTML report should contain html tag");
    assert!(html_content.contains("serde"), "HTML report should mention crate name");

    // Excel report
    assert!(excel_path.exists(), "Excel report file should be created");
    let excel_size = std::fs::metadata(&excel_path).expect("excel metadata").len();
    assert!(excel_size > 0, "Excel report should not be empty");

    // Console output
    let console_output = host.output_str();
    assert!(console_output.contains("serde"), "console output should mention the crate name");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_csv_output() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let csv_path = temp_dir.path().join("report.csv");

    let host = run_cli(&world, &["crates", "itoa@1.0.17", "--csv", csv_path.to_str().expect("valid path")]).await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());
    assert!(csv_path.exists(), "CSV report file should be created");

    let csv_content = std::fs::read_to_string(&csv_path).expect("read CSV");
    // CSV format: header row starts with "Metric", data rows follow
    let lines: Vec<&str> = csv_content.lines().collect();
    assert!(lines.len() >= 2, "CSV should have header + data, got {} lines", lines.len());
    assert!(
        lines[0].starts_with("Metric"),
        "first row should be the header starting with 'Metric'"
    );
    assert!(csv_content.contains("itoa"), "CSV should contain crate name");
    assert!(
        host.output_str().is_empty(),
        "requesting a report suppresses the default console output, got: {}",
        host.output_str()
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_console_output() {
    let world = MockWorld::new().await;
    let host = run_cli(&world, &["crates", "serde@1.0.200", "--console"]).await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    let output = host.output_str();
    assert!(output.contains("serde"), "console output should mention the crate name");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_multiple_crates() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "itoa@1.0.17",
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");

    let crates = parsed["crates"].as_array().expect("crates array");
    assert_eq!(crates.len(), 2, "should have 2 crate entries");

    let names: Vec<&str> = crates.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"serde"), "should contain serde");
    assert!(names.contains(&"itoa"), "should contain itoa");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_resolves_latest_version() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");

    let host = run_cli(&world, &["crates", "serde", "--json", json_path.to_str().expect("valid path")]).await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array");
    assert_eq!(crates.len(), 1);
    assert_eq!(
        crates[0]["version"].as_str(),
        Some("1.0.200"),
        "the pre-release and the yanked release must not be picked as latest"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_nonexistent_crate() {
    let world = MockWorld::new().await;
    let host = run_cli(
        &world,
        &["crates", "this-crate-definitely-does-not-exist-xyz-98765@0.0.1", "--console"],
    )
    .await;

    // Should succeed (non-existent crates are reported, not fatal)
    assert!(
        host.exit_code.is_none(),
        "crates command should not fail for unknown crates: {}",
        host.error_str()
    );
}

// ---------------------------------------------------------------------------
// CrateNotFound with non-empty suggestions ("Did you mean ...?").
// Uses a misspelled name close to a crate in the dump so suggestions are returned.
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_misspelled_crate_shows_suggestions() {
    let world = MockWorld::new().await;
    let host = run_cli(&world, &["crates", "serdee@1.0.0", "--console"]).await;

    // The command itself succeeds; the crate is reported as not found.
    let error_output = host.error_str();
    assert!(
        error_output.contains("Did you mean"),
        "error output should contain suggestions, got: {error_output}"
    );
    assert!(
        error_output.contains("serde"),
        "error output should suggest the real crate name, got: {error_output}"
    );
}

// ---------------------------------------------------------------------------
// VersionNotFound — a known crate with a nonexistent version.
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_nonexistent_version() {
    let world = MockWorld::new().await;
    let host = run_cli(&world, &["crates", "serde@99.99.99", "--console"]).await;

    let error_output = host.error_str();
    assert!(
        error_output.contains("Could not find information on version"),
        "error output should mention missing version, got: {error_output}"
    );
    assert!(
        error_output.contains("serde"),
        "error output should mention the crate name, got: {error_output}"
    );
}

// ---------------------------------------------------------------------------
// should_eval branch — --error-if-high-risk triggers expression evaluation and
// produces a ReportableCrate with an evaluation result.
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_with_error_if_high_risk_flag() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let json_path = temp_dir.path().join("report.json");
    let config_path = temp_dir.path().join("aprz.toml");
    std::fs::write(&config_path, EMPTY_CONFIG).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--error-if-high-risk",
            "--config",
            config_path.to_str().expect("valid path"),
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    // --error-if-high-risk with no expressions means evaluation succeeds (nothing to deny)
    assert!(
        host.error_str().is_empty(),
        "crates --error-if-high-risk should succeed: {}",
        host.error_str()
    );

    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");

    let crates = parsed["crates"].as_array().expect("crates array");
    assert_eq!(crates.len(), 1);
    assert_eq!(crates[0]["name"].as_str(), Some("serde"));

    // With --error-if-high-risk, the appraisal field should be present
    let eval = &crates[0]["appraisal"];
    assert!(!eval.is_null(), "appraisal should be present when --error-if-high-risk is used");
}

// ---------------------------------------------------------------------------
// Allow list integration tests — verify that allow_list entries in the config
// prevent error exit codes from --error-if-high-risk / --error-if-medium-risk.
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_error_if_high_risk_triggers_without_allow_list() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("aprz.toml");
    std::fs::write(&config_path, ALWAYS_FAIL_CONFIG).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--error-if-high-risk",
            "--config",
            config_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert_eq!(
        host.exit_code,
        Some(1),
        "should exit with code 1 when high risk crate is not on allow list"
    );
    let error = host.error_str();
    assert!(error.contains("- serde v1.0.200"));
    assert!(error.contains("    - FAILED: Always Fail; expected: Always flags crate as high risk"));
    assert!(error.contains("[[allow_list]]"));
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_error_if_high_risk_bypassed_by_allow_list() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_content = format!(
        r#"{ALWAYS_FAIL_CONFIG}
[[allow_list]]
name = "serde"
version = "=1.0.200"
"#
    );
    let config_path = temp_dir.path().join("aprz.toml");
    std::fs::write(&config_path, &config_content).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--error-if-high-risk",
            "--config",
            config_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(
        host.exit_code.is_none(),
        "should not exit with error when crate is on allow list, but got exit code {:?}: {}",
        host.exit_code,
        host.error_str()
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_error_if_high_risk_allow_list_wrong_version_still_fails() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_content = format!(
        r#"{ALWAYS_FAIL_CONFIG}
[[allow_list]]
name = "serde"
version = "=999.0.0"
"#
    );
    let config_path = temp_dir.path().join("aprz.toml");
    std::fs::write(&config_path, &config_content).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--error-if-high-risk",
            "--config",
            config_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert_eq!(
        host.exit_code,
        Some(1),
        "should exit with code 1 when allow list version doesn't match"
    );
}

// ---------------------------------------------------------------------------
// No evaluation at all — an empty config and no `--error-if` flag means the
// crates are reported without an appraisal.
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_without_any_policy_skips_appraisal() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("aprz.toml");
    let json_path = temp_dir.path().join("report.json");
    std::fs::write(&config_path, EMPTY_CONFIG).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--config",
            config_path.to_str().expect("valid path"),
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array");

    assert_eq!(crates.len(), 1);
    assert_eq!(crates[0]["name"].as_str(), Some("serde"));
    assert!(
        crates[0]["appraisal"].is_null(),
        "with no policy and no --error-if flag there is nothing to appraise"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_appraises_when_only_a_high_risk_gate_is_configured() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("aprz.toml");
    let json_path = temp_dir.path().join("report.json");
    std::fs::write(&config_path, ALWAYS_FAIL_CONFIG).expect("write config");

    let host = run_cli(
        &world,
        &[
            "crates",
            "serde@1.0.200",
            "--config",
            config_path.to_str().expect("valid path"),
            "--json",
            json_path.to_str().expect("valid path"),
        ],
    )
    .await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());

    let json_content = std::fs::read_to_string(&json_path).expect("read JSON");
    let parsed: serde_json::Value = serde_json::from_str(&json_content).expect("valid JSON");
    let crates = parsed["crates"].as_array().expect("crates array");

    // A `high_risk` gate on its own is enough to appraise: no `eval` section and no --error-if
    // flag are needed.
    assert!(
        !crates[0]["appraisal"].is_null(),
        "a high_risk gate alone must produce an appraisal"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_defaults_to_console_when_nothing_else_is_requested() {
    let world = MockWorld::new().await;
    let host = run_cli(&world, &["crates", "serde@1.0.200"]).await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());
    assert!(
        host.output_str().contains("serde"),
        "with no report and no --error-if flag the console is the default output"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn test_crates_command_suppresses_console_when_only_html_is_requested() {
    let world = MockWorld::new().await;
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let html_path = temp_dir.path().join("report.html");

    let host = run_cli(
        &world,
        &["crates", "serde@1.0.200", "--html", html_path.to_str().expect("valid path")],
    )
    .await;

    assert!(host.error_str().is_empty(), "crates command failed: {}", host.error_str());
    assert!(html_path.exists(), "HTML report file should be created");
    assert!(
        host.output_str().is_empty(),
        "requesting any single report suppresses the default console output, got: {}",
        host.output_str()
    );
}
