# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P3](#p3) — Amortize per-mutant process launch with a fork server
- [P7](#p7) — Prove cursor stalls through nested branches and lexical binding identities
- [P9](#p9) — Prove stalled nonempty grouping boundaries over immutable collections
- [P11](#p11) — Prove retry fixed points without mistaking cached calls for progress

### Features
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement

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
