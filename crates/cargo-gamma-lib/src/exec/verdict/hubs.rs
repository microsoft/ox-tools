// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The two pure synchronization hubs the reader and watchdog threads coordinate on.
//!
//! [`Pulse`] is the notify/wait cell that lets a reader thread wake the thread waiting on the child
//! the instant a pipe closes or a failure is announced, instead of on a polling timer. [`Readers`]
//! is the process-wide gauge that counts how many reader threads are live and how high that count
//! has ever risen. Neither touches a pipe or a process directly, which is what lets them be modelled
//! in isolation.
//!
//! # Why this is a separate module
//!
//! Everything else in `verdict` is entangled with real child processes and real pipes, which
//! [`loom`](https://docs.rs/loom) cannot drive — it replaces the threads and the atomics with a
//! deterministic scheduler that has no I/O. The synchronization these two types perform is *argued*
//! sound, but a mutation to a memory ordering or the lock discipline needs a schedule the ordinary
//! tests never force, so the suite would stay green. Pulling the pure pieces here lets a loom model
//! exercise them under every interleaving.
//!
//! # The loom shim
//!
//! Under `--cfg loom` the primitives below resolve to `loom`'s instrumented equivalents; otherwise
//! they are the ordinary standard-library types. The swap is invisible in an ordinary build: same
//! types, same code, no cost. See the `loom_models` module at the foot of this file for the models
//! and for the exact command that runs them.

#[cfg(not(loom))]
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
#[cfg(not(loom))]
use std::sync::{Condvar, Mutex};

#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::{Condvar, Mutex};

/// A wakeup shared between the reader threads and the thread waiting on the child.
///
/// Sleeping a fixed five milliseconds between polls would cost a binary that finished instantly a
/// full interval before the run noticed — once per reachable binary per mutant, which is the highest
/// multiplier in the tool. Against a suite that takes seconds this is nothing; against a unit-test
/// binary that runs in single-digit milliseconds it is a large fraction of the launch, and a
/// workspace of many small fast targets is exactly the shape where this tool is otherwise quickest.
///
/// A generation counter rather than a flag, because the wakeup must not be lost: the loop reads the
/// generation *before* it checks the child, so a signal raised while it was checking makes the wait
/// return at once instead of sleeping through news that had already arrived. Shrinking the interval
/// instead would trade the latency for a spin that steals cores from every other job.
#[derive(Default)]
pub(super) struct Pulse {
    /// How many times something worth waking for has happened.
    generation: Mutex<u64>,

    /// Signalled on every change to `generation`.
    woken: Condvar,
}

impl Pulse {
    /// The current generation, to be passed to a later [`Pulse::wait`].
    pub(super) fn seen(&self) -> u64 {
        #[expect(clippy::unwrap_used, reason = "the waiter only panics if the whole process is unwinding")]
        let generation = self.generation.lock().unwrap();

        *generation
    }

    /// Records that something the waiter cares about has happened, and wakes it.
    pub(super) fn signal(&self) {
        #[expect(clippy::unwrap_used, reason = "the waiter only panics if the whole process is unwinding")]
        let mut generation = self.generation.lock().unwrap();

        *generation = generation.wrapping_add(1);

        drop(generation);

        self.woken.notify_all();
    }

    /// Sleeps until the generation moves past `seen`, or `upto` elapses, whichever is sooner.
    pub(super) fn wait(&self, seen: u64, upto: Duration) {
        #[expect(clippy::unwrap_used, reason = "the waiter only panics if the whole process is unwinding")]
        let generation = self.generation.lock().unwrap();

        // Already moved on while the caller was checking the child, so there is nothing to wait for.
        if *generation != seen {
            return;
        }

        #[cfg(not(loom))]
        #[expect(clippy::unwrap_used, reason = "the waiter only panics if the whole process is unwinding")]
        let (_generation, _timed_out) = self.woken.wait_timeout(generation, upto).unwrap();

        // loom has no clock, so it cannot model the timeout backstop firing. That is deliberate: the
        // wait is modelled as blocking until it is *signalled*, which forces the model to rely on
        // the generation guard above and the `notify_all` in `signal` — the actual mechanism — and
        // not on the cap silently rescuing a lost wakeup. `WAIT_CAP` in the parent module is a
        // backstop, not the mechanism, and the model proves the mechanism.
        #[cfg(loom)]
        {
            let _ = upto;

            #[expect(clippy::unwrap_used, reason = "the waiter only panics if the whole process is unwinding")]
            let _generation = self.woken.wait(generation).unwrap();
        }
    }
}

