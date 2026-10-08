# cargo-gamma-engine — Implementation guide

This guide records replaceable implementation choices behind the contracts in
[`DESIGN.md`](DESIGN.md).

## Discovery pre-pass

Configuration-aware discovery combines stated-value validation and the
type/import indexes in one `syn::visit::Visit` traversal. The visitor gates
items, associated items, fields, statements, and expressions before either
pre-pass sees them. Standalone stated-value validation deliberately uses its own
whole-file traversal because it has no selected build configuration.

The combined visitor retains the audit and indexer as separate state objects.
Their per-node update methods are shared with the standalone implementations,
which keeps equivalence tests able to compare the fused and separate paths.
The indexes record declared field and constant types, aliases, local function
signatures, imports, numeric-only uses, and names constrained by index or slice
positions. Collector-local binding types add lexical scope and shadowing.
Expected-type walkers carry unsigned and default-payload evidence through
value-preserving expression shapes; unsigned propagation additionally follows
`map_or` defaults and closure results and the mapped source of `Iterator::sum`.
Comparisons, assignments, and calls to locally indexed functions transfer
written type evidence to their operands. Signed zero remains eligible for
decrement by default, unresolved zero is retained only when the applicable
mutator is explicitly selected, and an explicit textual, temporal, or
container type vetoes a conflicting numeric-use guess. Ambiguous numeric-use
evidence similarly makes expression increments and decrements optional rather
than default candidates. A numeric qualifier proves arithmetic only for
primitive integer and floating-point types and known associated constants;
function items and `NonZero*` values remain optimistic because `+ 1` and `- 1`
are not valid for them.

Package `Default` evidence is formed by order-independent unions of per-file
indexes. Keeping one index per package prevents unrelated same-named types in
different workspace members from colliding. Repeated names inside a package
preserve positive evidence when every definition is defaultable and negative
evidence when none are; a mixed collision remains unknown.

The import index records each binding's complete source path, including renamed
imports, and marks declared modules as local path roots. Root, nested-module,
and block bindings are indexed separately so wildcard imports and trait shadows
affect only their lexical scope. Blocks inherit their enclosing scope's
bindings before adding local evidence. Chained import aliases are expanded once
per scope after the pre-pass; ambiguous bindings and alias cycles remain unknown rather
than being repeatedly resolved at each type query. Parameter and local type
evidence retains the import map from its declaration scope, so a body-local
shadow cannot reinterpret a previously declared binding. Value synthesis
therefore applies package evidence to unambiguous bare types while leaving
qualified local paths, aliases, and dependency paths unresolved. A generic bare
local path retains the declaration's evidence, and its concrete arguments are
checked against derived or declared bounds. Function-local iterator bounds are
hidden at a nested function boundary while enclosing impl and trait bounds
remain available. When a
local type shadows a standard collection but its concrete arguments fail those
bounds, synthesis stops rather than falling through to the standard
collection's similarly spelled inherent constructor. Fixed-size arrays use the
standard library's implemented length range and require defaultable elements
except at length zero. Imported collection aliases whose resolved final segment
differs from the written name use `Default::default()` instead of assuming the
alias target has the standard type's inherent `new`.

Return-value construction resolves local aliases without replacing their
declared shape. Recursive products stop when any member lacks construction
evidence. Reference construction is deliberately limited to source-independent
promotable values such as `&[]`; the engine does not synthesize arbitrary
borrowed values by leaking allocations.

Deletion safety remains an intra-procedural syntax analysis. The collector
records deferred locals before examining assignments, protects explicit
`drop` calls and divergent `let`-`else` tails, and marks loops whose syntactic
breaks or enclosing expected type require a value. These marks gate deletion
and `continue`-to-`break` replacements without suppressing viable unit-loop
mutations. Method renames separately consult receiver types for APIs such as
slice/`Vec::first`, and unary-not removal dereferences a proven `&bool`.
The analysis does not attempt inter-procedural ownership or state-machine
analysis.

Iterator pipeline removal and `take`/`skip` exchange produce different
concrete adapter types. Their candidates therefore use an iterator-expression
schema shape that wraps both guard arms in `gamma_rt::Either`, analogous to the
existing opaque-iterator function-body shape without turning the expression
into a block.

Fallback mutations preserve the surrounding method-call spelling, replace only
the selected fallback span, and require positive direct-`Default` evidence from
that expression. Call suppression is limited to unqualified locally indexed
functions with a positively defaultable return type, and expression-type
inference applies that same unqualified-path check before consulting the local
return map. Same-named qualified APIs therefore cannot inherit bare local
evidence. Option-style `filter` receivers are excluded from iterator
removal. Receivers with positive iterator evidence remain default candidates;
unresolved receivers are optimistic and appear only under a non-default
selector containing `iter.remove_filter`. Attributed calls do not receive
call-default replacements because moving the attribute inside the generated
block would require unstable expression attributes. Boolean struct-field shorthand is traversed without offering an
expression replacement, and arrays borrowed through `.as_slice()` are not
reversed because a guarded expression would shorten their temporary lifetime.
Parameter shadowing is limited to simple by-value bindings in functions without
opaque `impl Trait` returns, retains written mutability, and requires the same
positive `Default` evidence. Regex candidates recognize `Regex::new` paths,
compute one semantic edit at a time, and retain only replacements accepted by
`regex-syntax`.

## Schema positions and text encoding

Instrumented guard positions are resolved from sorted byte offsets. Line lookup
uses the precomputed line-start table, avoiding a cursor-controlled open-ended
loop. Terminal-safe text encoding keeps a byte cursor because it must skip
complete UTF-8 controls and accepted SGR sequences; every branch asserts that
the cursor advanced.

## Test strategy

Focused fixtures make every cfg-bearing syntax level observable through either
a malformed stated value or candidate evidence. Agreement tests compare the
proc-macro and source scanners across generated syntax families without
exporting their corpus generators as supported API.

Selected positive counterexamples also apply each discovered replacement
directly and compile the resulting standalone fixture with the workspace Rust
edition. This oracle intentionally skips shapes such as `Shape::IterBlock`
whose viability depends on later schema instrumentation wrapping both branches
with `gamma_rt::Either`; dedicated instrumentation tests cover those shapes.
