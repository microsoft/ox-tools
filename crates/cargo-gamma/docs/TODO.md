# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P1](#p1) — Publish killers found after a file-learning fallback
- [P2](#p2) — Compare equal files correctly across short reads
- [P3](#p3) — Amortize per-mutant process launch with a fork server

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns
- [F3](#f3) — Native fork-server test harness as a `cargo test`/nextest replacement

### Documentation
- [D1](#d1) — Correct the cgroup watch-state documentation

### Testing
- [T1](#t1) — Isolate tests from the production interrupt registry

## Performance

<a id="p1"></a>
### P1 — Publish killers found after a file-learning fallback

**Area:** `cargo-gamma-lib::exec::sweep` · **Priority:** Low · **Effort:** Small

When a worker times out waiting for another file learner, or sees `Learning::Exhausted`, it runs
the full ordered judgement itself. Unlike the hinted and designated-learner paths, these fallback
paths return without publishing a killer they discover. Later mutants in the same file therefore
repeat the full binary order even though this run already found a reusable probe.

- `crates/cargo-gamma-lib/src/exec/sweep.rs:746-749` and
  `crates/cargo-gamma-lib/src/exec/sweep.rs:788-790` — the two normal paths publish or complete
  learning
- `crates/cargo-gamma-lib/src/exec/sweep.rs:762-784` — timeout and exhausted-state fallbacks return
  their judgement without either operation

**Done when:** every fallback judgement publishes a newly found killer without overwriting an
already learned one, and a regression test starts from `InProgress`, forces the bounded wait to
expire, returns a killing judgement, and observes `Learning::Learned`.

---

<a id="p2"></a>
### P2 — Compare equal files correctly across short reads

**Area:** `cargo-gamma-lib::exec::sync` · **Priority:** Low · **Effort:** Small

`same_contents` reads two files independently and treats unequal read lengths as unequal file
contents. `Read::read` may legally return a short non-EOF result, so identical files on a
filesystem that gives different chunk sizes can be needlessly recopied, changing timestamps and
invalidating Cargo fingerprints.

- `crates/cargo-gamma-lib/src/exec/sync.rs:308-330` — independent reads are compared as though both
  must fill equally sized chunks

**Done when:** comparison handles independent short reads without misaligning bytes, and a unit
test uses two readers with different chunk schedules over identical content.

---

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
`execve` per launch, with measured wall-clock improvement on a many-small-tests workspace, and a
documented, tested fallback to today's spawn-per-launch behavior on platforms without `fork()`
(Windows) or without a dynamic loader step (statically linked binaries).

**See also:** F3 (shares the same fork-server engine)

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

## Documentation

<a id="d1"></a>
### D1 — Correct the cgroup watch-state documentation

**Area:** `cargo-gamma-unsafe::cgroup` · **Priority:** Low · **Effort:** Trivial

The documentation on `Cgroup::is_watched` says the method records a published kill descriptor,
but the method only queries whether a watch exists. That description belongs to `watched_at`,
whose current one-line documentation omits the descriptor-lifetime invariant.

- `crates/cargo-gamma-unsafe/src/cgroup.rs:794-805` — the two adjacent methods carry each other's
  intended descriptions

**Done when:** `is_watched` documents its boolean query and `watched_at` documents publication and
lifetime ownership.

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
