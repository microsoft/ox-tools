// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// A named set of mutators.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    /// The preset name, written `@name` in selectors.
    pub name: &'static str,

    /// One-line description.
    pub description: &'static str,

    /// Families and names that make up the preset.
    pub members: &'static [&'static str],
}
