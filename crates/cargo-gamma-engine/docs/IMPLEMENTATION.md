# cargo-gamma-engine — Implementation guide

This guide records replaceable implementation choices behind the contracts in
[`DESIGN.md`](DESIGN.md).

## Discovery pre-pass

Configuration-aware discovery combines stated-value validation and the
numeric/import indexes in one `syn::visit::Visit` traversal. The visitor gates
items, associated items, fields, statements, and expressions before either
pre-pass sees them. Standalone stated-value validation deliberately uses its own
whole-file traversal because it has no selected build configuration.

The combined visitor retains the audit and indexer as separate state objects.
Their per-node update methods are shared with the standalone implementations,
which keeps equivalence tests able to compare the fused and separate paths.

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
