# cargo-gamma-process — Implementation guide

This guide records lifecycle mechanics behind [`DESIGN.md`](DESIGN.md).

## Launch ownership

`prepare` consumes `Command` and returns `PreparedCommand`. A failed spawn
returns the same preparation in `SpawnFailure`; a successful spawn returns
`SpawnedCommand`, which owns the child and containment until `ProcessTree::adopt`
consumes it. Dropping the successful pre-adoption state terminates and reaps the
child. Before calling the operating-system spawn, `PreparedCommand::spawn`
ensures the process-wide detached reaper is running. A failure to create that
thread therefore returns `SpawnFailure` with the preparation intact and no child
to recover.

## Output capture

The contained output path takes stdout and stderr exactly once and drains them
concurrently. It sweeps descendants after the leader exits so inherited write
ends do not keep readers open indefinitely.

## Bounded termination

`ProcessTree::terminate_bounded` requests the same subtree and leader kills as
ordinary termination, then polls `try_wait` until a caller-provided grace
expires. It never follows a failed kill with blocking `wait`: at the deadline
the leader handle moves to the shared detached reaper, the `ProcessTree` no
longer owns a child that Drop could wait for, and the original cleanup failure
is retained in the returned error. The reaper polls every retained child
without blocking on one leader and waits on a condition variable when its queue
is empty. It is durable: repository-controlled children are created only after
the thread exists, and direct `reap_later` startup failures return an unqueued
child in `ReapFailure` rather than abandoning ownership. Callers explicitly
recover that child with `ReapFailure::into_parts`; those whose local Drop path
would block transfer it to the process-wide retry queue. The retry queue is a
distinct owner, not evidence that a reaper is running. A live loop drains it
after notification, or a later successful startup drains it after startup
failure. Handoff rechecks the running state while holding the queue lock and
retries startup after a raced loop exit. A `try_wait` error also
transfers the still-owned leader handle before returning the observation error;
only a successful `Some(status)` proves that no later reaping is required. If a
later `try_wait` in the detached reaper is interrupted, the child remains queued
for another attempt. Any other observation error writes a warning to stderr and
permanently releases that child handle. The write is fallible, its error is
discarded, and it occurs outside the queue mutex. A lifecycle guard clears the
running state, returns active handles to the retry queue, and wakes readiness
waiters whenever the loop exits or unwinds. When retained handles remain, the
guard immediately starts a replacement, closing the window in which a handoff
could observe the old loop as running while diagnostics were outside the lock.
On Unix, the child may remain a zombie until this process exits.

## Platform composition

Unix launch preparation holds the interrupt spawn window only across child
creation and registration. Linux cgroups and Windows jobs are created before
that window opens. Fault injection substitutes failures at these lifecycle
boundaries without changing the production ownership transitions.
