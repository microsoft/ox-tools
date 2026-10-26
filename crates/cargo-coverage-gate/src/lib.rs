// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # cargo-coverage-gate
//!
//! A pull-request-time gate that compares per-package line coverage produced
//! by [`cargo-llvm-cov`] against per-package thresholds carried in
//! `Cargo.toml`. The accompanying `cargo-coverage-gate` binary can either read
//! existing LCOV tracefiles or collect them through a portable
//! cargo-llvm-cov and
//! nextest. It resolves each package's base policy from a small three-layer
//! lookup, applies any matching package target policy, and emits a verdict
//! table to stdout (and,
//! optionally, to a Markdown summary file for CI step summaries). A failing
//! verdict includes actionable details without relying on a later
//! coverage-service upload. A coverable line is a distinct LCOV `DA:` record.
//! Before aggregation, the CLI and released-surface library APIs filter each
//! tracefile using provable Cargo source and test-only boundaries.
//! Test, example, bench, build-script, and other non-release target sources
//! are removed when their non-release ownership is established. Within a
//! released source file, only items whose `cfg`
//! semantics require `test` (plus ordinary `#[test]`/`#[bench]` functions) are
//! removed; feature-gated `test-util` APIs remain measured when they can compile
//! in a non-test build. Local Cargo target `required-features` are treated as
//! enabled at the target root, so code that requires one of those features to
//! be absent is not part of that target's released surface. Dependency
//! selectors such as `dep/feature`, `dep?/feature`, and `dep:dep` do not create
//! local `cfg(feature = ...)` atoms. Dependency-feature availability does not
//! prove local-feature activation, even for optional or renamed dependencies
//! exposing a same-named local feature: another feature may activate the
//! dependency through `dep:dep`.
//!
//! A recognizable production `include!` makes indirect source ownership
//! uncertain. Filtering then warns on stderr, removes only file-local
//! test-only ranges, and retains ambiguous counters, including zero-hit
//! production lines. Such output is conservative, not complete production-only
//! classification. Ordinary authored `#[cfg(test)]` blocks still need no
//! annotations. Exceptional helpers can use the existing nightly
//! `#[cfg_attr(coverage_nightly, coverage(off))]` attribute when appropriate;
//! enable `#![cfg_attr(coverage_nightly, feature(coverage_attribute))]` in the
//! containing crate. This does not expand macros, classify generated tests,
//! or recover sources omitted from the input LCOV.
//!
//! Compiler-library records that escape upstream filename exclusions are
//! removed only after canonical source paths establish that they are beneath
//! the library-source tree reported by the selected `RUSTC --print sysroot`.
//! Cargo-owned project sources take precedence, including explicit sources
//! outside their package directory. Unknown project or dependency sources
//! still fail; directory names alone never establish compiler ownership.
//! Numeric failures show exact covered/coverable counts and uncovered ranges;
//! `expect-no-coverable-lines` failures show the unexpected coverable ranges;
//! and `NO DATA` explains that no records were attributed. Location lists are
//! bounded, with an exact count of omitted locations.
//!
//! ## 0.7 migration
//!
//! Version 0.7 intentionally changes [`evaluate`], [`evaluate_many`], and
//! [`evaluate_many_for_target`] to evaluate the released Cargo surface rather
//! than every attributed LCOV line. Existing callers that deliberately include
//! tests or other non-release targets should migrate to the corresponding
//! `evaluate_unfiltered*` function. The released-surface APIs also treat each
//! target's local Cargo `required-features` as enabled when classifying its
//! source. Dependency-qualified requirements do not imply local cfg features.
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
//! For each workspace member, the base threshold is the first match
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
//! ### Qualified test attributes
//!
//! Compiler-owned `#[test]` and `#[bench]` attributes are recognized
//! intrinsically. Qualified framework attributes are exact workspace policy,
//! never inferred from a final `test`/`bench` path segment:
//!
//! ```toml
//! [workspace.metadata.coverage-gate]
//! test-attributes = ["my_runtime::test"]
//! ```
//!
//! The list has no qualified defaults and cannot be set per package. Paths
//! are accepted if valid in any supported Rust edition, so mixed-edition
//! workspaces can list older attribute names without raw-identifier spelling.
//!
//! ### Target-specific policies
//!
//! A package can replace its base policy for a Cargo-style target selector:
//!
//! ```toml
//! [package.metadata.coverage-gate]
//! min-lines-percent = 100
//!
//! [package.metadata.coverage-gate.target.'cfg(not(windows))']
//! expect-no-coverable-lines = true
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
//! Target tables are package-scoped; they are invalid in workspace metadata.
//! Their keys accept exact Rust target triples or quoted `cfg(...)` expressions
//! using the target-derived subset of Cargo's target grammar. Target
//! configuration options such as `windows`, `unix`, `target_os`, and
//! `target_arch` are supported. Build-context options such as `feature`, `test`,
//! `debug_assertions`, and `proc_macro` are rejected because a standalone target
//! query cannot evaluate them. A selected target table sets either
//! `min-lines-percent` or `expect-no-coverable-lines = true`, completely
//! replacing the package's base policy to produce its effective policy. Exact
//! triples take precedence over matching `cfg(...)` expressions. Multiple
//! matching cfg policies are a configuration error rather than depending on
//! declaration order.
//!
//! A zero target-specific threshold disables gating on the matching target,
//! but does not disable test execution or instrumentation. Those test binaries
//! remain instrumented because they may contribute coverage to other packages.
//! When cargo-llvm-cov discovers only objects with no coverage maps, `run`
//! writes and evaluates an empty lcov tracefile: zero-threshold and
//! `expect-no-coverable-lines` packages pass, while positively gated packages
//! report `NO DATA`.
//!
//! ## Why lcov, not the JSON?
//!
//! `cargo-llvm-cov` exports the same instrumentation run in several
//! formats (JSON, lcov, cobertura, codecov-custom-JSON). The gate
//! consumes lcov because that is what every other coverage report fed by
//! the same data sees: Codecov and ADO ingest lcov uploads directly, and the
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
//! cargo coverage-gate [EVALUATION OPTIONS]
//! cargo coverage-gate run [SELECTION] [COLLECTION OPTIONS] [--summary-file <PATH>] [--quiet]
//! ```
//!
//! The bare command retains its existing CLI shape while filtering existing
//! LCOV files to the released surface before evaluation.
//! `cargo coverage-gate run` collects `all-features` and
//! `no-default-features` coverage with cargo-llvm-cov plus nextest by default,
//! writes released-surface-filtered `lcov-all-features.info` and
//! `lcov-no-default.info` under `target/coverage`, and evaluates those exact
//! files in-process. CI integrations can upload those same artifacts to
//! Codecov or Azure DevOps. Collection can
//! be limited with repeatable `--package` selectors; no selectors means the
//! workspace. Use repeatable `--configuration`, `--coverage-dir`, and `--jobs`
//! options to customize collection.
//!
//! Instrumented collection requires the invoking Cargo and rustc to be nightly
//! and cargo-llvm-cov 0.9.0 or newer. The command honors inherited `CARGO`,
//! `RUSTC`, `RUSTUP_TOOLCHAIN`, and `PATH` rather than selecting a toolchain
//! itself. Every selected package is instrumented regardless of its coverage
//! policy. Callers may repeat `--no-coverage-target <TRIPLE>` to explicitly run
//! plain nextest without LCOV or gating on listed targets. `run` resolves one
//! effective target from explicit `--target` or the rustc host, then passes it
//! explicitly to test execution, reporting, and evaluation. This deliberately
//! overrides `CARGO_BUILD_TARGET` and Cargo `build.target` configuration.
//! `--quiet` suppresses all collection and verdict stdout while preserving
//! stderr diagnostics and summary output.
//!
//! Every feature configuration uses a fresh target beneath private
//! per-invocation scratch, so collection never runs workspace-wide
//! `cargo llvm-cov clean` or deletes shared report and test artifacts. Ordinary
//! Cargo and cargo-llvm-cov target variables point at that same private
//! configuration directory, including metadata-derived test paths. Raw reports
//! are written inside that scratch space, released-surface filtered, staged
//! beside their stable `--coverage-dir` paths, and atomically published only
//! after filtering succeeds. On Windows error 206, the report child uses
//! deterministic Windows quoting and retries the validated `llvm-cov export`
//! arguments through a response file in the same scratch space before entering
//! that publication pipeline. Concurrent collection must use distinct coverage
//! directories; private profile isolation does not serialize stable artifact
//! writers.
//!
//! In standalone evaluation, `--lcov` may be repeated; each tracefile is released-surface filtered and
//! then the tracefiles are merged at the line level
//! (line sets combined, coverage retained when any input has a hit) so multiple feature-config exports
//! (`--all-features`, `--no-default-features`) can be gated together
//! without a separate, platform-specific merge step. `run` rejects `--lcov`;
//! its collected paths are selected by `--coverage-dir`.
//!
//! Exit codes: `0` if every gated package meets its threshold, `1` if any
//! gated package falls below its threshold, and `2` for configuration or
//! operational errors (unparseable lcov, an unreadable/unparseable source or
//! source path with no Cargo target owner, stale LCOV line numbers, missing data for a gated package, a
//! `--package` selector that matches no member, failed collection/export, an
//! out-of-range `min-lines-percent` value, …).
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
//! [`filter_lcov`] and [`filter_lcov_many`] produce released-surface LCOV
//! artifacts without evaluating policy. [`evaluate`], [`evaluate_many`], and
//! [`evaluate_many_for_target`] apply that filtering before evaluating policy.
//! Callers that intentionally need unfiltered attribution use the explicitly
//! named `evaluate_unfiltered*` functions.
//! Evaluation returns an [`EvaluatedReport`], which renders as plain
//! text via [`EvaluatedReport::render_text`] or GitHub-flavored Markdown via
//! [`EvaluatedReport::render_markdown`] and reduces to a [`Verdict`] via
//! [`EvaluatedReport::verdict`]. The accompanying binary loads tracefiles from
//! disk or collects it, then orchestrates rendering plus the appropriate exit
//! code.
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
mod package_glob;
mod render;
mod source_filter;
mod target;
mod threshold;
mod verdict;
mod workspace;

