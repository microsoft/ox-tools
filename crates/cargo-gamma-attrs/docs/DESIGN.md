# cargo-gamma-attrs — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-attrs`.

## Purpose

This proc-macro crate exposes the user-facing `gamma` attribute namespace used
to suppress mutations and state expected outcomes. Each attribute validates
its arguments and otherwise returns the annotated item unchanged.

## Boundaries

- The library target is deliberately named `gamma`, so users write
  `#[gamma::skip]`.
- Parsing and validation live in `cargo-gamma-attrs-impl`; this proc-macro
  crate remains a thin compiler-hosted shim.
- The macros must not instrument code or add runtime behavior.

## Public contract

The supported attributes, selector grammar, and diagnostics are part of
cargo-gamma's source-level configuration contract. Invalid directives fail at
compile time instead of becoming silent no-ops.
