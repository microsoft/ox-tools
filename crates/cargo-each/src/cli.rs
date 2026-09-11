// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line interface definitions for `cargo-each`.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

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
                  optionally filter it with Boolean expressions over Cargo metadata, and run a command over the result \
                  — once per member, once per matching Cargo target, or once for the whole set."
)]
#[expect(clippy::struct_excessive_bools, reason = "each bool is an independent clap CLI flag")]
pub(crate) struct EachArgs {
    // --- selection (mirrors cargo build) ---
    /// Select a workspace member. Repeatable. Accepts a name, a
    /// `name@version` spec, or a Unix glob (`tokio-*`).
    #[arg(short = 'p', long = "package", value_name = "SPEC")]
    pub(crate) packages: Vec<String>,

    /// Read package specs from a UTF-8 file, one per nonempty line.
    /// Repeatable; specs are unioned with --package.
    #[arg(long = "package-file", value_name = "PATH")]
    pub(crate) package_files: Vec<PathBuf>,

    /// Select every workspace member.
    #[arg(long, visible_alias = "all")]
    pub(crate) workspace: bool,

    /// Exclude a member from the selection (requires --workspace). Repeatable.
    #[arg(long, value_name = "SPEC", requires = "workspace")]
    pub(crate) exclude: Vec<String>,

    /// Explicitly select zero members (a no-op that exits 0).
    #[arg(long)]
    pub(crate) none: bool,

    // --- filtering ---
    /// Keep only members matching this Boolean expression. Repeatable;
    /// expressions are AND-combined. Supports `not`, `and`, `or`, and
    /// parentheses over predicates such as `target-kind:<kind>`,
    /// `publishable`, `feature:<name>`, `dep:<name>`, and
    /// `metadata:<key>[=<value>]`.
    #[arg(long = "filter", value_name = "EXPR")]
    pub(crate) filters: Vec<String>,

    /// Drop members matching this Boolean expression. Repeatable; expressions
    /// are OR-combined. Same expression grammar as --filter; exclusion wins.
    #[arg(long = "exclude-filter", value_name = "EXPR")]
    pub(crate) exclude_filters: Vec<String>,

    // --- execution ---
    /// Run the command exactly once for the whole set (skip when empty)
    /// instead of once per member. Use `{packages}` to inject the selection.
    #[arg(long)]
    pub(crate) once: bool,

    /// Run once for each selected Cargo target of this kind. Repeatable;
    /// kinds are OR-combined. Cannot be combined with --once.
    #[arg(long = "each-target", value_name = "KIND", conflicts_with = "once")]
    pub(crate) each_targets: Vec<String>,

    /// Retain targets requiring this feature. Repeatable; AND-combined.
    #[arg(long, value_name = "FEATURE", requires = "each_targets")]
    pub(crate) target_required_feature: Vec<String>,

    /// Run each per-package command from that member's crate root (the
    /// directory containing its Cargo.toml) instead of the current directory.
    /// Per-package and per-target modes only; cannot be combined with --once.
    #[arg(long)]
    pub(crate) chdir: bool,

    /// Run all commands even if some fail; exit non-zero if any failed.
    #[arg(long)]
    pub(crate) keep_going: bool,

    /// Run at most N per-package or per-target commands concurrently.
    #[arg(long, default_value_t = NonZeroUsize::MIN, value_name = "N")]
    pub(crate) jobs: NonZeroUsize,

    /// Terminate each invocation and its process tree after this duration.
    /// Requires sealed process-tree containment; unsupported hosts fail before
    /// starting the child. Accepts a positive integer followed by `ms`, `s`,
    /// or `m`.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub(crate) timeout: Option<Duration>,

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

fn parse_duration(value: &str) -> Result<Duration, String> {
    enum Unit {
        Milliseconds,
        Seconds,
        Minutes,
    }

    let (digits, unit) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, Unit::Milliseconds)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, Unit::Seconds)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, Unit::Minutes)
    } else {
        return Err("expected a positive integer followed by `ms`, `s`, or `m`".to_owned());
    };
    if digits.is_empty() {
        return Err("expected a positive integer followed by `ms`, `s`, or `m`".to_owned());
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected a positive integer followed by `ms`, `s`, or `m`".to_owned());
    }
    let amount = digits.parse::<u64>().map_err(|error| format!("duration is too large: {error}"))?;
    if amount == 0 {
        return Err("duration must be greater than zero".to_owned());
    }
    match unit {
        Unit::Milliseconds => Ok(Duration::from_millis(amount)),
        Unit::Seconds => Ok(Duration::from_secs(amount)),
        Unit::Minutes => amount
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| "duration is too large".to_owned()),
    }
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

    #[test]
    fn parses_documented_durations() {
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_mins(2)));
    }

    #[test]
    fn rejects_zero_malformed_and_overflowing_durations() {
        for value in ["0s", "1", "1h", "-1s", "1.5s", "ms", "18446744073709551615m"] {
            assert!(parse_duration(value).is_err(), "{value}");
        }
    }

    #[test]
    fn malformed_duration_uses_the_grammar_diagnostic() {
        let expected = Err("expected a positive integer followed by `ms`, `s`, or `m`".to_owned());
        assert_eq!(parse_duration("ms"), expected);
        assert_eq!(parse_duration("1.5s"), expected);
    }
}
