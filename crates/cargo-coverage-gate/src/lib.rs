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
//! tracefile to the source surface that can ship from Cargo library and binary
//! targets. Test, example, bench, build-script, and other non-release target
//! sources are removed. Within a released source file, only items whose `cfg`
//! semantics require `test` (plus ordinary `#[test]`/`#[bench]` functions) are
//! removed; feature-gated `test-util` APIs remain measured when they can compile
//! in a non-test build.
//! Numeric failures show exact covered/coverable counts and uncovered ranges;
//! `expect-no-coverable-lines` failures show the unexpected coverable ranges;
//! and `NO DATA` explains that no records were attributed. Location lists are
//! bounded, with an exact count of omitted locations.
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
//! cargo coverage-gate run [SELECTION] [COLLECTION OPTIONS] [EVALUATION OPTIONS]
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
//! configuration directory, including metadata-derived test paths. Ordinary
//! reports write directly to their stable `--coverage-dir` path. On Windows
//! error 206, the report child uses deterministic Windows quoting and the
//! validated `llvm-cov export` arguments are retried through a response file.
//! That exceptional export is staged beside the stable path and published only
//! after success or a valid no-data conversion. Concurrent collection must use
//! distinct coverage directories; private profile isolation does not serialize
//! stable artifact writers.
//!
//! `--lcov` may be repeated; each tracefile is released-surface filtered and
//! then the tracefiles are merged at the line level
//! (line sets combined, coverage retained when any input has a hit) so multiple feature-config exports
//! (`--all-features`, `--no-default-features`) can be gated together
//! without a separate, platform-specific merge step.
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
//! let report = cargo_coverage_gate::evaluate_released_surface(&lcov, None, &[])?;
//! report.render_text(&mut io::stdout())?;
//! let code = report.verdict().as_exit_code();
//! # let _ = code;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Public API
//!
//! [`filter_released_surface_lcov`] and
//! [`filter_released_surface_lcov_many`] produce released-surface LCOV
//! artifacts without evaluating policy. The established [`evaluate`],
//! [`evaluate_many`], and [`evaluate_many_for_target`] functions retain raw
//! LCOV evaluation semantics for compatibility. Their
//! `evaluate_released_surface*` counterparts filter before evaluation.
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

