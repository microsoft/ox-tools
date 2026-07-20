// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An unpublished crate for shared code used for writing Rust scripts

#![allow(clippy::missing_errors_doc, reason = "this is an internal crate for scripts")]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "panic-on-failure idioms are appropriate in tests"
    )
)]

use std::path::Path;
use std::process::Command;

use ohno::{AppError, IntoAppError};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<PackageMetadata>,
}

/// Metadata for a Cargo package
#[derive(Debug, Deserialize)]
pub struct PackageMetadata {
    /// Package name
    pub name: String,
    /// Package ID
    pub id: String,
    /// Path to the package's Cargo.toml
    pub manifest_path: String,
    /// Build targets in the package
    pub targets: Vec<Target>,
}

/// A Cargo build target
#[derive(Debug, Deserialize)]
pub struct Target {
    /// Target kinds (e.g., "lib", "bin")
    pub kind: Vec<String>,
    /// Target name
    pub name: String,
}

/// List all workspace packages using `cargo metadata`
pub fn list_packages(workspace_root: impl AsRef<Path>) -> Result<Vec<PackageMetadata>, AppError> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version=1")
        .arg("--no-deps")
        .current_dir(workspace_root.as_ref())
        .output()
        .into_app_err("failed to execute cargo metadata")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        ohno::bail!("cargo metadata failed: {stderr}");
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout).into_app_err("failed to parse cargo metadata output")?;

    Ok(metadata.packages)
}

/// Internal crates that should be skipped in CI checks
pub const INTERNAL_CRATES: &[&str] = &["automation", "testing_aids"];

/// Run a cargo command and pipe the output to stdout/stderr
pub fn run_cargo(args: impl Iterator<Item = impl AsRef<str>>) -> Result<(), AppError> {
    let args: Vec<_> = args.map(|s| s.as_ref().to_string()).collect();
    let args_str = args.join(" ");

    println!("cargo {args_str}");

    // `.unchecked()` stops duct from turning a non-zero exit into an `Err`
    // itself, so the status check below is the single, observable place that
    // decides success or failure. Output is inherited (piped live to the
    // parent's stdout/stderr), so there is nothing to capture here.
    let output = duct::cmd("cargo", args).unchecked().run()?;

    if !output.status.success() {
        ohno::bail!("cargo {args_str} failed with exit code {:?}", output.status.code());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_list_packages() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
        let packages = list_packages(workspace_root).expect("failed to list packages");
        assert!(!packages.is_empty());

        let automation = packages.iter().find(|p| p.name == "automation");
        assert!(automation.is_some(), "{packages:?}");
        assert!(!automation.unwrap().manifest_path.is_empty());
        assert!(!automation.unwrap().targets.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_run_cargo_success() {
        // `cargo --version` is fast and side-effect-free; a stubbed-out body
        // (`Ok(())`) would also pass here, but the failure test below pins the
        // real behavior.
        run_cargo(["--version"].into_iter()).expect("cargo --version should succeed");
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn test_run_cargo_reports_failure() {
        // An unknown subcommand makes cargo exit non-zero. This asserts that
        // `run_cargo` surfaces the failure as an `Err`, which kills the
        // "delete `!`" and "replace body with `Ok(())`" mutants.
        let result = run_cargo(["this-is-not-a-real-cargo-subcommand"].into_iter());
        assert!(result.is_err(), "expected an error for an unknown cargo subcommand");
    }
}
