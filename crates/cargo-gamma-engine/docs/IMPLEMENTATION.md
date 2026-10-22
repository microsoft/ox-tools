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
positions. Lexical-use indexes associate numeric constraints with binding
declarations, including closure and destructured parameters, rather than
file-wide identifier spellings. Call-signature resolution distinguishes free
calls, method syntax, and qualified receiver arguments. Async signatures do not
supply the type of their unawaited calls. Collector-local binding types add
lexical scope and shadowing.
The signature index canonicalizes parameter and result types at the declaring
scope, substituting the impl's `Self` and erasing unresolved callee type
parameters. Receiver types use the parsed `ReceiverKind`, preserving reference
and explicitly typed receivers. Alias instantiation precedes default-obligation
queries; renamed standard container imports also participate in return-shape
classification.
Alias maps follow module and block scope rather than a file-wide terminal name.
Targets are expanded with their declaration's import map before block
inheritance, so a later local import or alias cannot reinterpret an inherited
target. Generic parameter roles remain available for concrete instantiation.
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
retain separate declaration identities where the labeled module graph resolves
them. Name-only fallback preserves positive evidence when every definition is
defaultable and negative evidence when none are; a mixed collision remains
unknown. Aliases and public re-exports resolve through the same graph rather
than making every exported name indirect. Ambiguous imports and wildcard
re-exports still prevent unsupported name-only negative conclusions.

The import index records each binding's complete source path, including renamed
imports, and marks declared modules as local path roots. Root, nested-module,
and block bindings are indexed separately so wildcard imports and trait shadows
affect only their lexical scope. Blocks inherit their enclosing scope's
bindings before adding local evidence. Chained import aliases are expanded once
per scope after the pre-pass; ambiguous bindings and alias cycles remain unknown rather
than being repeatedly resolved at each type query. Parameter and local type
evidence retains the import map from its declaration scope, so a body-local
shadow cannot reinterpret a previously declared binding. Production discovery
retains source labels, binds unique active crate roots/module paths, and passes
that context into package indexes. Value synthesis can therefore resolve local
qualified paths, renamed imports, aliases, and cross-file re-export chains
without borrowing another module's same-named declaration. Literal `#[path]`
modules participate; conditional, missing, duplicate, ambiguous, and unlabeled
module contexts remain unknown, as do unresolved dependency paths. A generic bare
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
Generic implementation bounds not established by the index and conditional
default evidence do not become proven confidence. Synthesized set elements and
map keys additionally require ordering/hash evidence for proven confidence;
unknown custom key traits retain optimistic candidates. Hasher identity uses
the complete import-resolved standard path, not the terminal `RandomState`
spelling. Empty collections and synthesized elements carry separate obligations.

Negative default obligations are structural: a tuple or synthesized element
requires its members, whereas a valid empty vector does not require the element
to implement `Default`. Operator admission likewise consults resolved operand,
right-hand-side, and output types together, including fields and local-call
results. Supported standard/chrono contracts do not become blanket bans on
custom overloads or unknown nominal types. API-identity checks separate reader
and iterator adapters and preserve compatible custom enum qualifiers when
rewriting `Some`. A known reader receiver alone does not identify `Read::take`:
negative evidence also requires the exact standard `Read` trait in lexical
scope. Custom `take`/`skip` extension methods and custom iterator-returning
`by_ref` methods retain their candidates.

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
Slice, array, and `Vec::iter` item types flow into `any`/`all` closure patterns.
Unknown negation operands retain one optimistic plain-operand candidate, not a
speculative dereference or double-negation fallback.
Lexical binding and field-projection evidence tracks source-visible owning
places. When guarding a viable restoration or exit would introduce a
move/reinitialization conflict, the edit lifts to the nearest loop, or otherwise
the enclosing block. Its replacement contains uninstrumented source with only
that edit applied, while the original region retains nested guards. This
retains independently viable alternatives without requiring the borrow checker
to prove that their runtime guards are mutually exclusive. Lifted edits retain
distinct logical source identities.
The analysis does not attempt inter-procedural ownership or state-machine
analysis.

Resource admission reconstructs the proposed loop and compares its progress
with the original. Its fixed-point interpreter uses lexical environments,
typed primitive values, bounded field/collection provenance, and source-resolved
helper/cache summaries. It preserves writes made before shadow declarations
when restoring the enclosing environment. Guarded matches are rejected unless
their guard effects are understood; dropping a body update must not hide progress
performed by a guard. The current interpreter bounds each loop to 4,096 source
bytes, 64 bindings, 256 interpretation steps, and two iterations, with separately
bounded helper/cache syntax and provenance text. Helper analysis admits at most
512 nodes, depth eight, and 16-KiB bodies; source evidence admits 1-MiB files and
8 MiB in aggregate. Dependency contracts additionally require exact resolved
identity/version, official registry provenance, and a verified registry closure.
Evidence-only source hashes bind the same parsed read, including any restored
BOM, to plan provenance without selecting that source for instrumentation.
Conditional or conflicting helper/import definitions remain unknown.
Unsupported operations or an exhausted budget retain the candidate.

Source-size, environment-size, helper-work, type-expansion, namespace, and
dependency-inventory allowances are conservative policy defaults, not measured
capacities or Rust language limits. They bound distinct traversal, cloning, and
retention costs; equal scales do not imply shared accounting or a combined
memory ceiling. These tunable allowances are separate from the interpreter's
recurrence argument and supported value domain.

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
Parameter shadowing is limited to simple by-value bindings with supported
return contracts, retains written mutability, and requires the same positive
`Default` evidence. Opaque iterators, including supported nested `Option` and
`Result` payloads, use mapped `Either` shapes; unsupported opaque returns remain
excluded. Regex candidates recognize `Regex::new` paths,
compute one semantic edit at a time, and retain only replacements accepted by
`regex-syntax`.

## Schema positions and text encoding

Temporary-sensitive expression mutations lift their splice to the consuming
expression rather than returning a borrow of a branch-local temporary. Lifting
does not cross lazy expression boundaries. Attributes stay on the outer
source construct instead of being copied inside a generated unary operator.
Match scrutinees may lift through their consuming match; arm bodies remain
separate lazy regions.
Iterator-returning bodies lower explicit returns through a labeled result
block so both early returns and the tail pass through the same payload mapping.
Nested callable items, closures, and async blocks retain their own returns.
`IterWrappedBlock { depth }` maps supported nested `Option` and `Result`
success payloads rather than treating the outer container as an iterator.
Its depth is a positive `NonZeroU8`; positive depths retain their existing JSON
integer encoding, while a zero depth is rejected during deserialization.
Existing `IterBlock` and `IterExpr` serialized shapes are unchanged; new code
can read old records, but an older executable cannot read the new shape.

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

The cross-crate `mutation_correctness` target also drives collection,
definition assignment, and schema rendering against checked-in source/expected
pairs. Expected instrumented Rust is compiled directly as test modules, then
exercised with isolated active selections. These fixtures verify type checking
alongside inactive behavior, individual mutant behavior, evaluation and drop
order, and lazy execution without spawning a compiler from the tests.
