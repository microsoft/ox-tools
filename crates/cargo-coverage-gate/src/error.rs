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
    WorkspaceTargetPolicyError,
    InvalidTargetTableError,
    InvalidTargetPolicyShapeError,
    MissingTargetPolicyBehaviorError,
    InvalidTargetSelectorError,
    UnsupportedTargetSelectorError,
    AmbiguousTargetPolicyError,
    WorkspaceScopedNoCoverableLinesError,
    ResolveTargetError,
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

/// Target policies were declared in workspace metadata.
#[ohno::error]
#[display("coverage-gate target policies are package-scoped and cannot be set in workspace metadata")]
pub(crate) struct WorkspaceTargetPolicyError;

/// A target-policy container was not a table.
#[ohno::error]
#[display("{source}: coverage-gate `target` must be a table keyed by target triple or cfg expression")]
pub(crate) struct InvalidTargetTableError {
    pub(crate) source: String,
}

/// A selected target policy was not a table.
#[ohno::error]
#[display("{source}: coverage-gate target policy must be a table")]
pub(crate) struct InvalidTargetPolicyShapeError {
    pub(crate) source: String,
}

/// A target policy did not select an effective behavior.
#[ohno::error]
#[display("{source}: target policy must set `min-lines-percent` or `expect-no-coverable-lines = true`")]
pub(crate) struct MissingTargetPolicyBehaviorError {
    pub(crate) source: String,
}

/// A target-policy selector was syntactically invalid.
#[ohno::error]
#[display("{source}: invalid coverage-gate target selector `{selector}`")]
#[from(cargo_platform::ParseError)]
pub(crate) struct InvalidTargetSelectorError {
    pub(crate) source: String,
    pub(crate) selector: String,
}

/// A target selector depends on Cargo build-unit context.
#[ohno::error]
#[display("{source}: coverage-gate target selector `{selector}` uses unsupported build-context cfg attributes: {attributes}")]
pub(crate) struct UnsupportedTargetSelectorError {
    pub(crate) source: String,
    pub(crate) selector: String,
    pub(crate) attributes: String,
}

/// More than one `cfg(...)` target policy matched the selected Rust target.
#[ohno::error]
#[display(
    "{source}: multiple coverage-gate target policies match `{target}`: {selectors}; \
     use disjoint cfg expressions or an exact Rust target-triple override"
)]
pub(crate) struct AmbiguousTargetPolicyError {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) selectors: String,
}

/// The Rust target or its cfg values could not be obtained.
#[ohno::error]
#[display("failed to resolve Rust target")]
#[from(ExecuteRustcError, RustcCommandFailedError, MissingRustcHostTargetError, InvalidRustcCfgError)]
pub(crate) struct ResolveTargetError;

/// A rustc target-information command could not be launched.
#[ohno::error]
#[display("could not execute `{command}`")]
#[from(std::io::Error)]
pub(crate) struct ExecuteRustcError {
    pub(crate) command: String,
}

/// A rustc target-information command exited unsuccessfully.
#[ohno::error]
#[display("`{command}` exited with {status}: {stderr}")]
pub(crate) struct RustcCommandFailedError {
    pub(crate) command: String,
    pub(crate) status: String,
    pub(crate) stderr: String,
}

/// The rustc version output did not identify its host target.
#[ohno::error]
#[display("`{command}` did not report a host target")]
pub(crate) struct MissingRustcHostTargetError {
    pub(crate) command: String,
}

/// A cfg value emitted by rustc could not be parsed.
#[ohno::error]
#[display("rustc reported invalid cfg `{value}` for Rust target `{target}`")]
#[from(cargo_platform::ParseError)]
pub(crate) struct InvalidRustcCfgError {
    pub(crate) value: String,
    pub(crate) target: String,
}

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
    fn execute_rustc_error_preserves_io_source() {
        let error = ExecuteRustcError::caused_by("rustc -vV".to_owned(), std::io::Error::other("launch failed"));
        let source = std::error::Error::source(&error).expect("execute error must retain its IO source");
        assert_eq!(source.to_string(), "launch failed");
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

    #[test]
    fn ambiguous_target_policy_names_target_and_selectors() {
        let err = AmbiguousTargetPolicyError::new(
            "alpha".to_owned(),
            "x86_64-pc-windows-msvc".to_owned(),
            "cfg(windows), cfg(target_os = \"windows\")".to_owned(),
        );
        let rendered = err.to_string();
        assert!(rendered.contains("alpha"));
        assert!(rendered.contains("x86_64-pc-windows-msvc"));
        assert!(rendered.contains("cfg(windows)"));
    }
}
