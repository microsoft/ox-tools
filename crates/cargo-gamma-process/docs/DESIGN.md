# cargo-gamma-process — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-process`.

## Purpose

This crate provides cargo-gamma's bounded process-tree lifecycle: launch,
observation, memory accounting, interruption, and termination.

## Boundaries

- Policy such as timeout and memory-limit calculation belongs in
  `cargo-gamma-lib`.
- Platform calls are isolated behind the safe interfaces in
  `cargo-gamma-unsafe`.
- On Linux, a process tree retains its cgroup kill handle until interrupt
  deregistration completes, covering descendants that leave the process group.
- On Windows, each child normally receives a dedicated job. If an enclosing job
  refuses nested assignment, the spawn is rejected: an inherited job does not
  provide a handle through which this process can later terminate the child's
  descendants. Failure to create a job is tolerated only when no accounting was
  requested, with termination then limited to the child process.
- The `fault-injection` feature is test-only infrastructure.
- The crate forbids unsafe code.

## Stability

The crate is published only as an implementation dependency of cargo-gamma and
does not promise a stable downstream API.
