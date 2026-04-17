// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the `cargo-heather` binary.
//!
//! These tests exercise the full pipeline from CLI to output.

#![allow(deprecated, reason = "assert_cmd API deprecation warnings are not actionable")]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use cargo_heather::config::{self, HeatherConfig};
use cargo_heather::{checker, license, scanner};
use tempfile::TempDir;

fn default_config() -> HeatherConfig {
    HeatherConfig::with_defaults(String::new())
}

/// Helper to create a test project with config and source files.
fn create_project(config_content: &str, files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("failed to create temp dir");

    std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), config_content).expect("failed to write config file");

    for (path, content) in files {
        let full_path = dir.path().join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create parent dirs");
        }
        std::fs::write(&full_path, content).expect("failed to write test file");
    }

    dir
}

/// Helper to get the config exclusion path for a test project.
fn config_exclude(dir: &Path) -> PathBuf {
    dir.join(config::CONFIG_FILE_NAME)
}

// --- Full pipeline tests ---

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_mit_header_check_passes() {
    let header = license::header_for_license("MIT").unwrap();
    let comment = config::format_header_comment(header);
    let source = format!("{comment}\n\nfn main() {{}}\n");

    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", &source)]);

    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    assert_eq!(files.len(), 1);

    let cfg = config::load_config(dir.path()).unwrap();
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().all(|r| r.result == checker::CheckResult::Ok));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_apache_header_check_passes() {
    let header = license::header_for_license("Apache-2.0").unwrap();
    let comment = config::format_header_comment(header);
    let source = format!("{comment}\n\nfn main() {{}}\n");

    let dir = create_project("license = \"Apache-2.0\"\n", &[("src/main.rs", &source)]);

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().all(|r| r.result == checker::CheckResult::Ok));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_missing_header_detected() {
    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", "fn main() {}\n")]);

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().any(|r| r.result == checker::CheckResult::Missing));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_wrong_header_detected() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[("src/main.rs", "// Wrong license header\n\nfn main() {}\n")],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().any(|r| matches!(r.result, checker::CheckResult::Mismatch { .. })));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_custom_header() {
    let dir = create_project(
        "header = \"Copyright 2024 ACME Corp\"\n",
        &[("src/lib.rs", "// Copyright 2024 ACME Corp\n\npub fn foo() {}\n")],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().all(|r| r.result == checker::CheckResult::Ok));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_custom_multiline_header() {
    let dir = create_project(
        "header = \"\"\"Copyright 2024\nAll rights reserved.\"\"\"\n",
        &[("src/lib.rs", "// Copyright 2024\n// All rights reserved.\n\npub fn foo() {}\n")],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().all(|r| r.result == checker::CheckResult::Ok));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_fix_adds_header() {
    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", "fn main() {}\n")]);

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());

    for file in &files {
        checker::fix_file(file, &cfg).unwrap();
    }

    let content = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(content.starts_with("// Licensed under the MIT License."));
    assert!(content.contains("fn main()"));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_fix_replaces_wrong_header() {
    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", "// Old wrong header\n\nfn main() {}\n")]);

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());

    for file in &files {
        checker::fix_file(file, &cfg).unwrap();
    }

    let content = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(content.starts_with("// Licensed under the MIT License."));
    assert!(!content.contains("Old wrong header"));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_multiple_files_mixed() {
    let mit_header = license::header_for_license("MIT").unwrap();
    let comment = config::format_header_comment(mit_header);
    let good_source = format!("{comment}\n\nfn good() {{}}\n");

    let dir = create_project(
        "license = \"MIT\"\n",
        &[
            ("src/main.rs", &good_source),
            ("src/lib.rs", "fn bad() {}\n"),
            ("tests/test.rs", "// Wrong\n\n#[test] fn t() {}\n"),
        ],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    assert_eq!(files.len(), 3);

    let results = checker::check_files(&files, &cfg).unwrap();
    let ok_count = results.iter().filter(|r| r.result == checker::CheckResult::Ok).count();
    let fail_count = results.len() - ok_count;
    assert_eq!(ok_count, 1);
    assert_eq!(fail_count, 2);
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_scanner_skips_target() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[
            ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
            ("target/debug/build/gen.rs", "fn generated() {}\n"),
        ],
    );

    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    assert_eq!(files.len(), 1);
    assert!(files[0].to_string_lossy().contains("main.rs"));
}

// --- CLI binary tests ---

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_help() {
    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather").arg("--help");
    cmd.assert().success().stdout(predicates::str::contains("Validate license headers"));
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_version() {
    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather").arg("--version");
    cmd.assert().success().stdout(predicates::str::contains("0.1.0"));
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_check_passes() {
    let mit_header = license::header_for_license("MIT").unwrap();
    let comment = config::format_header_comment(mit_header);
    let source = format!("{comment}\n\nfn main() {{}}\n");

    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", &source)]);

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather").arg("--project-dir").arg(dir.path().to_str().unwrap());
    cmd.assert().success();
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_check_fails_on_missing_header() {
    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", "fn main() {}\n")]);

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather").arg("--project-dir").arg(dir.path().to_str().unwrap());
    cmd.assert().failure();
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_fix_mode() {
    let dir = create_project("license = \"MIT\"\n", &[("src/main.rs", "fn main() {}\n")]);

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather")
        .arg("--project-dir")
        .arg(dir.path().to_str().unwrap())
        .arg("--fix");
    cmd.assert().success();

    let content = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(content.starts_with("// Licensed under the MIT License."));
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_custom_config_path() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("my-config.toml");
    std::fs::write(&config_path, "license = \"MIT\"\n").unwrap();

    let mit_header = license::header_for_license("MIT").unwrap();
    let comment = config::format_header_comment(mit_header);
    let source = format!("{comment}\n\nfn main() {{}}\n");

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), &source).unwrap();

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather")
        .arg("--project-dir")
        .arg(dir.path().to_str().unwrap())
        .arg("--config")
        .arg(config_path.to_str().unwrap());
    cmd.assert().success();
}

#[test]
#[cfg_attr(miri, ignore)] // subprocess execution is not supported under Miri
fn binary_no_config_file_fails() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut cmd = Command::cargo_bin("cargo-heather").unwrap();
    cmd.arg("heather").arg("--project-dir").arg(dir.path().to_str().unwrap());
    cmd.assert().failure();
}

// --- License lookup integration ---

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn all_spdx_licenses_produce_valid_config() {
    for spdx_id in license::supported_licenses() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), format!("license = \"{spdx_id}\"\n")).unwrap();

        let cfg = config::load_config(dir.path());
        assert!(cfg.is_ok(), "Failed to load config for SPDX ID: {spdx_id}");

        let cfg = cfg.unwrap();
        assert!(!cfg.header_text.is_empty(), "Empty header for SPDX ID: {spdx_id}");
    }
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn all_spdx_headers_round_trip_through_checker() {
    for spdx_id in license::supported_licenses() {
        let header = license::header_for_license(spdx_id).unwrap();
        let comment = config::format_header_comment(header);
        let source = format!("{comment}\n\nfn main() {{}}\n");

        let cfg = HeatherConfig::with_defaults(header.to_owned());

        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, &source).unwrap();

        let result = checker::check_file(&file, &cfg).unwrap();
        assert_eq!(
            result.result,
            checker::CheckResult::Ok,
            "Header round-trip failed for SPDX ID: {spdx_id}"
        );
    }
}

