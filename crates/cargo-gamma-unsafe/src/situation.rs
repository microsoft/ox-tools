// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classifying a failed platform operation without reading its message.

/// What kind of platform failure a [`crate::PlatformError`] reports.
///
/// Carried separately from the message so a caller can classify the failure without matching on
/// prose that exists to be read by a person. This implementation crate keeps the enum
/// non-exhaustive so future platform distinctions can be added without breaking coordinating
/// callers; callers must preserve a conservative fallback for an unknown classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Situation {
    /// The host lacks the requested facility for the duration of this run.
    ///
    /// Examples include a Linux host without delegated cgroup v2 memory control and a non-Linux
    /// Unix host without an equivalent process-tree memory boundary.
    Unsupported,

    /// The facility exists, but this particular operation on it did not succeed.
    ///
    /// Examples include failure to create one cgroup leaf, write one interface file, or assign one
    /// child with `AssignProcessToJobObject`.
    Refused,

    /// Interruption-driven teardown has begun and process termination may occur immediately.
    Interrupted,
}
