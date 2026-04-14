// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-heather`.

use std::path::PathBuf;

use clap::{Args, Parser};

/// Cargo subcommand entry point.
///
/// Handles the `cargo heather` invocation pattern where cargo passes
/// "heather" as the first argument to the `cargo-heather` binary.
#[derive(Parser, Debug)]
#[command(name = "cargo", bin_name = "cargo")]
pub enum CargoCli {
    /// Validate license headers in Rust source files.
    Heather(HeatherArgs),
}

/// Arguments for the `cargo heather` command.
#[derive(Args, Debug, Clone)]
#[command(version, about = "Validate license headers in Rust source files")]
pub struct HeatherArgs {
    /// Path to the project directory (defaults to current directory).
    #[arg(long)]
    pub project_dir: Option<PathBuf>,

    /// Path to the configuration file (defaults to `.cargo-heather.toml` in project dir).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Fix files by adding or replacing missing/incorrect headers.
    #[arg(long)]
    pub fix: bool,
}

impl HeatherArgs {
    /// Returns the project directory, defaulting to the current directory.
    #[must_use]
    pub fn project_dir(&self) -> PathBuf {
        self.project_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_basic_args() {
        let cli = CargoCli::parse_from(["cargo", "heather"]);
        let CargoCli::Heather(args) = cli;
        assert!(args.project_dir.is_none());
        assert!(args.config.is_none());
        assert!(!args.fix);
    }

    #[test]
    fn parse_with_fix_flag() {
        let cli = CargoCli::parse_from(["cargo", "heather", "--fix"]);
        let CargoCli::Heather(args) = cli;
        assert!(args.fix);
    }

    #[test]
    fn parse_with_project_dir() {
        let cli = CargoCli::parse_from(["cargo", "heather", "--project-dir", "/my/project"]);
        let CargoCli::Heather(args) = cli;
        assert_eq!(args.project_dir, Some(PathBuf::from("/my/project")));
    }

    #[test]
    fn parse_with_config_path() {
        let cli = CargoCli::parse_from(["cargo", "heather", "--config", "/custom/config.toml"]);
        let CargoCli::Heather(args) = cli;
        assert_eq!(args.config, Some(PathBuf::from("/custom/config.toml")));
    }

    #[test]
    fn parse_with_all_flags() {
        let cli = CargoCli::parse_from([
            "cargo",
            "heather",
            "--fix",
            "--project-dir",
            "/my/project",
            "--config",
            "/my/config.toml",
        ]);
        let CargoCli::Heather(args) = cli;
        assert!(args.fix);
        assert_eq!(args.project_dir, Some(PathBuf::from("/my/project")));
        assert_eq!(args.config, Some(PathBuf::from("/my/config.toml")));
    }

    #[test]
    fn project_dir_default() {
        let args = HeatherArgs {
            project_dir: None,
            config: None,
            fix: false,
        };
        assert_eq!(args.project_dir(), PathBuf::from("."));
    }

    #[test]
    fn project_dir_custom() {
        let args = HeatherArgs {
            project_dir: Some(PathBuf::from("/custom/path")),
            config: None,
            fix: false,
        };
        assert_eq!(args.project_dir(), PathBuf::from("/custom/path"));
    }
}
