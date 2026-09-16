// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Whether this host can meter and bound a test subtree's memory.

use crate::PlatformError;
#[cfg(not(windows))]
use crate::Situation;

/// Reports whether this host can meter and bound a test subtree's memory, or why it cannot.
///
/// The answer is worked out once and cached, because settling it can involve creating a cgroup and
/// moving this process, and because a run that asked for a ceiling wants one diagnostic rather
/// than one per mutant.
///
/// # Errors
///
/// Returns a [`PlatformError`] whose [`situation`](PlatformError::situation) is
/// [`Situation::Unsupported`](crate::Situation::Unsupported), carrying the reason this host cannot
/// account for a whole test subtree's memory: no cgroup v2 unified hierarchy, no delegated cgroup,
/// no memory controller to hand to children, a kernel missing the interface files a leaf needs, or
/// a Unix that is not Linux. This classification describes a host-wide capability limitation
/// rather than the failure of one launch.
#[cfg(target_os = "linux")]
pub fn support() -> Result<(), PlatformError> {
    crate::cgroup::root()
        .map(|_root| ())
        .map_err(|reason| PlatformError::new(Situation::Unsupported, reason))
}

/// Windows job objects need no host-wide preparation.
#[cfg(windows)]
#[expect(
    clippy::missing_const_for_fn,
    clippy::unnecessary_wraps,
    reason = "the cross-platform signature reports unsupported hosts, while Windows support is \
              settled at compile time"
)]
pub fn support() -> Result<(), PlatformError> {
    Ok(())
}

/// Other hosts have no unprivileged whole-process-tree memory boundary.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn support() -> Result<(), PlatformError> {
    Err(PlatformError::new_static(
        Situation::Unsupported,
        "bounding a test subtree's memory needs cgroup v2 on Linux or a job object on Windows, \
         and this platform offers no unprivileged equivalent that accounts for a whole process \
         tree. An inherited `RLIMIT_AS` is not one: it bounds each process separately, and \
         bounds reserved address space rather than resident memory, so scaling it from a \
         measured peak would stop healthy tests while leaving the runaway case unbounded",
    ))
}
