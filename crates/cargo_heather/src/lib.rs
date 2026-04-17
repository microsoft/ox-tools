// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-heather
//!
//! A cargo sub-command to validate license headers in Rust (`.rs`) and TOML (`.toml`) source files.
//!
//! ## Installation
//!
//! ```bash
//! cargo install --path .
//! ```
//!
//! ## Setup
//!
//! Create a `.cargo-heather.toml` file in your project root, **or** simply set the `license` field in your `Cargo.toml` — the tool will use it automatically when no `.cargo-heather.toml` is present.
//!
//! ### Using an SPDX License Identifier
//!
//! ```toml
//! license = "MIT"
//! ```
//!
//! ### Using a Custom Header
//!
//! ```toml
//! header = """
//! Copyright (c) 2024 MyCompany
//! All rights reserved.
//! """
//! ```
//!
//! ## Usage
//!
//! ```bash
//! # Check all .rs and .toml files for correct license headers
//! cargo heather
//!
//! # Automatically fix files by adding/replacing headers
//! cargo heather --fix
//! ```
//!
//! ### Options
//!
//! - `--project-dir <PATH>` — Path to the project directory (defaults to current directory)
//! - `--config <PATH>` — Path to the configuration file (defaults to `.cargo-heather.toml` in project directory)
//! - `--fix` — Fix files by adding or replacing missing/incorrect headers
//! - `--help` — Print help
//! - `--version` — Print version
//!
//! ### Example
//!
//! ```bash
//! $ cargo heather
//! Checking 5 file(s)...
//! MISSING header: src/utils.rs
//! MISMATCH header: src/lib.rs
//! 2 file(s) have missing or incorrect license headers
//!
//! $ cargo heather --fix
//! Checking 5 file(s)...
//! Fixed (added header): src/utils.rs
//! Fixed (replaced header): src/lib.rs
//! Fixed 2 file(s).
//! ```
//!
//! ## Supported SPDX Identifiers
//!
//! | Identifier | License |
//! |---|---|
//! | `MIT` | MIT License |
//! | `Apache-2.0` | Apache License 2.0 |
//! | `GPL-2.0-only` | GNU General Public License v2.0 only |
//! | `GPL-2.0-or-later` | GNU General Public License v2.0 or later |
//! | `GPL-3.0-only` | GNU General Public License v3.0 only |
//! | `GPL-3.0-or-later` | GNU General Public License v3.0 or later |
//! | `LGPL-2.1-only` | GNU Lesser General Public License v2.1 only |
//! | `LGPL-2.1-or-later` | GNU Lesser General Public License v2.1 or later |
//! | `LGPL-3.0-only` | GNU Lesser General Public License v3.0 only |
//! | `LGPL-3.0-or-later` | GNU Lesser General Public License v3.0 or later |
//! | `BSD-2-Clause` | BSD 2-Clause "Simplified" License |
//! | `BSD-3-Clause` | BSD 3-Clause "New" or "Revised" License |
//! | `ISC` | ISC License |
//! | `MPL-2.0` | Mozilla Public License 2.0 |
//! | `AGPL-3.0-only` | GNU Affero General Public License v3.0 only |
//! | `AGPL-3.0-or-later` | GNU Affero General Public License v3.0 or later |
//! | `Unlicense` | The Unlicense |
//! | `BSL-1.0` | Boost Software License 1.0 |
//! | `0BSD` | BSD Zero Clause License |
//! | `Zlib` | zlib License |
//!
//! ## How it works
//!
//! 1. **Config loading** — Reads `.cargo-heather.toml` from the project root and resolves the expected header text (from SPDX identifier or custom text).
//! 2. **File scanning** — Walks the project directory to find all `.rs` and `.toml` files, skipping `target/`, hidden directories, and the config file itself.
//! 3. **Header validation** — Extracts the first comment block from each file (`//` for Rust, `#` for TOML) and compares it to the expected header. Reports missing or mismatched headers.
//! 4. **Fix mode** — When `--fix` is passed, automatically prepends the correct header to files that are missing it, or replaces incorrect headers.

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_heather/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo_heather/favicon.ico")]
#![deny(unsafe_code)]

