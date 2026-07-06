// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for the `cargo-coverage-gate` library.
//!
//! Built on [`ohno`] for backtrace capture and error-chain support.
//! The public surface is a single zero-field [`CoverageGateError`]
//! umbrella that every fallible library function returns. Each
//! distinct failure mode is a separate `pub(crate)` typed error that
//! converts `.into()` the umbrella via `#[from]`, so the `?` operator
//! propagates naturally.
//!
//! Per-call-site context (what we were trying to do when the failure
//! surfaced) is attached with [`ohno::enrich_err`] at function level,
//! which also stamps file and line into the error chain.

use serde_json::Value;

/// Top-level error returned from every fallible function in the
/// `cargo-coverage-gate` library.
///
/// Carries no free-form fields — the specific cause is encoded in the
/// chained source error (see the `From` impls). Callers surface the
/// message verbatim through their own diagnostic surface; the
/// [`Display`] rendering includes the chained source as `Caused by: …`
/// automatically.
///
/// [`Display`]: std::fmt::Display
#[ohno::error]
#[from(
    LoadMetadataError,
    InvalidThresholdValueError,
    ThresholdOutOfRangeError,
    InvalidNoCoverableLinesValueError,
    ConflictingCoverageMetadataError,
    WorkspaceScopedNoCoverableLinesError,
    ParseLcovError,
    ReadLcovError,
    UnknownPackageSelectorError
)]
pub struct CoverageGateError;

/// Failed to invoke `cargo metadata` to enumerate workspace members.
#[ohno::error]
#[display("failed to load workspace metadata")]
#[from(cargo_metadata::Error)]
pub(crate) struct LoadMetadataError;

/// The `coverage-gate.min-lines-percent` key was present in metadata
/// but its value was not a JSON number.
#[ohno::error]
#[display("{source}: `coverage-gate.min-lines-percent` must be a number, got {min}")]
pub(crate) struct InvalidThresholdValueError {
    pub source: String,
    pub min: Value,
}

/// The `coverage-gate.min-lines-percent` value was a number but fell
/// outside the accepted `[0.0, 100.0]` range.
#[ohno::error]
#[display(
    "invalid coverage-gate min-lines-percent value `{value}` for {source}: \
     expected a value in {lower:.1}..={upper:.1}"
)]
pub(crate) struct ThresholdOutOfRangeError {
    pub source: String,
    pub value: f64,
    pub lower: f64,
    pub upper: f64,
}

/// The `coverage-gate.expect-no-coverable-lines` key was present in
/// metadata but its value was not a JSON boolean.
#[ohno::error]
#[display("{source}: `coverage-gate.expect-no-coverable-lines` must be a boolean, got {value}")]
pub(crate) struct InvalidNoCoverableLinesValueError {
    pub source: String,
    pub value: Value,
}

/// A package set both `coverage-gate.min-lines-percent` and
/// `coverage-gate.expect-no-coverable-lines = true`. The two are
/// mutually exclusive: a numeric floor describes code that should be
/// covered, while the assertion declares there is no coverable code at
/// all.
#[ohno::error]
#[display(
    "{source}: `coverage-gate` cannot set both `min-lines-percent` and \
     `expect-no-coverable-lines`; pick one"
)]
pub(crate) struct ConflictingCoverageMetadataError {
    pub source: String,
}

/// `coverage-gate.expect-no-coverable-lines` was set in
/// `[workspace.metadata.coverage-gate]`. The assertion is about a single
/// package's contents, so it is only meaningful per-package.
#[ohno::error]
#[display(
    "`coverage-gate.expect-no-coverable-lines` is a package-level assertion and \
     cannot be set in `[workspace.metadata.coverage-gate]`"
)]
pub(crate) struct WorkspaceScopedNoCoverableLinesError;

/// An lcov tracefile was syntactically malformed.
#[ohno::error]
#[display("lcov tracefile is not well-formed")]
#[from(lcov::report::ParseError)]
pub(crate) struct ParseLcovError;

/// Failed to read an lcov tracefile from disk (the file itself was
/// inaccessible or unreadable, distinct from a malformed payload).
#[ohno::error]
#[display("failed to read lcov tracefile `{path}`")]
pub(crate) struct ReadLcovError {
    pub path: String,
}

/// A `--package` selector did not match any workspace member.
#[ohno::error]
#[display("`--package` selector `{selector}` did not match any workspace member")]
pub(crate) struct UnknownPackageSelectorError {
    pub selector: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn umbrella_propagates_load_metadata_chain() {
        let inner = LoadMetadataError::caused_by(std::io::Error::other("no manifest"));
        let outer: CoverageGateError = inner.into();
        let rendered = outer.to_string();
        assert!(rendered.contains("failed to load workspace metadata"));
        assert!(rendered.contains("no manifest"));
    }

    #[test]
    fn umbrella_propagates_parse_lcov() {
        let inner = ParseLcovError::new();
        let outer: CoverageGateError = inner.into();
        let rendered = outer.to_string();
        assert!(rendered.contains("lcov tracefile"));
    }

    #[test]
    fn unknown_package_selector_carries_pattern() {
        let err = UnknownPackageSelectorError::new("nope-*".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("nope-*"));
        assert!(rendered.contains("did not match"));
    }

    #[test]
    fn threshold_out_of_range_renders_value_and_bounds() {
        let err = ThresholdOutOfRangeError::new("alpha".to_owned(), 150.0, 0.0, 100.0);
        let rendered = err.to_string();
        assert!(rendered.contains("150"));
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("0.0..=100.0"));
    }

    #[test]
    fn invalid_no_coverable_lines_value_renders_source_and_value() {
        let err = InvalidNoCoverableLinesValueError::new("alpha".to_owned(), Value::from("yes"));
        let rendered = err.to_string();
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("expect-no-coverable-lines"));
        assert!(rendered.contains("boolean"));
        assert!(rendered.contains("yes"));
    }

    #[test]
    fn conflicting_coverage_metadata_renders_source() {
        let err = ConflictingCoverageMetadataError::new("alpha".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("min-lines-percent"));
        assert!(rendered.contains("expect-no-coverable-lines"));
    }

    #[test]
    fn workspace_scoped_no_coverable_lines_mentions_workspace() {
        let err = WorkspaceScopedNoCoverableLinesError::new();
        let rendered = err.to_string();
        assert!(rendered.contains("expect-no-coverable-lines"));
        assert!(rendered.contains("workspace.metadata.coverage-gate"));
    }
}
