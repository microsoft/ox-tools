# cargo-coverage-gate — AI Agents Guidelines

Inherits everything from the workspace-root `AGENTS.md`. Additions below
are specific to this crate.

## Mutation Testing

`cargo-mutants` runs in CI against this crate. Before claiming a change
is done, prefer adding direct unit tests for any private helper whose
arithmetic, comparisons, or branch conditions would otherwise only be
exercised through wider integration tests — those are the mutants the
runner catches most often, and surviving mutants block the build.

`#[mutants::skip]` is acceptable on functions where the mutant set is
genuinely uninteresting (e.g. trivial delegations), but justify the
attribute in a comment.
