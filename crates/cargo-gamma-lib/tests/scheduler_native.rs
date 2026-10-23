// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

//! Opt-in native smoke; required scheduler decisions use controlled unit tests.
//!
//! Run with `cargo test -p cargo-gamma-lib --features internals --test scheduler_native -- --ignored`.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct Watchdog {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    fn new() -> Self {
        let (stop, stopped) = mpsc::channel();
        let worker = thread::spawn(move || {
            if stopped.recv_timeout(Duration::from_secs(10)).is_err() {
                eprintln!("native scheduler smoke watchdog: terminating a stuck test harness");
                std::process::abort();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let _sent = self.stop.send(());
        self.worker
            .take()
            .expect("the watchdog owns its worker until drop")
            .join()
            .expect("the watchdog either receives cancellation or aborts, never panics");
    }
}

#[test]
#[ignore = "manual native smoke uses a wall-clock watchdog"]
fn checkpoint_abandonment_broadcasts_to_native_resource_waiters() {
    if gamma_rt::active() != gamma_rt::NONE {
        return;
    }
    let _watchdog = Watchdog::new();
    cargo_gamma_lib::internals::exec::run_scheduler_native_smoke();
}