pub use error::CoverageGateError;

/// Filter one LCOV tracefile using provable Cargo and test-only boundaries.
///
/// Cargo target roots and out-of-line module declarations establish source
/// ownership. Files reachable only from tests, examples, benches, build
/// scripts, or other non-release targets are removed when their ownership is
/// established. Within released files,
/// line records are removed only when Rust syntax proves they are test-only
/// (`#[test]`, `#[bench]`, `#[cfg(test)]`, a `cfg` predicate that requires
/// `test`, or an exact qualified attribute listed in workspace metadata).
/// Function and branch records are omitted because the proof is line-granular.
///
/// When a recognizable production `include!` makes indirect ownership
/// uncertain, filtering warns on stderr, retains ambiguous source counters,
/// and excludes only file-local test-only ranges. This conservative result
/// is not complete production-only classification. Sources without a known owner
/// and unrelated path, parse, checksum, and line-validation failures remain errors.
/// Macro expansion and counters absent from the input are not inferred.
///
/// Otherwise unknown records outside workspace member directories are excluded
/// only when their canonical paths are beneath the selected compiler's verified
/// `lib/rustlib/src/rust/library` tree. Discovery honors `RUSTC` and the workspace
/// working directory and is performed at most once per report batch, only when
/// needed. Missing source files or failed compiler queries remain errors;
/// an absent optional compiler source tree does not authorize any exclusion.
///
/// This is a workspace operation, not a pure text transform. It runs
/// `cargo metadata --no-deps`, reads and parses reachable Rust sources, and
/// resolves relative `SF:` paths against the Cargo workspace root. When
/// `manifest_path` is `None`, Cargo discovers the workspace from the current
/// directory. The returned LCOV is parsed and serialized canonically, so
/// retained `SF:` paths are normalized to the owning member path reported by
/// Cargo metadata, and record ordering and summary records may differ from the
/// input.
///
/// Qualified framework test macros are configured explicitly:
///
/// ```toml
/// [workspace.metadata.coverage-gate]
/// test-attributes = ["my_runtime::test"]
/// ```
///
/// There are no qualified default attributes; unknown `*::test` macros remain
/// measured.
///
/// The result is already released-surface filtered. To evaluate that returned
/// text without repeating source analysis, pass it to [`evaluate_unfiltered`]
/// (or its multi-input counterpart).
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
///
/// let raw = std::fs::read_to_string("target/coverage/lcov.info")?;
/// let filtered = cargo_coverage_gate::filter_lcov(&raw, Some(Path::new("Cargo.toml")))?;
/// std::fs::write("target/coverage/lcov-released.info", filtered)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns [`CoverageGateError`] when Cargo metadata cannot be loaded, the
/// LCOV input is malformed, any source reachable from a Cargo target root
/// cannot be resolved/read/parsed, target ownership cannot be determined, or
/// LCOV line data does not match the current source tree. Compiler-source
/// discovery failures are also returned when provenance is needed.
#[cfg_attr(coverage_nightly, coverage(off))]
// This adapter only selects the real metadata loader; the injected implementation below is mutation-covered.
#[mutants::skip]
pub fn filter_lcov(lcov_text: &str, manifest_path: Option<&Path>) -> Result<String, CoverageGateError> {
    filter_lcov_with_metadata_loader(lcov_text, manifest_path, workspace::load_metadata)
}

