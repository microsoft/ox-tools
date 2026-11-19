# cargo-gamma-engine design

## Responsibility

The engine turns Rust source and a mutation selection into stable mutation
metadata and an instrumented source tree. It owns parsing, discovery,
source-level admission and suppression, identities, and schema rendering.

It does not invoke Cargo or rustc, copy workspaces, run tests, choose campaign
verdicts, or calculate scores. Those responsibilities belong to the
[campaign coordinator](../../cargo-gamma-lib/docs/DESIGN.md).
The crate forbids unsafe code.

The engine is published so that cargo-gamma can be installed from crates.io,
not as an independently supported Rust API. Its Rust API can change
incompatibly; persisted mutation identities have a separate, explicit scheme.

## Inputs and outputs

Discovery receives source files, their package/module context, the selected
mutators, and the active build configuration. Package context keeps evidence
from unrelated packages separate and allows source-visible modules and
re-exports to be resolved together. Supplied dependency provenance can support
specific external API contracts; a familiar crate name alone cannot.

The output describes each admitted replacement: its original source region,
replacement, mutator, containing item, stable identity, confidence, and any
schema shape needed to preserve Rust semantics. Source spans identify the
logical edit even when rendering must guard a larger region.

Rendering receives the selected mutations with campaign-assigned ordinals.
It returns instrumented text and guard locations that the coordinator can use
to relate compiler diagnostics to replacements.

For a fixed source, configuration, and selection, discovery and rendering are
deterministic. A change to the build configuration can legitimately change the
population even when the file bytes are unchanged.

## Discovery pipeline

1. Parse the source with bounded syntax-shape checks.
2. Audit stated-value attributes and index active declarations, imports,
   signatures, aliases, and binding evidence.
3. Visit eligible runtime code and propose selected replacements.
4. Apply source-level suppressions and the admission rules below, preserving
   identity positions when a semantic rule withholds a candidate.
5. Emit mutation metadata for the coordinator to select and compile.

Discovery excludes code disabled by the supplied configuration and test-only
code. Runtime guards are not inserted into const contexts or patterns.
Evaluated match guards remain ordinary mutation sites.

Configuration gating applies to declarations, fields, parameters, statements,
expressions, and match arms. Return-tail and expected-type analysis use the
last active statement, not simply the last written statement. A disabled
declaration cannot hide an active tail or shift a call's parameter types.
An active item or semicolon still prevents an earlier expression from being
treated as the block's value.

A stated-value attribute that discovery cannot consume is an error rather
than a silently ineffective hint. The error retains its typed cause until
diagnostic rendering. The standalone stated-value audit has no selected build
configuration and deliberately examines the whole file.

Mutator names, presets, and individual replacement rules are documented in the
[mutator reference](../../cargo-gamma/docs/MUTATORS.md). Suppression attributes
are described by [cargo-gamma-attrs](../../cargo-gamma-attrs/docs/DESIGN.md).

## Evidence and uncertainty

The engine uses source analysis, not a second Rust type checker. Its evidence
must preserve the distinction between a known fact, a known incompatibility,
and an unresolved question.

| Confidence | Meaning |
|---|---|
| `Proven` | Positive source-visible evidence supports the replacement. |
| `Optimistic` | Compilation depends on a semantic fact discovery cannot establish. |
| `Explicit` | The replacement value was supplied by the user. |

Confidence is diagnostic evidence, not a blanket admission filter, and does
not participate in identity. The default selection withholds only designated
noisy optimistic site classes. A non-default selector containing the relevant
mutator admits those sites without changing their identities. Mandatory
construction, safety, and no-op rules still apply under explicit selection,
including `all`.

### Names retain their declaration context

Type and value evidence belongs to lexical declarations, not identifier
spellings. Imports, aliases, generic parameters, local shadows, and the type
and value namespaces affect resolution. A use of a local initializer sees
the preceding binding, not the binding being declared.

Indexed signatures, fields, constants, and statics retain their declaration
environment. Method results use the declaring receiver's `Self`; a callee's
generic parameter does not inherit a same-named caller parameter's bounds.
Alias arguments resolve in the caller's context before substitution into the
alias's declaration context. Cycles, ambiguous imports, incomplete module
graphs, and exhausted expansion budgets produce unknown evidence.

Standard-library recognition requires genuine external provenance. A local
lookalike or a foreign crate renamed to `std`, `core`, or another recognized
crate cannot establish that crate's contracts. Absolute paths bypass local
module shadows but still require valid crate-root provenance. Conversely,
later caller-local shadows cannot reinterpret an already resolved declaration
type. Source-crate aliases remain source-local rather than becoming external
library identities.

These rules apply recursively to composite types, operator operands and
outputs, method receivers, and inferred payloads. Complete resolved types can
establish equality or incompatibility; matching unresolved names cannot.

### Positive and negative evidence have different obligations

Inventing a value requires the relevant construction evidence. In particular,
synthesized whole-function results must justify every required default,
payload, collection element, and custom hasher. A defaultable payload does not
give its outer container a blanket `Default` implementation. A valid empty
collection does not require its element type to be defaultable.

An omission based on the absence of an API or trait needs a sufficiently
complete source inventory. Conditional declarations, transforming attributes,
unknown macros, external extension traits, and ambiguous resolution can defeat
that proof. Ordinary unresolved rewrites remain candidates for compilation;
uncertainty is not evidence that a custom operation is unsupported.