/// Filter one LCOV tracefile to the released Cargo library and binary surface.
///
/// Cargo target roots and out-of-line module declarations establish source
/// ownership. Files reachable only from tests, examples, benches, build
/// scripts, or other non-release targets are removed. Within released files,
/// lines, branches, and functions are removed only when Rust syntax proves
/// they are test-only (`#[test]`, `#[bench]`, `#[cfg(test)]`, or a `cfg`
/// predicate that requires `test`).
///
/// This is a workspace operation, not a pure text transform. It runs
/// `cargo metadata --no-deps`, reads and parses reachable Rust sources, and
/// resolves relative `SF:` paths against the Cargo workspace root. When
/// `manifest_path` is `None`, Cargo discovers the workspace from the current
/// directory. The returned LCOV is parsed and serialized canonically, so
/// record ordering and summary records may differ from the input.
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
///
/// let raw = std::fs::read_to_string("target/coverage/lcov.info")?;
/// let filtered =
///     cargo_coverage_gate::filter_released_surface_lcov(&raw, Some(Path::new("Cargo.toml")))?;
/// std::fs::write("target/coverage/lcov-released.info", filtered)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns [`CoverageGateError`] when Cargo metadata cannot be loaded, the
/// LCOV input is malformed, a referenced source cannot be resolved/read/parsed,
/// target ownership cannot be determined, or LCOV line data does not match the
/// current source tree.
pub fn filter_released_surface_lcov(lcov_text: &str, manifest_path: Option<&Path>) -> Result<String, CoverageGateError> {
    source_filter::filter_one(lcov_text, manifest_path)
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
/// as [`filter_released_surface_lcov`]; any failing input aborts the operation.
pub fn filter_released_surface_lcov_many(lcov_texts: &[&str], manifest_path: Option<&Path>) -> Result<Vec<String>, CoverageGateError> {
    source_filter::filter_many(lcov_texts, manifest_path)
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
    /// Raw evaluation may count paths outside every workspace member.
    /// Released-surface evaluation rejects such paths before aggregation, so
    /// its successful reports return zero.
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

/// Evaluate raw `lcov_text` (a [`cargo-llvm-cov`] tracefile) against the
/// workspace anchored at `manifest_path`.
///
/// This compatibility entry point does not apply released-surface filtering.
/// Use [`evaluate_released_surface`] when test-only regions and non-release
/// Cargo targets must be excluded.
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

/// Evaluate one or more raw [`cargo-llvm-cov`] tracefiles against the
/// workspace anchored at `manifest_path`.
///
/// This compatibility entry point does not apply released-surface filtering.
/// The tracefiles are merged at the line level before evaluation (line sets
/// combined, coverage retained when any input has a hit), so passing the `--all-features` and
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

/// Evaluate one or more lcov tracefiles for a selected Rust target.
///
/// `target` is a Rust target triple such as `x86_64-pc-windows-msvc`.
/// When omitted, the rustc host target is used. Target-specific
/// package policy is resolved before the gated package set is evaluated.
///
/// # Errors
///
/// Returns a [`CoverageGateError`] under the same conditions as
/// [`evaluate_many`]. Target-resolution failures are also reported when target
/// policy requires a Rust target.
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

/// Filter and evaluate one LCOV tracefile for the rustc host target.
///
/// This applies the same released-surface rules as
/// [`filter_released_surface_lcov`] before policy evaluation.
///
/// # Errors
///
/// Returns [`CoverageGateError`] when LCOV parsing, Cargo metadata, source
/// resolution/reading/parsing, module ownership, line validation, target
/// policy resolution, package selection, or verdict evaluation fails.
pub fn evaluate_released_surface(
    lcov_text: &str,
    manifest_path: Option<&Path>,
    gated_packages: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_released_surface_many(std::slice::from_ref(&lcov_text), manifest_path, gated_packages)
}

/// Filter and evaluate multiple LCOV tracefiles for the rustc host target.
///
/// Inputs are filtered independently, then merged at line level. An empty
/// slice represents an empty coverage report but still validates workspace
/// policy and package selection.
///
/// # Errors
///
/// Returns [`CoverageGateError`] under the same conditions as
/// [`evaluate_released_surface`]; any malformed input aborts the operation.
pub fn evaluate_released_surface_many(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
) -> Result<EvaluatedReport, CoverageGateError> {
    evaluate_released_surface_many_for_target(lcov_texts, manifest_path, gated_packages, None)
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
/// Returns [`CoverageGateError`] under the same conditions as
/// [`evaluate_released_surface_many`]. Target-resolution failures are also
/// reported when target policy requires a Rust target.
pub fn evaluate_released_surface_many_for_target(
    lcov_texts: &[&str],
    manifest_path: Option<&Path>,
    gated_packages: &[String],
    target: Option<&str>,
) -> Result<EvaluatedReport, CoverageGateError> {
    let reports = source_filter::parse_reports(lcov_texts)?;
    let metadata = workspace::load_metadata(manifest_path)?;
    let filtered = source_filter::filter_reports_with_metadata(reports, &metadata)?;
    let filtered_refs = filtered.iter().map(String::as_str).collect::<Vec<_>>();
    let report = lcov_cov::CoverageReport::from_strs(&filtered_refs)?;
    let ws = workspace::Workspace::from_metadata(&metadata, target)?;
    let inner = verdict::evaluate(&report, &ws, gated_packages)?;
    Ok(EvaluatedReport { inner })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;

    use tempfile::tempdir;

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

        let err = evaluate_released_surface("not lcov", Some(Path::new("missing-workspace/Cargo.toml")), &[])
            .expect_err("malformed LCOV must fail before workspace discovery");
        assert!(err.to_string().contains("lcov tracefile"));
        assert!(!err.to_string().contains("workspace metadata"));
    }

    #[test]
    fn empty_filter_batch_skips_workspace_discovery() {
        let filtered = filter_released_surface_lcov_many(&[], Some(Path::new("missing-workspace/Cargo.toml")))
            .expect("empty filtering is a documented no-op");
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
    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata and rustc")]
    fn evaluate_many_without_explicit_target_resolves_the_host() {
        let tmp = tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("alpha/src")).expect("create member");
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\"]\n",
        )
        .expect("write workspace manifest");
        fs::write(
            tmp.path().join("alpha/Cargo.toml"),
            "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [package.metadata.coverage-gate.target.'cfg(any(windows, unix))']\n\
             min-lines-percent = 0\n",
        )
        .expect("write member manifest");
        fs::write(tmp.path().join("alpha/src/lib.rs"), "").expect("write member source");

        let evaluated = evaluate_many(&[], Some(&tmp.path().join("Cargo.toml")), &[]).expect("host target resolves");
        assert_eq!(evaluated.verdict(), Verdict::Pass);
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and spawns cargo metadata")]
    fn evaluate_forwards_manifest_path_and_package_selectors() {
        let tmp = tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("explicit-manifest-only/src")).expect("create member");
        fs::create_dir_all(tmp.path().join("must-remain-unselected/src")).expect("create unselected member");
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"explicit-manifest-only\", \"must-remain-unselected\"]\n",
        )
        .expect("write workspace manifest");
        fs::write(
            tmp.path().join("explicit-manifest-only/Cargo.toml"),
            "[package]\nname = \"explicit-manifest-only\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [package.metadata.coverage-gate]\nmin-lines-percent = 0\n",
        )
        .expect("write member manifest");
        fs::write(tmp.path().join("explicit-manifest-only/src/lib.rs"), "").expect("write member source");
        fs::write(
            tmp.path().join("must-remain-unselected/Cargo.toml"),
            "[package]\nname = \"must-remain-unselected\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [package.metadata.coverage-gate]\nmin-lines-percent = 80\n",
        )
        .expect("write unselected member manifest");
        fs::write(tmp.path().join("must-remain-unselected/src/lib.rs"), "").expect("write unselected member source");

        let missing_source = tmp.path().join("explicit-manifest-only/src/missing.rs").display().to_string();
        let lcov = format!("TN:\nSF:{missing_source}\nDA:999,0\nend_of_record\n");
        let evaluated = evaluate(&lcov, Some(&tmp.path().join("Cargo.toml")), &["explicit-manifest-only".to_owned()])
            .expect("raw evaluation must preserve manifest/package forwarding without source validation");
        assert_eq!(evaluated.verdict(), Verdict::Pass);
    }
}
