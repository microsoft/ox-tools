// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A Cargo subcommand that rejects test-only feature requests in publishable workspace packages.
//!
//! # Installation
//!
//! ```bash
//! cargo install cargo-ensure-no-test-features --locked
//! ```
//!
//! # Usage
//!
//! ```bash
//! cargo ensure-no-test-features
//! cargo ensure-no-test-features --manifest-path path/to/Cargo.toml --policy policy.json
//! ```
//!
//! The checker scans publishable members of the selected Cargo workspace, even
//! when `--manifest-path` names a member. It checks explicit direct production
//! dependency requests and requests reachable from a member's default features.
//! It ignores dev dependencies, transitive activation and dependency defaults.
//!
//! By default it denies the exact, case-sensitive names `fakes`, `test-util`,
//! and `insecure`. A JSON policy passed with `--policy` can replace the list
//! with `features` or omit that field to keep the defaults. For example:
//!
//! ```json
//! {
//!   "exceptions": [
//!     {"package": "example", "feature": "fakes", "dependency": "transport"},
//!     {"package": "example", "feature": "test-util"}
//!   ]
//! }
//! ```
//!
//! The first exception matches a dependency manifest key (the alias if renamed).
//! The second matches only the package's own default activation. Success is
//! silent (exit 0); violations and runtime errors go to stderr (exit 1).
//! Invalid CLI usage exits 2.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod inspect;
mod policy;

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use ohno::AppError;

#[derive(Parser)]
#[command(bin_name = "cargo")]
struct Cli {
    #[command(subcommand)]
    command: CargoCommand,
}

#[derive(Subcommand)]
enum CargoCommand {
    /// Reject test-only feature requests in publishable workspace packages.
    #[command(version, display_name = "cargo-ensure-no-test-features")]
    EnsureNoTestFeatures {
        /// Select the workspace via a Cargo.toml path.
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,

        /// Read a JSON policy instead of using only the bundled defaults.
        #[arg(long, value_name = "PATH")]
        policy: Option<PathBuf>,
    },
}

/// Runs the Cargo subcommand and reports findings or errors to stderr.
#[must_use]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run() -> ExitCode {
    let CargoCommand::EnsureNoTestFeatures { manifest_path, policy } = Cli::parse().command;
    match check(manifest_path.as_deref(), policy.as_deref()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("cargo-ensure-no-test-features: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn check(manifest_path: Option<&Path>, policy_path: Option<&Path>) -> Result<ExitCode, AppError> {
    let policy = policy::load(policy_path, |path| std::fs::read(path))?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.args(["metadata", "--no-deps", "--format-version", "1"]);
    if let Some(path) = manifest_path {
        command.arg("--manifest-path").arg(path);
    }
    let output = command
        .output()
        .map_err(|error| AppError::new(format!("could not run cargo metadata: {error}")))?;
    if !output.status.success() {
        return Err(AppError::new(format!(
            "cargo metadata failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }

    let violations = inspect::check(&output.stdout, &policy)?;
    if violations.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    for violation in violations {
        eprintln!("cargo-ensure-no-test-features: {violation}");
    }
    eprintln!(
        "cargo-ensure-no-test-features: remove these requests from production dependencies and defaults (including inherited workspace dependencies), or use --policy with an exact exception"
    );
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_subcommand_and_optional_paths() -> Result<(), clap::Error> {
        let CargoCommand::EnsureNoTestFeatures { manifest_path, policy } =
            Cli::try_parse_from(["cargo-ensure-no-test-features", "ensure-no-test-features"])?.command;
        assert!(manifest_path.is_none());
        assert!(policy.is_none());
        let CargoCommand::EnsureNoTestFeatures { manifest_path, policy } = Cli::try_parse_from([
            "cargo-ensure-no-test-features",
            "ensure-no-test-features",
            "--manifest-path",
            "member/Cargo.toml",
            "--policy",
            "my policy.json",
        ])?
        .command;
        assert_eq!(manifest_path.as_deref(), Some(Path::new("member/Cargo.toml")));
        assert_eq!(policy.as_deref(), Some(Path::new("my policy.json")));
        Ok(())
    }
}
