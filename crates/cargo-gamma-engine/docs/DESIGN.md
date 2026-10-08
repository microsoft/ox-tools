# cargo-gamma-engine — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-engine`.

## Purpose

This crate owns Rust source parsing, mutation-site discovery, stable mutation
identity, mutator selection, and mutant-schema instrumentation.

## Discovery model

Discovery first performs a configuration-aware syntax-tree pre-pass. It audits
stated-value attributes and builds per-file indexes of source-visible type
evidence, aliases, local signatures, numeric uses, and imported paths. Candidate
collection then applies the selected mutators using those indexes and emits
stable, content-derived mutation identities. Each candidate is classified as
`Proven`, `Optimistic`, or `Explicit`: positive source evidence supports proven
candidates, unresolved semantic guesses are optimistic, and caller-supplied
replacement values are explicit. Confidence is diagnostic evidence rather than
an automatic filter. Site classes whose measured compile-failure cost is high
and whose killing tests are not exclusive may be withheld from the default;
a non-default selector that resolves to their mutator admits them without
changing their stable identity. The standalone `check_stated` API has no build
configuration and therefore audits the whole file.

## Boundaries

- Input is Rust source plus a mutator selection; output is deterministic
  mutation metadata and instrumented source.
- The engine does not invoke Cargo, run tests, supervise processes, or decide
  campaign verdicts.
- Stable identities are content-derived so reports and incremental knowledge
  remain meaningful as unrelated source changes.
- Mutation discovery uses source-visible type evidence and never identifier
  spelling to screen candidates. Unsigned, textual, and temporal evidence
  propagates through parameters, annotated locals, fields, casts, assignments,
  comparisons, index and slice positions, locally visible signatures,
  `map_or` results, and iterator sums whose source fixes the result. This
  includes backward constraints from a later local use to an earlier
  unsuffixed-zero initializer. Proven unsigned zero is not decremented, proven
  signed zero is decremented in the default campaign, and an unsuffixed zero
  with no resolved signedness is an optimistic candidate available only through
  a non-default selector that includes `literal.int_decrement`. Likewise,
  associated function items and numeric wrappers without integer-literal
  arithmetic are not sufficient evidence for expression increment or
  decrement; focused non-default selection can still request those optimistic
  sites. Proven text/time addition is not changed to any incompatible arithmetic
  operator; signed and unresolved arithmetic remains in the population.
- A mutation that invents `Default::default()` for a known payload requires
  positive evidence. Primitive and supported standard types, package-local
  derives or impls whose concrete generic arguments satisfy their bounds,
  aliases, renamed imports, and generic parameters with an applicable
  `Default` bound qualify. Package indexes remain separate so same-named types
  in different packages cannot make each other's evidence ambiguous. Import and
  shadow evidence is also kept per lexical scope, so a wildcard import or local
  trait in one module or block does not weaken otherwise proven candidates in
  a sibling module or function.
  Unambiguous bare local names resolve to package declarations; qualified local
  paths, aliases, and unresolved dependency paths remain unknown.
  A `Self::Assoc` projection is resolved
  through the enclosing impl when that impl declares the associated type;
  unresolved trait-associated types remain abstract. An unresolved concrete
  dependency type remains an optimistic fallback candidate unless source-visible
  evidence proves it non-defaultable. It remains in the default campaign because
  completed-run evidence shows exclusive killing tests for this class; confidence
  telemetry keeps its compilation cost measurable. Expected types flow
  through nested blocks, branches, closures, returns, local
  annotations, and assignment targets. When no default exists, a type-identical
  copy-safe parameter may be reused as the opposite payload. Direct-default
  mutations of parameters, call results, and early returns additionally exclude
  `Result`, which has defaultable payloads but no blanket `Default`
  implementation of its own. A return already written as `bool::default()`,
  `Option::default()`, or `String::default()` is recognized as the same value
  as the proposed `Default::default()` replacement and produces no mutant.
  Arrays follow the standard library's implemented lengths and element bounds,
  and known standard types without `Default` are rejected only after their
  complete import-resolved path identifies them. A wrapper contributes proven
  default evidence only when its path resolves to a supported standard wrapper
  and its payload is also proven defaultable.
