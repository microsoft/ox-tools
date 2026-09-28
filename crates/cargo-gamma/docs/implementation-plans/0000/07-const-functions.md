# Layer 7: mutate const functions by default

[Stack root](README.md) | [Previous layer](06-mutant-replay.md)

## Outcome and boundaries

An ordinary invocation discovers and executes eligible const-function mutations
using the same selected mutators, suppression rules, identities, and outcome
vocabulary as other functions. `--no-const-fns` is the explicit selection opt-out.

The internal mechanism is different: ordinary candidates use a shared runtime
schema; const candidates use one statically replaced source variant per build.
Preserve `const` on production functions and let compile-time callers observe
the selected replacement. Do not use unstable compiler intrinsics or split a
function into secretly different runtime and compile-time implementations.

Support const function bodies, including associated methods and representative
return/unary/arithmetic/relational/bitwise mutations. This does not automatically
expand the catalog to standalone constant/static initializers, array lengths,
discriminants, macros, or other intentionally unhandled syntax.

There is no const-specific public outcome, unsupported-region inventory, or
coverage percentage. Generated-but-unviable mutants use normal outcomes;
ungenerated constructs are not presented as tested.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-engine\src\ops\collect\collector.rs`, `candidate.rs`, `shape.rs` | Const traversal, strategy, edit meaning |
| `cargo-gamma-engine\src\ops\collect\stated.rs` | Stated-value validation independent of runtime guards |
| `cargo-gamma-engine\src\model\mutant_definition.rs`, `schema.rs` | Definition projection and source rewriting |
| `cargo-gamma-attrs-impl\src\implementation.rs` | Attribute acceptance; still expands to unchanged source |
| `cargo-gamma-lib\src\model\mutant.rs`, `discover\survey.rs` | Strategy routing, ordinals, population selection |
| `cargo-gamma-lib\src\exec\build.rs`, `workspace.rs`, `measure.rs` | Baseline/variant generations and compiler attribution |
| `cargo-gamma-lib\src\exec\sweep.rs`, `verdict.rs`, `nextest.rs` | Test attempts and correct unmutated confirmation |
| `cargo-gamma-lib\src\exec\session.rs`, `events.rs`, `diag` | Honest aggregate progress/cost reporting |
| `cargo-gamma-lib\src\estimate.rs` | Static build work and serialized execution in projections |
| `cargo-gamma-lib\src\exec\build\splices.rs` | Runtime-only guard projection and ordinal invariants |
| `cargo-gamma-lib\src\discover\record.rs`, `hints.rs`, `commands\suppress.rs` | Strategy-safe learning and postprocessing |

## Internal model

### Candidate strategy is separate from edit shape

Carry a small strategy distinction from collection to execution: runtime schema
or static replacement. `Shape` continues to describe what source construct is
edited. Do not overload `Outcome`, mutator name, or ordinal zero to select the
execution mechanism.

The run-local ordinal may remain common bookkeeping, but only runtime candidates
may become `GAMMA_ACTIVE` values or require emitted guards. Audit predicates
that currently treat every pending/positive-ordinal candidate as a runtime guard.

Recheck suppression annotations that justify ordinal-based predicates in
`estimate::project` and `build::splices`. Keep an annotation only when its
stated invariant still holds; do not preserve a stale justification that hides
a newly meaningful mutation.

Keep ordinary candidate IDs unchanged. Execution paths and cache locations do
not enter identity. New const candidates receive IDs through the same existing
scheme. The effective const opt-out participates in layer 1's population context.

### Artifact generations

Retain the existing runtime-schema workspace and its baseline behavior. For
static work, own an immutable pristine baseline source/artifact generation and
a reusable mutable variant generation under the same campaign ownership/lock.

The static baseline is deliberately distinct from the instrumented runtime
baseline. A static variant is compared with the same uninstrumented source shape,
not a differently instrumented executable with unrelated layout/cost changes.
A const-only campaign does not need to build an otherwise empty runtime schema.

Every static variant derives from the same captured pristine source generation
plus exactly one edit. Never copy a still-mutated prior variant as the next
baseline. Do not overwrite baseline binaries through a shared Cargo target path.

Serialize static build/run cycles initially. Reuse the variant artifact directory
between completed variants, but never replace it while one of its processes or
confirmation operations is still using it.

## Implementation sequence

### Enable discovery without allowing illegal runtime guards

Replace the blanket const-function inert behavior with strategy-aware traversal.
Keep inert reasons for test code, inactive cfg, and unsupported nested constant
positions distinct. A const function's body gets static candidates; its nested
items and contexts must follow their actual syntactic role, not accidentally
inherit runtime eligibility.

Remove the early function-value exclusion only for the supported static path.
Reuse the existing value/operator generators, no-op filtering, suppression, and
identity construction. Do not invent a second catalog.

Add `--no-const-fns` to shared mutation selection and
`no-const-fns` to configuration. Apply it in run, list, current-source explain,
and exact replay before execution. An explicitly requested const ID conflicting
with the opt-out produces a selection diagnostic, not a silent omission.

Update stated-value validation on both engine and attribute sides. Const
functions with nonempty bodies may state a syntactically valid expression;
the compiler still decides type and const-evaluation viability. Empty or
bodiless unsupported items and malformed/duplicate annotations retain explicit
diagnostics. The attribute never rewrites production behavior.

### Implement actual static edits

Do not assume every `Candidate.replacement` is a literal whole-span replacement
that can be spliced unchanged. `Shape::Block` requires braces, statement deletion
requires removing the statement, and arm/loop shapes currently encode runtime
guard behavior.

Provide a shape-aware static edit renderer that shares source-span validation
with the existing schema writer. Cover expression, function-block, statement,
loop-control, and match-arm behavior offered inside supported const functions.
For an arm made nonmatching, preserve valid syntax and fallthrough semantics
rather than blindly deleting the pattern span.

Validate UTF-8 boundaries, BOM handling, original-span content, and source
digests before writing. Invalid internal spans are programming errors, not
unchanged-source fallbacks. Use existing atomic/transactional publication helpers
where applicable to avoid a partially written source being compiled.

Use direct single-mutant replacement rather than a second compile-time-selected
shared schema: both pay per active variant, while direct edits avoid
type-checking inactive mutation branches. This choice is internal, not a
permanent public restriction on future optimization.

### Prepare and compile static generations

Partition the discovered population by strategy while preserving one coherent
report population. Static-only work must not trigger the current
`anything_live == false` early exit merely because no runtime guard was emitted.

Build the pristine static test binaries under the same target, feature, profile,
Cargo, Git, and oracle settings as variants. Establish the baseline normally.
`--no-baseline` skips measurement, not construction of the unmutated executable
needed for confirmation.

For each selected static candidate:

1. Validate the immutable generation and render one edit into the variant source.
2. Build eligible test artifacts with the layer 4 invocation policy.
3. Establish whether a failure is a compiler-unviable mutation or inability to
   proceed; retain structured diagnostics.
4. Run the configured oracle and settle the outcome with normal confirmation.
5. Finish process-tree cleanup before resetting/reusing the variant generation.

A pristine build that fails aborts the campaign. A structured compiler failure
caused by the selected replacement, including a compile-time assertion at a
caller, is unviability. A build timeout, missing dependency/tool, process failure,
or ambiguous unattributed failure is not a killed or automatically unviable
mutant. Recheck the pristine invocation when needed to establish attribution;
if that cannot establish a trustworthy comparison, report not-built/infrastructure
failure rather than guessing.

Apply the existing fixed build timeout to each Cargo invocation, including
static variants and any attribution recheck. If a timeout multiplier is selected,
use the successful static pristine build as the comparable reference for its
variant builds, retaining the existing floor. Do not scale against an unrelated
runtime-schema build or reset a budget repeatedly within one invocation.
There is no new implicit whole-campaign timeout. When neither build limit is
configured, preserve the existing unlimited-build policy rather than inventing
a hidden cutoff for const functions. Build-budget exhaustion is not-built/
infrastructure failure, never a test assertion detection.

### Refactor attempts to select the correct executable

Current confirmation clears `Attempt.active` and reruns the same binary. That
cannot undo a source-level static mutation.

Represent the mutated and unmutated execution contexts explicitly, including
workspace, binary, runner metadata, working directory, loader environment, and
baseline-derived budgets. Runtime attempts may use one binary with different
selector values; static attempts use corresponding binaries from separate
generations.

Map binaries using layer 4's logical target identity, not filename hashes.
Resolve package identities across scratch roots to the same source package.
Missing or ambiguous counterpart mapping is an infrastructure error, never a
successful confirmation.

Refactor both assertion-failure confirmation and nextest enumeration
confirmation. Remove the assumption that `active == None` always means a
baseline observation: a static mutated executable has no runtime selector but
is still a mutation attempt.

Retain existing timeout/stall/memory confirmation behavior against the mutated
variant. Unmutated confirmation uses the matching pristine baseline binary and
correct test selection. Nextest inventory, environment, and binary metadata are
generation-specific and must never be reused for a different executable.

### Execute conservatively and preserve accounting

Bypass runtime guard census and negative runtime reach observations for static
candidates. A const value can be computed entirely at compile time and still
affect a test. Absence of a guard event is no evidence of missing coverage.

Initially use all eligible tests within the configured package/oracle policy.
Only omit a target with actual proof that it cannot judge the candidate.
Checked positive killer hints may change ordering, but cannot replace complete
fallback execution when the probe does not detect the mutant.

Calibrate the static baseline under the concurrency actually used for static
execution. Account for both runtime and static baseline/build work honestly;
do not present a mixed campaign as one build or report only the first group's
outcomes. Avoid double-counting a candidate when combining results.

Extend `estimate::project` and the live completion estimator with remaining
static build work and serialized static test work. Do not divide those costs by
the runtime worker count. Before representative variant-build evidence exists,
label the projection as incomplete or uncertain rather than multiplying only
test time and presenting it as total remaining cost. Keep test-time ceilings
distinct from a campaign bound that would also need to cover compiler work.

Use observed build counts, representative timings, and recording fakes to expose
cost and catch accidental redundant rebuilds. Cost evidence informs estimates
and optimization; it is not a threshold that disables default const coverage.

Keep new strategy-sensitive cache context explicit. Invalidate or decline
legacy build/reach evidence that cannot distinguish mechanisms. Persist safe
learning and final outcomes only after their source generation is validated.
Never persist a missing runtime-reach event as negative coverage for a static
site.

Ensure final report source comes from pristine captured bytes, not the last
static replacement. Hints and suppression promotion operate on ordinary IDs,
source evidence, and outcomes, without depending on temporary variant paths.

## Acceptance matrix

| Scenario | Required result |
| --- | --- |
| Ordinary run/list with no const flag | Const candidates are discovered; runs execute them. |
| Free and associated const functions | Representative return, unary, arithmetic, relational, and bitwise candidates work. |
| Same function has runtime and compile-time callers | Signature remains const; selected replacement affects both consistently. |
| Test observes only a compile-time-computed value | Mutation can be detected; no false runtime-census uncovered result. |
| Const-only package | No empty-schema early success; population is actually judged. |
| Mixed runtime/static campaign | Each candidate has one ordinary outcome and coherent final reporting. |
| Stated value on const function | Valid syntax is accepted; const-invalid generated replacement is compiler-unviable. |
| Mutation breaks a compile-time assertion | Unviable, not killed. |
| Static mutated test fails, pristine counterpart passes | Normal detected outcome. |
| Pristine confirmation also fails | Flaky; layer 3 fails by default. |
| Nextest enumeration fails only with static mutation | Confirmation uses pristine metadata/executable, not the mutated generation. |
| Unsupported syntax or explicit opt-out | No invented verdicts or claim that omitted code was tested. |
| Exact-ID and existing survivor-only replay | Const candidates resolve and receive fresh execution. |
| Source changes during preparation | Generation check stops before testing wrong bytes. |
| Interrupted/failed build and cleanup | No source-checkout edits, cross-variant contamination, or false completed ledger. |
| Several static candidates in a mixed campaign | Count pristine/variant builds; confirmation reuses baseline artifacts. |
| Estimate with static work and multiple runtime workers | Includes compiler work; serialized work is not divided across lanes. |
| Fixed or scaled build limit reached | Not-built/infrastructure failure; no detection credit or hidden campaign timeout. |
| Runtime-only selection | Existing shared-schema behavior and stable ordinary IDs remain. |

Start with a free const function and an associated const function end to end,
then cover edit shapes and integrations before exposing the default behavior in
the completed layer. Do not land a diagnostics-only substitute for execution.

Use engine/attribute agreement tests, static edit compile fixtures,
`tests\instrumented_compiles.rs`, `tests\session.rs`, and fake observation
sequences for confirmation failures. Extend bounded parser/edit generators for
nested const/non-const contexts, UTF-8 spans, and reset/apply sequences; reject
non-progressing mutations instead of leaving tests hung.

## Documentation and completion

Update the architecture's single-build explanation, const omissions in
`MUTATORS.md`, CLI/configuration, stated-value contracts, cache compatibility,
and execution guide. Keep ordinary outcome vocabulary; describe cost accurately
without introducing a special report of unmutated regions.

Complete when default campaigns test supported const bodies, all confirmations
use genuinely unmutated counterparts, and no optimization can hide static
mutations behind an absent runtime reach event.