/// Filter multiple independent tracefiles using one Cargo target/source
/// analysis pass.
///
/// The returned vector preserves input order and length.
/// An empty input returns an empty vector without workspace discovery.
///
/// # Errors
///
/// For non-empty input, returns [`CoverageGateError`] under the same conditions
/// as [`filter_lcov`]; any failing input aborts the operation.
pub fn filter_lcov_many(lcov_texts: &[&str], manifest_path: Option<&Path>) -> Result<Vec<String>, CoverageGateError> {
    filter_lcov_many_with_metadata_loader(lcov_texts, manifest_path, workspace::load_metadata)
}

fn filter_lcov_with_metadata_loader(
    lcov_text: &str,
    manifest_path: Option<&Path>,
    load_metadata: impl FnOnce(Option<&Path>) -> Result<cargo_metadata::Metadata, CoverageGateError>,
) -> Result<String, CoverageGateError> {
    let mut filtered = filter_lcov_many_with_metadata_loader(&[lcov_text], manifest_path, load_metadata)?;
    Ok(filtered.pop().expect("one LCOV input always produces one filtered output"))
}

fn filter_lcov_many_with_metadata_loader(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    load_metadata: impl FnOnce(Option<&Path>) -> Result<cargo_metadata::Metadata, CoverageGateError>,
) -> Result<Vec<String>, CoverageGateError> {
    let reports = source_filter::parse_reports(lcov_texts)?;
    if reports.is_empty() {
        return Ok(Vec::new());
    }
    let metadata = load_metadata(manifest_path)?;
    source_filter::filter_reports_with_metadata(reports, &metadata)
}

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

    /// Number of source files whose path was not attributed to a workspace
    /// member.
    ///
    /// Released-surface evaluation rejects paths outside every workspace
    /// member before aggregation, so its successful reports return zero.
    /// Unfiltered evaluation may return a nonzero count.
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

