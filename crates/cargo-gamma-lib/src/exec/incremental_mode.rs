// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How an incremental run reuses state from the previous run.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// How an incremental run reuses state from the previous run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IncrementalMode {
    /// Re-run everything from scratch with no caching.
    No,

    /// Reuse compiler unviability and checked execution hints.
    #[default]
    Build,
}

impl IncrementalMode {
    /// Whether any caching or reuse is enabled.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::No)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reuse_is_the_default() {
        assert_eq!(IncrementalMode::default(), IncrementalMode::Build);
    }

    #[test]
    fn test_verdict_reuse_is_not_an_incremental_mode() {
        let _error = IncrementalMode::from_str("full", true).expect_err("full would reuse test verdicts");
    }
}
