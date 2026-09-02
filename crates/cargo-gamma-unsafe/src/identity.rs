// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Asking the kernel which user this process acts as.
//!
//! Needed to decide whether a directory handed to the tool is one only the invoking user can write
//! to. `std` exposes the owner of a file — [`std::os::unix::fs::MetadataExt::uid`] — but not the
//! identity to compare it against, so the one call that answers that question is made here.

/// The effective user this process acts as.
///
/// Effective rather than real, because it is the effective identity the kernel checks every open
/// and every write against; a comparison against the real one would describe a permission decision
/// nobody makes.
#[must_use]
pub fn effective_user() -> u32 {
    // SAFETY: `geteuid` reads a field of the calling process's own credentials. POSIX specifies it
    // as always succeeding, it takes no arguments, touches no memory the caller owns, and is one of
    // the handful of calls that is async-signal-safe and thread-safe by specification.
    unsafe { libc::geteuid() }
}
