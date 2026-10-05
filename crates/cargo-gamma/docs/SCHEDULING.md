# Scheduling

This document describes how cargo-gamma chooses test work, beginning with
target-linkage discovery before the unmutated baseline. It covers the current
implementation, not a proposed replacement.

## Contents

1. [Target-linkage discovery](#1-target-linkage-discovery)
2. [Baselining](#2-baselining)
3. [Loading hints](#3-loading-hints)
4. [Census admission](#4-census-admission)
5. [Census walk](#5-census-walk)
6. [Projecting census results](#6-projecting-census-results)
7. [Scheduling mutant work](#7-scheduling-mutant-work)
8. [Testing one mutant](#8-testing-one-mutant)
9. [Learning during the sweep](#9-learning-during-the-sweep)
10. [Persistence and reporting](#10-persistence-and-reporting)

```mermaid
flowchart TD
    build[1. Build and target-linkage discovery]
    baseline[2. Baseline measurements]
    hints[3. Loaded execution hints]
    candidates[4. Census candidates and budget]
    census[5. Per-binary reach observations]
    selections[6. Mutant/binary selections]
    queue[7. Assignment-time scheduler]
    tests[8. Mutant verdicts]
    learning[9. Updated in-memory knowledge]
    persistence[10. Run record and diagnostics]

    build --> baseline
    baseline --> candidates
    hints --> candidates
    candidates --> census --> selections
    baseline --> queue
    hints --> queue
    selections --> queue --> tests --> learning --> persistence
```

## Shared test resources

Tests may declare a named shared resource with
`#[gamma::resource("name")]`. A function annotation applies to that exact
test and requires a test attribute such as `#[test]` or `#[tokio::test]`; an
inline module annotation applies to every execution of its containing test target.
The source states identity, not machine policy. Capacities come from
the `[resources]` table in `gamma.toml`, may be overridden by repeatable
`--resource-concurrency NAME=N` arguments, and default to one when omitted.
Capacities for resources outside the selected packages remain inactive rather than
invalidating a package-scoped campaign.

Before launching a baseline, census, mutant, or confirmation process,
cargo-gamma atomically reserves every resource required by that selection and
releases it when the complete process tree has ended. Sweep assignment also
tracks conservative per-mutant resource requirements: when a capacity is
occupied, the scheduler skips blocked mutants and admits unrelated queued work
instead of assigning a worker that would wait at process launch. The
process-level reservation remains authoritative. Unrelated tests remain
parallel.

## 1. Target-linkage discovery

**Goal:** avoid building, baselining, censusing, or running test binaries that
can be proven unrelated to every pending mutant.

This is a logical scheduling phase within the existing build/preflight work; it
does not currently have its own progress-display label. Its purpose is to
remove test targets proven unable to link any pending mutated source before
paying to baseline or census them.

### 1.1. Coarse package admission

**Goal:** establish the broad set of test binaries policy and Cargo's package
graph permit to judge each mutant.

- The configured test scope first admits packages:
  - package-local testing admits the mutant's own package;
  - `--test-package` admits named oracle packages;
  - `--test-workspace` admits the workspace.
- Cargo dependency analysis then removes admitted package/binary relationships
  that cannot reach the mutated package.
- Unknown package relationships fail open and remain admitted.

This is crate/package-level reachability. It says that a binary may link the
mutated crate, not that it links a particular source file or executes a
particular mutation site.

**Output:** a conservative mapping from each mutant's package to the test
binaries permitted to judge it.

### 1.2. Compiler-captured exact source linkage

**Goal:** refine package-level candidates to test binaries that actually link
each pending mutated source file.

Phase 1.2 cannot replace phase 1.1:

- package admission is policy: a reverse-dependent package may link the source
  but is not allowed to judge it unless `--test-package` or `--test-workspace`
  admits that package;
- exact linkage is optional evidence and can be unavailable or ambiguous, so
  phase 1.1 is the conservative fallback;
- exact linkage only narrows the admitted set; it never introduces a binary
  excluded by package policy.

Conceptually, final candidates are the package-policy candidates intersected
with proven source-linked binaries when exact evidence is available. Without
that evidence, the package-policy candidates remain unchanged.

During the successful Cargo preflight, cargo-gamma installs itself as a
`RUSTC_WRAPPER`. The wrapper:

- forwards every invocation to the configured original wrapper or `rustc`;
- returns that compiler process's representable exit code to Cargo unchanged
  (or failure when it cannot launch the compiler or obtain an exit code);
- records only successful compiler invocations;
- records the crate name and type, `--test` state, primary Rust source,
  output directory, extra filename, and every path-bearing `--extern`;
- marks an invocation opaque when an external dependency cannot be associated
  with a concrete artifact path.

If Cargo configuration declares either `build.rustc-wrapper` or
`build.rustc-workspace-wrapper`, cargo-gamma conservatively stands down from
compiler interposition. A relative configured wrapper is resolved from the
configuration that declared it, and moving that path into the scratch
workspace could invoke a different program. Package-level reachability remains
the safe fallback when exact compiler capture is unavailable.

Cargo's JSON `compiler-artifact` messages independently identify each package,
target kind, target source, executable, and emitted artifact. Cargo-gamma
associates a test artifact with a compiler capture only when:

- the target source matches;
- the capture is a test compilation;
- output directory, crate name, and extra filename identify the artifact; and
- every artifact Cargo reported for that compilation yields the same answer.

Starting at that test compilation, cargo-gamma walks the captured `--extern`
graph:

- registry and Git artifacts terminate traversal because their source is
  outside the workspace;
- workspace artifacts are resolved to exactly one captured compiler
  invocation and traversed recursively;
- each invocation's dep-info file contributes its workspace-relative Rust
  source dependencies;
- the dep-info must include the invocation's primary source.

The result is a set of workspace source files proven to feed each test binary.
Cargo-gamma uses it twice:

- derive exact `--test <name>` Cargo selectors for integration-test targets;
- retain package-level `--tests` selection for library, binary, and example
  unit-test harnesses, which `cargo build` cannot name exactly;
- discard built test binaries whose proven linked-source set contains no
  pending mutated file.

This analysis is an optimization, never exclusion by assumption. Missing or
corrupt captures, ambiguous artifact matches, opaque workspace dependencies,
missing dep-info, inconsistent Cargo artifacts, or unsupported target kinds
abandon exact narrowing. The run retains the coarser package-level result. If
the exact-selector build fails, cargo-gamma retries the build with all test
targets before that failure can affect a mutant.

**Output:**

- each test binary annotated with either a proven set of linked workspace
  source files or `unknown`;
- exact Cargo target selectors when every relevant association is known;
- the retained test-binary set, after removing only binaries proven unrelated
  to every pending mutant.

## 2. Baselining

**Goal:** prove that the unmutated test oracle is green and measure the
execution characteristics later mutant runs must be compared against.

Cargo-gamma runs every retained test binary without an active mutant.

**Output:** for each test binary:

- package, target, executable, and the linked-source result established above;
- whole-binary duration;
- number of tests, when the harness reported it;
- timeout, stall, and memory calibration;
- the binary's candidate mutated source files: proven linked files when exact
  capture succeeded, or conservative package-level candidates when linkage is
  unknown. Neither case proves that a test executes a particular mutation
  site; that is the census's job.

## 3. Loading hints

**Goal:** start with previously learned test ordering instead of rediscovering
every likely killer and useful binary from scratch.

When incremental behavior is enabled, cargo-gamma merges:

- `gamma-hints.yaml`, the checked-in hints artifact; and
- `last-gamma-run.json`, the local run record, which wins on conflicts.

A mutant ID is the content-derived identifier of one specific generated
replacement. It is derived from the workspace-relative source file, enclosing
item, mutator, normalized original site text, occurrence, and replacement
index. It is independent of the run-local ordinal and source line number, so
unrelated edits elsewhere in a file do not invalidate it.

Hints can contain:

- an exact previously killing `{ package, target, test }` for a mutant ID;
- ranked exact tests for a stable `(source file, enclosing item)`;
- ranked test binaries for a source file;
- test sets known to reach a stable source site;
- likely compiler-unviable mutants, used only for build ordering.

Hints are guesses, not verdicts. A test or binary named by a hint is run again
before it can affect the result. Missing, stale, corrupt, or unsupported hints
fall back to colder scheduling.

The version-3 YAML artifact groups exact mutant hints by workspace-relative source
file. Each file group contains a deterministic table of distinct killer
identities, and its mutant entries refer to those identities by index. The
source path and repeated package, target, and test strings therefore occur
once per shared group instead of once per mutant. Generalized item, file, and
reach hints retain their independently versioned representation and interned
reach-test sets.

Its `context` contains the repository HEAD SHA and UTC generation date only.
Those fields explain where the artifact generation came from; they do not gate
score-neutral hints or attribute incrementally retained knowledge to that
commit. Malformed artifacts, foreign producers, and unsupported versions are
ignored rather than partially trusted.

**Output:**

- mutant ID to exact candidate killer;
- `(source file, item)` to ranked candidate tests;
- source file to ranked candidate binaries;
- stable source site to previously observed reaching tests;
- likely-unviable mutant IDs for build ordering only.

## 4. Census admission

**Goal:** decide whether spending test launches now to learn site reachability
is likely to avoid more test execution during the mutant sweep.

The census is disabled by default. `--optimize-test-execution` or the equivalent
`optimize-test-execution = true` configuration enables this experimental phase;
without the opt-in, cargo-gamma performs no census listing or sampling launches.
`--whole-test-binaries` conflicts with the opt-in and explicitly suppresses all
case-level selection.

When enabled, the census asks which baseline tests execute each pending mutation site.
Here, **cost** is the estimated time spent listing tests and launching census
scopes. **Savings** is the upper bound on future whole-binary baseline time
that narrower test selections could avoid across all pending mutants.

- Only binaries that may link pending mutated code are candidates.
- A mutant with a usable exact killer hint does not add to the census's
  potential savings, although its site may ride along in a census justified by
  other mutants.
- Maximum possible savings is the sum of whole-binary baseline durations over
  eligible mutant/binary pairs.
- Each binary is asked to list its tests, with a 30-second listing limit.
- Initial scopes are top-level test-module groups when names expose that
  structure; otherwise they are deterministic contiguous chunks.
- Estimated initial census cost is:

  ```text
  max(test-listing duration, 5 ms) * initial scope count
  ```

- The census is skipped when that estimate is not less than the maximum
  possible savings.
- If admitted, the maximum possible savings also becomes the global census
  deadline.

**Output:**

- a mapping from each pending `(mutated package, source file)` to its potential
  test binaries;
- a mapping from each census-candidate binary to the pending site ordinals it
  should observe;
- initial test scopes for each candidate binary;
- estimated census cost, maximum possible savings, and the admission decision.

When census is disabled or declined, the empty census carries no negative
reachability evidence. Exact and generalized probes remain active, and a probe
that does not kill falls back to the complete reachable binary.

Diagnostics classify census as `disabled`, `declined`, `incomplete`, or
`complete`. Attempted census timing, candidate binary and site counts, listing
attempts and successes, estimated walk cost and admission, sampling launches,
and complete versus partial binary evidence accompany the latter two states,
allowing its sweep savings to be compared with its cost.

## 5. Census walk

**Goal:** measure which baseline test groups execute which pending mutation
sites, without changing program behavior.

The current census walk is serial: binaries and their scopes are sampled one
after another even though `jobs` is passed into the census API.

For each scope:

- No mutant is active.
- Every reached guard writes its site ordinal to the census stream.
- The scope must pass and produce a complete, sealed, internally consistent
  stream.
- Every reached site is conservatively attributed to every test in that
  scope.

A scope is subdivided only when:

- it contains more than one test;
- it reaches some, but not all, wanted sites; and
- twice the estimated subdivision launch cost is less than the remaining
  mutant-test work that subdivision could save.

The resulting per-binary data contains:

- ordered test names;
- mutation-site ordinal to reaching-test-set mappings;
- deduplicated reaching-test sets;
- measured scope costs;
- whether the hierarchy completed;
- the number of sample processes launched.

**Output:** per test binary, a site-ordinal-to-reaching-test-set map, measured
scope costs, a completeness flag, and the total number of census launches.

## 6. Projecting census results

**Goal:** turn raw reach observations into conservative instructions the
mutant sweep can execute safely.

For each mutant and test binary, census data becomes one of:

| Selection | Meaning | Sweep behavior |
|---|---|---|
| `Whole` | No usable narrowing | Run the complete binary |
| `Uncovered` | A complete census proved no test reaches the site | Skip the binary |
| `Selected` | A complete census identified the reaching tests | Run only those tests |
| `Hinted` | An incomplete census observed some reaching tests | Try them first, then fall back to the whole binary |

Additional rules:

- A reaching set containing more than half the binary's tests becomes `Whole`.
- Missing, malformed, failed, or inconsistent evidence never proves absence.
- A non-passing `Selected` run is repeated against the whole binary before its
  outcome is accepted because filtering can change resource use and failure
  order.

**Output:** a `(mutant, test binary) -> Whole | Uncovered | Selected(tests) |
Hinted(tests)` selection map used by cost estimation and execution.

## 7. Scheduling mutant work

**Goal:** overlap independent work, expose useful learning before related work
starts, and avoid leaving expensive work as a long serial tail.

Cargo-gamma estimates each pending mutant's serial test cost:

- exact killer hint: a short filtered probe plus a small miss-weighted
  whole-suite fallback;
- `Selected`: half the measured duration of the selected tests for the initial
  killed-mutant prior;
- `Whole`: half the whole-binary baseline for the initial killed-mutant prior;
- `Hinted`: measured hinted-test duration plus a miss-weighted whole-binary
  fallback;
- `Uncovered`: zero.

Persisted generalized candidates seed their scheduling probability and probe
cost from the hint artifact's hit, miss, measured-time, and sample counters.
Speculative hint probes are bounded candidates, not an exhaustive isolation
search: a miss or inconclusive probe falls back to the authoritative admitted
selection instead of enumerating every possible test subset.

The cost order is still partitioned into package queues and interleaved. That
order supplies deterministic package-fair, longest-work-first secondary
priority, but workers no longer claim it through a fixed cursor. A worker asks
the scheduler for an assignment only when it is ready to run one.

Each package keeps an ordered queue of its currently eligible candidates, and
the global frontier contains only the best candidate from each package.
Selection reads the first global entry instead of rescanning all mutants.
Claims and completions rekey only candidates in the affected source file;
package-turn fairness updates only that package's global entry. Claimed work is
removed permanently, and a separate remaining count distinguishes an exhausted
sweep from a cold same-item tail that is waiting for its scout.

For unhinted work the scheduler ranks distance first:

1. an item in a file with no active mutant;
2. an inactive item in an active file, preferring the least-contended file;
3. no assignment for a still-cold item that already has an unhinted scout
   active.

The third rule is stronger than file separation because an exact same-item
test is more reusable than file-level binary evidence. Within the same
distance tier the scheduler preserves package turns, prefers greater learning
leverage per estimated evaluation cost, retains longest-work-first value, and
finally uses the stable pre-sweep order. Learning leverage is the number of
pending siblings that can reuse the scout.

A mutant with an exact killer hint is already informed and may share an active
item. It still participates in file contention so cold work is steered toward
more independent files when possible.

The explicit tail policy is **wait for learning**: when only unhinted
same-item conflicts remain for a cold item, capacity stays idle until the
active item changes state. Once that scout publishes, its informed siblings
may run concurrently. The waiter holds no file or item reservation and blocks
on the scheduler condition variable; there is no duration-based sleep or
timeout.

**Output:** immutable scheduling-cost metadata and a synchronized
assignment-time scheduler tracking remaining work and active file, item, and
shared-resource reservations.

## 8. Testing one mutant

**Goal:** reach the first trustworthy verdict with the least test work while
preserving canonical failure, resource, and metering precedence.

Reachable binaries have a canonical order:

1. binaries from the mutant's own package;
2. binaries from other selected test packages;
3. shorter baseline duration first within each tier;
4. stable package, target, and path tie-breakers.

Evidence is attempted in this order:

1. The mutant's exact checked-in or local killer hint.
2. Persisted exact-site reach candidates.
3. Ranked same-item exact tests.
4. Binaries observed reaching the same item.
5. Binaries observed reaching another site in the same file.
6. Binaries that killed another mutant in the same file.
7. Canonical census-selected or whole-binary execution.

The first valid detection stops testing that mutant. Candidate results from a
later binary are held until canonical iteration reaches that binary, so they
cannot bypass an earlier timeout, resource failure, flake, or metering error.

Generalized evidence distinguishes the exact kill or reach observation that
seeded a candidate from transfer evidence gathered against other mutants. A
candidate is admitted only while its smoothed transfer probability can repay
its measured launch cost from the fallback work it may avoid. Unmeasured
candidates may be explored once; two misses without a transfer hit retire
them. One mutant tries at most two same-item tests and one candidate binary
from each generalized tier. After eight attempts without a hit, that tier's
campaign-wide circuit breaker rejects further exploration. Exact hints and
canonical fallback remain available.

A candidate failure is a transfer hit and a clean pass is a transfer miss.
Timeouts, stalls, memory exhaustion, flakes, enumeration failures, metering
loss, and unjudged launches are inconclusive and do not alter that candidate's
hit or miss counters. Canonical execution also supplies soft negative evidence
for binaries that are already item or file candidates: two clean canonical
passes have the ranking weight of one explicit transfer miss. Soft evidence
may demote or economically retire a candidate, but it never creates one and
cannot exclude canonical work or determine a verdict.

**Output:** one mutant outcome, elapsed time, optional killing test, and
optional diagnostic note.

## 9. Learning during the sweep

**Goal:** let completed mutant work improve the ordering of related mutants
that have not yet committed to their test work.

The scheduler makes the first assigned unhinted mutant in an item its scout.
Same-item siblings remain unreserved while it runs. Workers use unrelated
files first, then inactive items in the least-contended active file. If no
independent item remains, they wait for a scheduler state-change notification
rather than for an elapsed duration.

A completed mutant publishes:

- its exact killing test for later mutants in the same item;
- its killing or reaching binary for later mutants in the same file;
- lower-weight negative ranking evidence when a known candidate binary passes
  cleanly during canonical execution;
- safe negative reach evidence for another replacement at the exact same
  stable source site.

The worker publishes all exact-test, reaching-binary, file-binary, soft
canonical-negative, and safe negative-reach learning before releasing its
file/item reservation. Releasing the reservation wakes waiters, so a related
follow-on assignment observes the completed learning. Hinted mutants are not
spaced, but their checked result is published before their reservation is
released in the same way.

**Output:** updated in-memory exact killers, bounded ranked item/file
candidates with separate seed and transfer evidence, and safe exact-site reach
exclusions for work that has not started yet.

## 10. Persistence and reporting

**Goal:** preserve score-neutral scheduling knowledge for later campaigns and
report whether census and hints repaid their execution cost.

After the sweep:

- exact mutant killers and generalized item/file/reach knowledge are stored in
  the local run record;
- `cargo gamma hints` incrementally promotes applicable knowledge into
  `gamma-hints.yaml`, preserving entries outside its selection; `--replace`
  deliberately rebuilds it from the selected population;
- build-round diagnostics attribute each round's withdrawals to packages. The
  elapsed time remains the measured workspace-wide Cargo invocation and is not
  divided into fictitious package durations;
- census diagnostics report admission size, listing and sampling work, and
  complete versus partial evidence;
- selection diagnostics count whole, case-selected, hinted-fallback, and
  uncovered decisions, plus selected and available named tests;
- hint funnels report candidates, attempts, hits, and economically, cap-, or
  circuit-breaker-rejected opportunities for exact mutant hints and
  generalized item, reach, file, and census tiers;
- `gamma-selection.jsonl` receives each mutant's completed attempts as one worker-side batch, then
  records and immediately flushes every retained selection attempt with its mutant ordinal,
  candidate tier and rank, binary and optional test identity, hit/miss/inconclusive result, elapsed
  time, and canonical fallback estimate. Serialization reuses one buffer and issues one write per
  complete record. The journal retains at most 64 MiB per campaign and appends a truncation record
  when the ceiling is reached, so diagnostic growth cannot stop mutant execution; the retained
  prefix of complete records can still be replayed without treating telemetry as verdict evidence;
- package sweep timelines report first start, final completion, and wall span
  relative to sweep start. These spans can overlap under concurrency and are
  distinct from the package CPU totals; and
- diagnostics retain total launches and launches saved, including the subset
  saved by learned reach evidence. The live dashboard's estimated hint savings subtract every
  attempted selection in the successful mutant's chain from the canonical fallback estimate, not
  only the final successful launch.

Promotion writes version 3 YAML atomically. File groups, mutant entries, killer
tables, generalized identities, and interned reach sets use canonical ordering,
so equivalent knowledge produces identical reviewable bytes regardless of
discovery or completion order. A no-op preserves `generated_on` and produces no
file change. A changed generation records the current HEAD SHA and UTC date.
Incremental promotion refuses a malformed, foreign, or unsupported existing
artifact rather than treating it as empty; `--replace` is the explicit
permission to discard such a generation. This includes unsupported
independently versioned generalized data, which today's serializer cannot
round-trip without losing future fields. Publication compares against the exact
YAML bytes used by the merge, so a concurrent update
is left intact and reported as a conflict.

The generalized section uses schema version 2, separating seed observations
from transfer hits and misses. Older generalized schemas are unsupported.

Persisted knowledge changes ordering and selection only. It never carries a
verdict into a new campaign without executing the relevant test again.

**Output:** `last-gamma-run.json`, promoted `gamma-hints.yaml` when requested,
final reports, and diagnostics containing census and sweep effectiveness
metrics.
