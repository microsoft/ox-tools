# cargo-gamma-lib — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-lib`.

## Purpose

This crate coordinates cargo-gamma campaigns: configuration, discovery,
scratch workspaces, instrumented builds, test selection, process supervision,
verdicts, incremental reuse, reporting, and command dispatch.

## Boundaries

- Rust parsing and instrumentation are delegated to `cargo-gamma-engine`.
- Process-tree mechanics are delegated to `cargo-gamma-process` and
  `cargo-gamma-unsafe`.
- The injected guard protocol is provided by dependency-free
  `cargo-gamma-rt`.
- The `internals` feature exists only for this crate's integration tests and
  is not a supported downstream API.
- The crate forbids unsafe code.

## Public contract

The primary public contract is the `cargo gamma` command surface and its
configuration, reports, diagnostics, and exit codes. The Rust API is an
implementation detail used by the thin executable crate.
