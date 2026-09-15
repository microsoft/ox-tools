# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Correctness
- [C1](#c1) — Investigate a missing guard for a whole-function boolean mutant
- [C2](#c2) — Remove the editorial baseline-failure explanation
- [C3](#c3) — Report every baseline failure before stopping

### Performance
- [P3](#p3) — Amortize per-mutant process launch with a fork server
- [P9](#p9) — Make the guard census an explicit opt-in

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement
- [F4](#f4) — Bound cache growth and reclaim abandoned workspaces
- [F5](#f5) — Incrementally update the workspace hints artifact
- [F6](#f6) — Schedule unhinted mutants to maximize in-run learning

### Testing
- [T1](#t1) — Isolate tests from the production interrupt registry

## Correctness

<a id="c1"></a>
### C1 — Investigate a missing guard for a whole-function boolean mutant

**Area:** mutant discovery, source instrumentation, and build convergence · **Priority:** High ·
**Effort:** Unknown

A large campaign discovered a `fn_value.bool_false` mutant but failed during the final test-binary
build because instrumentation emitted no corresponding guard:

```rust
pub fn is_azure_egress_clear_request(input: &[pb::AzureEgressTarget]) -> bool {
    input.len() == 1 && input[0].host.trim().is_empty()
}
```

The reported mutant replaced the whole function body block with `false`:

```text
replace { input.len() == 1...trim().is_empty() } with false [fn_value.bool_false]
```

Cargo-gamma then stopped with `internal error: no guard was emitted for the mutant`. Discovery and
instrumentation therefore disagreed about whether this selected mutant existed in the rewritten
schema. Investigate span normalization and replacement matching for a whole-function boolean
mutation around a block whose tail expression combines short-circuit boolean logic, indexed field
access, and a method chain. Also determine whether overlapping inner-expression mutants or source
rewrites can cause the function-value replacement to be skipped while its plan entry remains live.

**Done when:** a minimal deterministic fixture reproduces the reported function shape; every live
discovered mutant either emits exactly one guard or receives an explicit non-internal outcome
before the final build; the whole-function `bool_false` mutant reaches baseline and sweep; and
regression tests cover coexistence with the nested boolean-expression mutants generated from the
same body.

---

<a id="c2"></a>
### C2 — Remove the editorial baseline-failure explanation

**Area:** baseline error rendering · **Priority:** High · **Effort:** Trivial

The baseline failure currently follows `the baseline could not be measured` with:

> Every verdict in a run is a comparison against the baseline, so there is nothing to measure
> until this failure is resolved.

This does not help diagnose or resolve the failing target and reads as an unnecessary lecture after
a long campaign setup. Remove it. Lead directly with the package, target, runner, executable,
working directory, failure, elapsed time, and diagnostic artifact paths.

**Done when:** baseline failures contain no editorial explanation of why a baseline is required,
retain all actionable target and failure details, and rendering tests pin the concise form.

---

<a id="c3"></a>
### C3 — Report every baseline failure before stopping

**Area:** baseline coordination and diagnostic publication · **Priority:** High · **Effort:** Medium

One failed or timed-out test binary currently ends baseline measurement and reports only that
target. In a large workspace this forces repeated full build/baseline attempts to discover
independent failures one at a time.

Continue baselining every retained test binary after one fails. Preserve the bounded retry policy
for ordinary test failures, but do not let a terminal failure cancel or suppress measurements of
other binaries. After all binaries settle, return one aggregate baseline error that lists every
failed target and its actionable details.

Publish collision-free diagnostics for each failed test-binary identity. Use a stable,
filesystem-safe directory derived from at least package and target identity, with that target's
`baseline-failure.json` and relevant `gamma-diagnostics.json` beneath it, rather than allowing
multiple failures to compete for the same top-level filenames. The aggregate error should link
each target to its own artifacts. Publication must preserve complete diagnostics when targets
share a target name across packages or finish concurrently.

**Done when:** deterministic tests run several baseline binaries with a mixture of passes,
failures, and timeouts; every binary is attempted; the final error reports all failures; each
failure has distinct readable diagnostic artifacts under its target directory; no concurrent
writer overwrites another target's files; and a single failure retains the same actionable detail
without forcing callers to understand an aggregate-only format.

---

## Performance

<a id="p3"></a>
### P3 — Amortize per-mutant process launch with a fork server

**Area:** `cargo-gamma-rt`, `cargo-gamma-lib::exec` (census, sweep) · **Priority:** Medium ·
**Effort:** Large

The design accepts per-mutant process launch as an unavoidable floor: "process launch remains
per mutant... activating several mutants together would confound causality" (`docs/DESIGN.md:835`
in the "Costs and limitations" section). For workspaces with many small, fast-running tests, that
launch/link/static-init cost dominates both the census walk (one subprocess per `(binary, test)`
pair) and the mutant sweep, and currently cannot be amortized: no machinery in this crate reuses
a warm process across launches.

A fork-server model can remove that floor without weakening isolation: install a pre-main
constructor (the existing hook point already used for guard installation) that, in the runtime
crate, forks the warm, fully-linked, single-threaded process fresh for every test or mutant
launch instead of paying `execve`/dynamic-link/static-init again. Each child still gets full
process isolation, matching today's guarantees; only the one-time setup cost is shared. The
mutant ordinal can be set by direct memory write in the child (inherited via copy-on-write, no
env/argv needed); libtest-driven test selection needs the fork server's constructor to run before
std's argv-capturing constructor so each child's argv can be patched before libtest reads it.

- `crates/cargo-gamma-rt/src/runtime.rs:1708` (`install()`) and the `.init_array`/
  `__DATA,__mod_init_func`/`.CRT$XCU` link-section statics around it (`runtime.rs:1725-1892`) —
  the existing pre-main constructor mechanism to extend
- `crates/cargo-gamma-lib/src/exec/census.rs` (`walk_with`) and
  `crates/cargo-gamma-lib/src/exec/sweep.rs` — the two call sites that currently launch one fresh
  process per `(binary, test)` or per mutant
- `crates/cargo-gamma/docs/DESIGN.md:835` — the documented limitation this would relax

**Done when:** census and sweep launches on Linux reuse a warm forked process instead of a fresh
`execve` per launch, a test proves the forked child observes no repeated dynamic-link or
static-init work (e.g. a constructor-run counter stays at one across many forked launches), and a
documented, tested fallback to today's spawn-per-launch behavior on platforms without `fork()`
(Windows) or without a dynamic loader step (statically linked binaries).

**See also:** F3 (shares the same fork-server engine). Landing this changes the per-launch cost
used by adaptive learning waits and grouped-census subdivision; revisit those heuristics'
thresholds once this ships.

---

<a id="p9"></a>
### P9 — Make the guard census an explicit opt-in

**Area:** `cargo-gamma-lib::exec` census admission, CLI, and configuration · **Priority:** High ·
**Effort:** Small

The guard census is currently enabled by default and disabled only indirectly through
`--whole-test-binaries`. It can launch each retained test binary repeatedly to refine grouped test
scopes before the mutant sweep begins. Its admission model uses test-listing time as a proxy for
that work and does not account for exact or generalized proximity knowledge beyond excluding a
mutant with an eligible exact hint from justifying its own census. The added process launches and
policy complexity are therefore not yet supported by evidence that census selection reliably
saves more campaign time than it consumes.

Make census collection conditional and disabled by default:

- add an explicit CLI and configuration opt-in, such as `--census`;
- keep exact killer hints, persisted reach hints, and in-run same-item/file learning active when
  census is disabled;
- conservatively fall back to whole-binary execution whenever those probes do not kill, rather
  than treating absent census data as evidence that a site is uncovered;
- keep `--whole-test-binaries` as the stronger request to suppress all test-case selection, or
  replace it only through an explicit compatibility and migration decision;
- report whether census was disabled, declined by its economic gate, attempted incompletely, or
  completed, so campaign records can compare its cost and benefit; and
- document census as an experimental optimization whose opt-in does not change verdict semantics.

**Done when:** a default run performs no census listing or sampling launches; an explicit opt-in
retains the current conservative census behavior; deterministic tests prove that disabling census
still uses exact and generalized probes before whole-binary fallback and cannot create an
`Uncovered` verdict from missing reach data; CLI/configuration compatibility is covered; and
equivalent runs with census on and off produce the same verdicts.

---

## Features

<a id="f2"></a>
### F2 — Checkpoint and resume long-running campaigns

**Area:** execution coordinator and run records · **Priority:** High · **Effort:** Large

Persist completed work periodically so cancellation, interruption, or host restart loses a
bounded amount of a multi-day campaign. The coordinator thread should publish an explicitly
partial record atomically; workers must not contend on it. Throttle checkpoints by elapsed time
and completed-mutant count so short runs pay negligible overhead and long verdicts cannot prevent
a time-based checkpoint.

Persist completed verdict entries, exact killer probes, compiler-confirmed unviability, build
ordering data, and the pre-run workspace/context snapshots needed to validate them. Never infer a
verdict from an absent entry. Reuse partial entries under the same trust rules as completed
records: validated kills may settle, killer probes are verified, and survivors, timeouts, and
resource failures are rerun according to policy. `cargo gamma hints` should be able to promote
safe probe and build-order tiers from a partial record. Keep the last valid checkpoint if its
replacement is truncated, interrupted, or fails to sync, and keep final reports explicitly
incomplete until the population finishes.

**Done when:** interruption tests cover every publication boundary, an end-to-end resume test
proves that a partial record saves work without changing the final score, and the configured
checkpoint cadence places an explicit upper bound on progress at risk.

---

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

<a id="f4"></a>
### F4 — Bound cache growth and reclaim abandoned workspaces

**Area:** cache identity, workspace lifecycle, and cache administration · **Priority:** High ·
**Effort:** Large

The stable per-workspace cache makes repeated campaigns incremental, but its lifetime is currently
unbounded. Every distinct physical workspace path receives a new cache identity, successful runs
retain both the synchronized source tree and Cargo target directory indefinitely, and
`cargo gamma clean` can remove only the cache belonging to one workspace that still resolves.
Temporary workspaces are especially damaging: after their source directories disappear, their
owner-marked cache roots become unreachable through the CLI and remain forever. There is no
global size quota, age policy, orphan reclamation, or inventory that can answer what is consuming
disk.

Add a global cache-administration surface (for example `cargo gamma cache status` and
`cargo gamma cache gc`) that reports each cache's owner, owner existence, last use, approximate
size, and lock state. Garbage collection must:

- remove owner-marked, unlocked caches whose workspace no longer exists;
- enforce configurable age and total-size limits by evicting least-recently-used inactive caches;
- keep active caches by acquiring their existing locks rather than racing live commands;
- bound opportunistic cleanup work on ordinary command startup, leaving a full scan to the
  explicit command;
- distinguish default caches from user-selected `--cache-dir` locations, reporting redirected
  caches without silently deleting an arbitrary path the user named;
- retain only the generations needed for incremental reuse and remove incomplete or superseded
  state;
- refuse to claim the cache namespace directory itself as one workspace's cache; and
- provide a dry-run legacy recovery pass for owner-marked caches created by older versions,
  including malformed 16-hex cache roots written directly beneath the platform cache home.

A small atomically updated cache index should make inventory and LRU selection proportional to the
number of known caches without requiring an unbounded directory traversal on every command. The
index is an optimization, not authority: deletion must revalidate the owner marker, path shape,
and lock immediately before removing anything, and a missing or corrupt index must be safely
rebuildable. Cache data remains disposable performance state; durable hints, suppressions, and
published reports remain outside garbage collection.

- `crates/cargo-gamma-lib/src/exec/workspace.rs:717-735` — derives the stable external cache path
- `crates/cargo-gamma-lib/src/exec/workspace.rs:133-175` — settled runs deliberately retain source
  and build trees
- `crates/cargo-gamma-lib/src/exec/workspace.rs:775-829` — cleanup is limited to one resolved
  workspace's default cache
- `crates/cargo-gamma-lib/src/commands/clean.rs` — current one-workspace cleanup command
- `crates/cargo-gamma/docs/DESIGN.md:379-431` — current identity, retention, and cleanup contract

**Done when:** repeated runs of one workspace reuse one bounded cache; deleting a temporary
workspace makes its unlocked cache eligible for automatic reclamation; a configured global quota
is enforced without touching active or redirected caches; status and dry-run GC explain every
candidate before deletion; malformed legacy roots can be recovered safely; and stress tests with
hundreds of thousands of synthetic index entries do not require traversing hundreds of thousands
of cache directories during normal startup.

**See also:** T2 (prevents the test suite from manufacturing abandoned user caches)

---

<a id="f5"></a>
### F5 — Incrementally update the workspace hints artifact

**Area:** hint promotion, selection scope, and durable scheduling knowledge · **Priority:** High ·
**Effort:** Medium

`gamma-hints.json` is workspace-wide, but `cargo gamma hints` currently replaces it with entries
from the command's selected population. Running and promoting hints from one crate can therefore
delete valid hints for every other crate, even though the existing artifact was read and used by
the crate-local run. File, package, diff, and feature selections make the same replacement behavior
unsafe as a routine update workflow.

Make hint promotion an incremental, selection-aware update:

- load and validate the existing hints artifact before constructing its replacement;
- add or update exact mutant killers and generalized item, file-binary, and reach-cluster knowledge
  produced by the selected run;
- preserve entries outside the command's selected population;
- remove stale entries only where complete discovery proves that the selected scope owns the entry
  and the corresponding mutant, item, file, or site no longer exists;
- rebuild interned reach-test sets deterministically and remove sets no entry references;
- represent provenance honestly when one artifact contains knowledge promoted under different
  feature, target, test, or policy contexts;
- retain an explicit whole-artifact replacement mode for callers that intentionally want a clean
  regeneration;
- report added, updated, removed, and preserved counts so a surprising scope is visible before
  publication; and
- keep the existing atomic publication and concurrent-change detection guarantees.

Merging must remain score-neutral. Exact and generalized hints are still revalidated by execution;
incremental update must not promote cached verdicts or turn absence from a partial run into evidence
that a hint is stale.

- `crates/cargo-gamma-lib/src/commands/hints.rs` — currently promotes one selected population
- `crates/cargo-gamma-lib/src/discover/hints.rs` — currently renders and replaces the complete
  artifact
- `crates/cargo-gamma-lib/src/discover/record.rs` — source of exact and generalized scheduling
  knowledge
- `crates/cargo-gamma/docs/SCHEDULING.md` — documents persistence and the scopes that consume hints

**Done when:** a workspace-wide artifact followed by a crate-local run and `cargo gamma hints`
updates that crate's entries without changing entries for other crates; selected stale entries are
removed while unselected entries remain; generalized reach-set interning remains deterministic and
minimal; a deliberate replacement mode can rebuild the whole artifact; interrupted and concurrent
updates preserve the last complete generation; and tests cover package, file, diff, feature, and
whole-workspace selections.

---

<a id="f6"></a>
### F6 — Schedule unhinted mutants to maximize in-run learning

**Area:** `cargo-gamma-lib::exec::sweep` scheduling and file learning · **Priority:** High ·
**Effort:** Medium

The global mutant queue is fixed before the sweep. Workers can therefore claim nearby unhinted
mutants concurrently, then wait 5–200 ms for the same-item scout even though useful unrelated work
remains. Tests commonly outlast that wait, so the siblings proceed independently and repeat broad
test-binary work before the scout can publish a reusable killer.

Replace fixed scout pauses with dynamic, distance-aware work selection:

- choose a mutant only when a worker is ready to execute it, considering the files and items
  currently in flight;
- prefer an unhinted mutant from a source file with no active mutant;
- when every remaining file is active, prefer an inactive item in the least-contended file;
- treat file separation as a soft preference and same-item exclusion as the stronger constraint,
  because an exact test learned for an item is more reusable than file-level binary evidence;
- within an equally distant tier, preserve package fairness, useful longest-work-first behavior,
  stable tie-breaking, and any established canonical ordering guarantees;
- prefer scouts with high learning leverage, such as an item with many pending siblings, while
  accounting for estimated evaluation cost;
- publish all learning before releasing a mutant's file/item reservation and assigning follow-on
  work;
- when no independent work remains, make an explicit policy choice between allowing the
  least-contended duplicate and leaving capacity idle until a completion event; and
- if capacity waits, wait on scheduler state changes rather than sleeping for an arbitrary
  duration, and never let a worker reserve a mutant while waiting.

Hinted mutants do not require cold-scout spacing, but a checked hinted result may still publish
knowledge before the scheduler chooses subsequent work. Reordering must remain score-neutral:
for a deterministic, isolated suite, changing mutant order may change cost and which killing test
is recorded, but not the final verdict for any mutant.

**Done when:** no worker uses a fixed-duration scout wait; deterministic scheduler tests prove that
idle files are selected before active files, inactive items before active items, learning is
visible before the next related assignment, stable secondary ordering is preserved, and the
configured tail policy behaves as specified when only conflicting work remains; an execution test
with long-running scouts proves that unrelated mutants proceed while same-item siblings do not
race unnecessarily; and equivalent schedules produce the same final verdicts.

---

## Testing

<a id="t1"></a>
### T1 — Isolate tests from the production interrupt registry

**Area:** `cargo-gamma-unsafe` interrupt and cgroup tests · **Priority:** High · **Effort:** Medium
**Confidence:** High · **Scope:** five process-global test interactions — exhaustive
**Trigger:** libtest schedules production-handler tests before or concurrently with cgroup watch tests

Two tests call the production signal handler directly, permanently latching the process-global
registry's interrupt state. Cgroup tests use that same registry with fabricated process-group IDs
41 and 42. Once interrupted, registering either ID immediately invokes the production
`kill_group`, and a concurrent handler sweep does the same. The test binary can therefore send a
real `SIGKILL` to an unrelated host process group that happens to own either numeric ID, while
later tests also inherit interrupt state they did not arrange.

- `crates/cargo-gamma-unsafe/src/interrupt.rs:112-116` — registry interruption is deliberately
  never cleared
- `crates/cargo-gamma-unsafe/src/interrupt.rs:193-211` and
  `crates/cargo-gamma-unsafe/src/interrupt.rs:264-284` — claiming after interruption and sweeping
  invoke the supplied killer
- `crates/cargo-gamma-unsafe/src/interrupt.rs:469-471` and
  `crates/cargo-gamma-unsafe/src/interrupt.rs:575-581` — production paths supply a real
  `kill(-group, SIGKILL)`
- `crates/cargo-gamma-unsafe/src/interrupt.rs:755-769` — tests call the production handler against
  the global registry
- `crates/cargo-gamma-unsafe/src/cgroup.rs:913-914` and
  `crates/cargo-gamma-unsafe/src/cgroup.rs:966-1050` — cgroup tests register IDs 41 and 42 through
  that registry

**Done when:** handler tests mutate only an isolated registry or run in child processes, cgroup
tests inject a recording killer instead of using the production registry, and order-randomized
parallel execution cannot signal a real process group or leak interrupt state between tests.
