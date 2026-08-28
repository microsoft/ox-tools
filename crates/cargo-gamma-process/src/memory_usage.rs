// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// What the platform observed about one invocation's memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    /// The highest aggregate memory the process tree reached, when the host could measure it.
    pub peak: Option<u64>,

    /// Whether the kernel reported stopping the workload for passing its ceiling.
    pub exhausted: bool,
}
