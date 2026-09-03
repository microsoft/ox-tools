// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![doc(hidden)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! Platform calls that [`cargo-gamma`](https://crates.io/crates/cargo-gamma) cannot make safely
//! are concentrated behind an interface that is safe to call. This crate is an implementation
//! detail of the tool; you should never need to depend on it directly.
//!
//! Two things the tool does have no safe expression in `std`: killing a whole process subtree (a
//! process group on Unix, a job object on Windows) and bounding what that subtree allocates (a
//! cgroup leaf on Linux, the same job object on Windows). Neither is a case of reaching for
//! `unsafe` to go faster — there is no safe version to prefer.
//!
//! Concentrating those calls here is what lets every other crate in the workspace carry
//! `#![forbid(unsafe_code)]`, which turns "we reviewed the unsafe code" into a property the
//! compiler checks on every build. `cargo-gamma-rt` is the one exception, and only because it is
//! injected into the dependency graph of the crate under test and so can depend on nothing at
//! all.
//!
//! Policy does not live here. What a memory ceiling *should* be is arithmetic on a baseline
//! measurement, and it stays in `cargo-gamma-lib` where it can be tested without a kernel. This
//! crate answers "what can the platform do, and do it"; its caller answers "what should we ask
//! for".
//
// Every raw platform call cargo-gamma makes, behind an interface that is safe to call.
//
// # Why this crate exists
//
// Two things the tool has to do have no safe expression in `std`.
//
// **Killing a whole subtree.** A test that shells out to a server, a database or another build
// leaves those behind when the harness above them is cut off. `std` can start a child and kill
// *that* child; it has no way to say "and everything descended from it". Both platforms do — a
// process group on Unix and a job object on Windows — and neither is reachable except through the
// C or Win32 interface.
//
// **Bounding what a subtree allocates.** The same boundary is the only place that can account for
// the whole tree rather than the one process at its root, which on Linux means a cgroup leaf and
// the `pre_exec` hook that puts the child in it before it runs.
//
// Neither is a case of reaching for `unsafe` to go faster. There is no safe version to prefer.
//
// # What this crate promises
//
// **Nothing here is `unsafe` to call.** Every entry point is a safe function, and every obligation
// the platform imposes is discharged inside this crate rather than passed to the caller — with no
// exception, since the one obligation that could not be discharged here, mutating the process
// environment on multithreaded Unix, is not offered at all: a value that has to reach a child
// belongs on that child's `Command`, not on this process.
//
// Concentrating it here is what lets every other crate in the workspace carry
// `#![forbid(unsafe_code)]`, which turns "we reviewed the unsafe code" into a property the compiler
// checks on every build. `cargo-gamma-rt` is the one exception, and only because it is injected
// into the dependency graph of the crate under test and so cannot depend on this crate — or on
// anything else.
//
// # What belongs here
//
// A raw platform call, and the smallest amount of logic needed to make it safe to expose. The
// bounded process lifecycle that composes these calls lives in `cargo-gamma-process`; policy does
// not belong in either crate. What a memory ceiling should be is arithmetic on a baseline
// measurement, and it stays in `cargo-gamma-lib` where it can be tested without a kernel.

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(unix)]
pub mod group;
#[cfg(unix)]
pub mod identity;
#[cfg(unix)]
pub mod interrupt;
#[cfg(windows)]
pub mod job;

#[cfg(all(windows, test))]
mod native_faults;
mod platform_error;
mod situation;
mod support;

#[doc(inline)]
pub use platform_error::PlatformError;
#[doc(inline)]
pub use situation::Situation;
#[doc(inline)]
pub use support::support;

/// Runs the deterministic concurrency models selected by the dedicated Loom test target.
#[cfg(all(loom, unix))]
#[doc(hidden)]
pub fn run_loom_models() {
    interrupt::run_loom_models();

    #[cfg(target_os = "linux")]
    cgroup::loom_models::run();
}

/// There are no Unix signal-registry models on non-Unix targets.
#[cfg(all(loom, not(unix)))]
#[doc(hidden)]
pub const fn run_loom_models() {}

#[cfg(test)]
mod unwind_contracts {
    use core::panic::{RefUnwindSafe, UnwindSafe};

    use crate::Situation;
    #[cfg(target_os = "linux")]
    use crate::cgroup::Cgroup;

    fn assert_unwind_safe<T: UnwindSafe + RefUnwindSafe>() {}

    #[test]
    fn public_value_types_are_unwind_safe() {
        assert_unwind_safe::<Situation>();

        #[cfg(target_os = "linux")]
        assert_unwind_safe::<Cgroup>();
    }
}
