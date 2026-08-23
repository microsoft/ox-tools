// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Direct tests for the process-group and signal helpers in `group.rs`.
//!
//! Every one of these calls is otherwise reached only incidentally, through integration paths that
//! assert an outcome several layers up. A mutant that signals the wrong group, probes the wrong
//! process, or inverts the liveness check would have to disturb one of those distant outcomes to be
//! caught, and mostly would not: a subtree that is not reaped leaks quietly, and a group probed
//! wrongly reports "gone" for a process that is still running. So each helper is exercised here for
//! its *observable effect* — a killed group's member disappears, the probe tells a live process
//! from a reaped one, the group query returns the group the child was placed in, and `raise`
//! actually delivers its signal.
//!
//! On a platform without process groups the whole `group` module is compiled away, so these tests
//! stand down loudly rather than pretending to pass silently.

#[cfg(unix)]
use core::time::Duration;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
use cargo_gamma_unsafe::group::{exists, exited, group_of, kill, raise_interrupt};

/// Spawns a long-lived child that has made itself the leader of a brand-new process group.
///
/// `setpgid(0, 0)` in the pre-exec hook means the child's group id equals its own pid, so the tests
/// can name a group that contains exactly this child and nothing of the harness's — killing it can
/// never touch this test process or any sibling test running in parallel.
#[cfg(unix)]
fn spawn_group_leader() -> Child {
    let mut command = Command::new("sleep");
    let _configured = command.arg("60");

    let make_leader = || {
        // SAFETY: `setpgid(0, 0)` takes two integers, allocates nothing and is async-signal-safe, so
        // it is legal in the post-fork/pre-exec window where this closure runs; it only moves the
        // child into a new group of its own and cannot affect this process.
        if unsafe { libc::setpgid(0, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    };

    // SAFETY: `pre_exec` runs `make_leader` in the forked child between fork and exec, and that
    // closure honours the async-signal-safety that this call requires of it.
    unsafe {
        let _hooked = command.pre_exec(make_leader);
    }

    command.spawn().expect("a `sleep` child spawns")
}

/// The pid of a child, as the signed id the group helpers speak in.
#[cfg(unix)]
fn pid_of(child: &Child) -> i32 {
    i32::try_from(child.id()).expect("a pid fits in an i32")
}

/// Polls `done` until it is satisfied or a generous deadline passes, without ever hanging.
///
/// The failure being guarded against — a child that is never reaped — does not finish in any budget,
/// so a bounded deadline turns "the kill did not work" into a red test rather than a wedged run. The
/// budget is far longer than reaping a `sleep` ever takes, so a busy machine does not fail spuriously.
#[cfg(unix)]
fn poll_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);

    while Instant::now() < deadline {
        if done() {
            return true;
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    done()
}

/// Killing a group makes the member that was placed in it disappear, terminated by the group kill.
#[cfg(unix)]
#[test]
fn killing_a_group_makes_its_members_disappear() {
    let mut child = spawn_group_leader();
    let pid = pid_of(&child);

    let group = group_of(pid).expect("the child leads a group of its own");
    assert_eq!(group, pid, "a fresh group leader's group id is its own pid");

    kill(group).expect("the group can be killed");

    let mut reaped = None;
    let gone = poll_until(|| {
        reaped = child.try_wait().expect("querying the child does not error");
        reaped.is_some()
    });
    assert!(gone, "the killed group's member is reaped within the deadline");

    let status = reaped.expect("a reaped status is available once the child is gone");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the member was terminated by the group kill, not by anything else"
    );
}

/// The signal-zero probe reports a running child alive and a killed, reaped one gone.
#[cfg(unix)]
#[test]
fn the_liveness_probe_tells_a_live_child_from_a_reaped_one() {
    let mut child = spawn_group_leader();
    let pid = pid_of(&child);

    assert!(exists(pid), "a running child is reported as existing");

    kill(group_of(pid).expect("the child leads a group of its own")).expect("the group can be killed");
    let _status = child.wait().expect("the killed child is reaped");

    assert!(!exists(pid), "a killed and reaped child is reported as gone");
}

/// A non-reaping observation keeps a group leader's identifier unavailable for reuse.
#[cfg(unix)]
#[test]
fn observing_an_exit_keeps_its_group_reserved_until_the_explicit_reap() {
    let mut child = spawn_group_leader();
    let pid = pid_of(&child);

    kill(group_of(pid).expect("the child leads a group of its own")).expect("the group can be killed");

    assert!(
        poll_until(|| exited(child.id()).expect("the child can be observed")),
        "the killed child never became observable"
    );
    assert_eq!(
        group_of(pid),
        Some(pid),
        "the leader's group must stay reserved until the caller explicitly reaps it"
    );

    let status = child.wait().expect("the observed child remains waitable");

    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_eq!(group_of(pid), None, "the group is released only by the explicit reap");
}

/// The group query returns the group the child put itself in, and nothing once the pid is gone.
#[cfg(unix)]
#[test]
fn the_group_query_returns_the_group_the_child_was_placed_in() {
    let mut child = spawn_group_leader();
    let pid = pid_of(&child);

    assert_eq!(
        group_of(pid),
        Some(pid),
        "the queried group is the one the child set for itself with `setpgid(0, 0)`"
    );

    kill(pid).expect("the group can be killed");
    let _status = child.wait().expect("the child is reaped");

    assert_eq!(group_of(pid), None, "querying a reaped pid yields no group rather than a stale one");
}

/// `raise_interrupt` really delivers `SIGINT` to the process that calls it.
///
/// Exercised in a forked child, never in the test process: raising `SIGINT` in the harness itself
/// could tear down the whole run. The child resets `SIGINT` to its default disposition first, so the
/// delivery is guaranteed to terminate it rather than run any handler inherited from the harness, and
/// the parent reads back the terminating signal to confirm the wrapper sent the signal it claims to.
#[cfg(unix)]
#[test]
fn raising_the_interrupt_delivers_sigint_to_the_caller() {
    // SAFETY: `fork` takes no arguments and touches no caller memory. The child below reaches only
    // async-signal-safe libc functions (`signal`, `raise` via `raise_interrupt`, `_exit`) before it
    // exits, so the usual hazard of forking a multi-threaded process — running non-async-signal-safe
    // code in the child — does not arise.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "the fork succeeds");

    if pid == 0 {
        // SAFETY: `signal` takes two integers, allocates nothing and is async-signal-safe; it only
        // restores the default disposition so the coming `SIGINT` terminates this child.
        unsafe {
            let _previous = libc::signal(libc::SIGINT, libc::SIG_DFL);
        }

        raise_interrupt();

        // SAFETY: only reached if the signal failed to terminate the child; `_exit` takes one
        // integer and is async-signal-safe, and exiting non-lethally lets the parent notice.
        unsafe {
            libc::_exit(0);
        }
    }

    let mut status: libc::c_int = 0;

    // SAFETY: `waitpid` writes only through `&mut status`, a valid local, and otherwise reads no
    // caller memory; it reaps the child forked above.
    let waited = unsafe { libc::waitpid(pid, &raw mut status, 0) };
    assert_eq!(waited, pid, "the forked child is reaped");

    assert!(
        libc::WIFSIGNALED(status),
        "the child was terminated by a signal rather than exiting normally"
    );
    assert_eq!(
        libc::WTERMSIG(status),
        libc::SIGINT,
        "the terminating signal was the SIGINT that `raise_interrupt` raised"
    );
}

/// Loud, uncaptured stand-down for platforms where `group` does not exist, so a skipped test is
/// visible in an ordinary run instead of masquerading as a pass.
#[cfg(not(unix))]
mod without_process_groups {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::io::{self, Write};

    /// How many of these tests have stood down, so the lines can be counted and told apart.
    static STOOD_DOWN: AtomicUsize = AtomicUsize::new(0);

    /// Writes the stand-down line straight to standard error, the way `without_memory_support` does,
    /// because libtest replays captured output only for tests that fail; a passing test has no other
    /// way to put a line in an ordinary run.
    fn standing_down(what: &str) -> bool {
        let count = STOOD_DOWN.fetch_add(1, Ordering::Relaxed) + 1;
        let line = format!(
            "standing down ({count}): {what} — this platform has no process groups, so \
             `cargo-gamma-unsafe::group` is not compiled and there is nothing to exercise\n"
        );
        let _written = io::stderr().write_all(line.as_bytes());

        true
    }

    #[test]
    fn killing_a_group_makes_its_members_disappear() {
        assert!(standing_down("killing a group makes its members disappear"));
    }

    #[test]
    fn the_liveness_probe_tells_a_live_child_from_a_reaped_one() {
        assert!(standing_down("the liveness probe tells a live child from a reaped one"));
    }

    #[test]
    fn observing_an_exit_keeps_its_group_reserved_until_the_explicit_reap() {
        assert!(standing_down("observing an exit while preserving its process group"));
    }

    #[test]
    fn the_group_query_returns_the_group_the_child_was_placed_in() {
        assert!(standing_down("the group query returns the group the child was placed in"));
    }

    #[test]
    fn raising_the_interrupt_delivers_sigint_to_the_caller() {
        assert!(standing_down("raising the interrupt delivers SIGINT to the caller"));
    }
}
