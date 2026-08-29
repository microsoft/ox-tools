// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classifying a platform refusal without reading its message.

/// What kind of refusal a [`crate::PlatformError`] reports.
///
/// Carried separately from the message so a caller can decide what to do — refuse the run, degrade
/// it, or record it against one mutant — without matching on prose that exists to be read by a
/// person.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Situation {
    /// The host has no facility that can do what was asked, and never will during this run.
    ///
    /// A kernel without cgroup v2 delegation and a Unix that is not Linux both answer this way. It
    /// is a standing fact about the machine rather than about the operation, so retrying is
    /// pointless and the run's response is to refuse or to degrade, once.
    Unsupported,

    /// The facility exists, but this particular operation on it did not succeed.
    ///
    /// A leaf that could not be created, an interface file that could not be written, a job that
    /// would not take its child. Another attempt might succeed, and the caller is expected to
    /// refuse the one launch rather than the whole run.
    Refused,

    /// A terminal signal has already begun taking this run apart.
    ///
    /// Nothing further may be created: the process is free to die at the next instruction, and a
    /// child made now would outlive it.
    Interrupted,
}
