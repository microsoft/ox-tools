// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for the `cargo-each` crate.
//!
//! Built on [`ohno`] for backtrace capture and error-chain support. Every
//! fallible function returns a single zero-field [`EachError`] umbrella; each
//! distinct failure mode is a separate `pub(crate)` typed error that converts
//! into the umbrella via `#[from]`, so `?` propagates naturally.
//!
//! The umbrella is **intentionally opaque** (an application-style error, per
//! M-APP-ERROR): everything here is crate-internal — `cargo-each` exposes no
//! library API — and failures are surfaced to a human via [`Display`] (the
//! chained source renders as `Caused by: …`), not branched on in code. If the
//! crate ever needs to react to a specific failure category, match on a
//! promoted variant then — not speculatively.
//!
//! [`Display`]: std::fmt::Display

/// Top-level error returned from every fallible function in the
/// `cargo-each` crate.
///
/// Carries no free-form fields — the specific cause is encoded in the
/// chained source error (see the `From` impls). The [`Display`] rendering
/// includes the chained source as `Caused by: …` automatically.
///
/// [`Display`]: std::fmt::Display
#[ohno::error]
#[from(
    LoadMetadataError,
    UnknownSelectorError,
    InvalidFilterExpressionError,
    InvalidTargetKindError,
    PlaceholderMisuseError,
    ChdirConflictsWithOnceError,
    JobsConflictWithOnceError,
    PackageFileReadError,
    PackageFileUtf8Error,
    InvalidPackageFileLineError,
    WorkspaceManifestReadError,
    WorkspaceManifestParseError,
    WorkspaceRustVersionError
)]
pub(crate) struct EachError;

/// Failed to invoke `cargo metadata` to enumerate workspace members.
#[ohno::error]
#[display("failed to load workspace metadata")]
#[from(cargo_metadata::Error)]
pub(crate) struct LoadMetadataError;

/// A `-p` / `--package` (or `--exclude`) selector matched no workspace
/// member. Surfaced loudly so typos fail rather than silently skipping.
#[ohno::error]
#[display("package selector `{selector}` did not match any workspace member")]
pub(crate) struct UnknownSelectorError {
    pub(crate) selector: String,
}

/// A `--filter` / `--exclude-filter` expression could not be parsed.
#[ohno::error]
#[display("invalid filter expression `{expression}`: {reason}")]
pub(crate) struct InvalidFilterExpressionError {
    pub(crate) expression: String,
    pub(crate) reason: String,
}

/// A `--each-target` value is not a supported Cargo target kind.
#[ohno::error]
#[display(
    "invalid target kind `{kind}`; expected one of: lib, rlib, dylib, cdylib, staticlib, proc-macro, bin, example, test, bench, custom-build"
)]
pub(crate) struct InvalidTargetKindError {
    pub(crate) kind: String,
}

/// A placeholder token was used in a mode that does not support it.
#[ohno::error]
#[display("placeholder `{token}` cannot be used here: {reason}")]
pub(crate) struct PlaceholderMisuseError {
    pub(crate) token: String,
    pub(crate) reason: String,
}

/// `--chdir` was combined with `--once`. Changing into a member's crate root
/// is only meaningful when there is one member per invocation, i.e. in
/// per-package or per-target mode.
#[ohno::error]
#[display("`--chdir` cannot be combined with `--once`")]
pub(crate) struct ChdirConflictsWithOnceError;

/// `--jobs` greater than one was combined with `--once`.
#[ohno::error]
#[display("`--jobs` must be 1 when combined with `--once`")]
pub(crate) struct JobsConflictWithOnceError;

/// A `--package-file` could not be read.
#[ohno::error]
#[display("could not read package file `{path}`")]
#[from(std::io::Error)]
pub(crate) struct PackageFileReadError {
    pub(crate) path: String,
}

/// A `--package-file` was not valid UTF-8.
#[ohno::error]
#[display("package file `{path}` is not valid UTF-8")]
#[from(std::string::FromUtf8Error)]
pub(crate) struct PackageFileUtf8Error {
    pub(crate) path: String,
}

/// A nonempty line in a `--package-file` was not a package spec.
#[ohno::error]
#[display("invalid package spec in `{path}` at line {line}: `{spec}` ({reason})")]
pub(crate) struct InvalidPackageFileLineError {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) spec: String,
    pub(crate) reason: String,
}

/// The root manifest could not be read while resolving
/// `{workspace-rust-version}`.
#[ohno::error]
#[display("could not read workspace manifest `{path}`")]
#[from(std::io::Error)]
pub(crate) struct WorkspaceManifestReadError {
    pub(crate) path: String,
}

/// The root manifest could not be parsed while resolving
/// `{workspace-rust-version}`.
#[ohno::error]
#[display("could not parse workspace manifest `{path}`")]
#[from(toml::de::Error)]
pub(crate) struct WorkspaceManifestParseError {
    pub(crate) path: String,
}

/// The workspace Rust-version contract is incomplete or inconsistent.
#[ohno::error]
#[display("cannot resolve `{{workspace-rust-version}}`: {reason}")]
pub(crate) struct WorkspaceRustVersionError {
    pub(crate) reason: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn umbrella_propagates_load_metadata_chain() {
        let inner = LoadMetadataError::caused_by(std::io::Error::other("no manifest"));
        let outer: EachError = inner.into();
        let rendered = outer.to_string();
        assert!(rendered.contains("failed to load workspace metadata"));
        assert!(rendered.contains("no manifest"));
    }

    #[test]
    fn unknown_selector_carries_pattern() {
        let err = UnknownSelectorError::new("nope-*".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("nope-*"));
        assert!(rendered.contains("did not match"));
    }

    #[test]
    fn invalid_filter_expression_renders_reason() {
        let err = InvalidFilterExpressionError::new("dep:".to_owned(), "empty dependency name".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("dep:"));
        assert!(rendered.contains("empty dependency name"));
    }

    #[test]
    fn invalid_target_kind_names_value() {
        let err = InvalidTargetKindError::new("nope".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("nope"));
        assert!(rendered.contains("target kind"));
    }

    #[test]
    fn placeholder_misuse_renders_token_and_reason() {
        let err = PlaceholderMisuseError::new("{name}".to_owned(), "per-package token in --once mode".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("{name}"));
        assert!(rendered.contains("--once"));
    }

    #[test]
    fn chdir_conflict_renders() {
        let err = ChdirConflictsWithOnceError::new();
        let rendered = err.to_string();
        assert!(rendered.contains("--chdir"));
        assert!(rendered.contains("--once"));
    }

    #[test]
    fn package_file_line_error_names_source() {
        let err = InvalidPackageFileLineError::new(
            "affected.packages".to_owned(),
            3_usize,
            "--workspace".to_owned(),
            "command-line tokens are not package specs".to_owned(),
        );
        let rendered = err.to_string();
        assert!(rendered.contains("affected.packages"));
        assert!(rendered.contains("line 3"));
        assert!(rendered.contains("--workspace"));
    }

    #[test]
    fn workspace_rust_version_error_renders_reason() {
        let err = WorkspaceRustVersionError::new("member `alpha` does not declare `rust-version`".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("{workspace-rust-version}"));
        assert!(rendered.contains("alpha"));
    }
}
