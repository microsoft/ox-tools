// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::windows::io::{AsHandle as _, AsRawHandle as _};
use std::process::Child;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

/// Harness safety only: native smoke must never supply a mutant's verdict.
pub(crate) fn native_smoke_enabled() -> bool {
    gamma_rt::active() == gamma_rt::NONE
}

/// Bounds native smoke independently of its correctness assertions.
pub(crate) struct Watchdog {
    cancel: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Watchdog {
    pub(crate) fn new(child: &Child) -> Self {
        assert!(native_smoke_enabled(), "native watchdogs must not run under mutation testing");
        let process = child.as_handle().try_clone_to_owned().expect("retain the watchdog process handle");
        let (cancel, cancelled) = channel();
        let worker = thread::spawn(move || {
            if cancelled.recv_timeout(Duration::from_secs(5)) == Err(RecvTimeoutError::Timeout) {
                // SAFETY: the independently owned process handle stays live through both calls.
                let _killed = unsafe { TerminateProcess(process.as_raw_handle(), 79) };
                // Give cleanup a bounded chance to finish before ending this stuck harness.
                // SAFETY: the handle still names the owned child, not a potentially reused PID.
                let _reaped = unsafe { WaitForSingleObject(process.as_raw_handle(), 5_000) } == WAIT_OBJECT_0;
                eprintln!("native job smoke watchdog expired; terminating the stuck test harness");
                std::process::abort();
            }
        });
        Self {
            cancel,
            worker: Some(worker),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _sent = self.cancel.send(());
        if let Some(worker) = self.worker.take() {
            worker.join().expect("the cancelled native watchdog completes");
        }
    }
}
