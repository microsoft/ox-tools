// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// What a directive asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Do not generate the named mutants here.
    Skip,

    /// The named mutants here are expected to survive; report it if they do not.
    ExpectSurvived,

    /// The named mutants here are expected to be caught; report it if they are not.
    ExpectKilled,
}

impl Intent {
    /// Resolves an attribute name to an intent.
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "skip" => Some(Self::Skip),
            "expect_survived" => Some(Self::ExpectSurvived),
            "expect_killed" => Some(Self::ExpectKilled),
            _ => None,
        }
    }
}
