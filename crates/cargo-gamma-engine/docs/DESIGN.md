# cargo-gamma-engine — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-engine`.

## Purpose

This crate owns Rust source parsing, mutation-site discovery, stable mutation
identity, mutator selection, and mutant-schema instrumentation.

## Boundaries

- Input is Rust source plus a mutator selection; output is deterministic
  mutation metadata and instrumented source.
- The engine does not invoke Cargo, run tests, supervise processes, or decide
  campaign verdicts.
- Stable identities are content-derived so reports and incremental knowledge
  remain meaningful as unrelated source changes.
- Mutation discovery uses source-visible type evidence to avoid replacements
  known not to compile. This includes concrete `Self` and associated types in
  implementations, standard time types without `Default`, and standard
  `fmt::Result` aliases.
- Every pre-pass that feeds discovery — the stated-value audit and the
  numeric/import indexes, whether run standalone or fused into one walk — is
  confined to the code discovery itself would mutate, by the same rule: not
  configured out, and not test-only. That gate applies at items, associated
  items, struct fields, statements, and expressions, because conditional
  compilation inside a body is written on statements. A `#[gamma::value(...)]`
  outside that region is therefore not diagnosed by the fused entry point;
  `check_stated`, which takes no configuration, still reads the whole file.
- A stated value is reported as an error where discovery would never read it —
  on a declaration, a `const fn`, or an empty body — matching the proc macro's
  compile-time rejections, so a hint that generates nothing is never silent.
- The crate forbids unsafe code.

## Stability

The crate is published so `cargo-gamma` can be installed from crates.io. Its
Rust API is internal and carries no independent compatibility guarantee. Its
rustdoc is hidden, and its hand-written README warns downstream users not to
depend on it.
