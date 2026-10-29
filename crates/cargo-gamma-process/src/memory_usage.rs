// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// What the platform observed about one invocation's memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    /// The highest aggregate memory the process tree reached, when the host could measure it.
    pub peak: Option<u64>,

    /// Whether the memory boundary found the workload past its ceiling.
    ///
    /// This records the breach, not that cleanup succeeded: macOS sampling sets it before its
    /// sweep and keeps it when that sweep fails.
    pub exhausted: bool,
}
