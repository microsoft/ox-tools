# Implementation plan 0000: cargo-gamma validation improvements

## Goals and boundaries

Deliver reliable targeted reruns, stricter handling of inconclusive results,
library-only test selection, flexible compiler-artifact placement, and broader
mutation coverage. Const functions participate in ordinary campaigns by default;
an execution optimization must not determine whether important production code
is tested.

This plan is the root of a PR stack. Each linked layer specifies a complete
review unit, including its implementation sequence and acceptance criteria.
The plans use the architecture in [DESIGN.md](../../DESIGN.md) and
[IMPLEMENTATION.md](../../IMPLEMENTATION.md), and the selection policy in
[MUTATORS.md](../../MUTATORS.md).

Source entry points are based on
cf91cdaf41b504106ecd024ce1dbb4973e68b5f9. Paths in implementation tables are
relative to the repository's `crates` directory. Proposed internal type and
field names communicate responsibilities, not a requirement to introduce a
separate abstraction for every name.

## Stack and delivery order

| Layer | PR title | Detailed plan | Semantic prerequisites |
| --- | --- | --- | --- |
| 1 | cargo-gamma: preserve population scope in partial reports and merges | [Population scope](01-population-scope.md) | None |
| 2 | cargo-gamma: enable greater-than-to-equality mutation by default | [Relational mutation](02-relational-default.md) | None |
| 3 | cargo-gamma: fail on unresolved flaky outcomes by default | [Flaky-result gate](03-flaky-gate.md) | Layer 1 provenance |
| 4 | cargo-gamma: enforce library-only compilation and verdict targets | [Library targets](04-library-targets.md) | Layer 1 context |
| 5 | cargo-gamma: support external Cargo artifacts with unchanged source handling | [External artifacts](05-external-artifacts.md) | None |
| 6 | cargo-gamma: replay and explain exact mutant identities | [Exact replay](06-mutant-replay.md) | Layer 1 compatibility |
| 7 | cargo-gamma: mutate const functions by default | [Const functions](07-const-functions.md) | Layers 1, 3, 4, 5, and 6 |

The bottom PR targets `main`; every higher PR targets the preceding layer's
branch. The linear review order does not imply a code dependency between every
adjacent layer. Lower layers may merge before const support is complete.

Create one implementation session per layer. Commit and push a lower layer
before creating the next session from its branch. Each session owns its Git
mutations and opens its own PR through the app. Register native stack membership
after the PR chain exists. Keep this plan-only change separate from the feature
PRs unless the implementation kickoff explicitly chooses it as the first base.

## Requirements traceability

Report numbers identify the supplied validation reports, not issue numbers.
The requirements are reproduced here so implementation does not depend on access
to the original report directory.

