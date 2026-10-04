// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rehearse selected release candidates without unpublished workspace dependency shortcuts.
//!
//! Invoke `cargo release-guard check --base origin/main --output-dir <fresh-directory>`.
//! The guard never publishes or edits the development checkout. See `--help` for
//! candidate-report reuse, explicit selections, and feature/test-runner options.

mod cli;
mod config;
mod execute;
mod materialize;
mod model;
mod selection;
mod source;
#[cfg(test)]
mod test_support;

use std::process::ExitCode;

/// Run the command-line application.
///
/// Errors are emitted as diagnostics and, once the output is claimed, JSON artifacts.
#[must_use]
pub fn run() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("release guard: {}", execute::redact(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, ohno::AppError>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(std::io::Error::other(message.into()).into())
}
