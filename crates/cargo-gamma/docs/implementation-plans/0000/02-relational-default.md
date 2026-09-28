# Layer 2: greater-than to equality in the default catalog

[Stack root](README.md) | [Previous layer](01-population-scope.md) |
[Next layer](03-flaky-gate.md)

## Outcome and policy

Add `relational.gt_to_eq`, replacing an eligible `left > right` expression with
`left == right`, and enable it by default.

The existing policy in [MUTATORS.md](../../MUTATORS.md#choosing-what-to-run)
places the main catalog in `@default` and reserves `@pedantic` for valid
mutations with evidence of low yield. Its narrow exception policy is reinforced
by `default_and_pedantic_presets_partition_everything` and
`the_noisier_families_are_still_on`.

This addition uses an established relational transformation, produces a boolean,
and does not require extra operand evaluations. It overlaps existing boundary
mutations; the plan does not claim unique marginal detection value. No supplied
evidence establishes a low-yield exception for it. Preserving the old population
size or score is not a reason to keep it disabled.

## Entry points

| Location | Change |
| --- | --- |
| `cargo-gamma-engine\src\ops\registry\catalog.rs` | Add registry entry with `default_on = true` and `ROR` alias. |
| `cargo-gamma-engine\src\ops\collect\collector\tables.rs` | Extend the `BinOp::Gt` replacement table. |
| `cargo-gamma-engine\src\ops\collect\collector.rs` | Reuse normal binary emission, no-op checks, and identical-edit deduplication. |
| `cargo-gamma-engine\src\ops\registry\selection.rs` | Preserve preset partition and update relevant expected membership. |
| `cargo-gamma-engine\src\ops\collect\tests.rs` | Add discovery/selection regressions. |
| `cargo-gamma-lib\src\docs.rs` and `tests\docs.rs` | Generate and check catalog, family, and preset reference blocks. |
| `cargo-gamma-lib\tests\instrumented_compiles.rs` and `tests\session.rs` | Compiler and executed-verdict fixtures. |

## Implementation sequence

### Register and generate the replacement

Add the stable name and description next to `gt_to_ge` and `gt_to_lt`. Append the
new replacement without renumbering existing replacement positions. Verify the
actual identity construction, rather than assuming table order cannot matter.

Keep the family/preset resolver generic. Selecting `relational`, `ROR`,
`@boundary`, `all`, or `@default` includes the new mutator without special cases.
`@pedantic` remains unchanged. Negated selectors and source suppression work
through the existing registry.

Do not add operand type inference. Let the same collector and compiler
viability machinery used by the other comparison mutations decide whether a
candidate can run.

### Preserve emission semantics

Use the existing whole-expression replacement, parentheses, and runtime guard
shape. Do not evaluate either operand outside its chosen branch or introduce
temporary bindings that alter ownership/destruction behavior.

An explicit selection must generate the replacement even if a broader
selection deduplicates an identical edit from another mutator. Do not require
every broad selection to retain duplicate mutations under separate names.

### Update documentation from authoritative data

Regenerate `MUTATORS.md` catalog/family/preset blocks and command references using
the existing documentation generator. Update prose that states a fixed
relational-family count; prefer a description that does not need manual
recounting.

Clarify the existing default-policy explanation only as needed: useful additions
can enter `@default`; evidence supports exceptions; exact no-op filtering is a
separate site-level concern. Do not introduce a new parity preset.

## Acceptance matrix

| Scenario | Required result |
| --- | --- |
| `left > right` with only `relational.gt_to_eq` | One matching equality replacement. |
| Default discovery | The new mutator is enabled and can produce candidates. |
| Family, alias, and boundary preset | Include the new selector through normal resolution. |
| Each existing greater-than mutator selected alone | Existing replacement and identity remain unchanged. |
| `@default,!relational.gt_to_eq` or source suppression | New candidate is excluded through ordinary policy. |
| Runtime fixture over less/equal/greater inputs | Expected failing assertion detects the equality replacement. |
| Operands with observable evaluation counters | Each operand is evaluated once for original and selected mutation. |
| Unsupported compiler combination | Normal unviable/build diagnostic; no special false detection. |
| Default plus pedantic | Covers the complete registry without changing the existing exception. |

Pin replacement text and identities, not just total candidate counts. A fixture
with explicit assertions over ordering cases tests behavior; it is not evidence
of comparative cross-repository yield.

## Completion

Complete when the mutator is independently selectable, available by default,
uses the ordinary schema/suppression/viability paths, and its documentation is
generated from the same registry the executable uses.
