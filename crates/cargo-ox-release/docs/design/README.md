# cargo-ox-release — design

## Problem

Oxidizer-style Cargo workspaces publish many small, interdependent crates.
Deciding *what to release and at which version* when a change lands is subtle:
a breaking change in one crate can ripple through the dependency graph, but only
along edges that actually expose the change. Proc-macro crates complicate this
further — a macro can break its consumers' compilation without any type-level
signal that `cargo semver-checks` can see, and a macro's *runtime partner*
crate can be forced to move even though the macro crate's own source is
untouched.

This crate is that planner. Its goal is output that is a pure function of its
inputs: the same facts and the same classified decisions always yield the same
plan, regardless of which reasoning model produced the classifications.

## Guiding principles

1. **Mechanical resolver, model-supplied judgment.** The resolver performs only
   mechanical work — token parsing, version arithmetic, dependency closure,
   type- and macro-contract-aware cascades, pin reconciliation, ambiguity
   reporting, and topological ordering. Judgment calls — classifying a source
   diff, reviewing a proc-macro's behavior — are the *caller's* responsibility,
   supplied through the request. The resolver never guesses; when it lacks a
   required judgment it emits a `blocked` plan naming exactly what to classify.

2. **Determinism is the contract.** Every ordering is total and explicit (ties
   broken by folder), every floor is derived from evidence, and every verdict is
   a checked assertion rather than a bare declaration. Two runs over the same
   frozen facts and request must produce byte-identical plans.

3. **Evidence over assertion.** A "behavior fix" must be demonstrated by a probe
   that failed at the baseline and passes now; a macro's breaking verdict must
   be backed by a compile fixture that flipped pass→fail. Unbacked claims block
   the plan.

## Shape

The tool is a Cargo subcommand invoked as `cargo ox-release <command>`. It is a
thin CLI over a library; the library is the reusable, testable core.

### Phases

The release process has three phases:

1. **facts** — a deterministic workspace snapshot: the dependency graph, public
   type exposure, macro publication, implementation closures, runtime partners,
   modification state, and external-dependency requirement changes.
2. **resolve** — the mechanical planning step. **This crate implements this
   phase.**
3. **apply** — atomic version writes plus changelog and README generation.

Facts and apply are **out of scope** for this crate so far: facts are consumed
as JSON from a separate fact-gathering step, and application is handled
elsewhere. The resolver is the determinism-critical core and the natural first
target: it is pure computation with a well-defined `facts + request → plan`
contract and a golden fixture to validate against.

### Inputs and outputs

`cargo ox-release resolve --facts <facts.json> --request <request.json>` reads
the two JSON contracts and prints the canonical plan JSON to stdout.

- **facts.json** (schema version 5) — the workspace snapshot. See
  [`model::facts`](../../src/model/facts.rs).
- **request.json** — `mode` (`targeted`/`changed`/`all`), accepted `tokens`,
  per-candidate `selectionDecisions`, per-package `classifications`, per-macro
  `macroContracts`, and an optional `force`. See
  [`model::request`](../../src/model/request.rs).
- **plan.json** — `status` (`resolved`/`blocked`), the ordered `releases`, echoed
  `selectionDecisions` and `macroContracts`, `ambiguities` (when blocked), and
  `warnings`. See [`model::plan`](../../src/model/plan.rs).

### Exit codes

- `0` — the plan was produced (whether `resolved` or `blocked`; a blocked plan is
  a valid, actionable result, not a failure).
- non-zero — a hard input error the resolver refuses outright (unknown mode,
  unpublishable or duplicate token, contradictory pin, malformed request), or an
  I/O / parse failure. Clap parse errors and `--help`/`--version` follow clap's
  conventional codes.

## Resolver mechanics

The resolver is organized as a set of cohesive steps over shared state:

- **Change types** are ranked `none < patch < non-breaking < breaking`; the
  "stronger of two" is a `max`. Version arithmetic honors Cargo's `0.x` rules
  (on `0.x.y`, a breaking change moves the *minor*; on `0.0.x`, everything is
  breaking). See [`version`](../../src/version.rs).

- **Selection** grades each candidate against a deterministic table of reasons
  (`breaking`, `nonbreaking-api`, `behavior-fix`, `authored-doc-fix`,
  `runtime-manifest-change`, `first-release`, and the decline reasons). The
  table encodes precedence rules — for example, a runtime-manifest change owns
  the reason over an `authored-doc-fix`, and a rustdoc-visible doc-only change
  with no implementation change must be accepted as `authored-doc-fix`.

- **Macro contracts** derive a verdict *floor* from measured compile-fixture
  evidence (pass→fail is breaking, fail→pass is non-breaking) and block any
  declared verdict below that floor. A contract must cover the computed review
  scope (self plus modified implementation-closure members and runtime
  partners).

- **Floors.** A breaking external requirement change on a *publicly exposed*
  dependency forces a breaking floor (the "syn 2→3" case: a macro-impl crate
  that re-exports `syn` types breaks its consumers even though its own workspace
  rustdoc is unchanged). A crate may not claim breaking/non-breaking on its own
  account unless its packaged Rust source actually changed.

- **Cascade.** From each released crate, the resolver walks to dependents along
  three edge classes — ordinary `type` exposure, `macroPublic`/`macroPrivate`
  macro edges, and `macroRuntime` edges into the proc macros whose generated
  code names a broken runtime dependency — strengthening each dependent's change
  type and recording an evidenced cascade reason.

- **Ordering.** The release set is topologically ordered (dependencies before
  dependents, ties broken by folder), and a dependency cycle is a hard error.

## Determinism validation

The resolver is validated against a real captured `changed`-mode simulation of
39 releases (18 breaking, 2 non-breaking, 19 patch; 13 user-seeded, 26 cascade).

- [`tests/golden.rs`](../../tests/golden.rs) asserts the resolver produces the
  captured plan exactly, and cross-checks the headline counts.
- Many independent runs over the same frozen facts — each with a *different*
  request (different classifications and evidence prose) — resolve to the
  identical plan, byte-for-byte.

## Out of scope / future work

- **facts** gathering (git/cargo/manifest analysis) and **apply** (version
  writes, changelog/README generation, validation, rollback).
- Publishing. The tool plans; it never publishes.
