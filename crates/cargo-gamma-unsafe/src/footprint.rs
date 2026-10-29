// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Readings for sampled process-group memory accounting on macOS.

use core::mem::MaybeUninit;
use std::io;

// `sys/proc_info.h` defines this selector; libc exposes the call but not the constant.
const PROC_PGRP_ONLY: u32 = 2;

/// A process's current and lifetime maximum physical footprint, including compressed memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// The physical footprint at the instant of the reading, in bytes.
    pub current: u64,
    /// The largest physical footprint the process has held, in bytes.
    pub lifetime_max: u64,
}

/// Lists the processes currently belonging to a process group.
///
/// Membership is a snapshot: a listed process can exit before its footprint is read.
///
/// # Errors
///
/// Returns the operating system's reason when listing fails, or rejects an invalid group id
/// or a listing too large for the platform's buffer size.
pub fn members(group: i32) -> io::Result<Vec<i32>> {
    let group = u32::try_from(group)
        .ok()
        .filter(|group| *group > 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "process-group identifiers must be greater than one"))?;

    list_with(|members| {
        let bytes = i32::try_from(size_of_val(members))
            .map_err(|_too_large| io::Error::other("process-group listing exceeds the platform buffer size"))?;
        // SAFETY: __error has no preconditions and returns this thread's errno cell.
        let errno = unsafe { libc::__error() };
        // SAFETY: errno points to this thread's writable cell. libproc maps a syscall failure
        // to zero, also the valid empty-list count, so clear errno to distinguish them.
        unsafe { *errno = 0 };
        // SAFETY: `members` is initialized, writable pid_t storage, `bytes` is its exact byte
        // length, and proc_listpids writes at most that length. The selector takes a numeric pgid.
        let listed = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, members.as_mut_ptr().cast(), bytes) };
        // SAFETY: errno still points to this thread's initialized cell; no other call intervenes.
        let error = unsafe { *errno };
        listing_result(listed, error)
    })
}

fn listing_result(listed: i32, error: i32) -> io::Result<usize> {
    if listed <= 0 && error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    usize::try_from(listed).map_err(|_negative| io::Error::other("process-group listing returned a negative byte count"))
}

fn list_with(mut list: impl FnMut(&mut [i32]) -> io::Result<usize>) -> io::Result<Vec<i32>> {
    let mut members = vec![0; 32];
    loop {
        let bytes = list(&mut members)?;
        if bytes < size_of_val(members.as_slice()) {
            members.truncate(bytes / size_of::<i32>());
            members.retain(|pid| *pid > 0);
            return Ok(members);
        }
        let length = members
            .len()
            .checked_mul(2)
            .filter(|length| *length <= i32::MAX as usize / size_of::<i32>())
            .ok_or_else(|| io::Error::other("process-group listing exceeds the platform buffer size"))?;
        members.resize(length, 0);
    }
}

/// Reads a process's physical footprint, or reports that the process is unavailable.
///
/// The lifetime maximum remains readable for an exited, unreaped process even when its current
/// footprint is zero. Callers must retain the leader's wait right while reading its group.
///
/// # Errors
///
/// Returns errors other than `ESRCH` and `EPERM`. A vanished or inaccessible member returns
/// `Ok(None)` so that a listing/reading race or changed credentials does not invalidate the
/// other members' readings. Inaccessible members are not counted.
pub fn read(pid: i32) -> io::Result<Option<Footprint>> {
    let mut usage = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: the flavor is paired with writable storage for exactly rusage_info_v4. Although
    // libc spells the argument as a pointer to rusage_info_t, libproc treats it as the address
    // of that structure, not an additional indirection, and writes no more than its size.
    let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, usage.as_mut_ptr().cast()) };
    if result != 0 {
        return unreadable_or_error(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the supplied v4 structure, whose fields are integers.
    let usage = unsafe { usage.assume_init() };
    Ok(Some(Footprint {
        current: usage.ri_phys_footprint,
        lifetime_max: usage.ri_lifetime_max_phys_footprint,
    }))
}

fn unreadable_or_error(cause: io::Error) -> io::Result<Option<Footprint>> {
    if matches!(cause.raw_os_error(), Some(libc::ESRCH | libc::EPERM)) {
        Ok(None)
    } else {
        Err(cause)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_full_listing_is_retried_and_padding_is_removed() {
        let mut calls = 0;
        let members = list_with(|buffer| {
            calls += 1;
            match calls {
                1 => {
                    assert_eq!(buffer.len(), 32);
                    Ok(size_of_val(buffer))
                }
                2 => {
                    assert_eq!(buffer.len(), 64);
                    buffer[..4].copy_from_slice(&[41, 42, 0, 43]);
                    Ok(4 * size_of::<i32>())
                }
                _ => panic!("unexpected listing"),
            }
        })
        .unwrap();
        assert_eq!(members, [41, 42, 43]);
        assert_eq!(calls, 2);
    }

    #[test]
    fn only_vanished_or_inaccessible_processes_are_skipped() {
        assert_eq!(unreadable_or_error(io::Error::from_raw_os_error(libc::ESRCH)).unwrap(), None);
        assert_eq!(unreadable_or_error(io::Error::from_raw_os_error(libc::EPERM)).unwrap(), None);
        let error = unreadable_or_error(io::Error::from_raw_os_error(libc::EIO)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }

    #[test]
    fn listing_errors_are_preserved() {
        let error = list_with(|_| Err(io::Error::from_raw_os_error(libc::EPERM))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    }

    #[test]
    fn a_zero_listing_count_distinguishes_failure_from_an_empty_group() {
        assert_eq!(listing_result(0, 0).unwrap(), 0);
        assert_eq!(listing_result(8, libc::EPERM).unwrap(), 8);
        assert_eq!(listing_result(0, libc::EPERM).unwrap_err().raw_os_error(), Some(libc::EPERM));
        assert_eq!(listing_result(-1, libc::EPERM).unwrap_err().raw_os_error(), Some(libc::EPERM));
    }
}
