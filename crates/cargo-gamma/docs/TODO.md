# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Features
- [F2](#f2) — Checkpoint and resume long-running campaigns

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
