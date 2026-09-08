// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(all(test, miri)))]
#![doc(hidden)]
#![forbid(
    unsafe_code,
    reason = "raw platform calls stay in `cargo-gamma-unsafe`; this crate only composes its safe interfaces"
)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

//! This crate is an internal implementation detail of
//! [`cargo-gamma`](https://crates.io/crates/cargo-gamma). It contains cargo-gamma's bounded
//! process-tree containment, accounting, observation, and termination lifecycle.
//!
//! Do not depend on it directly. Its API may change incompatibly without notice; it is published
//! only so that `cargo-gamma` can be installed through crates.io.
//!
//! # Process-tree lifecycle
//!
//! Killing the process a run started is not enough. A test that shells out to a server, a database
//! or another build leaves those behind when the harness above them is cut off, and they take two
//! things with them: file locks inside the scratch tree, which turn the next run into a failure
//! that has nothing to do with any mutant, and inherited pipe handles, which keep whoever is
//! reading this tool's output from ever seeing end of file. A run that ends with a hung consumer is
//! worse than one that ends with a wrong verdict, because nobody can even see the verdict.
//!
//! Both platforms have a way to say "this process and everything descended from it" — a process
//! group on Unix and a job object on Windows — but neither is reachable from `std`. The raw calls
//! live in `cargo-gamma-unsafe`, which exposes safe interfaces; this crate composes them into the
//! lifecycle used by the rest of the tool.
//!
//! The same boundary accounts for memory because it is the only place that knows the whole process
//! tree rather than only its leader. A [`MemoryRequest`] passed to [`prepare`] asks for measurement,
//! a ceiling, or neither; [`ProcessTree::usage`] answers once the process tree is gone. On Windows
//! requested accounting requires a dedicated job carrying the limit and accounting; on Linux a
//! cgroup leaf supplied by `cargo-gamma-unsafe` serves both purposes.
//!
//! Containment itself is not conditional on that request. A Unix process group is escapable — a
//! descendant that calls `setsid` leaves it, and every later signal to the group misses it — so
//! every Windows launch enters a job and every Linux launch attempts to enter a cgroup leaf whether
//! or not anything is being measured. An unmetered Linux launch may use best-effort process-group
//! containment only when the host has no supported cgroup facility. Sealed containment means a
//! boundary descendants cannot leave: [`containment`] reports when the host cannot provide one
//! before repository-controlled code executes, and [`ProcessTree::sealed`] reports the resulting
//! state for one launch.
//!
//! The boundary is in force from the child's first instruction. On Linux the child moves itself
//! into the cgroup between fork and exec; on Windows it normally starts suspended, enters the
//! dedicated job, and only then runs. A child that cannot enter a job already created for it is
//! rejected because an inherited job cannot be opened later to terminate its descendants. A peak
//! that reached the limit is therefore enforced by the kernel rather than inferred after the fact.
//!
//! Because the Linux boundary is installed as a pre-exec step naming one particular leaf, and a
//! [`Command`](std::process::Command) accumulates every such step it is given, a command can only
//! be prepared once. That is stated in the types rather than checked: [`prepare`] consumes the
//! command and returns a [`PreparedCommand`], whose consuming spawn advances to
//! [`SpawnedCommand`]. A failed spawn returns [`SpawnFailure`] with the preparation intact. The
//! caller classifies its underlying operating-system error, retries transient resource-related
//! spawn failures after [`PreparedCommand::backoff`], and propagates permanent failures. A
//! successful spawn can only be surrendered to [`ProcessTree::adopt`], so one preparation cannot
//! leave an earlier child outside containment and launch another; dropping the post-spawn state
//! before adoption terminates and reaps that child.
//!
//! [`output`] is the contained counterpart of
//! [`Command::output`](std::process::Command::output). It drains stdout and stderr concurrently,
//! then sweeps descendants before waiting for inherited pipe handles to close.
//!
//! A terminal delivers `Ctrl-C` to the whole foreground process group, so a child sharing this
//! process's group dies with it automatically while a child leading its own group does not. Windows
//! normally preserves that guarantee through a dedicated job that dies with its last handle. Unix
//! installs explicit interruption handling through `cargo-gamma-unsafe`.

pub use cargo_gamma_unsafe::{PlatformError, Situation, support};
#[doc(inline)]
pub use memory_request::MemoryRequest;
#[doc(inline)]
pub use memory_usage::MemoryUsage;
#[doc(inline)]
pub use process_tree::{OutputError, PreparedCommand, ProcessTree, SpawnFailure, SpawnedCommand, capacity, containment, output, prepare};

mod memory_request;
mod memory_usage;
mod process_tree;

#[cfg(any(test, feature = "fault-injection"))]
pub mod faults;
#[cfg(test)]
mod testing;

#[cfg(test)]
mod unwind_contracts {
    use core::panic::{RefUnwindSafe, UnwindSafe};

    use super::{ProcessTree, SpawnedCommand};

    fn assert_unwind_safe<T: UnwindSafe + RefUnwindSafe>() {}

    #[test]
    fn public_launch_states_are_unwind_safe() {
        assert_unwind_safe::<SpawnedCommand>();
        assert_unwind_safe::<ProcessTree>();
    }
}
