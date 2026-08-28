# cargo-gamma-unsafe — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-unsafe`.

## Purpose

This crate contains the operating-system calls cargo-gamma cannot express
safely through the standard library, exposed through safe interfaces.

## Boundaries

- Unix process groups and Windows job objects provide process-tree control.
  Unix group identifiers less than or equal to one are rejected because
  `killpg` reserves them for special or undefined behavior.
- Linux cgroups and Windows job objects provide process-tree memory limits.
- On Linux, terminal interrupts kill both the process group and the cgroup, so
  descendants that create a new session cannot escape cleanup.
- Cgroup registration borrows the live `Cgroup`; releasing its watch waits for
  any signal-handler sweep before the owning kill descriptor can be closed.
- Linux controller delegation moves only cargo-gamma itself and refuses a
  cgroup shared with any other process.
- Policy and limit calculation remain in `cargo-gamma-lib`; this crate only
  reports and applies platform capabilities.
- Unsafe blocks are concentrated here, documented, and hidden behind
  invariants that callers can satisfy safely.

## Portability

Unix and Windows implementations are selected with target-specific
dependencies. Unsupported hosts report the missing capability rather than
silently substituting weaker semantics.
