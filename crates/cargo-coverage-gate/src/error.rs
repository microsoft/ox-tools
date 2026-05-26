// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error type for the `cargo-coverage-gate` library.

use std::fmt;

/// Errors that can occur while evaluating coverage.
///
/// The variant set will grow as later phases land; in the Phase 1
/// skeleton only [`CoverageGateError::NotImplemented`] is reachable.
#[derive(Debug)]
#[non_exhaustive]
pub enum CoverageGateError {
    /// The requested operation is not implemented yet in the current
    /// build of the library.
    NotImplemented,
}

impl fmt::Display for CoverageGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => f.write_str("cargo-coverage-gate: not implemented yet"),
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
}
