// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-each`: run a command over a cargo-style selection of workspace
//! members.
//!
//! `cargo-each` resolves a package selection expressed with the same
//! selectors as `cargo build`, optionally narrows it with a small metadata
//! predicate language, and runs a command over the result — either once per
//! member (with placeholder substitution) or exactly once for the whole set.
//! It exists to replace hand-rolled for-each-package shell loops with a
//! single cargo-native, cross-platform command.
//!
//! # Usage
//!
//! ```text
//! cargo each [SELECTION] [FILTERS] [EXECUTION] -- <COMMAND> [ARG...]
//! ```
//!
//! Everything after `--` is the command template; `cargo-each` spawns it
//! directly (argv, not a shell string) after substituting placeholders.
//!
//! ## Selection (mirrors `cargo build`)
//!
//! - `-p` / `--package <SPEC>` — select a member. Repeatable. `SPEC` is a
//!   package name, a `name@version` spec, or a Unix glob (`tokio-*`).
//! - `--workspace` / `--all` — select every workspace member.
//! - `--exclude <SPEC>` — drop a member (with `--workspace`). Repeatable.
//! - `--none` — explicitly select zero members (a no-op that exits 0).
//!
//! When nothing is named the default is cargo `default-members`, exactly
//! like `cargo build`; pass `--workspace` for every member. A selector that
//! matches no member is an error, so typos fail loudly. A computed selection
//! (for example a CI affected-packages set) is fed in as ordinary flags via
//! shell expansion — `cargo-each` has no file or environment-variable source
//! of its own.
//!
//! ## Filters
//!
//! `--filter <PRED>` keeps only members matching `PRED`; `--exclude-filter
//! <PRED>` drops them. Both are repeatable and AND-combined
//! (`--exclude-filter` wins on conflict). Predicates:
//!
//! - `lib` / `bin` — the member has a target of that kind.
//! - `dep:<name>` — the member declares `<name>` as a dependency.
//! - `metadata:<dotted.key>` — `package.metadata.<dotted.key>` is present.
//! - `metadata:<dotted.key>=<value>` — that key equals `<value>` (numeric
//!   compare when both sides parse as a number, else string compare).
//!
//! ## Execution modes
//!
//! - *per-package* (default): run the command once per selected member, in
//!   name order, substituting the per-package placeholders below.
//! - `--once`: run the command exactly once when the set is non-empty (skip
//!   when empty), using the `{packages}` placeholder to inject the selection.
//!
//! `--keep-going` runs every invocation and exits non-zero if any failed
//! (default is fail-fast); `--chdir` runs each per-package command from that
//! member crate root (its `Cargo.toml` directory) instead of the current
//! directory — per-package mode only, so it cannot be combined with `--once`;
//! `--dry-run` prints the fully-substituted commands without running them.
//!
//! ## Placeholders
//!
//! Substituted inside each command argument:
//!
//! - `{name}` — bare package name (per-package mode).
//! - `{spec}` — `name@version` (per-package mode).
//! - `{version}` — package version (per-package mode).
//! - `{manifest}` — absolute path to the member `Cargo.toml` (per-package).
//! - `{packages}` — the cargo selection flags for the resolved set
//!   (`--workspace` for the whole workspace, else `--package name@version …`);
//!   valid only in `--once` mode and only as a standalone argument.
//!
//! Using a placeholder in the wrong mode is a usage error.
//!
//! # Behavior
//!
//! An empty resolved selection (via `--none`, or a filter that removes every
//! member) is a **successful no-op**: `cargo-each` prints a one-line note and
//! exits 0. This is what lets callers drop bespoke nothing-to-do guards.
//! Otherwise the exit code is the first failing command code (fail-fast),
//! `1` under `--keep-going` if any command failed, or `2` for a `cargo-each`
//! usage error (unknown selector, bad predicate, misused placeholder).
//!
//! # Examples
//!
//! Run a per-manifest tool over every library crate:
//!
//! ```text
//! cargo each --workspace --filter lib -- \
//!     cargo check-external-types --manifest-path {manifest}
//! ```
//!
//! Run one clippy invocation over a computed subset, skipping when it is empty:
//!
//! ```text
//! cargo each -p crate-a -p crate-b --once -- \
//!     cargo clippy {packages} --all-targets -- -D warnings
//! ```
//!
//! # Library
//!
//! The binary (`src/bin/cargo-each`) is a thin shell over this library, which
//! owns the reusable, testable spine:
//!
//! - [`Workspace`] / [`Member`] — `cargo metadata` discovery.
//! - [`Selection`] — parse selectors and resolve them against a workspace.
//! - [`Predicate`] — the `--filter` / `--exclude-filter` metadata language.
//! - [`substitute`] — placeholder expansion for the command template.
//! - [`Plan`] — the resolved list of command invocations to run.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod error;
mod filter;
mod plan;
mod select;
mod substitute;
mod workspace;

pub use error::EachError;
pub use filter::Predicate;
pub use plan::{Invocation, Mode, Plan};
pub use select::Selection;
pub use substitute::{Placeholders, substitute};
pub use workspace::{Member, Workspace};
