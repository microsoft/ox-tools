# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P3](#p3) — Amortize per-mutant process launch with a fork server

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement

## Performance

<a id="p3"></a>
### P3 — Amortize per-mutant process launch with a fork server

**Area:** `cargo-gamma-rt`, `cargo-gamma-lib::exec` (census, sweep) · **Priority:** Medium ·
**Effort:** Large

The design accepts per-mutant process launch as an unavoidable floor: "process launch remains
per mutant... activating several mutants together would confound causality" (the
"Costs and limitations" section of `docs/DESIGN.md`). For workspaces with many small, fast-running
tests, that launch/link/static-init cost dominates both the census walk (one subprocess per sampled
scope, including every subdivision) and the mutant sweep (one per selected binary or probe), and
currently cannot be amortized: no machinery in this crate reuses a warm process across launches.

A fork-server model can remove that floor without weakening isolation: install a pre-main
constructor (the existing hook point already used for guard installation) that, in the runtime
crate, forks the warm, fully-linked, single-threaded process fresh for every test or mutant
launch instead of paying `execve`/dynamic-link/static-init again. Each child still gets full
process isolation, matching today's guarantees; only the one-time setup cost is shared. The
mutant ordinal can be set by direct memory write in the child (inherited via copy-on-write, no
env/argv needed); libtest-driven test selection needs the fork server's constructor to run before
std's argv-capturing constructor so each child's argv can be patched before libtest reads it.

- `crates/cargo-gamma-rt/src/runtime.rs:1736` (`install()`) and the `.init_array`/
  `__DATA,__mod_init_func`/`.CRT$XCU` link-section statics around it —
  the existing pre-main constructor mechanism to extend
- `crates/cargo-gamma-lib/src/exec/census.rs` (`walk_with`) and
  `crates/cargo-gamma-lib/src/exec/sweep.rs` — the two call sites that currently launch one fresh
  process per sampled scope or mutant/binary attempt
- `crates/cargo-gamma/docs/DESIGN.md` — the documented limitation this would relax

**Done when:** census and sweep launches on Linux reuse a warm forked process instead of a fresh
`execve` per launch, a test proves the forked child observes no repeated dynamic-link or
static-init work (e.g. a constructor-run counter stays at one across many forked launches), and a
documented, tested fallback to today's spawn-per-launch behavior on platforms without `fork()`
(Windows) or without a dynamic loader step (statically linked binaries).

**See also:** F3 (shares the same fork-server engine). Landing this changes the per-launch cost
used by census admission and subdivision, assignment-time cost estimates, and learning-value
priority; revisit those heuristics' thresholds once this ships.

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
