// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// One diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// A stable identifier, so a finding can be referred to, suppressed or searched for.
    pub code: &'static str,

    /// The measured symptom, in one line.
    pub headline: String,

    /// Supporting measurements, one per line.
    pub detail: Vec<String>,

    /// What to do about it.
    pub remedy: String,

    /// What taking the remedy costs in signal. Never omitted, never softened.
    pub cost: String,
}
