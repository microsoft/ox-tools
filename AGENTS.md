# AI Agents Guidelines

Code in this repository should follow the guidelines specified in the [Microsoft Rust Guidelines](https://microsoft.github.io/rust-guidelines/agents/all.txt).

## README Files

Crate README files are auto-generated via `just readme`. Do not manually update them.

## Executing `just` commands

If you only touch one crate, you may use `just package=crate_name command` to narrow command scope to one crate.

## Pre-commit Checklist

- Run `just format` to format code.
- Run `just readme` to regenerate crate-level readme files.
- Run `just spellcheck` to check spelling in code comments and docs.

## Spelling

The spell checker dictionary is in the `.spelling` file, one word per line in arbitrary order.

## Changelogs

Do not manually edit `CHANGELOG.md` files. Changelogs are automatically updated on release.

## Design Docs Are Part of the Source

Every crate captures its design under `crates/<crate>/docs/`, kept in
lockstep with the code (as pioneered by `cargo-anvil` and
`cargo-coverage-gate`). Design is the contract; code is the realization.

### Layout

- `docs/design/README.md` — the top-level conceptual design: the problem,
  guiding principles, and the user-visible shape (UX, inputs/outputs,
  exit-code semantics, configuration, CI integration, cross-cutting
  concerns, out-of-scope). Named `README.md` so it renders as the
  `docs/design/` landing page on GitHub and ADO. For larger crates, split
  detail into companion docs (e.g. `docs/design/<topic>.md`) linked from
  it.

Optionally, a crate may keep implementation plans under
`docs/implementation-plans/NNNN.md` (rolling, zero-padded) to sequence
large multi-commit efforts. They are never required — skip them for small
crates or self-contained changes.

### Why they exist

1. **Review aid** — a human or AI reviewer can validate the solution
   end-to-end without reading every file.
2. **AI-friendly evolution** — future agentic changes start from a
   current, accurate spec instead of reverse-engineering intent, which
   lowers the cost of every later review/refactor pass.

### Rules for changes

When you implement a change in a crate:

- **If an externally observable contract changes** (behavior, UX, exit
  codes, config shape, output format, public API), update that crate's
  design doc in the *same* change.
- **If the change is purely internal** (refactor with no contract delta,
  dependency bump, test-only), no doc update is required — but fix any
  design drift you notice opportunistically.

### Don't reference design docs from code

Design docs are a review and evolution aid, not a stable API. Do **not**
cite design sections from rustdoc, code comments, or other source
(e.g. "see design §6.5"): it couples the code to the doc's structure and
blocks reorganizing the design without churning source. References flow
one way — design docs may point at code, not the reverse. Link between
design docs freely.

### New features and new crates

- A new user-facing feature starts with a design-doc update (or a new
  companion doc) *before or alongside* the implementation.
- A new crate lands with at least `docs/design/README.md`. Add a
  crate-level `AGENTS.md` only if the crate has rules beyond this root
  file (e.g. extra CI gates); it need not restate the shared policy.

### Don't delete the design docs

The design docs — and any implementation plans — are intentional,
persistent artifacts. Don't remove `docs/` to "clean up" — keeping them
in sync is far cheaper than regenerating that context on every review or
extension.

## Maintainability

While it is fine to use `.expect()`, the precondition is that it is either a programming error (the caller did something wrong)
or a situation that can never happen (in the absence of bugs). The expect-message must document either what the caller did wrong
in their code or why we believe the situation could never happen.

This is bad code: `self_span.get(self_offset..).expect("self_offset out of bounds")` - it does not explain what the caller did
wrong and it does not explain why we believe this access can never be out of bounds.

This is good code: `self_span.get(self_offset..).expect("guarded by min() above to never exceed span length")` - this explains
why we believe the operation can never cause an out of bounds access.
