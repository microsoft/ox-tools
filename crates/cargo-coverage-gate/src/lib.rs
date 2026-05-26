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
//! The library exposes [`evaluate`], which returns an
//! [`EvaluatedReport`]. The report can be rendered as plain text via
//! [`EvaluatedReport::render_text`] or as GitHub-flavored Markdown
//! via [`EvaluatedReport::render_markdown`], and reduced to a single
//! [`Verdict`] via [`EvaluatedReport::verdict`]. The accompanying
//! binary loads the JSON from disk and orchestrates rendering plus
//! the appropriate exit code.
//!
//! [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
//! [`docs/design/main.md`]: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-coverage-gate/docs/design/main.md

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/logo.png")]
#![doc(html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/favicon.ico")]
#![deny(unsafe_code)]

use std::io;
use std::path::Path;

mod aggregate;
mod attribute;
mod error;
mod llvm_cov;
mod render;
mod threshold;
mod verdict;
mod workspace;

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

/// An evaluated coverage report.
///
/// Produced by [`evaluate`]; renderable to either a fixed-width plain
/// text table or a GitHub-flavored Markdown table, and reducible to a
/// single [`Verdict`].
#[derive(Debug)]
pub struct EvaluatedReport {
    inner: verdict::Report,
}

impl EvaluatedReport {
    /// The overall verdict for this evaluation.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.inner.verdict()
    }

    /// Number of coverage entries whose file paths did not match any
    /// workspace member. These files are dropped from the aggregation.
    #[must_use]
    pub fn unattributed_count(&self) -> usize {
        self.inner.unattributed
    }

    /// Render the verdict table as plain text to `out`.
    ///
    /// # Errors
    ///
    /// Returns whatever IO error `out` produces.
    pub fn render_text(&self, out: &mut dyn io::Write) -> io::Result<()> {
        render::text::render(out, &self.inner)
    }

    /// Render the verdict table as GitHub-flavored Markdown to `out`.
    ///
    /// # Errors
    ///
    /// Returns whatever IO error `out` produces.
    pub fn render_markdown(&self, out: &mut dyn io::Write) -> io::Result<()> {
        render::markdown::render(out, &self.inner)
    }
}

/// Evaluate `json_text` (a `cargo-llvm-cov` JSON v2 report) against
/// the workspace anchored at `manifest_path` and return the resolved
/// [`EvaluatedReport`].
///
/// `gated_crates` restricts the operation to a named subset; when
/// empty, every workspace member is in scope.
///
/// # Errors
///
/// Returns [`CoverageGateError::JsonParse`] if `json_text` does not
/// parse, [`CoverageGateError::Metadata`] for workspace-discovery
/// failures or unknown crate names in `gated_crates`, and
/// [`CoverageGateError::InvalidThreshold`] if a configured
/// `min-lines` value is outside `[0.0, 100.0]`.
pub fn evaluate(
    json_text: &str,
    manifest_path: Option<&Path>,
    gated_crates: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    let report = llvm_cov::CoverageReport::from_str(json_text)?;
    let ws = workspace::Workspace::load(manifest_path)?;
    let inner = verdict::evaluate(&report, &ws, gated_crates)?;
    Ok(EvaluatedReport { inner })
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
    fn evaluate_rejects_malformed_json() {
        let err = evaluate("not json", None, &[]).expect_err("malformed JSON must error");
        assert!(matches!(err, CoverageGateError::JsonParse { .. }));
    }
}
