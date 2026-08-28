// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-coverage-gate
//!
//! A pull-request-time gate that compares per-package line coverage produced
//! by [`cargo-llvm-cov`] against per-package thresholds carried in
//! `Cargo.toml`. The accompanying `cargo-coverage-gate` binary reads the
//! coverage lcov tracefile, resolves each package's threshold from a small
//! three-layer lookup, and emits a verdict table to stdout (and,
//! optionally, to a Markdown summary file for CI step summaries).
//!
//! ## Configuration
//!
//! ### Numeric thresholds
//!
//! A workspace can define the default line-coverage threshold:
//!
//! ```toml
//! # Illustrative workspace policy.
//! [workspace.metadata.coverage-gate]
//! min-lines-percent = 80
//! ```
//!
//! Individual packages can override it:
//!
//! ```toml
//! # Illustrative package policy, intentionally stricter than the workspace.
//! [package.metadata.coverage-gate]
//! min-lines-percent = 95
//! ```
//!
//! For each workspace member, the effective threshold is the first match
//! among:
//!
//! 1. `[package.metadata.coverage-gate] min-lines-percent = N` in the package's
//!    `Cargo.toml`,
//! 2. `[workspace.metadata.coverage-gate] min-lines-percent = N` in the workspace
//!    root `Cargo.toml`, or
//! 3. The built-in default of `100.0` — full coverage required.
//!
//! Setting `min-lines-percent = 0.0` explicitly opts a package out of
//! gating: it always passes, regardless of attributed data. Thresholds must
//! be in the inclusive range `0.0..=100.0`.
//!
//! ### Packages with no coverable lines
//!
//! A package that legitimately contains no coverable lines (pure re-exports,
//! type definitions, or a thin binary shim) can make that invariant explicit:
//!
//! ```toml
//! [package.metadata.coverage-gate]
//! expect-no-coverable-lines = true
//! ```
//!
//! The gate passes only while the package has no attributed coverable lines
//! and fails as a regression if coverable code later appears. This differs
//! from `min-lines-percent = 0`, which keeps passing if the package grows
//! coverable code. The two keys are mutually exclusive, and
//! `expect-no-coverable-lines` is package-scoped only.
//!
//! ### Target-specific policies
//!
//! A package can replace that policy for a Cargo-style target selector:
//!
//! ```toml
//! [package.metadata.coverage-gate]
//! min-lines-percent = 100
//!
//! [package.metadata.coverage-gate.target.'cfg(not(windows))']
//! min-lines-percent = 0
//!
//! [package.metadata.coverage-gate.target.x86_64-unknown-linux-gnu]
//! min-lines-percent = 100
//! ```
//!
//! A target-specific no-coverable-lines assertion uses the same nesting:
//!
//! ```toml
//! [package.metadata.coverage-gate.target.thumbv7em-none-eabihf]
//! expect-no-coverable-lines = true
//! ```
//!
//! Target keys accept exact Rust target triples or quoted `cfg(...)`
//! expressions using Cargo's target grammar. A target table sets either
//! `min-lines-percent` or `expect-no-coverable-lines = true`, replacing the
//! package's base policy for that target. Exact triples take precedence over
//! matching `cfg(...)` expressions. Multiple matching cfg policies are a
//! configuration error rather than depending on declaration order.
//!
//! A zero target-specific threshold disables gating on the matching target,
//! but does not disable test execution or instrumentation. Those test binaries
//! remain instrumented because they may contribute coverage to other packages.
//! If cargo-llvm-cov reports that an instrumented run produced no coverage
//! data, automation can supply an empty lcov tracefile: zero-threshold and
//! `expect-no-coverable-lines` packages pass, while positively gated packages
//! report `NO DATA`.
//!
//! ## Why lcov, not the JSON?
//!
//! `cargo-llvm-cov` exports the same instrumentation run in several
//! formats (JSON, lcov, cobertura, codecov-custom-JSON). The gate
//! consumes lcov because that is what every other coverage report fed by
//! the same data sees: Codecov ingests lcov uploads directly, ADO
//! consumes cobertura that cargo-llvm-cov derives from lcov, and the
//! lcov line semantics ("a line is covered if any region on it was
//! hit") match the human reading of "did we hit this line". The JSON
//! export uses a stricter "every region on the line must be hit"
//! interpretation that systematically reports a couple of
//! percentage-points lower, which makes calibrating thresholds against
//! Codecov / ADO numbers confusing.
//!
//! ## Binary usage
//!
//! ```text
//! cargo coverage-gate  [--lcov <path>]... [-p|--package <spec>]...
//!                      [--target <triple>]
//!                      [--summary-file <path>] [--quiet]
//! ```
//!
//! `--lcov` may be repeated; the tracefiles are merged at the line level
//! (per-line counts summed) so multiple feature-config exports
//! (`--all-features`, `--no-default-features`) can be gated together
//! without a separate, platform-specific merge step.
//!
//! Exit codes: `0` if every gated package meets its threshold, `1` if any
//! gated package falls below its threshold, and `2` for configuration
//! errors (unparseable lcov, missing data for a gated package, a `--package`
//! selector that matches no member, an out-of-range `min-lines-percent`
//! value, …).
//!
//! When `--summary-file` is unset, the binary falls back to
//! `$GITHUB_STEP_SUMMARY` and then `$COVERAGE_GATE_SUMMARY` to decide
//! where to write the Markdown verdict table.
//!
//! ## Library usage
//!
//! ```no_run
//! use std::io;
//!
//! let lcov = std::fs::read_to_string("target/coverage/lcov.info")?;
//! let report = cargo_coverage_gate::evaluate(&lcov, None, &[])?;
//! report.render_text(&mut io::stdout())?;
//! let code = report.verdict().as_exit_code();
//! # let _ = code;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Public API
//!
//! [`evaluate`] gates one lcov tracefile for the rustc host target, while
//! [`evaluate_many`] merges multiple tracefiles at line level.
//! [`evaluate_many_for_target`] evaluates an explicit Rust target triple.
//! Evaluation returns an [`EvaluatedReport`], which renders as plain
//! text via [`EvaluatedReport::render_text`] or GitHub-flavored Markdown via
//! [`EvaluatedReport::render_markdown`] and reduces to a [`Verdict`] via
//! [`EvaluatedReport::verdict`]. The accompanying binary loads tracefiles from
//! disk and orchestrates rendering plus the appropriate exit code.
//!
//! [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov

