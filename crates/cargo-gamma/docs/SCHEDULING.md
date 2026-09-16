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
7. [Building the mutant queue](#7-building-the-mutant-queue)
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
    queue[7. Fixed mutant queue]
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
- records only successful compiler invocations;
- records the crate name and type, `--test` state, primary Rust source,
  output directory, extra filename, and every path-bearing `--extern`;
- marks an invocation opaque when an external dependency cannot be associated
  with a concrete artifact path.

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

- `gamma-hints.json`, the checked-in hints artifact; and
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

**Output:**

- mutant ID to exact candidate killer;
- `(source file, item)` to ranked candidate tests;
- source file to ranked candidate binaries;
- stable source site to previously observed reaching tests;
- likely-unviable mutant IDs for build ordering only.

## 4. Census admission

**Goal:** decide whether spending test launches now to learn site reachability
is likely to avoid more test execution during the mutant sweep.

The census asks which baseline tests execute each pending mutation site.
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

## 7. Building the mutant queue

**Goal:** keep workers occupied while reducing the chance that expensive
mutants form a long serial tail at the end of the campaign.

Cargo-gamma estimates each pending mutant's serial test cost:

- exact killer hint: baseline duration of the hinted binary;
- `Selected`: measured duration of the selected tests;
- `Whole`: whole-binary baseline duration;
- `Hinted`: whole-binary baseline duration because fallback may be required;
- `Uncovered`: zero.

It then:

1. Sorts mutants by descending estimated cost, breaking ties by stable plan
   position.
2. Partitions that order into package queues.
3. Interleaves one mutant from each non-empty package queue.
4. Lets workers claim positions from the resulting fixed queue atomically.

This is longest-work-first load balancing across packages. The queue is not
reordered after execution begins, and source file is not an explicit sort key.
File siblings may remain near one another through plan order, but there is no
file barrier.

**Output:** one immutable, ordered list of pending mutants shared by all
workers, plus each mutant's estimated serial test duration.

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

**Output:** one mutant outcome, elapsed time, optional killing test, and
optional diagnostic note.

## 9. Learning during the sweep

**Goal:** let completed mutant work improve the ordering of related mutants
that have not yet committed to their test work.

When an unhinted item is first encountered:

- the first worker becomes its scout;
- workers that claim sibling mutants in the same item synchronously wait for
  `estimated candidate cost / sibling count`, clamped to 5–200 ms;
- after that timeout they proceed even if the scout is still running;
- mutants in different items of the same file do not wait for one another.

A completed mutant publishes:

- its exact killing test for later mutants in the same item;
- its killing or reaching binary for later mutants in the same file;
- safe negative reach evidence for another replacement at the exact same
  stable source site.

The short wait prevents workers from remaining idle, but tests taking seconds
or minutes commonly outlive it. Several same-item or same-file mutants can
therefore begin expensive fallback work before the first result is published.

**Output:** updated in-memory exact killers, ranked item/file candidates, and
safe exact-site reach exclusions for work that has not started yet.

## 10. Persistence and reporting

**Goal:** preserve score-neutral scheduling knowledge for later campaigns and
report whether census and hints repaid their execution cost.

After the sweep:

- exact mutant killers and generalized item/file/reach knowledge are stored in
  the local run record;
- `cargo gamma hints` promotes applicable knowledge into
  `gamma-hints.json`;
- diagnostics report census samples, total sweep launches, exact and
  generalized probe hits, and launches saved.

Persisted knowledge changes ordering and selection only. It never carries a
verdict into a new campaign without executing the relevant test again.

**Output:** `last-gamma-run.json`, promoted `gamma-hints.json` when requested,
final reports, and diagnostics containing census and sweep effectiveness
metrics.