#[test]
fn format_header_comment_produces_valid_rust_comments() {
    let header = "Line 1\n\nLine 3";
    let comment = config::format_header_comment(header);

    for line in comment.lines() {
        assert!(line.starts_with("//"), "Line does not start with '//': {line}");
    }
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn scanner_finds_nested_files() {
    let dir = TempDir::new().unwrap();
    for path in &["src/main.rs", "src/a/b/c.rs", "tests/integration.rs", "examples/demo.rs"] {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, "// stub\n").unwrap();
    }

    let files = scanner::find_source_files(dir.path(), None, &default_config());
    let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
    assert_eq!(rs_count, 4);
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn config_with_gpl3_produces_correct_header() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), "license = \"GPL-3.0-only\"\n").unwrap();

    let cfg = config::load_config(dir.path()).unwrap();
    assert!(cfg.header_text.contains("GNU General Public License"));
    assert!(cfg.header_text.contains("version 3"));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn config_with_mpl2_produces_correct_header() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), "license = \"MPL-2.0\"\n").unwrap();

    let cfg = config::load_config(dir.path()).unwrap();
    assert!(cfg.header_text.contains("Mozilla Public"));
    assert!(cfg.header_text.contains("https://mozilla.org/MPL/2.0/"));
}

// --- TOML file integration tests ---

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_toml_header_check_passes() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[
            ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
            ("Cargo.toml", "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n"),
        ],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().all(|r| r.result == checker::CheckResult::Ok));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_toml_missing_header_detected() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[
            ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
            ("Cargo.toml", "[package]\nname = \"foo\"\n"),
        ],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    let results = checker::check_files(&files, &cfg).unwrap();
    assert!(results.iter().any(|r| r.result == checker::CheckResult::Missing));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_toml_fix_adds_header() {
    let dir = create_project("license = \"MIT\"\n", &[("Cargo.toml", "[package]\nname = \"foo\"\n")]);

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());

    for file in &files {
        checker::fix_file(file, &cfg).unwrap();
    }

    let content = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(content.starts_with("# Licensed under the MIT License."));
    assert!(content.contains("[package]"));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_toml_fix_replaces_wrong_header() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[("Cargo.toml", "# Wrong header\n\n[package]\nname = \"foo\"\n")],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());

    for file in &files {
        checker::fix_file(file, &cfg).unwrap();
    }

    let content = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
    assert!(content.starts_with("# Licensed under the MIT License."));
    assert!(content.contains("[package]"));
    assert!(!content.contains("Wrong header"));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_config_file_excluded_from_scan() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n")],
    );

    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());
    assert!(files.iter().all(|f| !f.to_string_lossy().contains(config::CONFIG_FILE_NAME)));
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn full_pipeline_mixed_rs_and_toml() {
    let dir = create_project(
        "license = \"MIT\"\n",
        &[
            ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
            ("Cargo.toml", "# Licensed under the MIT License.\n\n[package]\nname = \"foo\"\n"),
            ("deny.toml", "[licenses]\n"),
        ],
    );

    let cfg = config::load_config(dir.path()).unwrap();
    let files = scanner::find_source_files(dir.path(), Some(&config_exclude(dir.path())), &default_config());

    let results = checker::check_files(&files, &cfg).unwrap();
    let ok_count = results.iter().filter(|r| r.result == checker::CheckResult::Ok).count();
    let fail_count = results.len() - ok_count;
    assert_eq!(ok_count, 2); // main.rs and Cargo.toml
    assert_eq!(fail_count, 1); // deny.toml missing header
}

#[test]
#[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
fn run_fix_replaces_wrong_header() {
    use cargo_heather::cli::HeatherArgs;

    let dir = create_project(
        "license = \"MIT\"\n",
        &[("src/main.rs", "// Wrong license header\n\nfn main() {}\n")],
    );

    let args = HeatherArgs {
        project_dir: Some(dir.path().to_path_buf()),
        config: None,
        fix: true,
    };

    cargo_heather::run(&args).unwrap();

    let content = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
    assert!(content.contains("Licensed under the MIT License"));
    assert!(!content.contains("Wrong license header"));
}