- Whole-function replacements preserve the written return shape. Alias
  parameters are substituted by their declared roles before `Option` or
  `Result` payload values are chosen. Collection
  aliases use `Default::default()` rather than rebuilding a standard
  constructor with different generic defaults. Imported aliases whose source
  name differs from the concrete collection type use the same trait
  constructor, avoiding assumptions about an inherent `new`. A local collection-shaped type
  that shadows a standard collection name and positively implements `Default`
  also uses the trait constructor instead of assuming an inherent `new`. If
  its concrete generic arguments do not satisfy that implementation's bounds,
  discovery emits no empty constructor: the standard type's inherent `new`
  cannot be borrowed as evidence for the local shadow.
  Unsized standard wrappers use shape-specific constructors, opaque iterators
  retain iterator-shaped replacements, and tuple/collection products require
  evidence for every member. Shared empty slices may use `&[]`; arbitrary shared or mutable
  references are not fabricated with `Box::leak`, and mutable string slices
  receive no automatic replacement because the only source-independent
  construction would leak once per invocation.
- Statement and loop deletion is gated only for concrete intra-procedural
  compile hazards: deferred-binding initialization, explicit `drop` calls, and
  branch/loop type transitions already proven by local syntax. The divergent
  tail of a `let`-`else` remains intact, and a `continue` is not changed to a
  valueless `break` when the target loop must produce a value. Ordinary calls,
  assignments, and viable loop transitions remain candidates.
- Method renames require source-visible receiver compatibility when the
  replacement API is not shared by all same-named methods. In particular,
  `last` becomes `first` only for proven slices, arrays, or `Vec` values, not
  iterator chains. Removing `!` from a borrowed boolean dereferences the
  operand so the replacement remains a boolean.
- Rust-semantic mutation covers logical operand contribution, `Option` and
  `Result` predicates, `?` propagation, fallback values, collection order,
  additional literal classes, control-flow values, typed boolean values,
  calls, iterator pipelines, parameters, and parsed regular expressions.
  Different concrete iterator types are joined by the runtime `Either`
  adapter. Option-style `filter` chains are excluded, because replacing their
  `Option` result with an iterator adapter changes the type of later methods.
  Removing `filter` is enabled by default only when source-visible evidence
  identifies the receiver as an iterator; unresolved receivers remain
  available through a non-default selector containing `iter.remove_filter`.
  Boolean struct-field shorthand remains shorthand rather than becoming an
  expression-shaped guarded replacement. Fallback mutation requires positive
  evidence that the fallback result directly implements `Default`, and is
  withheld when that result is already an obvious primitive default such as
  `false` or numeric zero. Early-return defaulting applies the same no-op
  rule. Floating-point negation excludes zero because changing only its sign
  bit is not a useful behavioral mutation for ordinary numeric code. Call
  mutation additionally requires an unattributed, unqualified call whose local
  signature is unambiguous. Qualified calls never consume a same-named bare local signature;
  they retain only independently proven standard or associated inference.
  Array reversal is withheld when guarding would shorten the
  lifetime of a borrowed array temporary, and parameter shadowing is withheld
  for opaque `impl Trait` returns. Regex mutation is confined to string
  literals passed to a path ending in `Regex::new`; both the original and each
  replacement must parse with `regex-syntax`.
- The discovery pre-pass is confined to code discovery would mutate: not
  configured out and not test-only, including field-level gates.
- A stated value is reported as an error where discovery would never read it —
  on a declaration, a `const fn`, or an empty body — matching the proc macro's
  compile-time rejections, so a hint that generates nothing is never silent.
  The public error retains a typed `StatedValueError` source until diagnostic
  rendering, including through the fused audit-and-collect entry point.
- The crate forbids unsafe code.

Replaceable traversal and test mechanics are recorded in the
[implementation guide](IMPLEMENTATION.md).

## Semantic oracle

rustc remains the final authority on whether the complete instrumented schema
compiles. Narrow compiler-assisted witnesses were evaluated for unresolved
questions such as whether a dependency type implements `Default`, but are not
an additional discovery stage: they would repeat Cargo setup under the same
features, target, and cfg values before the convergence build that already
answers the question exactly. Completed-run evidence also shows that unresolved
default construction can exercise killing tests no other candidate class
reaches, so dropping it wholesale would lose measured testing value. Confidence
telemetry keeps that cost visible while ordinary convergence retains the valid
candidates. A witness is warranted only if future diagnostics identify a
repeatable semantic question whose avoided proof cost exceeds the extra Cargo
invocation and whose answer can be reproduced under the exact build context.

## Stability

The crate is published so `cargo-gamma` can be installed from crates.io. Its
Rust API is internal and carries no independent compatibility guarantee. Its
rustdoc is hidden, and its hand-written README warns downstream users not to
depend on it.
