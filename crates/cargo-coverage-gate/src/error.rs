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
}

impl fmt::Display for CoverageGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => f.write_str("cargo-coverage-gate: not implemented yet"),
            Self::JsonParse { message } => {
                write!(f, "failed to parse coverage JSON: {message}")
            }
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
}