pub mod checker;
pub mod cli;
pub mod comment;
pub mod config;
pub mod error;
pub mod license;
pub mod scanner;

use std::path::Path;

use checker::{CheckResult, FileCheckResult};
use cli::HeatherArgs;
use config::HeatherConfig;
use error::HeatherError;
use ohno::AppError;
use tracing::info;

/// Run the `cargo-heather` tool with the given arguments.
///
/// Loads config, scans for source files, and validates headers.
/// In `--fix` mode, automatically adds or replaces headers.
///
/// # Errors
///
/// Returns an error if config loading fails, files can't be read,
/// or (in check mode) validation fails.
pub fn run(args: &HeatherArgs) -> Result<(), AppError> {
    let project_dir = args.project_dir();
    let config_path = resolve_config_path(args, &project_dir);
    let config = load_config(args, &project_dir)?;
    let files = scanner::find_source_files(&project_dir, Some(&config_path), &config);

    if files.is_empty() {
        info!("No source files found in '{}'.", project_dir.display());
        return Ok(());
    }

    info!("Checking {} file(s)...", files.len());

    if args.fix {
        run_fix(&files, &config, &project_dir)?;
        Ok(())
    } else {
        run_check(&files, &config, &project_dir)
    }
}

fn resolve_config_path(args: &HeatherArgs, project_dir: &Path) -> std::path::PathBuf {
    match &args.config {
        Some(path) => path.clone(),
        None => config::config_path_for(project_dir),
    }
}

fn load_config(args: &HeatherArgs, project_dir: &Path) -> Result<HeatherConfig, HeatherError> {
    match &args.config {
        Some(path) => config::load_config_from_path(path),
        None => config::load_config(project_dir),
    }
}

fn run_check(files: &[std::path::PathBuf], config: &HeatherConfig, project_dir: &Path) -> Result<(), AppError> {
    let results = checker::check_files(files, config)?;
    let failures = report_results(&results, project_dir);

    if failures > 0 {
        ohno::bail!(HeatherError::ValidationFailed(failures));
    }

    info!("All {} file(s) have correct license headers.", results.len());
    Ok(())
}

fn run_fix(files: &[std::path::PathBuf], config: &HeatherConfig, project_dir: &Path) -> Result<usize, AppError> {
    let mut fixed_count: usize = 0;

    for file in files {
        let result = checker::fix_file(file, config)?;
        let relative = make_relative(&result.path, project_dir);

        match &result.result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                info!("  Fixed (added header): {}", relative.display());
                fixed_count += 1;
            }
            CheckResult::Mismatch { .. } => {
                info!("  Fixed (replaced header): {}", relative.display());
                fixed_count += 1;
            }
        }
    }

    match fixed_count {
        0 => info!("All files already have correct headers."),
        n => info!("Fixed {n} file(s)."),
    }

    Ok(fixed_count)
}

fn report_results(results: &[FileCheckResult], project_dir: &Path) -> usize {
    let mut failures = 0;

    for result in results {
        let relative = make_relative(&result.path, project_dir);

        match &result.result {
            CheckResult::Ok => {}
            CheckResult::Missing => {
                info!("  MISSING header: {}", relative.display());
                failures += 1;
            }
            CheckResult::Mismatch { .. } => {
                info!("  MISMATCH header: {}", relative.display());
                failures += 1;
            }
        }
    }

    failures
}