/// How many output readers are running, and how far that count has ever risen.
///
/// A reader is abandoned rather than joined when the bounded drain gives up on it: the thread is
/// still blocked in a read on a pipe whose write end some descendant of the test binary inherited
/// and never closed, so joining it would block the run forever. One per affected mutant is nothing,
/// but a sweep of thousands of mutants over a suite that habitually leaves a daemon behind
/// accumulates threads and descriptors, and descriptors have a hard ceiling.
///
/// Every exit now sweeps the subtree before draining, which closes those write ends in the ordinary
/// case and should make this stay at roughly two per running job. This counts rather than caps,
/// because whether anything still escapes is a question about a real suite that no amount of
/// reasoning about the code will answer — and a cap chosen without that number would either never
/// fire or stop runs that were never in trouble. `--diag` reports the peak, which is the number
/// that decides whether a bound is worth building.
#[derive(Debug)]
pub struct Readers {
    /// How many reader threads are running right now.
    live: AtomicUsize,

    /// The most that have ever been running at once.
    peak: AtomicUsize,
}

/// The run's reader gauge.
///
/// Global because what it measures is: threads and open descriptors belong to the process, not to
/// any one launch, and the accumulation this is here to detect is precisely the one that outlives
/// the mutant that caused it. Threading it through every launch path would say otherwise.
#[cfg(not(loom))]
pub static READERS: Readers = Readers::new();

// loom's atomics are not `const`-constructible, so the plain `static` above will not compile under
// `--cfg loom`. The gauge is never driven by a model — the models build their own fresh `Readers`
// inside `loom::model` — so this exists only to keep the crate compiling under the cfg. `--diag`
// and the reader tests never run in a loom build.
#[cfg(loom)]
loom::lazy_static! {
    pub static ref READERS: Readers = Readers::new();
}

impl Readers {
    /// A gauge that has seen nothing yet.
    #[cfg(not(loom))]
    pub(super) const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// A gauge that has seen nothing yet.
    #[cfg(loom)]
    pub(super) fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Notes a reader starting, and raises the peak if this is the most yet.
    pub(super) fn started(&self) {
        let live = self.live.fetch_add(1, Ordering::Relaxed).saturating_add(1);

        // Raced against other readers rather than locked: a peak that loses a race understates by
        // the width of that race, which is not worth a mutex on every stream of every mutant.
        let _raised = self.peak.fetch_max(live, Ordering::Relaxed);
    }

    /// Notes a reader finishing, whether it was waited for or abandoned.
    pub(super) fn finished(&self) {
        let _was = self.live.fetch_sub(1, Ordering::Relaxed);
    }

    /// How many reader threads are running right now.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    /// The most reader threads that have ever been running at once.
    ///
    /// Read at the end of a run. With `jobs` concurrent mutants and two streams apiece, a run that
    /// strands nothing peaks at about `2 * jobs`; anything far above that is the accumulation this
    /// exists to detect.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
}

/// Deterministic-scheduler models of the two hubs above, run under [`loom`](https://docs.rs/loom).
///
/// These are **not** part of the ordinary test suite. They only compile and run under `--cfg loom`,
/// where the standard-library primitives the hubs use are swapped for loom's instrumented ones and
/// every thread interleaving is explored exhaustively. That is slow, and it needs the whole crate
/// recompiled for the cfg, so it is kept off the default `cargo test` path.
///
/// Run them with:
///
/// ```text
/// RUSTFLAGS="--cfg loom" cargo test -p cargo-gamma-lib --lib hubs::loom_models
/// ```
///
/// Each model builds its own fresh state inside `loom::model` — loom re-runs the closure once per
/// schedule and forbids state that outlives an execution, so the global `READERS` static is never
/// used here.
#[cfg(loom)]
mod loom_models {
    use core::time::Duration;

    use loom::sync::Arc;
    use loom::sync::atomic::AtomicUsize;

    use super::{Pulse, Readers};

    /// The smallest topology that exercises a race between identical reader operations.
    const CONCURRENT_READERS: usize = 2;

