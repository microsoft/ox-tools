// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level `update` driver.
//!
//! In this commit it is a no-op: it prints a banner and returns. Later commits
//! land manifest I/O, drift detection, and the per-host emitters.

use anyhow::Result;
use tracing::info;

use crate::cli::{Command, UpdateArgs};

/// Run the parsed CLI command.
///
/// # Errors
///
/// Returns an error when the underlying subcommand fails. In this commit no
/// subcommand can fail, so this always returns `Ok(())`.
pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Update(args) => run_update(&args),
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "no-op today; will become fallible in subsequent commits"
)]
fn run_update(args: &UpdateArgs) -> Result<()> {
    info!(
        backends = ?args.backends,
        no_backends = args.no_backends,
        dry_run = args.dry_run,
        "cargo-ox-check update: scaffolding not yet implemented (see docs/implementation-plans/0000.md)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_update_is_noop() {
        run(Command::Update(UpdateArgs::default())).unwrap();
    }

    #[test]
    fn run_update_with_backends_is_noop() {
        run(Command::Update(UpdateArgs {
            backends: vec!["github".into()],
            no_backends: false,
            dry_run: true,
        }))
        .unwrap();
    }
}
