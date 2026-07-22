// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-each`.

use std::path::PathBuf;

use clap::{Args, Parser};

/// Cargo sub-command entry point.
///
/// Handles the `cargo each` invocation pattern where cargo passes `each`
/// as the first argument to the `cargo-each` binary. The single-variant
/// enum is the standard clap pattern for cargo subcommands.
#[derive(Parser, Debug)]
#[command(name = "cargo", bin_name = "cargo")]
pub(crate) enum CargoCli {
    /// Run a command over a cargo-style selection of workspace members.
    Each(EachArgs),
}

/// Arguments for the `cargo each` command.
#[derive(Args, Debug, Clone)]
#[command(
    version,
    about = "Run a command over a cargo-style selection of workspace members",
    long_about = "Resolve a cargo-style package selection (-p/--package, --workspace, --exclude), \
                  optionally filter it by a metadata predicate, and run a command over the result \
                  — once per member (with {name}/{spec}/{version}/{manifest} substitution) or once \
                  for the whole set (--once, with {packages})."
)]
#[expect(clippy::struct_excessive_bools, reason = "each bool is an independent clap CLI flag")]
pub(crate) struct EachArgs {
    // --- selection (mirrors cargo build) ---
    /// Select a workspace member. Repeatable. Accepts a name, a
    /// `name@version` spec, or a Unix glob (`tokio-*`).
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub(crate) packages: Vec<String>,

    /// Select every workspace member.
    #[arg(long, visible_alias = "all")]
    pub(crate) workspace: bool,

    /// Exclude a member from the selection (requires --workspace). Repeatable.
    #[arg(long, value_name = "SPEC")]
    pub(crate) exclude: Vec<String>,

    /// Explicitly select zero members (a no-op, exit 0). Replaces the CI
    /// `--skip` sentinel for empty impact tiers.
    #[arg(long)]
    pub(crate) none: bool,

    // --- filtering ---
    /// Keep only members matching this predicate. Repeatable (AND).
    /// Predicates: `lib`, `bin`, `dep:<name>`, `metadata:<key>[=<value>]`.
    #[arg(long = "filter", value_name = "PRED")]
    pub(crate) filters: Vec<String>,

    /// Drop members matching this predicate. Repeatable. Same predicate
    /// grammar as --filter; wins over --filter on conflict.
    #[arg(long = "exclude-filter", value_name = "PRED")]
    pub(crate) exclude_filters: Vec<String>,

    // --- execution ---
    /// Run the command exactly once for the whole set (skip when empty)
    /// instead of once per member. Use `{packages}` to inject the selection.
    #[arg(long)]
    pub(crate) once: bool,

    /// Run each per-package command from that member's crate root (the
    /// directory containing its Cargo.toml) instead of the current directory.
    /// Per-package mode only; cannot be combined with --once.
    #[arg(long)]
    pub(crate) chdir: bool,

    /// Run all commands even if some fail; exit non-zero if any failed.
    #[arg(long)]
    pub(crate) keep_going: bool,

    /// Print the fully-substituted commands without executing them.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Path to the workspace root Cargo.toml (default: auto-detect).
    #[arg(long, value_name = "PATH")]
    pub(crate) manifest_path: Option<PathBuf>,

    /// The command to run, after `--`. Placeholders are substituted per the
    /// selected mode.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub(crate) command: Vec<String>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_well_formed() {
        CargoCli::command().debug_assert();
    }
}
