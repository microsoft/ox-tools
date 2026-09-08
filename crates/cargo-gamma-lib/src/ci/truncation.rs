// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a SARIF rendering left out, if anything.

/// What a SARIF rendering left out, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    /// How many findings there were.
    ///
    /// Uncovered mutants are among them, not only survivors.
    pub found: usize,

    /// How many the log contains.
    pub written: usize,
}
