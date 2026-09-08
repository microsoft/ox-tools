// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only failures supplied by the fake Win32-call backend.
//!
//! Every one of these calls can fail on a real machine — a handle quota reached, a thread that
//! ended between the snapshot and the open, a job the process is no longer permitted to configure —
//! and none of them can be made to fail on demand. Their error arms are therefore the part of
//! containment least likely to have ever executed, and the part where a mistake is quietest: a
//! leaked handle, a child left suspended forever, or a boundary the caller was told it had.
//!
//! Armed per thread and spent when it fires, so one test arms one failure and the tests that run
//! beside it are unaffected.

use core::cell::RefCell;
use std::thread;

/// A Win32 call a test can ask to fail once on its own thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeCall {
    /// The thread snapshot a suspended child is found through cannot be taken.
    Snapshot,

    /// The snapshot can be taken but lists no thread, so the child's own is never found.
    ThreadEnumeration,

    /// The child's thread cannot be opened for resumption.
    OpenThread,

    /// The child's thread cannot be brought out of suspension.
    ResumeThread,

    /// The job object itself cannot be created.
    CreateJob,

    /// The completion port a memory ceiling reports through cannot be created.
    CompletionPort,

    /// The completion port cannot be associated with the job.
    AssociatePort,

    /// The job's limits and policy flags cannot be installed.
    ConfigureJob,

    /// The job's peak-memory accounting cannot be queried.
    QueryAccounting,

    /// The completion port cannot be checked for a memory-limit notification.
    CompletionStatus,

    /// The job refuses the child it was created for.
    AssignProcess,

    /// The job cannot be terminated.
    TerminateJob,
}

/// Arms `call` on this thread until the returned value is dropped.
#[must_use = "the fault is disarmed as soon as this is dropped"]
pub(crate) fn arm(call: NativeCall) -> Armed {
    ARMED.with_borrow_mut(|armed| armed.push(call));

    Armed(call)
}

/// Whether a test asked this call to fail, spending the request if so.
pub(crate) fn fired(call: NativeCall) -> bool {
    ARMED.with_borrow_mut(|armed| {
        armed
            .iter()
            .position(|candidate| *candidate == call)
            .map(|at| armed.remove(at))
            .is_some()
    })
}

/// Keeps one fault armed until it fires or this guard is dropped.
#[derive(Debug)]
pub(crate) struct Armed(NativeCall);

impl Drop for Armed {
    fn drop(&mut self) {
        ARMED.with_borrow_mut(|armed| {
            if let Some(at) = armed.iter().position(|candidate| *candidate == self.0) {
                let _spent = armed.remove(at);
            }
        });
    }
}

thread_local! {
    static ARMED: RefCell<Vec<NativeCall>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_fires_once() {
        let armed = arm(NativeCall::CreateJob);

        assert!(fired(NativeCall::CreateJob));
        assert!(!fired(NativeCall::CreateJob));

        drop(armed);
    }

    #[test]
    fn nothing_fires_unasked() {
        assert!(!fired(NativeCall::Snapshot));
        assert!(!fired(NativeCall::AssignProcess));
    }

    #[test]
    fn one_call_does_not_stand_in_for_another() {
        let _armed = arm(NativeCall::OpenThread);

        assert!(!fired(NativeCall::ResumeThread));
        assert!(fired(NativeCall::OpenThread));
    }

    #[test]
    fn an_unfired_fault_does_not_outlive_its_guard() {
        drop(arm(NativeCall::ConfigureJob));

        assert!(!fired(NativeCall::ConfigureJob));
    }

    #[test]
    fn faults_are_thread_local() {
        let _armed = arm(NativeCall::TerminateJob);
        let elsewhere = thread::spawn(|| fired(NativeCall::TerminateJob)).join().expect("the probe thread");

        assert!(!elsewhere, "a fault armed here fired on another thread");
        assert!(fired(NativeCall::TerminateJob));
    }
}