/// Filter and evaluate one LCOV tracefile for the rustc host target.
///
/// This applies the same released-surface rules as [`filter_lcov`] before
/// policy evaluation.
///
/// `gated_packages` restricts the operation to a named subset; when
/// empty, every workspace member is in scope. Package selection limits policy
/// evaluation, not released-surface discovery: source graphs for every
/// workspace member are validated before filtering.
///
/// # Errors
///
/// Returns [`CoverageGateError`] when LCOV parsing, Cargo metadata, source
/// resolution/reading/parsing, module ownership, line validation, target
/// policy resolution, package selection, or verdict evaluation fails.
///
/// [`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
pub fn evaluate(lcov_text: &str, manifest_path: Option<&Path>, gated_packages: &[String]) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_many(std::slice::from_ref(&lcov_text), manifest_path, gated_packages)
}

/// Filter and evaluate multiple LCOV tracefiles for the rustc host target.
///
/// Inputs are filtered independently, then merged at line level. An empty
/// slice represents an empty coverage report but still validates workspace
/// policy and package selection.
///
/// `gated_packages` restricts the operation to a named subset; when
/// empty, every workspace member is in scope. Package selection limits policy
/// evaluation, not released-surface discovery: source graphs for every
/// workspace member are validated before filtering.
///
/// # Errors
///
/// Returns [`CoverageGateError`] under the same conditions as [`evaluate`];
/// any malformed input aborts the operation.
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

