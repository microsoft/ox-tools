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
- The **discovery pre-pass** is the syntax-tree walk that runs before candidate
  collection to audit stated-value attributes and build per-file indexes of
  numeric evidence and imported type paths. Its configuration-aware audit is
  confined to code discovery would mutate: not configured out and not
  test-only, including field-level gates. The standalone `check_stated` API has
  no build configuration and therefore audits the whole file.
- A stated value is reported as an error where discovery would never read it —
  on a declaration, a `const fn`, or an empty body — matching the proc macro's
  compile-time rejections, so a hint that generates nothing is never silent.
- The crate forbids unsafe code.

Replaceable traversal and test mechanics are recorded in the
[implementation guide](IMPLEMENTATION.md).

## Stability

The crate is published so `cargo-gamma` can be installed from crates.io. Its
Rust API is internal and carries no independent compatibility guarantee. Its
rustdoc is hidden, and its hand-written README warns downstream users not to
depend on it.
