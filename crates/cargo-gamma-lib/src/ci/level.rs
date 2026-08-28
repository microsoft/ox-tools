// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How loudly a survivor is reported to a SARIF consumer.

use clap::ValueEnum;

/// How loudly a survivor is reported to a SARIF consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Level {
    /// An observation about the test suite. The default.
    #[default]
    Note,

    /// A problem the team wants raised.
    Warning,
}

impl Level {
    /// The SARIF spelling.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Warning => "warning",
        }
    }
}
