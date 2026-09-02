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
- Containment is independent of memory accounting. A Unix process group can be
  left with one unprivileged call, so every launch enters a cgroup leaf on Linux
  and a job object on Windows whether or not anything is being measured; the
  memory request decides only whether that boundary's readings are reported.
- On Linux, a process tree's cgroup keeps its own interrupt registration for as
  long as the cgroup exists, covering descendants that leave the process group.
  Handing the process-group slot back when the leader is reaped does not retract
  it; dropping the cgroup does, after waiting for any handler sweep in flight.
- A command is prepared for launch exactly once, and the type system is what says
  so. Preparation appends a pre-exec step naming one specific boundary, and a
  command carrying two such steps cannot be launched correctly, so preparation
  consumes the command and yields a prepared launch that never hands the command
  back. There is no run-time refusal to bypass and no mark on the command for a
  caller to erase. Spawning consumes that prepared typestate: failure returns it
  with the operating-system error so a transient shortage can back off and retry,
  while success yields a distinct bundle coupling the child to its boundary.
  Adoption consumes that bundle, so a successful launch cannot be reused to
  create an earlier, unadopted sibling.
- The contained `output` convenience mirrors `Command::output`: it disconnects
  stdin and captures stdout and stderr. Both pipes are drained concurrently
  while the child runs, avoiding pipe-capacity deadlocks. When the leader exits,
  descendants are swept before their inherited pipe handles are drained to end
  of file.
- On Windows, each child receives a dedicated job. If an enclosing job refuses
  nested assignment, the spawn is rejected: an inherited job does not provide a
  handle through which this process can later terminate the child's descendants.
  Failure to create a job is likewise a refusal rather than a degraded launch.
- A host that offers no sealed boundary at all is reported once through
  `containment`, before any repository-controlled code runs, rather than
  degrading each launch silently. A host that can seal but fails one launch
  refuses that launch.
- An observation that finds the leader already reaped elsewhere revokes the
  numeric capabilities naming it — the retained child handle and the stored
  process-group id — after sweeping the boundary that names a directory or a
  handle, because only the latter can still be proven to reach this run's own
  descendants.
- The `fault-injection` feature is test-only infrastructure.
- Tests that need a capability the host may not have are marked ignored and fail
  when asked for by name on a host that cannot supply it, rather than returning
  early and reporting a pass. Tests that spend process-wide state which cannot be
  restored — the interrupt registry's interrupted flag, its finite watch slots —
  run in a subprocess of their own.
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
