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

## Bounded termination

`ProcessTree::terminate_bounded` requests the same subtree and leader kills as
ordinary termination, then polls `try_wait` until a caller-provided grace
expires. It never follows a failed kill with blocking `wait`: at the deadline
the leader handle is detached, the `ProcessTree` no longer owns a child that
Drop could wait for, and the original cleanup failure is retained in the
returned error.

## Platform composition

Unix launch preparation holds the interrupt spawn window only across child
creation and registration. Linux cgroups and Windows jobs are created before
that window opens. Fault injection substitutes failures at these lifecycle
boundaries without changing the production ownership transitions.