/// Filter and evaluate multiple LCOV tracefiles for a selected Rust target.
///
/// `target` is a Rust target triple such as `x86_64-pc-windows-msvc`.
/// When omitted, target policy uses the rustc host when target resolution is
/// required.
///
/// One `cargo metadata --no-deps` snapshot supplies both source ownership and
/// coverage policy, so filtering and evaluation cannot observe different
/// manifests.
///
/// # Errors
///
/// Returns [`CoverageGateError`] under the same conditions as [`evaluate_many`].
/// Target-resolution failures are also reported when target policy requires a
/// Rust target.
pub fn evaluate_many_for_target(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_many_for_target_with_metadata_loader(lcov_texts, manifest_path, gated_packages, target, workspace::load_metadata)
}

fn evaluate_many_for_target_with_metadata_loader(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
    load_metadata: impl FnOnce(Option<&Path>) -> Result<cargo_metadata::Metadata, CoverageGateError>,
) -> Result<EvaluatedReport, CoverageGateError> {
    let reports = source_filter::parse_reports(lcov_texts)?;
    let metadata = load_metadata(manifest_path)?;
    evaluate_reports_with_metadata(reports, &metadata, gated_packages, target)
}

fn evaluate_reports_with_metadata(
    reports: Vec<lcov::Report>,
    metadata: &cargo_metadata::Metadata,
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<EvaluatedReport, CoverageGateError> {
    let filtered = source_filter::filter_reports_with_metadata(reports, metadata)?;
    let filtered_refs = filtered.iter().map(String::as_str).collect::<Vec<_>>();
    let report = lcov_cov::CoverageReport::from_strs(&filtered_refs)?;
    let ws = workspace::Workspace::from_metadata(metadata, target)?;
    let inner = verdict::evaluate(&report, &ws, gated_packages)?;
    Ok(EvaluatedReport { inner })
}

/// Evaluate one LCOV tracefile without released-surface filtering.
///
/// This escape hatch preserves every LCOV source that can be attributed by
/// path, including test targets and test-only regions.
///
/// # Errors
///
/// Returns [`CoverageGateError`] when LCOV parsing, Cargo metadata, target
/// policy resolution, package selection, or verdict evaluation fails.
pub fn evaluate_unfiltered(
    lcov_text: &str,
    manifest_path: Option<&Path>,
    gated_packages: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_unfiltered_many(std::slice::from_ref(&lcov_text), manifest_path, gated_packages)
}

/// Evaluate multiple LCOV tracefiles without released-surface filtering.
///
/// Inputs are merged at line level. An empty slice represents an empty report.
///
/// # Errors
///
/// Returns [`CoverageGateError`] under the same conditions as
/// [`evaluate_unfiltered`]; any malformed input aborts the operation.
pub fn evaluate_unfiltered_many(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_unfiltered_many_for_target(lcov_texts, manifest_path, gated_packages, None)
}

/// Evaluate multiple unfiltered LCOV tracefiles for a selected Rust target.
///
/// # Errors
///
/// Returns [`CoverageGateError`] under the same conditions as
/// [`evaluate_unfiltered_many`]. Target-resolution failures are also reported
/// when target policy requires a Rust target.
pub fn evaluate_unfiltered_many_for_target(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_unfiltered_many_for_target_with_metadata_loader(lcov_texts, manifest_path, gated_packages, target, workspace::load_metadata)
}

fn evaluate_unfiltered_many_for_target_with_metadata_loader(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
    load_metadata: impl FnOnce(Option<&Path>) -> Result<cargo_metadata::Metadata, CoverageGateError>,
) -> Result<EvaluatedReport, CoverageGateError> {
    let report = lcov_cov::CoverageReport::from_strs(lcov_texts)?;
    let metadata = load_metadata(manifest_path)?;
    let ws = workspace::Workspace::from_metadata(&metadata, target)?;
    let inner = verdict::evaluate(&report, &ws, gated_packages)?;
    Ok(EvaluatedReport { inner })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture_metadata() -> cargo_metadata::Metadata {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source-index");
        let member = root.join("member");
        let package_id = "path+file:///coverage-public-wrapper#member@0.1.0";
        serde_json::from_value(json!({
            "packages": [{
                "name": "member",
                "version": "0.1.0",
                "authors": [],
                "id": package_id,
                "source": null,
                "description": null,
                "dependencies": [],
                "license": null,
                "license_file": null,
                "targets": [{
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "member",
                    "src_path": member.join("src/lib.rs"),
                    "edition": "2024",
                    "doc": true,
                    "doctest": true,
                    "test": true
                }],
                "features": {},
                "manifest_path": member.join("Cargo.toml"),
                "categories": [],
                "keywords": [],
                "readme": null,
                "repository": null,
                "homepage": null,
                "documentation": null,
                "edition": "2024",
                "metadata": {"coverage-gate": {"min-lines-percent": 100}},
                "links": null,
                "publish": null,
                "default_run": null,
                "rust_version": null
            }],
            "workspace_members": [package_id],
            "workspace_default_members": [package_id],
            "resolve": null,
            "workspace_root": root,
            "target_directory": root.join("target"),
            "build_directory": root.join("target"),
            "metadata": {},
            "version": 1
        }))
        .expect("fixture metadata parses")
    }

    fn assert_unknown_selector(error: &CoverageGateError, expected: &str) {
        let cause = std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |source| source.source())
            .find_map(|source| source.downcast_ref::<crate::error::UnknownPackageSelectorError>())
            .expect("unknown selector cause");
        assert_eq!(cause.selector, expected);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(Verdict::Pass.as_exit_code(), 0);
        assert_eq!(Verdict::Fail.as_exit_code(), 1);
        assert_eq!(Verdict::ConfigError.as_exit_code(), 2);
    }

    #[test]
    fn evaluate_rejects_malformed_lcov() {
        let err = evaluate("not lcov", Some(Path::new("missing-workspace/Cargo.toml")), &[])
            .expect_err("malformed LCOV must fail before workspace discovery");
        assert!(err.to_string().contains("lcov tracefile"));
        assert!(!err.to_string().contains("workspace metadata"));

        let err = evaluate_unfiltered("not lcov", None, &[]).expect_err("unfiltered evaluation also rejects malformed LCOV");
        assert!(err.to_string().contains("lcov tracefile"));
    }

    #[test]
    fn empty_filter_batch_skips_workspace_discovery() {
        let filtered =
            filter_lcov_many(&[], Some(Path::new("missing-workspace/Cargo.toml"))).expect("empty filtering is a documented no-op");
        assert!(filtered.is_empty());
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

    #[test]
    #[cfg_attr(miri, ignore = "canonicalizes checked-in fixture paths, which Miri isolation forbids")]
    fn public_wrappers_forward_manifest_and_package_selectors_in_process() {
        let manifest = Path::new("/synthetic/workspace/Cargo.toml");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/source-index/member/src/lib.rs");
        let lcov = format!("TN:\nSF:{}\nDA:4,1\nend_of_record\n", source.display());
        let target = Some("x86_64-unknown-linux-gnu");

        let filtered = filter_lcov_with_metadata_loader(&lcov, Some(manifest), |actual| {
            assert_eq!(actual, Some(manifest));
            Ok(fixture_metadata())
        })
        .expect("filter wrapper succeeds");
        let filtered = source_filter::parse_reports(&[&filtered]).expect("filtered LCOV parses");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].sections.len(), 1);

        let selected = ["member".to_owned()];
        let evaluated = evaluate_many_for_target_with_metadata_loader(&[&lcov], Some(manifest), &selected, target, |actual| {
            assert_eq!(actual, Some(manifest));
            Ok(fixture_metadata())
        })
        .expect("filtered evaluation wrapper succeeds");
        assert_eq!(evaluated.verdict(), Verdict::Pass);

        let unknown = ["missing-member".to_owned()];
        let error = evaluate_many_for_target_with_metadata_loader(&[&lcov], Some(manifest), &unknown, target, |_| Ok(fixture_metadata()))
            .expect_err("filtered wrapper forwards package selectors");
        assert_unknown_selector(&error, "missing-member");

        let error = evaluate_unfiltered_many_for_target_with_metadata_loader(&[&lcov], Some(manifest), &unknown, target, |actual| {
            assert_eq!(actual, Some(manifest));
            Ok(fixture_metadata())
        })
        .expect_err("unfiltered wrapper forwards package selectors");
        assert_unknown_selector(&error, "missing-member");
    }
}
