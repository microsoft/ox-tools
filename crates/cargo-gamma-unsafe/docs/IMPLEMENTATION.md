# cargo-gamma-unsafe — Implementation guide

This guide records platform mechanics behind
[`DESIGN.md`](DESIGN.md).

## Unix interrupt registry

The signal handler uses fixed arrays of atomics because it cannot allocate or
lock. Process-group identifiers and Linux cgroup kill descriptors occupy
separate registries: the leader's numeric group can be released when reaped,
while the descriptor remains registered until its owning `Cgroup` is dropped.
The cgroup registration is one-shot and non-copying. Its drop removes the
descriptor, waits for active sweeps, and only then permits the owned file to
close.

Group release likewise waits for extracted kills, not merely slot removal.
All lifecycle callers must release before reaping the group leader. The sweep
counter is announced before extraction, so a release that clears the slot either
prevents extraction or observes the outstanding sweep before allowing reuse.

Spawn windows and sweeps share an atomic owner word. Its high bit latches
interruption; the low bits count cleanup owners. The last-owner decrement tests
the flag in the *same* atomic snapshot. Loading the signal after a separate
last-owner decision would let a calm final-close wrongly take fatal responsibility
for a later handler whose effects are still pending. Each handler owns cleanup
before it publishes the signal and latches the flag, and releases ownership only
after all effects complete. Handlers never wait: a nested handler must return to
the interrupted owner rather than deadlock on it. Only ordinary lifecycle release
waits for active sweeps.

Production signal-handler tests execute in child processes so installed
handlers and process-global registries cannot leak into unrelated tests.
Registry and cgroup unit tests use isolated registries and recording killers;
they never signal fabricated process groups from the main test process.
The saturation oracle fills every slot, rejects an extra registration, then
interrupts the fake registry and compares the complete recorded identity list.
The quiescence unit regression scripts an outstanding effect through the registry's
existing wait callback: it asserts slot clearing, exactly one wait notification,
effect completion, reuse, and a notification-free quiescent release. It has no
thread rendezvous or clock dependency. This is a decision oracle, not proof of
native cross-thread termination. The existing parallel claim/sweep smoke remains,
and Loom's extracted-kill model explores release/reap/reuse across schedules.
Nested sweeper
and final-close regressions exercise same-thread nonblocking handoff for both
group and cgroup effects. Loom separately explores release/reuse, competing
sweepers, final close, cancellation, and eventual fatal responsibility.

The signal registry and its models compile only on Unix. The unsafe crate's
Windows Loom entry point is intentionally empty, so a successful Windows Loom
run does not validate this protocol.

## Linux cgroups

Creation opens `cgroup.kill` before launch and installs a `pre_exec` write to
`cgroup.procs`. A shared reaper retries directory removal after bounded
foreground cleanup. Leaf names include creator identity so stale leaves can be
distinguished from live ownership.

## Windows jobs

Private backend functions isolate Win32 calls from the ownership algorithms.
Unit tests inject per-thread failures at selected calls while retaining the same
job, process, completion-port, and handle lifetimes as production.

### Native test oracles and isolation

| Family in `job.rs::tests` | Break rejected by the oracle | Native-state isolation |
| --- | --- | --- |
| Limit conversion, flags, ownership and completion mapping | Wrong byte width, policy bits, requested thread, message/key/payload, or sticky exhaustion | Local values or independently owned handles; completion packets use owned ports |
| System snapshot and resume | Truncated enumeration, wrong thread, excess access rights, premature execution, or wrong previous suspension count | Two known live owned threads must appear; disposable suspended children and owned snapshot/thread handles |
| Drop, termination and accounting | Natural child completion substituted for cleanup, zero peak substituted for measured growth, or ordinary failure relabelled exhaustion | The last-handle integration child announces readiness and blocks on parent-held stdin; returning from that gate uses a distinct failure status. Native termination smoke asserts its requested exit code, while controlled call/result tests assert the termination decision |
| Creation, assignment, resume and query failures | Partial jobs accepted, errors swallowed, invented accounting, or handles leaked | Faults are thread-local; process handle-count loops re-execute one exact test and verify it ran |
| Error-mode policy | Policy not installed, or a renamed filter executing zero tests | A separate child owns its process-wide mode and emits a required success marker |
| Unassigned-child failure cleanup (`tests/job.rs`) | An assertion before job assignment leaks a suspended child | The same private `TestChild` used by the unit tests kills/reaps on drop; an integration unwind regression queries an independent owned process handle |
| Actual memory-limit violation | Merely posting a synthetic event mistaken for kernel enforcement | Explicitly ignored allocation child in its own limited job; successful child completion and observed exhaustion are both required |

These are source-level oracle contracts, not claims that native tests passed on
a host without the required Windows capabilities.

The resource-boundary and job-close regressions live in the `job` integration
target, using existing public APIs and a shared private child owner; no default
test API is exported. Its native smoke tests and subprocess helper are ignored
in ordinary required runs. Run them manually with
`cargo test -p cargo-gamma-unsafe --test job -- --ignored`. A runner pause can
race watchdog cancellation on resume, so these are opt-in diagnostics rather
than required CI gates. Readiness is an exact child message, and death is the
kernel exit status, never a timed poll. The job-close and unassigned-child smoke
and native resume-failure, membership, and termination scenarios return before
creating any native resources when the runtime's startup-captured mutation ID
is active. The child helper also returns before reading stdin in that mode.
Termination success/failure, exact handle
dispatch, error propagation, close dispatch, and the installed kill-on-close
policy remain covered by unit oracles under mutation testing.

Native smoke alone has a separate five-second last-chance watchdog, implemented
outside `src/` and never armed for an active mutant. It owns a duplicated process
handle, kills a stuck child, waits a bounded interval for cleanup, and aborts a
stuck harness. Expiry is a harness failure, not the oracle for notification or
termination. The normal path cancels and joins the watchdog; each child owner
still kills and reaps on unwind. These smoke checks exercise native behavior;
the controlled unit cases do not substitute for that coverage.
