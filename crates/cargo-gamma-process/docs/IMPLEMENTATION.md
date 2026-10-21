# cargo-gamma-process — Implementation guide

This guide records lifecycle mechanics behind [`DESIGN.md`](DESIGN.md).

## Launch ownership

`prepare` consumes `Command` and returns `PreparedCommand`. A failed spawn
returns the same preparation in `SpawnFailure`; a successful spawn returns
`SpawnedCommand`, which owns the child and containment until `ProcessTree::adopt`
consumes it. Dropping the successful pre-adoption state terminates and reaps the
child.

## Output capture

The contained output path takes stdout and stderr exactly once and drains them
concurrently. It sweeps descendants after the leader exits so inherited write
ends do not keep readers open indefinitely.

## Platform composition

Unix launch preparation holds the interrupt spawn window only across child
creation and registration. Linux cgroups and Windows jobs are created before
that window opens. Fault injection substitutes failures at these lifecycle boundaries without
changing the production ownership transitions. The same fault branches compile
for unit tests and for dependency builds that enable `fault-injection`.

## Lifecycle test oracles and isolation

| Family in `process_tree.rs::tests` | Break rejected by the oracle | Isolation |
| --- | --- | --- |
| Preparation, spawn retry, adoption and backoff | Lost prepared state, incorrect error precedence, skipped delay, or an unwatched refused child | Each launch has its own typestate and boundary; injected failures are thread-local |
| Registry release, empty drop and external reap | Wrong slot cleared, numeric capability retained, or replacement registration removed | Real registry fixtures use owned unreaped leaders, including separately pinned capabilities in the external-reap fixture; fabricated identities stay in the unsafe adapter's fake registry |
| Observation ordering and failures | Reap before sweep, cleanup on a pending child, or cleanup error overriding reap failure | Fake group callbacks record reuse without any OS signals |
| Termination, drop and descendant escape | Cleanup misses a started descendant or relies on natural leader exit | Owned per-test boundaries and unique marker directories; process-group/session escape tests demand delegated Linux containment |
| Accounting | Zero/missing peak, failed allocation helper, or unlimited workload reported exhausted | Page-touched 32 MiB helper must finish successfully and be fully accounted for; capability-dependent tests are explicitly ignored on unsupported default hosts |
| Interrupt and cancellation | Spawn-window leak, reset-on-second-signal leak, or helper returning normally rather than dying of a signal | One exact test per subprocess, required child-pid marker and signal death; the write-once registry flag is never changed in the shared harness |
| Output readers and error cleanup | Reader start/read/join errors swallowed, incorrect primary failure, retained buffers, or continued capture after detachment | Local stand-in readers or owned gated children; both independent buffers and their retention flags are checked |

Registry exhaustion itself is tested with fake effects in `cargo-gamma-unsafe`.
The process layer injects the resulting registration refusal and checks that
the actual child is killed and reaped; it never fills a native-effect registry
with invented group identifiers.
