// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only failures at process lifecycle boundaries the host cannot refuse on demand.

use core::cell::RefCell;
use core::time::Duration;
use std::time::Instant;

/// A process lifecycle operation a test can ask to fail once on its own thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// Moving an already-spawned child into its containment boundary is refused.
    Adopt,

    /// Installing containment before a spawn is refused.
    Prepare,

    /// Creating a requested accounting boundary is refused.
    Boundary,

    /// The spawn window refuses to open.
    Window,

    /// Terminating a contained subtree reports a cleanup failure.
    Terminate,
}

/// Arms `fault` on this thread until the returned value is dropped.
#[must_use = "the fault is disarmed as soon as this is dropped"]
pub fn arm(fault: Fault) -> Armed {
    ripe_at(fault, Instant::now())
}

/// Arms `fault` on this thread after `delay` has elapsed.
#[must_use = "the fault is disarmed as soon as this is dropped"]
pub fn arm_late(fault: Fault, delay: Duration) -> Armed {
    ripe_at(fault, Instant::now() + delay)
}

fn ripe_at(fault: Fault, ripe: Instant) -> Armed {
    ARMED.with_borrow_mut(|armed| armed.push((fault, ripe)));

    Armed(fault)
}

pub(crate) fn fired(fault: Fault) -> bool {
    fired_at(fault, Instant::now())
}

fn fired_at(fault: Fault, now: Instant) -> bool {
    ARMED.with_borrow_mut(|armed| {
        armed
            .iter()
            .position(|&(candidate, ripe)| candidate == fault && ripe <= now)
            .map(|at| armed.remove(at))
            .is_some()
    })
}

fn disarm(fault: Fault) {
    ARMED.with_borrow_mut(|armed| {
        if let Some(at) = armed.iter().position(|&(candidate, _ripe)| candidate == fault) {
            let _spent = armed.remove(at);
        }
    });
}

/// Keeps one fault armed until it fires or this guard is dropped.
#[derive(Debug)]
pub struct Armed(Fault);

impl Drop for Armed {
    fn drop(&mut self) {
        disarm(self.0);
    }
}

thread_local! {
    static ARMED: RefCell<Vec<(Fault, Instant)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fault_fires_once() {
        let armed = arm(Fault::Adopt);

        assert!(fired(Fault::Adopt));
        assert!(!fired(Fault::Adopt));

        drop(armed);
    }

    #[test]
    fn nothing_fires_unasked() {
        assert!(!fired(Fault::Adopt));
        assert!(!fired(Fault::Prepare));
        assert!(!fired(Fault::Boundary));
        assert!(!fired(Fault::Window));
        assert!(!fired(Fault::Terminate));
    }

    #[test]
    fn one_fault_does_not_stand_in_for_another() {
        let _armed = arm(Fault::Prepare);

        assert!(!fired(Fault::Adopt));
        assert!(!fired(Fault::Boundary));
        assert!(fired(Fault::Prepare));
    }

    #[test]
    fn an_unfired_fault_does_not_outlive_its_guard() {
        drop(arm(Fault::Prepare));

        assert!(!fired(Fault::Prepare));
    }

    #[test]
    fn faults_do_not_displace_each_other() {
        let _adopt = arm(Fault::Adopt);
        let _window = arm(Fault::Window);

        assert!(fired(Fault::Window));
        assert!(fired(Fault::Adopt));
    }

    #[test]
    fn faults_are_thread_local() {
        let _armed = arm(Fault::Adopt);
        let elsewhere = std::thread::spawn(|| fired(Fault::Adopt)).join().expect("the probe thread");

        assert!(!elsewhere);
        assert!(fired(Fault::Adopt));
    }

    #[test]
    fn a_delayed_fault_waits_and_then_fires_once() {
        let now = Instant::now();
        let ripe = now + Duration::from_millis(25);
        let _armed = ripe_at(Fault::Window, ripe);

        assert!(!fired_at(Fault::Window, now));
        assert!(fired_at(Fault::Window, ripe));
        assert!(!fired_at(Fault::Window, ripe));
    }

    #[test]
    fn a_delayed_fault_that_never_fired_does_not_outlive_its_guard() {
        drop(arm_late(Fault::Window, Duration::from_millis(1)));

        std::thread::sleep(Duration::from_millis(5));

        assert!(!fired(Fault::Window));
    }
}
