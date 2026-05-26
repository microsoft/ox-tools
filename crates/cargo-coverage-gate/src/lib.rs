// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-coverage-gate
//!
//! A pull-request-time gate that compares per-crate line coverage produced
//! by [`cargo-llvm-cov`] against per-crate thresholds carried in
//! `Cargo.toml`. The accompanying `cargo-coverage-gate` binary reads the
//! coverage JSON report, resolves each crate's threshold from a small
//! three-layer lookup, and emits a verdict table to stdout (and,
//! optionally, to a Markdown summary file for CI step summaries).
//!
//! The full design is in [`docs/design/main.md`] in the source tree.
//!
//! ## Threshold resolution
//!
//! For each workspace member, the effective threshold is the first match
//! among:
//!
//! 1. `[package.metadata.coverage-gate] min-lines = N` in the crate's
//!    `Cargo.toml`,
//! 2. `[workspace.metadata.coverage-gate] min-lines = N` in the workspace
//!    root `Cargo.toml`, or
//! 3. The built-in default of `100.0` — full coverage required.
//!
//! ## Public API
//!
//! The library surface is intentionally minimal: a single [`run`]
//! function that drives an evaluation end-to-end. Phase 1 of the
//! implementation only exposes the entry point as a placeholder; later
//! phases fill in JSON parsing, workspace discovery, threshold
//! resolution, attribution, and rendering.
//!
//! [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
//! [`docs/design/main.md`]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-coverage-gate/docs/design/main.md

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/favicon.ico")]
#![deny(unsafe_code)]

mod error;

pub use error::CoverageGateError;

/// Outcome of a coverage-gate evaluation.
///
/// Maps onto the process exit code: [`Verdict::Pass`] is `0`,
/// [`Verdict::Fail`] is `1`, and [`Verdict::ConfigError`] is `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every gated crate met its threshold.
    Pass,
    /// At least one gated crate fell below its threshold.
    Fail,
    /// A configuration error prevented evaluation (for example, a gated
    /// crate had no coverage data, or the JSON failed to parse).
    ConfigError,
}

impl Verdict {
    /// The process exit code associated with this verdict.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::ConfigError => 2,
        }
    }
}

/// Placeholder for the end-to-end evaluation entry point.
///
/// Returns [`CoverageGateError::NotImplemented`] until the full
/// implementation lands in later phases.
///
/// # Errors
///
/// Returns [`CoverageGateError::NotImplemented`] in the current phase.
pub fn run() -> Result<Verdict, CoverageGateError> {
    Err(CoverageGateError::NotImplemented)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(Verdict::Pass.exit_code(), 0);
        assert_eq!(Verdict::Fail.exit_code(), 1);
        assert_eq!(Verdict::ConfigError.exit_code(), 2);
    }

    #[test]
    fn run_is_unimplemented_for_now() {
        assert!(matches!(run(), Err(CoverageGateError::NotImplemented)));
    }
}