#![doc(html_logo_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/logo.png")]
#![doc(
    html_favicon_url = "https://media.githubusercontent.com/media/microsoft/ox-tools/refs/heads/main/crates/cargo-coverage-gate/favicon.ico"
)]
#![deny(unsafe_code)]

use std::io;
use std::path::Path;

mod aggregate;
mod attribute;
mod error;
mod lcov_cov;
mod render;
mod target;
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
    /// Every gated package met its threshold.
    Pass,
    /// At least one gated package fell below its threshold.
    Fail,
    /// A configuration error prevented evaluation (for example, a gated
    /// package had no coverage data, or the lcov tracefile failed to parse).
    ConfigError,
}

impl Verdict {
    /// The process exit code associated with this verdict.
    #[must_use]
    pub fn as_exit_code(self) -> i32 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::ConfigError => 2,
        }
    }
}

/// An evaluated coverage report.
///
/// Produced by [`evaluate`]; can be rendered as either a fixed-width plain
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

    /// Number of source files in the lcov tracefile whose path did not
    /// match any workspace member. Such files are dropped from the
    /// per-package aggregation; this count surfaces them as a single
    /// aggregated warning rather than per-file noise.
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

/// Evaluate `lcov_text` (a [`cargo-llvm-cov`] lcov tracefile) against
/// the workspace anchored at `manifest_path` and return the resolved
/// [`EvaluatedReport`].
///
/// `gated_packages` restricts the operation to a named subset; when
/// empty, every workspace member is in scope.
///
/// # Errors
///
/// Returns a [`CoverageGateError`] when the tracefile does not parse,
/// workspace discovery fails, a package selector is unknown, coverage
/// metadata or target policy is invalid or ambiguous, a configured threshold
/// is out of range, or required Rust target discovery or cfg queries fail.
/// The error message identifies which case occurred; callers usually just
/// propagate it.
///
/// [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
pub fn evaluate(lcov_text: &str, manifest_path: Option<&Path>, gated_packages: &[String]) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_many(std::slice::from_ref(&lcov_text), manifest_path, gated_packages)
}

