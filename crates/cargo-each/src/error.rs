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
    InvalidPredicateError,
    PlaceholderMisuseError,
    ChdirRequiresPerPackageError
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

/// A `--filter` / `--exclude-filter` predicate could not be parsed.
#[ohno::error]
#[display("invalid filter predicate `{predicate}`: {reason}")]
pub(crate) struct InvalidPredicateError {
    pub(crate) predicate: String,
    pub(crate) reason: String,
}

/// A placeholder token was used in a mode that does not support it (e.g.
/// a per-package token like `{name}` under `--once`, or `{packages}`
/// outside `--once`).
#[ohno::error]
#[display("placeholder `{token}` cannot be used here: {reason}")]
pub(crate) struct PlaceholderMisuseError {
    pub(crate) token: String,
    pub(crate) reason: String,
}

/// `--chdir` was combined with `--once`. Changing into a member's crate root
/// is only meaningful when there is one member per invocation, i.e. in
/// per-package mode.
#[ohno::error]
#[display("`--chdir` requires per-package mode; it cannot be combined with `--once`")]
pub(crate) struct ChdirRequiresPerPackageError;

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
    fn invalid_predicate_renders_reason() {
        let err = InvalidPredicateError::new("dep:".to_owned(), "empty dependency name".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("dep:"));
        assert!(rendered.contains("empty dependency name"));
    }

    #[test]
    fn placeholder_misuse_renders_token_and_reason() {
        let err = PlaceholderMisuseError::new("{name}".to_owned(), "per-package token in --once mode".to_owned());
        let rendered = err.to_string();
        assert!(rendered.contains("{name}"));
        assert!(rendered.contains("--once"));
    }

    #[test]
    fn chdir_requires_per_package_renders() {
        let err = ChdirRequiresPerPackageError::new();
        let rendered = err.to_string();
        assert!(rendered.contains("--chdir"));
        assert!(rendered.contains("--once"));
    }
}
