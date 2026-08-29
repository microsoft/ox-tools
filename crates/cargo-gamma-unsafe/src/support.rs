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
/// a Unix that is not Linux. It is a standing fact about the machine rather than about one launch,
/// so a caller degrades or refuses the whole run once rather than retrying.
#[cfg_attr(
    windows,
    expect(
        clippy::missing_const_for_fn,
        clippy::unnecessary_wraps,
        reason = "the answer is settled at compile time on Windows alone; every other platform has \
                  to go and look, and the signature is shared"
    )
)]
pub fn support() -> Result<(), PlatformError> {
    #[cfg(target_os = "linux")]
    {
        crate::cgroup::root()
            .map(|_root| ())
            .map_err(|reason| PlatformError::new(Situation::Unsupported, reason))
    }

    #[cfg(windows)]
    {
        // A job object needs no delegation and no privilege, and one is created for every child
        // already. Whether this particular one can be created is settled per invocation.
        // #[gamma::skip(result.ok_to_err, reason = "this compile-time branch is observable only in a Windows build; Linux mutation runs cannot execute it")]
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        // #[gamma::skip(result.err_to_ok, literal.str_to_empty, literal.str_to_xyzzy, reason = "this compile-time branch exists only on unsupported non-Linux, non-Windows targets and cannot be executed by the Linux mutation run")]
        Err(PlatformError::new(
            Situation::Unsupported,
            "bounding a test subtree's memory needs cgroup v2 on Linux or a job object on Windows, \
             and this platform offers no unprivileged equivalent that accounts for a whole process \
             tree. An inherited `RLIMIT_AS` is not one: it bounds each process separately, and \
             bounds reserved address space rather than resident memory, so scaling it from a \
             measured peak would stop healthy tests while leaving the runaway case unbounded",
        ))
    }
}
