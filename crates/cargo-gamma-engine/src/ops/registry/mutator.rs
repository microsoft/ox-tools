// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// A registered mutator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mutator {
    /// The stable name, `family.transform`.
    pub name: &'static str,

    /// One-line description, used by `explain` and by the report.
    pub description: &'static str,

    /// Whether the mutator is enabled by the default preset.
    pub default_on: bool,

    /// Academic or industry aliases that resolve to this mutator.
    pub aliases: &'static [&'static str],
}