| Report | Requirement | Disposition |
| --- | --- | --- |
| 02 | Library selection constrains compilation and all test phases, using full target identity. | Layer 4 |
| 03 | Const mutations compile and can be detected even with compile-time callers. | Layer 7, enabled by default |
| 05 | Reported IDs select exact candidates for fresh execution without silently shrinking requests. | Layers 1 and 6 |
| 06 | `explain` accepts advertised mutant IDs as well as registry selectors. | Layer 6 |
| 07 | Broader unresolved-result iteration. | Deferred; use `--only-survivors-from`. See [F4](../../TODO.md#f4). |
| 08 | Flaky outcomes fail independently of the percentage score. | Layer 3, strict by default |
| 09 | Optional no-match exclusions for reusable policies. | Current strict behavior accepted; no implementation |
| 10 | Greater-than to equality is independently selectable. | Layer 2, also in `@default` |
| 12 | Heavy Cargo artifacts can move outside the checkout without changing existing scratch-source/Git handling. | Layer 5 |

No layer introduces `--iterate-from`, optional exclusions, a generalized
unsupported-region inventory, or a const-specific public verdict vocabulary.
No layer reuses historical test detections as new-run evidence.
Layer 5 is artifact-only placement, not Git virtualization: no proxy, global Git
redirection, or original-checkout root/dirty-state guarantees are introduced.

## Shared behavioral contracts

### Defaults and configuration

| Surface | Contract |
| --- | --- |
| Default mutators | Main useful catalog, including `relational.gt_to_eq`; evidence-based low-yield exceptions remain in `@pedantic`. |
| Flaky outcomes | `run` and `merge` fail by default; `--no-fail-on-flaky` opts out only of that gate. |
| Library targets | `--lib` opts into library-unit-test-only judging and harness compilation; ordinary selection remains available. |
| Const functions | Included by default; `--no-const-fns` explicitly narrows mutation selection. |
| Exact replay | Repeated `--mutant-id ID`; `--from-report PATH` supplies an exact population or corroborates a named subset. |
| Explanation | `explain SUBJECT`; mutant subjects support current discovery or `--report PATH` for historical evidence. |

Mirror new booleans in `gamma.toml` using `no-fail-on-flaky`, `lib`, and
`no-const-fns`. Follow existing configuration resolution: absence retains the
file/default setting; an explicit enabling flag takes effect; `--no-config`
provides the existing escape from configured opt-outs. Do not invent positive
aliases or silently alter other negative-flag semantics.

The run reads `gamma.toml` as it does today. `merge` continues to operate on named
reports, with its own `--no-fail-on-flaky` option rather than inheriting a
producer's decision to disable a gate or implicitly loading an unrelated
workspace configuration.

### Population, evidence, and outcomes

A **population** is the set of mutation candidates selected under a discovery
context. A **verdict** is evidence obtained for a candidate. Discovering a complete
population does not mean all its candidates were executed: an unfiltered `list`
can describe the population while carrying pending verdicts.

Keep these concepts separate in report metadata. A partial run cannot withdraw
identities it omitted. A merged report preserves the original provenance of each
verdict rather than presenting all observations as freshly obtained.

Gamma's standard report statuses are not a lossless outcome vocabulary by
themselves. Timeout and memory outcomes use `Survived` plus reason prefixes;
flaky and not-built outcomes use `Ignored` plus distinct reason prefixes.
Readers must use one consistent interpretation for Gamma reports.

Const mutants use the ordinary identity and outcome paths. Internal selection
of a different executable is not a reason to change what `killed`, `survived`,
`unviable`, `notbuilt`, `uncovered`, or `flaky` means.

### Exit semantics

| Condition | Exit behavior |
| --- | --- |
| Invalid selector, contradictory effective options, unresolved explicit ID request | Existing usage-error code, `1` |
| Failed percentage, expectation, or default flaky-result gate | Existing gate-failure code, `2` |
| Infrastructure/build/baseline failure preventing a trustworthy campaign | Existing inability-to-proceed code, `3` |
| No failing enabled gate and a successfully completed operation | `0` |

Preserve existing precedence and reporting for build failures. The flaky opt-out
does not disable baseline, confirmation-independent resource handling, score,
pending, ungraded, or expectation protections. Empty ungated selections do not
become flaky failures merely because the default policy is stricter.

### Internal versus public commitments

Document CLI meanings, configuration precedence, candidate selection, identity
compatibility, outcomes, report provenance, and exit behavior as contracts.
Keep scratch-directory names, internal enums, cache key layouts, and serialized
build scheduling in implementation documentation unless already public.

Do not cite implementation-plan sections from rustdoc or code comments. Explain
non-obvious code decisions locally. Design and implementation documents may link
to code and to these plans.

## Cross-layer handoffs

| Owning layer | Information consumed later |
| --- | --- |
| 1 | Versioned scope, inherited completeness assertions, verdict provenance, bounded report reading, legacy unknown-state handling |
| 3 | Lossless Gamma outcome decoding, effective confirmation policy, default flaky-gate evaluation |
| 4 | Unambiguous test-target identity and one policy used by every Cargo/harness path |
| 5 | Independently located artifacts, unchanged scratch-source/Git handling, owned state lookup |
| 6 | All-or-nothing exact resolution and corroborated report-to-current-source matching |
| 7 | Runtime/static execution routing without changing the public meaning of a mutant or verdict |

Extend these handoffs in their owning layer or deliberately in the consuming
layer. Do not create competing outcome decoders, target classifiers, or report
loaders simply to keep file diffs disjoint.

## Verification and documentation

Each layer contains its own acceptance matrix. Use the existing engine/unit tests
for pure transformations, fake hosts and fixed report timestamps for CLI gates,
and owned temporary real-Cargo fixtures where compiler, target, or Git behavior
is the actual requirement. Use controlled state transitions, not random flaky
tests or real-time timeout failures. Progress checks and bounded generators must
keep tests and fuzz inputs finite under mutation testing.

Use the generated Anvil recipes as the source of truth. During implementation,
run the affected focused tests and check recipes first, then `just anvil-pr`.
For CI-backed work, `just anvil-pr-fast` is an agreed partial local alternative,
not full verification. Install missing tooling only with permission; follow a
missing-prerequisite failure with the appropriate setup recipe.

Common documentation work:

- Update `DESIGN.md` and `IMPLEMENTATION.md` with each observable contract change.
- Update `CONFIG.md`, `gamma.toml`, `CMDLINE.md`, `MUTATORS.md`, and the executable's
  Rust documentation when their surfaces change.
- Generate README content with `just anvil-readme --fix`.
- Regenerate command/catalog blocks using the existing
  `cargo-gamma-lib` documentation test with `GAMMA_BLESS_DOCS=1`, then run it
  without blessing. In PowerShell, use a temporary process environment and the
  package-qualified `cargo test -p cargo-gamma-lib --all-features --test docs`;
  remove the blessing variable afterward.
- Format source with `just anvil-fmt --fix`. Do not hand-edit generated reference
  blocks or changelogs.

Coordinate the doc generators: update Rust documentation first, regenerate the
README, bless reference tables, and check both generated outputs for consistency.
Do not leave a generated README different from its Rust documentation source.

## Principal implementation risks

| Risk | Required resolution |
| --- | --- |
| A narrow report looks like a population snapshot | Layer 1 proves completeness and context compatibility explicitly. |
| Repeated sites inherit another site's old ID | Layer 6 corroborates matches using enclosing-source context, not just IDs. |
| Library-only fallback widens targets | Layer 4 tests every command-producing path with a forbidden integration target. |
| Artifact relocation changes source handling | Layer 5 compares equivalent Gamma runs without changing or proxying Git. |
| Confirmation reruns a static mutant | Layer 7 uses an immutable baseline executable for unmutated confirmation. |
| Runtime reach evidence hides a compile-time mutation | Layer 7 bypasses negative runtime-census conclusions for static candidates. |
| Estimates omit const compilation | Layer 7 accounts for per-variant builds and serialized work without suppressing coverage. |

Resolve a failed mechanism proof within its owning layer. Do not weaken the
accepted behavior to make a convenient implementation pass. If no supported
mechanism can satisfy a contract, bring that specific design decision back for
approval rather than silently reducing scope.
