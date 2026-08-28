// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Whether this host can meter and bound a test subtree's memory.

/// Reports whether this host can meter and bound a test subtree's memory, or why it cannot.
///
/// The answer is worked out once and cached, because settling it can involve creating a cgroup and
/// moving this process, and because a run that asked for a ceiling wants one diagnostic rather
/// than one per mutant.
///
/// # Errors
///
/// Returns the reason this host cannot account for a whole test subtree's memory.
#[cfg_attr(
    windows,
    expect(
        clippy::missing_const_for_fn,
        clippy::unnecessary_wraps,
        reason = "the answer is settled at compile time on Windows alone; every other platform has \
                  to go and look, and the signature is shared"
    )
)]
pub fn support() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        crate::cgroup::root().map(|_root| ()).map_err(str::to_owned)
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
        Err(
            "bounding a test subtree's memory needs cgroup v2 on Linux or a job object on Windows, \
             and this platform offers no unprivileged equivalent that accounts for a whole process \
             tree. An inherited `RLIMIT_AS` is not one: it bounds each process separately, and \
             bounds reserved address space rather than resident memory, so scaling it from a \
             measured peak would stop healthy tests while leaving the runaway case unbounded"
                .to_owned(),
        )
    }
}
