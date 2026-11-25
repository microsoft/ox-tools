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
//! when `--manifest-path` names a member. It checks explicit requests on direct
//! normal and build dependencies and requests reachable from a member's default features.
//! It ignores dev dependencies, transitive activation and dependency defaults.
//!
//! By default it denies the exact, case-sensitive names `fakes`, `test-util`,
//! and `insecure`. A JSON policy passed with `--policy` can replace the list
//! with a nonempty list of unique feature names in `features`, or omit that
//! field to keep the defaults. Exceptions must name a feature on the active
//! deny list. Unknown fields are errors except for the optional top-level
//! `$comment` field. Duplicate exceptions are also errors. For example:
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
//! The second matches only the package's own `test-util` feature when reached
//! from `default`, not a request for `test-util` on a dependency.
//! The checker exits silently with 0 when there are no findings. It reports
//! findings and runtime errors to stderr and exits 1. Invalid CLI usage exits 2.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod inspect;
mod policy;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::{env, io};

use cargo_metadata::MetadataCommand;
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
    execute(
        &Cli::parse().command,
        |name| env::var_os(name),
        |path| std::fs::read(path),
        Command::output,
        &mut io::stderr().lock(),
    )
}

fn execute(
    args: &CargoCommand,
    env: impl FnOnce(&str) -> Option<OsString>,
    read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
    output: impl FnOnce(&mut Command) -> io::Result<Output>,
    stderr: &mut impl Write,
) -> ExitCode {
    let mut report = |message: &str| {
        // An unavailable stderr must not replace the failure status with a panic.
        let _ = writeln!(stderr, "{message}");
    };
    match check(args, env, read, output) {
        Ok(violations) if violations.is_empty() => return ExitCode::SUCCESS,
        Ok(violations) => {
            for violation in violations {
                report(&format!("cargo-ensure-no-test-features: {violation}"));
            }
            report(
                "cargo-ensure-no-test-features: remove these requests from production dependencies and defaults (including inherited workspace dependencies), or use --policy with an exact exception",
            );
        }
        Err(error) => report(&format!("cargo-ensure-no-test-features: {error}")),
    }
    ExitCode::FAILURE
}

