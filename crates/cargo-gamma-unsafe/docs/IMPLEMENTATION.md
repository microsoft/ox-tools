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

## Linux cgroups

Creation opens `cgroup.kill` before launch and installs a `pre_exec` write to
`cgroup.procs`. A shared reaper retries directory removal after bounded
foreground cleanup. Leaf names include creator identity so stale leaves can be
distinguished from live ownership.

## Windows jobs

Private backend functions isolate Win32 calls from the ownership algorithms.
Unit tests inject per-thread failures at selected calls while retaining the same
job, process, completion-port, and handle lifetimes as production.
