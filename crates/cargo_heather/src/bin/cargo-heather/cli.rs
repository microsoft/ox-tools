// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-heather`.

use std::path::PathBuf;

use clap::{Args, Parser};

/// Cargo sub-command entry point.
///
/// Handles the `cargo heather` invocation pattern where cargo passes
/// "heather" as the first argument to the `cargo-heather` binary.
#[derive(Parser, Debug)]
#[command(name = "cargo", bin_name = "cargo")]
pub(crate) enum CargoCli {
    /// Validate license headers in Rust source files.
    Heather(HeatherArgs),
}

/// Arguments for the `cargo heather` command.
#[derive(Args, Debug, Clone)]
#[command(version, about = "Validate license headers in Rust source files")]
pub(crate) struct HeatherArgs {
    /// Path to the project directory (defaults to current directory).
    #[arg(long)]
    pub(crate) project_dir: Option<PathBuf>,

    /// Path to the configuration file (defaults to `.cargo-heather.toml` in project directory).
    #[arg(long)]
    pub(crate) config: Option<PathBuf>,

    /// Fix files by adding or replacing missing/incorrect headers.
    #[arg(long)]
    pub(crate) fix: bool,
}

impl HeatherArgs {
    /// Returns the project directory, defaulting to the current directory.
    pub(crate) fn project_dir(&self) -> PathBuf {
        self.project_dir.clone().unwrap_or_else(|| PathBuf::from("."))
    }
}
