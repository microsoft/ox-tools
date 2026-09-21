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
//! `unsafe` to go faster — there is no safe version to prefer. The same applies to interruptible
//! reads from anonymous child pipes, which output capture uses to stop readers after a bounded
//! drain grace.
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
pub mod pipe;

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
