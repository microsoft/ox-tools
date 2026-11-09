# Implementation reference

Contributor instructions are maintained in [AGENTS.md](../AGENTS.md).
Reader-facing behavior and limitations are defined in the [design](design/README.md).

For implementation work, the relevant source boundaries are:

- `src/catalog/` and `src/anvil/artifacts/`: catalog construction and built-in artifacts.
- `src/update.rs`, `src/run.rs`, and `src/plan.rs`: repository I/O, planning, and application.
- `src/region.rs` and `src/emit/`: ownership boundaries and source-preserving edits.
- `templates/`: generated recipe, configuration, and CI content.

The source and its tests define implementation details. This document does not
maintain a second description of their algorithms.
