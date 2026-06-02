# cargo-coverage-gate — AI Agents Guidelines

Inherits everything from the workspace-root `AGENTS.md`. Additions below
are specific to this crate.

## Design Docs Are Part of the Source

This crate keeps a living design and implementation plan under `docs/`:

- `docs/design/main.md` — the conceptual design (problem, UX, inputs &
  outputs, exit-code semantics, threshold lifecycle, CI integration,
  cross-cutting concerns, out-of-scope extensions).
- `docs/implementation-plans/0000.md` — the rolling implementation plan
  for v1.

These documents are **not** historical artifacts. They exist for two
reasons:

1. **Review aid** — a human or AI reviewer can understand and validate
   the solution end-to-end without having to read every file. The design
   is the contract; the code is the realization.
2. **AI-friendly evolution** — by keeping the design in lockstep with
   the code, future agentic changes can be initialized from a current,
   accurate spec instead of having to reverse-engineer intent from
   source. This dramatically lowers the cost of subsequent automated
   review and refactoring passes.

### Rules for changes

When you make an implementation change to this crate:

- **If behavior, UX, exit codes, configuration shape, output format, or
  any other externally observable contract changes**, update
  `docs/design/main.md` in the same change. Keep section numbers and
  cross-references stable when possible — other rustdoc and code
  comments reference them (e.g. `render/markdown.rs` cites §6.5).
- **If the change is part of (or completes) a planned work item**,
  update `docs/implementation-plans/0000.md`. Tick the item off, append
  the landing commit hash, and adjust scope notes if the plan needs to
  evolve.
- **If the change is purely internal** (refactor with no observable
  contract delta, dependency bump, test-only change), no doc update is
  required — but if you notice the design is already drifting from the
  code, fix the drift opportunistically.

### Don't delete the design docs

The design and implementation plan are intentional, persistent
artifacts. Don't propose removing `docs/` to "clean up" — the cost of
keeping them in sync is far smaller than the cost of regenerating that
context every time the crate is reviewed or extended.

## Mutation Testing

`cargo-mutants` runs in CI against this crate. Before claiming a change
is done, prefer adding direct unit tests for any private helper whose
arithmetic, comparisons, or branch conditions would otherwise only be
exercised through wider integration tests — those are the mutants the
runner catches most often, and surviving mutants block the build.

`#[mutants::skip]` is acceptable on functions where the mutant set is
genuinely uninteresting (e.g. trivial delegations), but justify the
attribute in a comment.