fn check(
    args: &CargoCommand,
    env: impl FnOnce(&str) -> Option<OsString>,
    read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
    output: impl FnOnce(&mut Command) -> io::Result<Output>,
) -> Result<std::collections::BTreeSet<String>, AppError> {
    let CargoCommand::EnsureNoTestFeatures { manifest_path, policy } = args;
    let policy = policy::load(policy.as_deref(), read)?;
    // Use the invoking Cargo executable so its toolchain is preserved.
    let cargo = PathBuf::from(env("CARGO").unwrap_or_else(|| "cargo".into()));
    let mut command = MetadataCommand::new();
    command.cargo_path(cargo).no_deps();
    if let Some(path) = manifest_path {
        command.manifest_path(path);
    }
    let output = output(&mut command.cargo_command()).map_err(|error| AppError::new(format!("could not run cargo metadata: {error}")))?;
    if !output.status.success() {
        return Err(AppError::new(format!(
            "cargo metadata failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        )));
    }

    inspect::check(&output.stdout, &policy)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    const CLEAN: &[u8] = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":[],"packages":[]}"#;
    const FINDING: &[u8] = br#"{"version":1,"target_directory":"target","workspace_root":".","workspace_members":["app"],"packages":[
        {"id":"app","name":"app","version":"0.1.0","targets":[],"manifest_path":"Cargo.toml","publish":null,
         "features":{"default":["test-util"],"test-util":[]},"dependencies":[]}
    ]}"#;

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

    #[test]
    fn uses_selected_cargo_and_reports_findings_from_the_selected_manifest() {
        let args = CargoCommand::EnsureNoTestFeatures {
            manifest_path: Some("member/Cargo.toml".into()),
            policy: Some("my policy.json".into()),
        };
        let mut messages = Vec::new();
        let code = execute(
            &args,
            |name| {
                assert_eq!(name, "CARGO");
                Some("selected cargo".into())
            },
            |path| {
                assert_eq!(path, Path::new("my policy.json"));
                Ok(br#"{"features":["test-util"]}"#.to_vec())
            },
            |command| {
                assert_eq!(command.get_program(), "selected cargo");
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    [
                        "metadata",
                        "--format-version",
                        "1",
                        "--no-deps",
                        "--manifest-path",
                        "member/Cargo.toml"
                    ]
                );
                Ok(Output {
                    status: ExitStatus::default(),
                    stdout: FINDING.to_vec(),
                    stderr: Vec::new(),
                })
            },
            &mut messages,
        );
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(
            String::from_utf8_lossy(&messages).lines().collect::<Vec<_>>(),
            [
                "cargo-ensure-no-test-features: Cargo.toml: 'app' enables its own 'test-util' through default features",
                "cargo-ensure-no-test-features: remove these requests from production dependencies and defaults (including inherited workspace dependencies), or use --policy with an exact exception",
            ]
        );
    }

    #[test]
    fn default_scan_is_silent_and_metadata_failures_are_reported() {
        let args = CargoCommand::EnsureNoTestFeatures {
            manifest_path: None,
            policy: None,
        };
        #[cfg(unix)]
        let failed = ExitStatus::from_raw(1 << 8);
        #[cfg(windows)]
        let failed = ExitStatus::from_raw(1);
        for (result, code, expected) in [
            (
                Ok(Output {
                    status: ExitStatus::default(),
                    stdout: CLEAN.to_vec(),
                    stderr: Vec::new(),
                }),
                ExitCode::SUCCESS,
                None,
            ),
            (
                Ok(Output {
                    status: failed,
                    stdout: CLEAN.to_vec(),
                    stderr: b"bad manifest \n".to_vec(),
                }),
                ExitCode::FAILURE,
                Some(format!(
                    "cargo-ensure-no-test-features: cargo metadata failed ({failed}): bad manifest"
                )),
            ),
            (
                Err(io::Error::other("cannot start Cargo")),
                ExitCode::FAILURE,
                Some("cargo-ensure-no-test-features: could not run cargo metadata: cannot start Cargo".into()),
            ),
        ] {
            let mut messages = Vec::new();
            let actual = execute(
                &args,
                |_| None,
                |_| panic!("default policy is bundled"),
                |command| {
                    assert_eq!(command.get_program(), "cargo");
                    assert_eq!(
                        command.get_args().collect::<Vec<_>>(),
                        ["metadata", "--format-version", "1", "--no-deps"]
                    );
                    result
                },
                &mut messages,
            );
            assert_eq!(actual, code);
            assert_eq!(
                String::from_utf8_lossy(&messages).lines().collect::<Vec<_>>(),
                expected.into_iter().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn policy_errors_precede_metadata() {
        let args = CargoCommand::EnsureNoTestFeatures {
            manifest_path: None,
            policy: Some("policy.json".into()),
        };
        let mut messages = Vec::new();
        let code = execute(
            &args,
            |_| panic!("policy must be read before looking up Cargo"),
            |_| Err(io::Error::other("scripted read failure")),
            |_| panic!("metadata must not run with an unreadable policy"),
            &mut messages,
        );
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(
            messages,
            b"cargo-ensure-no-test-features: could not read policy at policy.json: scripted read failure\n"
        );
    }

    #[test]
    fn diagnostic_write_failure_does_not_change_the_exit_code() {
        let args = CargoCommand::EnsureNoTestFeatures {
            manifest_path: None,
            policy: Some("policy.json".into()),
        };
        let mut stderr = &mut [][..];
        let code = execute(
            &args,
            |_| panic!("policy must be read before looking up Cargo"),
            |_| Err(io::Error::other("scripted read failure")),
            |_| panic!("metadata must not run with an unreadable policy"),
            &mut stderr,
        );
        assert_eq!(code, ExitCode::FAILURE);
    }
}
