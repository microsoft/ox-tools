// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Failures a test can ask for at boundaries the host will not fail on demand.
//!
//! Some machine failures cannot be provoked safely on demand: `waitpid` reports on a handle that is
//! gone, a process table fills, or a thread cannot be created. Each has a written response in this
//! crate, and arranging one for real would disrupt the test runner rather than isolate the branch
//! being tested. `cargo-gamma-process` owns process-lifecycle faults; this module owns failures at
//! library-level boundaries.
//!
//! This is the seam that makes them askable. A test arms one fault, on its own thread, for one
//! occurrence; the production code checks whether one is armed at the point the syscall would have
//! failed and takes exactly the branch it would have taken. The check is the *only* thing added to
//! the production path, and it is compiled away entirely outside `cfg(test)`, so the shipped binary
//! carries neither the branch nor the state behind it.
//!
//! Thread-local rather than global, and one-shot rather than sticky, for the same reason: the suite
//! runs in parallel in one process, and a fault left armed on a shared cell would surface in some
//! unrelated test as a failure nobody could reproduce. A thread that arms a fault is the only
//! thread that can fire it, and firing it disarms it.

use core::cell::RefCell;
use core::time::Duration;
use std::time::Instant;

/// A place a test can ask for a failure that the host would otherwise have to produce.
///
/// Each name is the syscall boundary it stands at, not the function that calls it, because the same
/// failure reaches several callers and a test wants to talk about the kernel's refusal rather than
/// about whichever path noticed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Fault {
    /// Asking after a running child fails, rather than reporting that it is still running.
    ///
    /// `waitpid` reports an error only when the handle itself is gone, which for a child this
    /// process spawned and has not reaped is not a state a test can otherwise arrange.
    Wait,

    /// Starting a child is refused for want of a machine resource.
    ///
    /// A full process or descriptor table under a `jobs`-wide sweep, which is a shortage the run
    /// creates for itself and recovers from as soon as one of the other workers finishes. Arranging
    /// it for real would mean exhausting a table the test harness is itself using.
    Spawn,

    /// Creating a pipe-reader thread is refused for want of process resources.
    Thread,

    /// Taking the advisory lock on the scratch directory fails outright.
    ///
    /// A filesystem that cannot lock at all — NFS without `lockd`, some CIFS mounts, `ENOLCK` under
    /// descriptor pressure — rather than a lock another run is holding. Reproducing it for real
    /// would mean mounting such a filesystem.
    Lock,
}

/// Arms `fault` on this thread until the value returned is dropped.
///
/// Held rather than fired-and-forgotten so that a test which panics before the fault is reached
/// still leaves the thread clean for whatever libtest runs on it next.
#[must_use = "the fault is disarmed as soon as this is dropped"]
pub(super) fn arm(fault: Fault) -> Armed {
    ripe_at(fault, Instant::now())
}

/// Arms `fault` on this thread, but not for the checks made in the next `after`.
///
/// Some responses are only worth asking about once the run has got somewhere. A wait that fails on
/// the first check fails before the child has finished starting, so a test of what the failure
/// takes down with it would be watching an empty subtree and would pass however little was killed.
/// Delaying the fault is what gives the child time to become the thing under test.
///
/// The delay is a floor and not a schedule: the fault fires at the first check after it, whenever
/// that comes. A test that depends on this asserts that the child got where it was going, rather
/// than assuming the machine was fast enough.
#[must_use = "the fault is disarmed as soon as this is dropped"]
pub(super) fn arm_late(fault: Fault, after: Duration) -> Armed {
    ripe_at(fault, Instant::now() + after)
}

/// Records `fault` as armed from `ripe` onwards.
fn ripe_at(fault: Fault, ripe: Instant) -> Armed {
    ARMED.with_borrow_mut(|armed| armed.push((fault, ripe)));

    Armed(fault)
}

/// Whether `fault` is armed on this thread, disarming it if it is.
///
/// One-shot, because every caller of this is inside a loop or a retry of some kind, and a fault
/// that stayed armed would turn a test of one refusal into a test of a host that refuses forever —
/// which is a different claim, and one no caller is written to survive.
pub(super) fn fired(fault: Fault) -> bool {
    let now = Instant::now();

    ARMED.with_borrow_mut(|armed| {
        armed
            .iter()
            .position(|&(candidate, ripe)| candidate == fault && ripe <= now)
            .map(|at| armed.remove(at))
            .is_some()
    })
}