Reference construction is restricted to supported, source-independent values.
The engine does not invent arbitrary borrowed results by leaking allocations.
Reusing an input as an output additionally requires compatible type, binding,
ownership, and lifetime evidence.

## Admission policy

Discovery removes a candidate only under a defined rule. Three important
classes must remain distinct:

| Class | Reason for omission |
|---|---|
| Known compile hazard | The proposed replacement violates an established local type or control-flow requirement. |
| Proven no-op | The replacement has the same recognized behavior as the original. |
| Proven resource hazard | The replacement introduces a supported non-progressing or unbounded-growth loop. |

Omitted candidates are not tested mutants, compiler failures, survivors, or
score contributions. Resource exclusions are not equivalence claims.

### Compile hazards

Local control-flow and ownership evidence protects required initialization,
diverging `let ... else` branches, value-producing loops, and nonunit or
never-returning tails. It does not justify deleting ordinary calls or assuming
unknown calls return normally.

Operator and method rules use resolved receiver, operand, and output evidence.
Known standard APIs can rule out incompatible alternatives without imposing
those contracts on custom overloads or similarly named methods. Return-value
construction preserves the declared container and iterator shape rather than
defaulting an unrelated outer type.

A replacement that is viable by itself may require a larger schema guard to
preserve moves or restoration. Guard incompatibility is not proof that the
individual mutation is invalid.

### No-ops

The engine omits recognized token-identical and semantic identities, not every
expression that appears mathematically equivalent. Effectful operands,
overloaded operations, allocation state, and unknown implementations can make
a seemingly redundant change observable.

Identity checks therefore require the same namespace and type evidence as
construction. Replacing a unit-valued call's result does not justify deleting
the call's side effects. An empty collection does not prove equal capacity or
hasher state. Explicit selection does not restore a proven no-op.

### Resource hazards

Resource analysis compares a proposed loop with the original. It can certify
non-progress or retained growth only within its supported, bounded model:
typed scalar state, supported collection/iterator operations, closed helper
effects, and specific repeated-state or cache-hit patterns.

The proof respects lexical bindings, field provenance, evaluation effects,
match guards, escape paths, and Rust's typed arithmetic. Overflow, wrapping,
and finite scalar domains are not treated as unbounded mathematical integers.
An open-ended range alone does not prove infinite distinct insertions.

Unknown calls, callbacks, aliases, drops, macros, custom operations, changing
state, and possible exits retain the candidate. Exhausting a source-size,
traversal, interpretation, or provenance allowance is also unknown, never a
proof. A problematic original loop cannot justify removing an unrelated edit.
Certificates are derived from source and validated dependency identities,
not application function names or historical campaign outcomes.

## Stable identities

The current identity scheme is 8. Identities are content-derived from the
source path, containing item, mutator, normalized site, occurrence, and
replacement alternative. They are not line numbers or runtime guard ordinals.
Unrelated source edits should not transfer one mutant's history to another.

Replacement content is included where alternative ordering alone is
insufficient, including stated values and composed synthesized results.
Stated expressions are token-normalized: changing the expression changes its
identity, while whitespace and comments do not.

Semantic admission reserves occurrences before withholding sites where a
later identical site could otherwise inherit their identities. Improving
inference must not move an ID-based suppression or cached verdict onto a
different mutation. Token-identical replacements remain rejected before
occurrence allocation.

The coordinator must still bind historical outcomes to the relevant source
and compilation context. A stable identity alone is not evidence that an old
verdict is reusable.

## Schema semantics

Each selected mutation is guarded by a campaign ordinal, for example:

```rust,ignore
if ::gamma_rt::a(7u32) { (left) <= (right) } else { left < right }
```

Only one mutant is active in a test process. Nested guards appear in the
original branch; the replacement branch contains uninstrumented source with
only that mutation applied. This avoids recursively duplicating every nested
alternative and prevents activating multiple logical edits together.

With no guard active, the schema must retain the original behavior. Rendering
therefore preserves evaluation order, single evaluation, short-circuiting,
temporary lifetimes, drop order, binding scope, and configuration attributes.
It does not eagerly bind operands merely to share text between guard arms.

When a branch-local temporary would escape, the guard lifts to its consuming
expression. Ownership/restoration conflicts can require a containing loop or
block. Assignment destinations must remain valid places and evaluate their
right-hand side once. Attributes stay outside generated operators, and
block-shaped expressions retain the statement/tail grammar of their context.

Different iterator implementations can be joined through the runtime's
`Either` adapter, including supported iterator payloads inside containers.
Unsupported opaque return contracts are not made valid by arbitrary wrapping.

Guard positions distinguish a replacement region from the whole guarded
site. A diagnostic inside a replacement can identify that alternative;
an inference consequence elsewhere does not by itself establish blame.
Compilation and isolation policy belong to the coordinator.

## Compiler boundary and verification

rustc is the final authority on whether the complete schema compiles under the
selected Cargo configuration. Discovery does not launch separate compiler
witnesses for unresolved semantic questions. An optimistic candidate may be
valid and detect a test gap; discarding all uncertainty would lose that value.

The central verification obligations are deterministic identities,
configuration-correct discovery, evidence that survives namespace changes,
and schema behavior with guards inactive and active. Checked-in immutable
fixtures keep source-inspection tests independent of instrumentation applied
to the implementation itself.

Traversal structure, concrete analysis allowances, text encoding, and test
mechanics are recorded in the [implementation guide](IMPLEMENTATION.md).
