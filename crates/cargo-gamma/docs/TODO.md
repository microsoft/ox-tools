# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P1](#p1) — Publish killers found after a file-learning fallback
- [P2](#p2) — Compare equal files correctly across short reads

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns

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
