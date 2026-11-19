# TODO

The forward-looking backlog: what is still worth doing to this codebase. Completed items are
deleted; this file is not a changelog or a record of rejected work.

## Contents

### Performance
- [P14](#p14) — Preserve separate incremental compilation lanes for convergence and proofs
- [P16](#p16) — Validate and amortize selective compiler replay costs

## Compiler-exclusion implementation order

Implement this work in the following order; existing unrelated items retain their IDs and positions.
This sequence addresses hint-free campaigns as well as runs without a reusable compiler-exclusion cache.

### Convergence and isolation

Keep the existing optimization sequence; generation
repairs and stronger evidence complement these changes rather than replace them.

1. [P14](#p14) — Reduce schema switching and preserve incremental compilation state.
2. [P16](#p16) — Validate selective compiler reuse after accounting for snapshot costs.

## Performance

<a id="p14"></a>
### P14 — Preserve separate incremental compilation lanes for convergence and proofs

**Area:** `cargo-gamma-lib::exec::build` schemas and scratch workspace ownership · **Priority:** High · **Effort:** Large

Isolation removes mutations from affected dependency components and ordinary convergence restores
the remaining full schema. On a connected workspace these transitions can rewrite many sources
and invalidate downstream library and test-harness artifacts. Evaluate persistent, separately
owned convergence and proof workspaces/target directories against the batched, dependency-ordered
search so proof experiments do not continually displace the main schema's incremental compilation state.
Each lane also needs independent source and guard state: switching a single `Splices` between
workspace roots clears those caches.

- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:658` — constructs the proof background from affected dependency components.
- `crates/cargo-gamma-lib/src/exec/build/isolation.rs:1214` — writes the active proof schema before invoking Cargo.
- `crates/cargo-gamma-lib/src/exec/build/splices.rs:164` — switches the files carrying active proof guards.
- `crates/cargo-gamma-lib/src/exec/build.rs:1602` — ordinary convergence instruments its remaining full schema.

**Done when:** the selected lane strategy measurably reduces total source rewrites, rebuilt compiler
units, and cold convergence time after accounting for both lanes' warm-up and disk usage. Unchanged
files keep their timestamps, each lane has stable paths and exclusive ownership, and dependency
artifacts cannot be reused across incompatible schemas. Fixtures preserve identical exclusion and
interaction decisions, and scratch cleanup cannot affect another running campaign.

---

<a id="p16"></a>
### P16 — Validate and amortize selective compiler replay costs

**Area:** `cargo-gamma-lib::exec` compiler snapshot validation · **Priority:** Medium · **Effort:** Large

Selective metadata-unit reuse retains Cargo's original invocation and validates exact
compiler commands, source/dependency snapshots, directory inventories and outputs.
However, validation repeats toolchain and source-tree hashing per eligible unit.
Fewer compiler starts do not establish lower total convergence cost: repeated snapshot
work can erase the saved compilation, and there is no aggregate hashing-work budget
across a proof.

- `crates/cargo-gamma-lib/src/exec/rustc_replay.rs:58` — fingerprints the compiler command and its toolchain.
- `crates/cargo-gamma-lib/src/exec/rustc_replay.rs:112` — inventories source paths for snapshot validation.
- `crates/cargo-gamma-lib/src/exec/rustc_replay.rs:738` — wrapper decision boundary for selective compiler reuse.

**Done when:** snapshot validation is safely amortized or bounded without weakening
source, environment, dependency, compiler or output invalidation. Representative
convergence evidence accounts for hashing and capture costs and demonstrates lower
end-to-end cost, not only fewer compiler starts. Unsupported or exhausted validation
still executes the original compiler, preserving diagnostic and exclusion decisions.

**See also:** P14 (artifact isolation).