/// Takes one arming of `fault` away, whether or not it was ever due to fire.
///
/// One rather than all of them, so that two guards for the same fault are spent one apiece and
/// dropping the first does not silently disarm the second.
fn disarm(fault: Fault) {
    ARMED.with_borrow_mut(|armed| {
        if let Some(at) = armed.iter().position(|&(candidate, _ripe)| candidate == fault) {
            let _spent = armed.remove(at);
        }
    });
}

/// Keeps a fault armed for as long as a test wants it, and takes it away afterwards.
#[derive(Debug)]
pub(super) struct Armed(Fault);

impl Drop for Armed {
    fn drop(&mut self) {
        disarm(self.0);
    }
}

thread_local! {
    /// What this thread has asked to fail, most recently armed last, each with the moment it is due.
    static ARMED: RefCell<Vec<(Fault, Instant)>> = const { RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An armed fault fires once and then is gone, so one request does not become a broken host.
    #[test]
    fn a_fault_fires_once() {
        let armed = arm(Fault::Wait);

        assert!(fired(Fault::Wait), "the fault was armed");
        assert!(!fired(Fault::Wait), "and firing it must have taken it away");

        drop(armed);
    }

    /// Nothing fires that was not asked for, which is what keeps the production check inert.
    #[test]
    fn nothing_fires_unasked() {
        assert!(!fired(Fault::Wait));
        assert!(!fired(Fault::Spawn));
    }

    /// Only the fault that was armed fires, so a test cannot accidentally prove the wrong branch.
    #[test]
    fn one_fault_does_not_stand_in_for_another() {
        let _armed = arm(Fault::Wait);

        assert!(!fired(Fault::Spawn), "arming one fault must not arm the others");
        assert!(!fired(Fault::Thread));
        assert!(fired(Fault::Wait));
    }

    /// A fault the test never reached is taken away when the guard drops.
    ///
    /// Without this, a test that panics on its way to the boundary — or simply changes its mind —
    /// would leave the fault armed on a thread libtest is about to hand to somebody else, and the
    /// failure would surface in an unrelated test that has no way to explain it.
    #[test]
    fn an_unfired_fault_does_not_outlive_its_guard() {
        drop(arm(Fault::Spawn));

        assert!(!fired(Fault::Spawn), "the guard must disarm what it armed");
    }

    /// Two faults can be armed at once, and each is spent separately.
    #[test]
    fn faults_do_not_displace_each_other() {
        let _spawn = arm(Fault::Spawn);
        let _wait = arm(Fault::Wait);

        assert!(fired(Fault::Wait));
        assert!(fired(Fault::Spawn));
    }

    /// A fault armed on one thread is not armed on another.
    ///
    /// The suite runs in parallel in one process, so a fault in shared state would fire inside
    /// whichever unrelated test happened to reach the same boundary first.
    #[test]
    fn a_fault_does_not_reach_another_thread() {
        let _armed = arm(Fault::Spawn);
        let elsewhere = std::thread::spawn(|| fired(Fault::Spawn)).join().expect("the probe thread");

        assert!(!elsewhere, "a fault must not escape the thread that armed it");
        assert!(fired(Fault::Spawn), "and must still be waiting on the thread that did");
    }

    /// A delayed fault is inert until its delay has passed, and fires once afterwards.
    ///
    /// Both halves matter to the tests that use it: firing early would put the fault back where it
    /// was, before the child under test has done anything, and never firing at all would turn the
    /// response being tested into code the suite silently skips.
    #[test]
    fn a_late_fault_waits_for_its_moment() {
        let _armed = arm_late(Fault::Wait, Duration::from_millis(50));

        assert!(!fired(Fault::Wait), "a fault that is not due yet must not fire");

        std::thread::sleep(Duration::from_millis(75));

        assert!(fired(Fault::Wait), "and must fire at the first check after it is");
        assert!(!fired(Fault::Wait), "then be spent like any other");
    }

    /// A delayed fault that never came due is still taken away with its guard.
    ///
    /// The one case the ordinary disarming would miss, since it is written in terms of firing and
    /// this fault is by construction not fireable yet.
    #[test]
    fn a_late_fault_that_never_fired_does_not_outlive_its_guard() {
        drop(arm_late(Fault::Spawn, Duration::from_millis(1)));

        std::thread::sleep(Duration::from_millis(5));

        assert!(!fired(Fault::Spawn), "an undue fault must not be left behind for the next test");
    }
}
