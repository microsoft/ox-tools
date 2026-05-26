// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error type for the `cargo-coverage-gate` library.

use std::fmt;

/// Errors that can occur while evaluating coverage.
///
/// The variant set will grow as later phases land.
#[derive(Debug)]
#[non_exhaustive]
pub enum CoverageGateError {
    /// The requested operation is not implemented yet in the current
    /// build of the library.
    NotImplemented,
    /// Failed to parse a `cargo-llvm-cov` JSON report.
    JsonParse {
        /// Human-readable description of the parse failure, including
        /// the underlying line/column where available.
        message: String,
    },
    /// Failed to load workspace metadata (typically a `cargo metadata`
    /// invocation failure, or unreadable / malformed `Cargo.toml`).
    Metadata {
        /// Human-readable description of the failure.
        message: String,
    },
    /// A `min-lines` threshold value was outside the allowed
    /// `[0.0, 100.0]` range.
    InvalidThreshold {
        /// Where the offending value was found — either a crate name
        /// or `"workspace"` for the workspace-level default.
        source: String,
        /// The offending value.
        value: f64,
    },
}

impl fmt::Display for CoverageGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => f.write_str("cargo-coverage-gate: not implemented yet"),
            Self::JsonParse { message } => {
                write!(f, "failed to parse coverage JSON: {message}")
            }
            Self::Metadata { message } => {
                write!(f, "failed to load workspace metadata: {message}")
            }
            Self::InvalidThreshold { source, value } => write!(
                f,
                "invalid coverage-gate min-lines value `{value}` for {source}: \
                 expected a number in 0.0..=100.0"
            ),
        }
    }
}

impl std::error::Error for CoverageGateError {}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_displays() {
        let err = CoverageGateError::NotImplemented;
        assert!(err.to_string().contains("not implemented"));
    }

    #[test]
    fn json_parse_displays() {
        let err = CoverageGateError::JsonParse {
            message: "expected `,` or `}`".to_owned(),
        };
        let s = err.to_string();
        assert!(s.contains("coverage JSON"));
        assert!(s.contains("expected `,` or `}`"));
    }

    #[test]
    fn metadata_displays() {
        let err = CoverageGateError::Metadata {
            message: "cargo exited with code 101".to_owned(),
        };
        assert!(err.to_string().contains("cargo exited with code 101"));
    }

    #[test]
    fn invalid_threshold_displays() {
        let err = CoverageGateError::InvalidThreshold {
            source: "alpha".to_owned(),
            value: 150.0,
        };
        let s = err.to_string();
        assert!(s.contains("alpha"));
        assert!(s.contains("150"));
        assert!(s.contains("0.0..=100.0"));
    }
}
