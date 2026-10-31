# cargo-coverage-gate — AI Agents Guidelines

Inherits everything from the workspace-root `AGENTS.md`. Additions below
are specific to this crate.

## Mutation Testing

`cargo-gamma` runs in CI against this crate. Before claiming a change
is done, prefer adding direct unit tests for any private helper whose
arithmetic, comparisons, or branch conditions would otherwise only be
exercised through wider integration tests — those are the mutants the
runner catches most often, and surviving mutants block the build.

Keep `gamma::skip` comment suppressions narrow: identify the specific
mutation kinds and include a `reason`. Reserve them for equivalent
mutants or native boundaries that cannot be exercised in-process.
