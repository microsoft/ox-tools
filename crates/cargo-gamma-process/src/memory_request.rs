// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// What one invocation asks the platform to account for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryRequest {
    /// Measure the peak memory of the whole process tree.
    pub meter: bool,

    /// Stop the process tree if its aggregate memory passes this many bytes.
    pub limit: Option<u64>,
}

impl MemoryRequest {
    /// Whether this asks the platform for anything at all.
    #[must_use]
    pub const fn wanted(&self) -> bool {
        self.meter || self.limit.is_some()
    }
}
