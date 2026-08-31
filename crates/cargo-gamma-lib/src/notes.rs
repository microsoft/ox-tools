// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Diagnostics raised from below the output seam.
//!
//! Everything the tool says to the user goes out through a [`Host`], which the caller supplies and
//! a test can stand in for. A few places have something worth saying and no `Host` within reach:
//! the cargo command line is assembled several layers down from any caller, and a verdict is
//! reached on a worker thread while the progress display owns the terminal. Writing to `stderr`
//! from there bypasses whatever the `Host` does about colour, about width and about the progress
//! display, and makes the message invisible to any test that captures output the way the rest of
//! the tool is captured.
//!
//! [`Host`]: crate::Host

use core::cell::RefCell;
use core::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// How many notes are kept before further ones are counted rather than stored.
const LIMIT: usize = 64;

/// The notes raised by one invocation of the library.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    pending: Arc<Mutex<Pending>>,
}

impl Run {
    pub(crate) fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Pending::new())),
        }
    }
}

thread_local! {
    /// The run which owns notes raised by this thread.
    static ACTIVE: RefCell<Option<Run>> = const { RefCell::new(None) };
}

/// Installs a run for notes raised on this thread until the guard is dropped.
pub(crate) fn enter(run: Option<&Run>) -> Scope {
    let previous = ACTIVE.with(|active| active.replace(run.cloned()));

    Scope {
        previous,
        // A scope is tied to the thread that installed it. Moving it would restore a different
        // thread's state when dropped.
        not_send: PhantomData,
    }
}

/// Restores the note run that preceded [`enter`].
pub(crate) struct Scope {
    previous: Option<Run>,
    not_send: PhantomData<Rc<()>>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let _previous = active.replace(self.previous.take());
        });
    }
}

/// The run installed on the current thread, for a worker thread to inherit explicitly.
pub(crate) fn current() -> Option<Run> {
    ACTIVE.with(|active| active.borrow().clone())
}

/// What has been raised and not yet drained.
#[derive(Debug)]
struct Pending {
    /// The notes, in the order they were raised, capped at [`LIMIT`].
    lines: Vec<String>,

    /// How many notes were raised after the cap was reached.
    dropped: usize,
}

impl Pending {
    const fn new() -> Self {
        Self {
            lines: Vec::new(),
            dropped: 0,
        }
    }
}

/// Raises one diagnostic line for the running command to say when it can.
///
/// A note without an active run has no host that can say it, so it is ignored. A poisoned lock is
/// ignored for the same reason: failing a run over a courtesy diagnostic makes that diagnostic the
/// reason the command stopped.
pub(crate) fn note(message: impl Into<String>) {
    let Some(run) = current() else {
        return;
    };
    let Ok(mut pending) = run.pending.lock() else {
        return;
    };

    if pending.lines.len() < LIMIT {
        pending.lines.push(message.into());
    } else {
        pending.dropped += 1;
    }
}

/// Takes this thread's running command's notes, leaving its buffer empty.
#[must_use]
pub(crate) fn drain() -> Vec<String> {
    let Some(run) = current() else {
        return Vec::new();
    };
    let Ok(mut pending) = run.pending.lock() else {
        return Vec::new();
    };

    let mut lines = core::mem::take(&mut pending.lines);
    let dropped = core::mem::take(&mut pending.dropped);

    if dropped > 0 {
        lines.push(format!("and {} more like these", crate::report::quantity(dropped, "diagnostic")));
    }

    lines
}

/// Runs `body` with a new, exclusive note run.
#[cfg(test)]
pub(crate) fn alone<T>(body: impl FnOnce() -> T) -> T {
    static EXCLUSIVE: Mutex<()> = Mutex::new(());

    let _guard = EXCLUSIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let run = Run::new();
    let _scope = enter(Some(&run));
    let _discarded = drain();

    let outcome = body();

    let _discarded = drain();

    outcome
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::*;

    #[test]
    fn notes_come_back_in_the_order_they_were_raised() {
        alone(|| {
            note("first");
            note("second");

            assert_eq!(drain(), vec!["first".to_owned(), "second".to_owned()]);
        });
    }

    #[test]
    fn draining_empties_the_buffer() {
        alone(|| {
            note("said once");

            assert_eq!(drain().len(), 1);
            assert!(drain().is_empty());
        });
    }

    #[test]
    fn notes_past_the_cap_are_counted_rather_than_kept() {
        alone(|| {
            for index in 0..LIMIT + 5 {
                note(format!("note {index}"));
            }

            let drained = drain();

            assert_eq!(drained.len(), LIMIT + 1);
            assert_eq!(drained[0], "note 0");
            assert_eq!(drained[LIMIT - 1], format!("note {}", LIMIT - 1));
            assert_eq!(drained[LIMIT], "and 5 diagnostics more like these");
        });
    }

    #[test]
    fn a_process_that_raised_nothing_drains_nothing() {
        alone(|| assert!(drain().is_empty()));
    }

    #[test]
    fn concurrent_runs_drain_only_their_own_notes() {
        let first = Run::new();
        let second = Run::new();
        let ready = Barrier::new(2);

        std::thread::scope(|scope| {
            let first_ready = &ready;
            let first = first.clone();
            let left = scope.spawn(move || {
                let _scope = enter(Some(&first));
                note("first");
                let _ready = first_ready.wait();
                drain()
            });

            let second_ready = &ready;
            let second = second.clone();
            let right = scope.spawn(move || {
                let _scope = enter(Some(&second));
                note("second");
                let _ready = second_ready.wait();
                drain()
            });

            assert_eq!(left.join().expect("first run"), vec!["first"]);
            assert_eq!(right.join().expect("second run"), vec!["second"]);
        });
    }
}