fn make_relative(path: &Path, base: &Path) -> std::path::PathBuf {
    path.strip_prefix(base).unwrap_or(path).to_path_buf()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn setup_project(license_toml: &str, files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();

        std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), license_toml).unwrap();

        for (path, content) in files {
            let full_path = dir.path().join(path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full_path, content).unwrap();
        }

        dir
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_check_all_pass() {
        let dir = setup_project(
            "license = \"MIT\"\n",
            &[
                ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
                ("src/lib.rs", "// Licensed under the MIT License.\n\npub fn hello() {}\n"),
            ],
        );

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: false,
        };

        run(&args).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_check_some_fail() {
        let dir = setup_project(
            "license = \"MIT\"\n",
            &[
                ("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n"),
                ("src/lib.rs", "fn hello() {}\n"),
            ],
        );

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: false,
        };

        run(&args).unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_no_source_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(config::CONFIG_FILE_NAME), "license = \"MIT\"\n").unwrap();

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: false,
        };

        run(&args).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_fix_mode() {
        let dir = setup_project("license = \"MIT\"\n", &[("src/main.rs", "fn main() {}\n")]);

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: true,
        };

        run(&args).unwrap();

        let content = std::fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
        assert!(content.starts_with("// Licensed under the MIT License."));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_fix_already_correct() {
        let dir = setup_project(
            "license = \"MIT\"\n",
            &[("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n")],
        );

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: true,
        };

        run(&args).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_with_custom_config_path() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("custom.toml");
        std::fs::write(&config_path, "license = \"MIT\"\n").unwrap();

        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "// Licensed under the MIT License.\n\nfn main() {}\n",
        )
        .unwrap();

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: Some(config_path),
            fix: false,
        };

        run(&args).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_config_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: false,
        };

        run(&args).unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_with_custom_header() {
        let dir = setup_project(
            "header = \"Copyright 2024 MyCompany\"\n",
            &[("src/main.rs", "// Copyright 2024 MyCompany\n\nfn main() {}\n")],
        );

        let args = HeatherArgs {
            project_dir: Some(dir.path().to_path_buf()),
            config: None,
            fix: false,
        };

        run(&args).unwrap();
    }

    #[test]
    fn make_relative_strips_prefix() {
        let result = make_relative(Path::new("/project/src/main.rs"), Path::new("/project"));
        assert_eq!(result, Path::new("src/main.rs"));
    }

    #[test]
    fn make_relative_keeps_path_when_no_prefix() {
        let result = make_relative(Path::new("/other/src/main.rs"), Path::new("/project"));
        assert_eq!(result, Path::new("/other/src/main.rs"));
    }

    #[test]
    fn report_results_counts_failures() {
        let results = vec![
            FileCheckResult {
                path: std::path::PathBuf::from("src/main.rs"),
                result: CheckResult::Ok,
            },
            FileCheckResult {
                path: std::path::PathBuf::from("src/lib.rs"),
                result: CheckResult::Missing,
            },
            FileCheckResult {
                path: std::path::PathBuf::from("src/mod.rs"),
                result: CheckResult::Mismatch {
                    expected: "Expected".into(),
                    actual: "Actual".into(),
                },
            },
        ];

        let failures = report_results(&results, Path::new("."));
        assert_eq!(failures, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_fix_returns_count_of_fixed_files() {
        let dir = setup_project(
            "license = \"MIT\"\n",
            &[
                ("src/main.rs", "fn main() {}\n"),
                ("src/lib.rs", "// Wrong header\n\npub fn hello() {}\n"),
                ("src/ok.rs", "// Licensed under the MIT License.\n\nfn ok() {}\n"),
            ],
        );

        let config = config::load_config(dir.path()).unwrap();
        let config_path = config::config_path_for(dir.path());
        let files = scanner::find_source_files(dir.path(), Some(&config_path), &config);

        let fixed = run_fix(&files, &config, dir.path()).unwrap();
        assert_eq!(fixed, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn run_fix_returns_zero_when_all_correct() {
        let dir = setup_project(
            "license = \"MIT\"\n",
            &[("src/main.rs", "// Licensed under the MIT License.\n\nfn main() {}\n")],
        );

        let config = config::load_config(dir.path()).unwrap();
        let config_path = config::config_path_for(dir.path());
        let files = scanner::find_source_files(dir.path(), Some(&config_path), &config);

        let fixed = run_fix(&files, &config, dir.path()).unwrap();
        assert_eq!(fixed, 0);
    }
}