/// Evaluate one or more [`cargo-llvm-cov`] lcov tracefiles against the
/// workspace anchored at `manifest_path` and return the resolved
/// [`EvaluatedReport`].
///
/// The tracefiles are merged at the line level before evaluation (per-line
/// counts summed, line sets combined), so passing the `--all-features` and
/// `--no-default-features` exports yields the same per-package line
/// coverage as a single merged report — without a platform-specific lcov
/// merger. An empty slice is treated as an empty report. Packages with
/// positive thresholds then report NO DATA, which rolls the overall result
/// up to [`Verdict::ConfigError`] (process exit code 2). Packages with zero
/// thresholds pass, while `expect-no-coverable-lines` packages report EMPTY
/// and pass.
///
/// `gated_packages` restricts the operation to a named subset; when
/// empty, every workspace member is in scope.
///
/// # Errors
///
/// Returns a [`CoverageGateError`] under the same conditions as
/// [`evaluate`]; additionally, any tracefile that fails to parse aborts
/// the merge.
///
/// [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
#[inline]
pub fn evaluate_many(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_many_for_target(lcov_texts, manifest_path, gated_packages, None)
}

/// Evaluate one or more lcov tracefiles for an explicit Rust target.
///
/// `target` is a Rust target triple such as `x86_64-pc-windows-msvc`.
/// When omitted, the rustc host target is used. Target-specific
/// package policy is resolved before the gated package set is evaluated.
///
/// # Errors
///
/// Returns a [`CoverageGateError`] under the same conditions as
/// [`evaluate_many`]. When target policy requires Rust target resolution,
/// rustc launch, exit-status, host-output, and cfg-output failures are also
/// reported.
pub fn evaluate_many_for_target(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<EvaluatedReport, CoverageGateError> {
    let report = lcov_cov::CoverageReport::from_strs(lcov_texts)?;
    let ws = workspace::Workspace::load(manifest_path, target)?;
    let inner = verdict::evaluate(&report, &ws, gated_packages)?;
    Ok(EvaluatedReport { inner })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(Verdict::Pass.as_exit_code(), 0);
        assert_eq!(Verdict::Fail.as_exit_code(), 1);
        assert_eq!(Verdict::ConfigError.as_exit_code(), 2);
    }

    #[test]
    fn evaluate_rejects_malformed_lcov() {
        let err = evaluate("not lcov", None, &[]).expect_err("malformed lcov must error");
        assert!(err.to_string().contains("lcov tracefile"));
    }

    #[test]
    fn evaluated_report_unattributed_count_round_trips() {
        // Construct an EvaluatedReport whose inner Report has a known
        // unattributed count, then verify the public accessor returns it.
        let inner = verdict::Report {
            outcomes: Vec::new(),
            unattributed: 3,
        };
        let report = EvaluatedReport { inner };
        assert_eq!(report.unattributed_count(), 3);
    }

    #[test]
    fn evaluated_report_unattributed_count_zero_for_empty() {
        let inner = verdict::Report {
            outcomes: Vec::new(),
            unattributed: 0,
        };
        let report = EvaluatedReport { inner };
        assert_eq!(report.unattributed_count(), 0);
    }
}
