# cargo-gamma-unsafe — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-unsafe`.

## Purpose

This crate contains the operating-system calls cargo-gamma cannot express
safely through the standard library, exposed through safe interfaces.

Process-subtree termination and memory containment have no safe standard
library equivalents; the unsafe code here is required for platform capability,
not performance. Concentrating those calls in this crate lets the rest of the
workspace use `#![forbid(unsafe_code)]`, turning the boundary from a review
convention into a property checked by the compiler on every build.
`cargo-gamma-rt` is the sole exception because it is injected into the subject
crate's dependency graph and must remain dependency-free.

## Boundaries

- Unix process groups and Windows job objects provide process-tree control.
  Unix group identifiers less than or equal to one are rejected because
  `killpg` reserves them for special or undefined behavior.
- Linux cgroups and Windows job objects provide process-tree memory limits.
- On Linux, terminal interrupts kill both the process group and the cgroup, so
  descendants that create a new session cannot escape cleanup.
- Cgroup registration is owned by the `Cgroup` it watches and discharged by that
  cgroup's own `Drop`, wherever it has been moved to; the release waits for any
  signal-handler sweep still using the kill descriptor before the owning file
  closes it. A safe caller has no way to release it late, twice, or not at all.
- Linux controller delegation moves only cargo-gamma itself and refuses a
  cgroup shared with any other process.
- Policy and limit calculation remain in `cargo-gamma-lib`; this crate only
  reports and applies platform capabilities. For example, choosing a memory
  ceiling is testable arithmetic in `cargo-gamma-lib`; this crate answers only
  what the host can enforce and applies the requested value.
- Unsafe blocks are concentrated here, documented, and hidden behind
  invariants that callers can satisfy safely.
- Fallible platform boundaries return a structured `PlatformError` carrying a
  `Situation` — unsupported host, refused operation, or interrupted run — and a
  captured backtrace, so callers classify failures without parsing messages.
  The compatible `Into<String>` constructors accept dynamic and non-static
  borrowed messages; explicit static-message constructors avoid allocating for
  diagnostics embedded in the program.
- Replaceable registry, cgroup, job-backend, and fault-injection mechanics are
  recorded in the [implementation guide](IMPLEMENTATION.md).

## Portability

Unix and Windows implementations are selected with target-specific
dependencies. Unsupported hosts report the missing capability rather than
silently substituting weaker semantics.

## Stability

The crate is published only as an implementation dependency of cargo-gamma. Its
rustdoc is hidden, and its hand-written README warns downstream users not to
depend on its unstable Rust API.
