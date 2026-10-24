// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Thread-local creation/configuration/cleanup script for the private job backend.
//! Tokens are distinct identities, not kernel handles: an installed script intercepts
//! every supported operation, including cleanup. Unscripted calls use the native backend.

use std::cell::RefCell;
use std::collections::VecDeque;

use windows_sys::Win32::Foundation::HANDLE;

pub(super) const JOB: HANDLE = core::ptr::without_provenance_mut(1);
pub(super) const COMPLETION: HANDLE = core::ptr::without_provenance_mut(2);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Call {
    CreateJob,
    CreateCompletionPort,
    Associate(HANDLE, HANDLE, usize),
    Configure(HANDLE, u32, usize),
    CloseJob(HANDLE),
    CloseCompletionPort(HANDLE),
}

thread_local! {
    static EXPECTED: RefCell<Option<VecDeque<Call>>> = const { RefCell::new(None) };
}

pub(super) fn call(actual: Call) -> bool {
    EXPECTED.with_borrow_mut(|script| {
        let Some(expected) = script else {
            return false;
        };
        // Cleanup must remain scripted without panicking again while unwinding a failed expectation.
        if !std::thread::panicking() {
            assert_eq!(expected.pop_front(), Some(actual), "unexpected job-creation call");
        }
        true
    })
}

pub(super) fn run(expected: impl IntoIterator<Item = Call>, operation: impl FnOnce()) {
    struct Restore(Option<VecDeque<Call>>);

    impl Drop for Restore {
        fn drop(&mut self) {
            let remaining = EXPECTED.with_borrow_mut(|script| std::mem::replace(script, self.0.take()));
            if !std::thread::panicking() {
                assert_eq!(remaining, Some(VecDeque::new()), "unconsumed job-creation calls");
            }
        }
    }

    let _restore = Restore(EXPECTED.with_borrow_mut(|script| script.replace(expected.into_iter().collect())));
    operation();
}
