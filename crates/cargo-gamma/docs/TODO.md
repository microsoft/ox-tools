# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P3](#p3) — Amortize per-mutant process launch with a fork server
- [P7](#p7) — Prove cursor stalls through nested branches and lexical binding identities
- [P9](#p9) — Prove stalled nonempty grouping boundaries over immutable collections
- [P11](#p11) — Prove retry fixed points without mistaking cached calls for progress
- [P12](#p12) — Harvest attributable compiler errors from every isolation proof
- [P13](#p13) — Find multiple independent failures in one isolation search
- [P14](#p14) — Preserve separate incremental compilation lanes for convergence and proofs
- [P15](#p15) — Reuse proof results across convergence rounds with complete context keys
- [P16](#p16) — Replay only affected compiler units without changing Cargo's compilation context

### Features
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement

### Benchmarks
- [B1](#b1) — Measure cold compiler convergence and isolation work

## Compiler-exclusion implementation order

Implement this work in the following order; existing unrelated items retain their IDs and positions.
This sequence addresses hint-free campaigns as well as runs without a reusable compiler-exclusion cache.

### Convergence and isolation

Keep the existing optimization sequence after establishing B1's measurements; generation
repairs and stronger evidence complement these changes rather than replace them.

1. [B1](#b1) — Establish the work-count trace and cold-run comparison baseline.
2. [P12](#p12) — Obtain more exclusions from each proof's compiler diagnostics.
3. [P13](#p13) — Collect multiple failures before returning to ordinary convergence.
4. [P14](#p14) — Reduce schema switching and preserve incremental compilation state.
5. [P15](#p15) — Reuse exact-context proof results across convergence rounds.
6. [P16](#p16) — Selectively replay compiler units once the cheaper improvements are measured.

## Performance

<a id="p3"></a>
### P3 — Amortize per-mutant process launch with a fork server

**Area:** `cargo-gamma-rt`, `cargo-gamma-lib::exec` sweep · **Priority:** Medium ·
**Effort:** Large

The design accepts per-mutant process launch as an unavoidable floor: "process launch remains
per mutant... activating several mutants together would confound causality" (the
"Costs and limitations" section of `docs/DESIGN.md`). For workspaces with many small, fast-running
tests, that launch/link/static-init cost dominates the mutant sweep (one process per selected binary or probe), and
currently cannot be amortized: no machinery in this crate reuses a warm process across launches.

A fork-server model can remove that floor without weakening isolation: install a pre-main
constructor (the existing hook point already used for guard installation) that, in the runtime
crate, forks the warm, fully-linked, single-threaded process fresh for every test or mutant
launch instead of paying `execve`/dynamic-link/static-init again. Each child still gets full
process isolation, matching today's guarantees; only the one-time setup cost is shared. The
mutant ordinal can be set by direct memory write in the child (inherited via copy-on-write, no
env/argv needed); libtest-driven test selection needs the fork server's constructor to run before
std's argv-capturing constructor so each child's argv can be patched before libtest reads it.

- `crates/cargo-gamma-rt/src/runtime.rs` (`install()`) and the `.init_array`/
  `__DATA,__mod_init_func`/`.CRT$XCU` link-section statics around it —
  the existing pre-main constructor mechanism to extend
- `crates/cargo-gamma-lib/src/exec/sweep.rs` — launches one fresh
  process per mutant/binary attempt
- `crates/cargo-gamma/docs/DESIGN.md` — the documented limitation this would relax

**Done when:** sweep launches on Linux reuse a warm forked process instead of a fresh
`execve` per launch, a test proves the forked child observes no repeated dynamic-link or
static-init work (e.g. a constructor-run counter stays at one across many forked launches), and a
documented, tested fallback to today's spawn-per-launch behavior on platforms without `fork()`
(Windows) or without a dynamic loader step (statically linked binaries).

**See also:** F3 (shares the same fork-server engine). Landing this changes the per-launch cost
used by assignment-time cost estimates and learning-value
priority; revisit those heuristics' thresholds once this ships.

---

<a id="p7"></a>
### P7 — Prove cursor stalls through nested branches and lexical binding identities

**Area:** collector resource proofs and binding/flow evidence · **Priority:** High · **Effort:** Large

The remaining campaign cases include an unchanged Tarjan edge cursor and two unchanged
diff diagonals. These require effects through `last_mut()` frame aliases and stack
updates, or closure/index/let-chain effects, beyond the supported tokenizer and text-scan
proofs. Extend bounded evidence without treating an unknown call, alias, or branch as
harmless.

- `crates/cargo-gamma-engine/src/ops/collect/collector/resource/` — bounded resource/effect proofs.
- `crates/cargo-gamma-lib/src/discover/survey.rs` — frame edge advancement.
- `crates/cargo-gamma-lib/src/fix/diff.rs` — diagonal advancement.

**Done when:** bounded, path-sensitive evidence can prove the remaining Tarjan and diff
repeated cursor states with no progress or escape.
It distinguishes retained token growth from spinning and tracks exact bindings and field
aliases, not names. Fixtures retain finite scans, cursors advanced on another branch,
escaping/error paths, alternate writes, custom indexing, and unknown calls; unsupported
patterns and exhausted proof budgets remain eligible. Campaign examples are fixtures for
proof development, not a blanket list of exclusions.

**See also:** P9 (derived grouping boundaries).

---

<a id="p9"></a>
### P9 — Prove stalled nonempty grouping boundaries over immutable collections

**Area:** collector derived-index and retained-collection resource proofs · **Priority:** Medium · **Effort:** Large

Deleting `start = end` in `GroupedHints::from` repeatedly builds the same nonempty
group. Certifying this remaining case requires bounded effects through cloning,
sorting, deduplication, predicates, and retained nonempty groups, rather than relying
on the supported zero-times-offset empty-group relation. Unknown callbacks, aliases,
and collection effects must remain eligible.

- `crates/cargo-gamma-lib/src/discover/hints.rs:973` — initial cursor and boundary derivation.
- `crates/cargo-gamma-lib/src/discover/hints.rs:1002` — retained group append and cursor update.
- `crates/cargo-gamma-engine/src/ops/collect/collector/resource/` — extend bounded effects beyond empty-group relations.
- Campaign example: `e0204c97c85a`.

**Done when:** bounded evidence proves an unchanged boundary over stable input and retained
nonempty-group growth, while summarizing intervening calls only
with verified nonescaping effects. Fixtures retain empty inputs, advancing boundaries,
normal zero offsets outside this relation, mutable inputs, side-effecting predicates,
custom collection APIs, and unknown aliases. No exclusion relies solely on the replacement
containing multiplication or the result being an empty group.

**See also:** P7 (direct cursor stalls).

---

<a id="p11"></a>
### P11 — Prove retry fixed points without mistaking cached calls for progress

**Area:** collector resource proofs for bounded local helper effects · **Priority:** Medium · **Effort:** Large

Eleven timeouts occur in `minimize_interaction`. Eight edits specifically remove a
retry's progress or terminal escape: forcing either `reduced` check true, disabling the
granularity stop, changing `>=` to `>`, removing/replacing that break, or disabling
granularity growth. Repeating a cached proof does not consume the proof budget, so the
budget conditions alone do not guarantee termination. The two negated `reduced` checks
and deleted complement assignment also change control flow, but may produce different
finite outcomes; they are not justified exclusions just because they share this function.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:537` — retry state, budget guards, and partitions.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:550` — first restart.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:570` — second restart, terminal escape, and granularity growth.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:637` — cached results return before proof counters advance.
- Campaign examples: `f22a7b6da1a4`, `35326a00d297`, `5b33335c0b1c`,
  `fda02c1203e4`, `37ae5eb40807`, `d72ac26e7f21`.

**Done when:** bounded, resolved local effect summaries can certify a repeated retry state
on cache-hit paths without overlooking writes to counters, collections, or external
state. Positive fixtures exercise stagnation after warm-cache hits; negative fixtures
cover fresh proofs consuming budgets, changed populations, finite alternate results,
escaping errors, and unresolved helpers. Do not suppress all mutations in retry loops or
assume every call advances a budget.

---

<a id="p12"></a>
### P12 — Harvest attributable compiler errors from every isolation proof

**Area:** `cargo-gamma-lib::exec::build` proof diagnostics and mutation guards · **Priority:** High · **Effort:** Medium

An isolation proof currently reduces Cargo's result to compiled/failed and discards its diagnostic
stream for attribution. A smaller schema can produce precise errors for several replacements even
when the original full-schema failure could not be attributed. Harvesting those errors can settle
multiple candidates for the cost of one proof rather than spending that invocation on a boolean.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:893` — `run_proof` executes the subset build.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:931` — stdout is reduced to a boolean.
- `crates/cargo-gamma-lib/src/exec/build.rs:1305` — active-schema instrumentation returns written paths; attribution also needs the guards for that exact schema.

**Done when:** proof outcomes retain all safely attributable compiler failures and publish their
confirmed exclusions without requiring separate singleton searches. Deterministic fixtures cover
multiple independent errors, macro expansions, unrelated failures, and broad secondary spans that
overlap innocent guards; an overlap alone never proves individual unviability. On B1's fixed cold
population, fewer proof invocations are required without changing the correctly eligible population
or weakening interaction handling.

**See also:** B1 (measurement prerequisite), P13 (batch search).

---

<a id="p13"></a>
### P13 — Find multiple independent failures in one isolation search

**Area:** `cargo-gamma-lib::exec::build::isolation` partition search · **Priority:** High · **Effort:** Large

The current search follows a failing partition to one culprit, removes that result, and searches
the remaining population again. Retain a frontier of independently failing partitions and collect
their results before returning to ordinary convergence. Reuse earlier partition results under the
same complete schema context rather than repeatedly reconstructing a search around one culprit.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:648` — `isolate_candidates` narrows one candidate population.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:482` — `isolate_scoped` removes resolved candidates and retries.

**Done when:** one search can settle multiple independent failures with fewer compiler invocations
than repeated one-culprit searches on B1's fixed population. Fixtures distinguish independently
failing halves from interaction-only failures and masking effects. A clean group is not treated as
proof that each member compiles alone, interaction exclusions are not reported as individual
unviability, and all search paths retain deterministic progress and bounded work.

**See also:** P12 (diagnostic harvesting), P14 (amortize schema transitions), B1 (work-count comparison).

---

<a id="p14"></a>
### P14 — Preserve separate incremental compilation lanes for convergence and proofs

**Area:** `cargo-gamma-lib::exec::build` schemas and scratch workspace ownership · **Priority:** High · **Effort:** Large

Isolation removes mutations from affected dependency components and ordinary convergence restores
the remaining full schema. On a connected workspace these transitions can rewrite many sources
and invalidate downstream library and test-harness artifacts. First exploit P13's batching, then
evaluate persistent, separately owned convergence and proof workspaces/target directories so proof
experiments do not continually displace the main schema's incremental compilation state.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:482` — constructs the proof background from affected dependency components.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:893` — writes the active proof schema before invoking Cargo.
- `crates/cargo-gamma-lib/src/exec/build/splices.rs:160` — switches the files carrying active proof guards.
- `crates/cargo-gamma-lib/src/exec/build.rs:1295` — ordinary convergence restores its instrumented schema.

**Done when:** the selected lane strategy measurably reduces total source rewrites, rebuilt compiler
units, and cold convergence time after accounting for both lanes' warm-up and disk usage. Unchanged
files keep their timestamps, each lane has stable paths and exclusive ownership, and dependency
artifacts cannot be reused across incompatible schemas. Fixtures preserve identical exclusion and
interaction decisions, and scratch cleanup cannot affect another running campaign.

**See also:** P13 (batch exclusions before restoring the schema), P15 (lane-aware proof cache), B1 (include warm-up costs).

---

<a id="p15"></a>
### P15 — Reuse proof results across convergence rounds with complete context keys

**Area:** `cargo-gamma-lib::exec::build::isolation` proof cache · **Priority:** Medium · **Effort:** Large

Proof-result reuse currently ends with one isolation invocation. Later convergence rounds can
repeat equivalent subset checks. Extend the lifetime of those results only when the complete proof
context matches; candidate ordinals alone do not describe the active background, dependency
artifacts, or compilation configuration.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:141` — `Proofs` stores subset results.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:506` — the cache is recreated inside `isolate_scoped`.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:893` — proof schemas combine active candidates with a background population.

**Done when:** equivalent proofs across rounds reuse definitive results, keyed by source and schema
identity, active/background mutations, package and target scope, features, compiler/toolchain,
profile, relevant environment, and dependency artifact identity. Context changes invalidate reuse;
timeouts and incomplete invocations do not become definitive verdicts. Deterministic cache-hit
fixtures and B1's traces demonstrate fewer invocations without changing exclusions or allowing
cache-hit retry loops to stall.

**See also:** P14 (stable lane identity), P11 (cache hits are not progress), B1 (cache-hit accounting).

---

<a id="p16"></a>
### P16 — Replay only affected compiler units without changing Cargo's compilation context

**Area:** `cargo-gamma-lib::exec` compiler capture and isolation orchestration · **Priority:** Medium · **Effort:** Large

Frozen-context proofs currently retain the broad package and test-target invocation to preserve
Cargo feature unification. Investigate capturing enough of pristine Cargo's compilation graph to
rerun only the affected compiler units and their necessary dependents. Simply switching to
`cargo check -p affected_crate --lib` is not equivalent: it can select different features,
development dependencies, cfgs, and dependency artifacts.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:542` — frozen roots select the original check verb rather than a narrower target verb.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:893` — every proof still invokes Cargo with that context.
- `crates/cargo-gamma-lib/src/exec/rustc_wrapper.rs:19` — existing invocation records capture identities and externs, not the complete replayable argument/environment context.

**Done when:** selective execution preserves the original compiler arguments, features, cfgs,
package identities, environment, build-script/proc-macro inputs, and compatible dependency artifacts.
Changed unit outputs invalidate all required dependents. Fixtures cover resolver differences,
development dependencies, proc macros, and cross-target builds, with a safe full-context fallback
when replay equivalence cannot be established. B1's comparisons show fewer rebuilt units and lower
end-to-end convergence time without changing valid exclusions.

**See also:** B1 (measure after the cheaper improvements), P14 (artifact isolation), P15 (context identity).

---

## Features

<a id="f3"></a>
### F3 — Native fork-server test harness as a `cargo test`/nextest replacement

**Area:** new fork-server engine crate, `cargo-gamma-lib::exec` baseline/nextest integration ·
**Priority:** Low · **Effort:** Large

Today, "run tests under process isolation" is not something this crate provides itself: baseline
measurement launches the whole binary as one process with `Only::All`
(`crates/cargo-gamma-lib/src/exec/baseline.rs:215`) — exactly as fragile to a crashing or
state-leaking test as plain `cargo test`. Per-test isolation only exists when configured to
delegate to `cargo-nextest` as the harness (`crates/cargo-gamma-lib/src/exec/nextest.rs`), which
pays nextest's own exec-per-test cost; this crate is a client of nextest there, not a competitor
to it. Meanwhile, CI pipelines that run both a normal test runner and `cargo gamma` today execute
the test suite twice: once for real verification, once as this crate's own baseline calibration.

Once P3's fork-server engine exists, expose it as a first-class native test-execution mode (e.g.
`cargo gamma --no-mutants`) that gives per-test isolation intrinsically, at fork-server speed,
without invoking `cargo-nextest`. Let baseline optionally emit CI-grade output (JUnit, per-test
pass/fail, timing) as a byproduct of the run it already performs, so a pipeline can opt to drop
its separate test-runner invocation instead of running the suite twice. Because this crate's
instrumented tree is not guaranteed behavior-identical to a plain build in every case (guards
"alter size and layout" and "may expose stack or compiler limits in unusually deep code" per the
design's own limitations section), this must stay an explicit, documented opt-in for teams
willing to verify against the instrumented tree — not a silent replacement of their release gate.

- `crates/cargo-gamma-lib/src/exec/baseline.rs:215` — current single-process, whole-binary
  baseline launch
- `crates/cargo-gamma-lib/src/exec/nextest.rs` — current nextest delegation for opt-in isolation
- `crates/cargo-gamma/docs/DESIGN.md` — "Costs and limitations" section documenting the
  instrumented-tree behavioral caveats this mode must disclose

**Done when:** a `--no-mutants` (or equivalent) mode runs the workspace's tests with native
per-test isolation via the fork-server engine, can emit CI-consumable per-test results, and its
documentation explicitly discloses the instrumented-tree caveats a team must accept to use it as
their primary test runner.

**See also:** P3 (this mode's execution engine)

---

## Benchmarks

<a id="b1"></a>
### B1 — Measure cold compiler convergence and isolation work

**Area:** `cargo-gamma-lib::exec::Events` and compiler-convergence measurement · **Priority:** High · **Effort:** Medium

The live ox1 campaign showed substantial repeated compiler work, but captured invocation counts
alone cannot distinguish search overhead, schema switching, and necessary dependency rebuilding.
Expose the existing convergence/proof evidence as a compact trace before optimizing the algorithm.
Compare the same fixed mutation population without test-selection hints or prelearned compiler
exclusions, and distinguish cold artifact caches from warm incremental compilation.

- `crates/cargo-gamma-lib/src/exec/events.rs:130` — convergence evidence already exposes written files and fresh/rebuilt targets.
- `crates/cargo-gamma-lib/src/exec/events.rs:147` — proof evidence already exposes active population, rewritten files, and rebuilt units.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:893` — proof boundary for invocation timing and outcome collection.

**Done when:** a reproducible baseline and compact trace record rounds/proofs, elapsed time,
rewritten files, fresh/rebuilt compiler units, proof-cache hits, and confirmed exclusions per
invocation, keeping interaction exclusions distinct. Deterministic checked-in fixtures pin work
counts and prevent duplicate or missing events. Representative cold-run comparisons pin source,
toolchain, configuration, population, and cache conditions, include all lane warm-up work, and
compare exclusion correctness as well as time. Evidence collection remains opt-in so normal runs
do not pay for transcript analysis they do not consume.

Cold-campaign comparisons in CI must pin those same inputs and distinguish genuine
invalid candidates, unsupported encodings, and interaction exclusions. Candidate
population counts and schema parsing alone do not measure residual compiler failures
or convergence time.

**Sizing baseline:** the stopped ox1 snapshot at 2026-11-02 15:46:19 Pacific contains
1,991 recorded exclusions, all mapped to a fixed 74,068-candidate listing:

| Mutation family | Exclusions |
|---|---:|
| Whole-function return replacements | 1,795 |
| Conditions, comparisons, logical operators | 105 |
| Statement deletion and loop-exit changes | 25 |
| Iterator/string method substitutions | 19 |
| Arithmetic and range substitutions | 18 |
| Literal substitutions, all in const contexts | 18 |
| Array reversal in assignment destinations | 3 |
| Parameter default shadowing | 2 |
| Match-arm removal | 2 |
| Loop-break payload defaults | 2 |
| Option/propagation substitutions | 2 |
| **Total** | **1,991** |

Whole-function replacements account for 90.2% of exclusions, but only 2.42% of the listed
population: 1,653 E0277 errors, 134 E0599 errors, six E0271 errors, and two isolation-only
iterator failures. Of these replacements, 1,738 contain `Default::default()`; this overlaps
the compiler-code categories. Redacted trait names and 143 isolation-only reasons prevent
treating every category as a proven root cause. These counts size the measurement work,
not safely avoidable exclusions or proportional wall-clock savings.

The paired post-repair read-only listing contains 75,410 candidates: 73,832 unchanged
IDs, 17 widened sites with new IDs, and 219 old candidates no longer generated. Within
the 1,991 historical exclusions, 203 are no longer generated and four have widened
sites; all 1,788 retained or widened schemas parse, including 67 formerly malformed
schemas. None of those schemas targets a const-only context or assignment destination
directly. The larger total also retains uncertain macro/conditional cases rather than
treating missing evidence as proof of invalidity. A fresh compiler campaign must measure
the remaining failures and elapsed work; neither syntax success nor population size
establishes that result.

**See also:** P12, P13, P14, P15, P16 (implement and compare in the order listed above).
