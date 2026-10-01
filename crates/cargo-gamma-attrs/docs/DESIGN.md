# cargo-gamma-attrs — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-attrs`.

## Purpose

This proc-macro crate exposes the user-facing `gamma` attribute namespace used
to suppress mutations, state expected outcomes, and declare shared resources
used by tests. Mutation-control attributes return the annotated item unchanged.
`#[gamma::resource("name")]` additionally emits an ignored test marker whose
harness name lets cargo-gamma associate the resource with an exact test or
test target without runtime registration.

## Boundaries

- The library target is deliberately named `gamma`, so users write
  `#[gamma::skip]`.
- Parsing and validation live in `cargo-gamma-attrs-impl`; this proc-macro
  crate remains a thin compiler-hosted shim.
- The macros must not instrument production code or add runtime behavior.
- Resource markers are ignored tests: they appear in harness listings but
  never execute.
- Resource names start with an ASCII letter and otherwise contain only ASCII
  letters, digits, `.`, `_`, or `-`.
- A function resource annotation requires a test attribute and applies to that
  test. A module resource annotation requires an inline module and applies to
  the containing test target.
- Resource arguments and annotated items are rejected before recursive parsing
  when their token shape exceeds the shared nesting and expression-chain
  limits, so malformed input produces a diagnostic rather than exhausting the
  compiler stack.

## Public contract

The supported attributes, selector grammar, and diagnostics are part of
cargo-gamma's source-level configuration contract. Invalid directives fail at
compile time instead of becoming silent no-ops.
