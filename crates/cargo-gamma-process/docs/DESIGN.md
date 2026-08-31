# cargo-gamma-process — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-process`.

## Purpose

This crate provides cargo-gamma's bounded process-tree lifecycle: launch,
observation, memory accounting, interruption, and termination.

Killing only the process a run started is insufficient. A test can spawn a
server, database, or nested build that outlives its harness, retaining scratch
tree file locks that break the next run. Descendants can also inherit output
pipe handles, preventing consumers from observing end of file and leaving an
otherwise completed run hung before its verdict can be read. Containment
therefore covers the complete descendant tree.

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

Containment is active from the child's first instruction so descendants cannot
escape during a launch-time gap. Linux moves the child into its cgroup between
fork and exec. Windows normally creates the child suspended, assigns it to its
dedicated job, and resumes it only after assignment succeeds. This boundary
also owns memory accounting because it observes the complete tree rather than
only its leader; kernel-enforced cgroup or job limits cannot be reconstructed
reliably from a post-exit peak.

## Stability

The crate is published only as an implementation dependency of cargo-gamma and
does not promise a stable downstream API. Its rustdoc is hidden, and its
hand-written README warns downstream users not to depend on it.