    /// A wakeup is never lost, whatever order the waiter and the notifier run in.
    ///
    /// This models the waiter's `seen` → check-the-child → `wait` sequence racing both output
    /// readers. One reader announces the first failure and later reaches end-of-stream; the other
    /// also reaches end-of-stream, which is the maximum production signal pattern for one pulse.
    /// `seen` is captured before either notifier is spawned, so the signals can land before or
    /// during the wait. The waiter must always return; loom reports a deadlock if any interleaving
    /// leaves it blocked.
    ///
    /// This has teeth against three separate weakenings, each of which makes loom report the
    /// deadlock: dropping the `notify_all` in `signal`, dropping the generation increment in
    /// `signal`, and dropping the `*generation != seen` guard in `wait`.
    pub(super) fn a_pulse_wakeup_is_never_lost_under_any_interleaving() {
        loom::model(|| {
            let pulse = Arc::new(Pulse::default());
            let seen = pulse.seen();

            let announcing_reader = {
                let pulse = Arc::clone(&pulse);
                loom::thread::spawn(move || {
                    pulse.signal();
                    pulse.signal();
                })
            };
            let other_reader = {
                let pulse = Arc::clone(&pulse);
                loom::thread::spawn(move || pulse.signal())
            };

            pulse.wait(seen, Duration::from_secs(0));

            announcing_reader.join().unwrap();
            other_reader.join().unwrap();

            assert_eq!(pulse.seen(), seen.wrapping_add(3), "a concurrent signal was lost");
        });
    }

    /// The live count returns to exactly zero after concurrent readers finish.
    ///
    /// The model begins from the valid gauge state produced by prior successful starts, keeping the
    /// decrement race separate from the start race covered below. The contended `live` observation
    /// also models diagnostics racing abandoned readers as they finally close. Every prior
    /// increment must still have exactly one decrement.
    pub(super) fn a_readers_gauge_returns_to_exactly_zero_under_any_interleaving() {
        loom::model(|| {
            let readers = Arc::new(Readers {
                live: AtomicUsize::new(CONCURRENT_READERS),
                peak: AtomicUsize::new(CONCURRENT_READERS),
            });
            let mut finishers = Vec::with_capacity(CONCURRENT_READERS);

            for _reader in 0..CONCURRENT_READERS {
                let readers = Arc::clone(&readers);
                finishers.push(loom::thread::spawn(move || readers.finished()));
            }

            assert!(
                readers.live() <= CONCURRENT_READERS,
                "a contended live read exceeded the number of started readers"
            );

            for finisher in finishers {
                finisher.join().unwrap();
            }

            assert_eq!(readers.live(), 0, "a decrement was lost or double-counted");
        });
    }

    /// The peak never understates readers started concurrently.
    ///
    /// Participants start but do not finish, so each remains genuinely live. The observations
    /// before the joins race the `fetch_add` and `fetch_max` operations, exercising the production
    /// loads under contention; the observations after the joins must see the exact count and peak.
    pub(super) fn a_readers_peak_never_understates_concurrent_starts() {
        loom::model(|| {
            let readers = Arc::new(Readers::new());
            let mut workers = Vec::with_capacity(CONCURRENT_READERS);

            for _reader in 0..CONCURRENT_READERS {
                let readers = Arc::clone(&readers);
                workers.push(loom::thread::spawn(move || readers.started()));
            }

            assert!(
                readers.live() <= CONCURRENT_READERS,
                "a contended live read exceeded the number of starters"
            );
            assert!(
                readers.peak() <= CONCURRENT_READERS,
                "a contended peak read exceeded the number of starters"
            );

            for worker in workers {
                worker.join().unwrap();
            }

            assert_eq!(readers.live(), CONCURRENT_READERS, "a concurrent increment was lost");
            assert_eq!(readers.peak(), CONCURRENT_READERS, "the peak understated concurrent readers");
        });
    }
}

#[cfg(loom)]
pub(crate) fn run_loom_models() {
    loom_models::a_pulse_wakeup_is_never_lost_under_any_interleaving();
    loom_models::a_readers_gauge_returns_to_exactly_zero_under_any_interleaving();
    loom_models::a_readers_peak_never_understates_concurrent_starts();
}
