# cargo-gamma-process — Sampled memory control on macOS

> Status: **Implemented**.
> Companion to the [crate design](DESIGN.md).

## Summary

Linux and Windows bound a test subtree's memory with a kernel boundary (a
cgroup leaf, a job object). macOS has no unprivileged equivalent. Before this
design, `--memory enforce` silently degraded to `off` there, and a mutant that
turned bounded allocation into unbounded allocation ran until the wall-clock
timeout. On a developer machine, that is long enough to exhaust physical and
compressed memory and take every other application down with it.

cargo-gamma closes that gap with **sampled enforcement**. On macOS, it samples
the physical footprint of every process in the launch's process group at a
short fixed interval. It kills the group when the sum passes the ceiling the
run already computes. Everything above the mechanism is reused unchanged: the
baseline-derived ceiling, the `MemoryPolicy` arithmetic, the `outofmem`
outcome, its note, and its suppression eligibility.

The bound is soft. A runaway can overshoot the ceiling by roughly its growth
rate times the sampling interval. A descendant that leaves the process group
is not counted. Both limits are stated to the user rather than hidden.

## Motivating example

cargo-gamma generates mutants that allocate without bound from ordinary code.
The example below shows which mutants do that and why. The incident after it
shows what one such mutant did to a machine when the only guard was a timeout.

### The code

A tree stores `(child, parent)` edges. One method looks up a node's parent,
and another walks from a node up to the root. The following complete crate is
reduced from the original, keeps its shape, and compiles and passes as shown.
It is `src/lib.rs`, and the line numbers below refer to it:

```rust
pub struct Tree {
    /// `(child, parent)` edges.
    pub edges: Vec<(String, String)>,
}

impl Tree {
    pub fn parent_of(&self, id: &str) -> Option<String> {                                    // line 7
        self.edges.iter().find(|(child, _)| child == id).map(|(_, parent)| parent.clone())   // line 8
    }

    pub fn ancestor_chain(&self, id: &str) -> Vec<String> {                                  // line 11
        let mut chain = Vec::new();
        let mut cur = id.to_owned();
        while let Some(p) = self.parent_of(&cur) {
            chain.push(p.clone());                                                           // line 15
            cur = p;                                                                         // line 16
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_to_the_root() {
        let tree = Tree { edges: vec![("b".into(), "a".into()), ("c".into(), "b".into())] };
        assert_eq!(tree.ancestor_chain("c"), ["b", "a"]);
    }
}
```

The `while let` loop in `ancestor_chain` has no bound of its own. It ends only
because `parent_of` eventually returns `None`. On this data, the walk from
`"c"` visits `"b"`, then `"a"`, and stops because no edge has `"a"` as a child.
Any mutation that makes `parent_of` keep returning `Some`, or stops `cur` from
moving toward the root, turns the loop into an unbounded `push`.

### The mutants cargo-gamma generates for it

`cargo gamma list` on this crate, with the default mutators:

```text
src/lib.rs:7:57: replace { self.edges.iter(...| parent.clone()) } with None [fn_value.none]
src/lib.rs:8:45: replace child == id with (child) != (id) [relational.eq_to_ne]
src/lib.rs:11:59: replace { let mut chain = ... cur = p; } chain } with Vec::new() [fn_value.empty_collection]
src/lib.rs:11:59: replace { let mut chain = ... cur = p; } chain } with core::iter::on...ew()).collect() [fn_value.one_element]
src/lib.rs:11:59: replace { let mut chain = ... cur = p; } chain } with core::iter::on...ed()).collect() [fn_value.one_element]
src/lib.rs:15:13: delete chain.push(p.clone()); [stmt.delete_call]
src/lib.rs:16:13: delete cur = p; [stmt.delete_assign]
```

With `--mutators @pedantic`, the following are also generated:

```text
src/lib.rs:7:57: replace { self.edges.iter(...| parent.clone()) } with Some(String::new()) [fn_value.some]
src/lib.rs:7:57: replace { self.edges.iter(...| parent.clone()) } with Some("xyzzy".to_owned()) [fn_value.some]
```

What each mutant does to `walks_to_the_root`:

| Mutant | What the mutated code does | Terminates? | Memory |
|---|---|---|---|
| `7:57` → `None` | `parent_of` always says "no parent"; the chain is empty | yes, assertion fails | bounded |
| **`8:45` `==` → `!=`** | `parent_of("c")` returns the parent of the first edge whose child is *not* `"c"`. That is edge `(b, a)`, so it returns `"a"`. `parent_of("a")` again picks `(b, a)` and returns `"a"`, forever. | **no** | **unbounded**: pushes `"a"` forever |
| `11:59` → `Vec::new()` / one-element vectors | `ancestor_chain` returns a constant | yes, assertion fails | bounded |
| `15:13` delete `chain.push(..)` | `cur` still walks `c → b → a`, so the loop ends; the chain is empty | yes, assertion fails | bounded |
| **`16:13` delete `cur = p`** | `cur` stays `"c"`, so `parent_of("c")` returns `"b"` on every iteration | **no** | **unbounded**: pushes `"b"` forever |
| **`7:57` → `Some(String::new())`** (`@pedantic`) | every node, including `""`, has the parent `""` | **no** | **unbounded**: pushes `""` forever |
| **`7:57` → `Some("xyzzy".to_owned())`** (`@pedantic`) | every node, including `"xyzzy"`, has the parent `"xyzzy"` | **no** | **unbounded**: one heap string plus one vector slot per iteration |

So with default mutators, cargo-gamma generates two mutants of this code that
allocate without bound. With `@pedantic`, it generates four. None of them
depends on anything unusual. A loop whose termination depends on a lookup
eventually failing, and whose body allocates, is common in ordinary code.

A `Vec` doubles its capacity as it grows, so a runaway like these touches new
memory about as fast as the process can copy it. In the incident below, the
test binary grew at roughly 5–9 GB/s.

### The incident

The incident happened in a downstream workspace with a different mutation tool
(cargo-mutants 27.1.0), before cargo-gamma was in use there. Its tree code had
the shape above. The tool generated three of these runaways: `!=` in the
lookup, `Some(String::new())`, and `Some("xyzzy".into())`. Seven tests that
exercised the walk ran in parallel in one test binary.

### What happened to the machine

The run used `--timeout 180`. At 5 GB/s, 180 seconds is about 900 GB; the
machine had 64 GB. The timeout is the wrong unit for this failure: memory runs
out within seconds, minutes before the timeout fires.

macOS does not fail the allocation. It compresses memory, then swaps, then, when
compressor space runs out, jetsam kills processes by priority. Background
applications go first and the foreground terminal last. That ordering matches
what the user saw: every application closed except the terminal.

Five incidents across two days involved the same three mutants:

| When (PDT) | Test-process footprint | Consequence |
|---|---|---|
| 10-05 17:48 | ~121 GB | jetsam, mass application kill |
| 10-05 17:59 | ~109 GB | jetsam, mass application kill |
| 10-06 09:12 | unknown | kernel watchdog panic, "100% of compressed pages limit", reboot |
| 10-06 09:39 | ~90 GB | jetsam, `vm-compressor-space-shortage`, ~12.6k kills |
| 10-06 09:49 | ~96 GB | jetsam, `vm-compressor-space-shortage`, ~10.1k kills |

The footprint figures come from the jetsam reports in
`/Library/Logs/DiagnosticReports`. The kill counts include system daemons that
launchd relaunched and jetsam killed again. The figures exceed physical memory
because they count compressed pages, which is why resident size alone would
have misread the situation.

At 09:48:26 the tool's timeout terminated the `cargo` process it had started.
The jetsam snapshot at 09:48:59 still lists the grandchild test binary alive at
about 94 GB. Terminating the direct child did not terminate the process that
held the memory.

### What the incident establishes

1. **A time budget cannot bound memory.** Growth of several GB/s exhausts a
   workstation in seconds; any timeout long enough for a healthy suite is
   orders of magnitude too long.
2. **The boundary has to be the process tree, not the direct child.** The
   allocating process was a grandchild.
3. **The metric has to include compressed memory.** Resident size shrinks as
   macOS compresses, so a resident-size guard reads lower exactly when the
   machine is in most danger.
4. **The risk is generic, not mutator-specific.** Nothing about these mutants
   is special: any mutant that breaks a loop's termination while the loop
   allocates does this. A guard tied to one mutator family does not cover it.

The downstream project has since wrapped these tests in a 500 ms in-test
deadline, which cuts the peak to about 5.4 GB. That is good test hygiene, but it
protects only the tests that were wrapped. It is bounded by time rather than
memory, and the abandoned thread keeps allocating until the test binary exits.
The guard belongs in the tool that generates the mutants.

## What macOS offers

Each candidate was checked on macOS 15.7.2, arm64, 64 GB.

| Mechanism | Result |
|---|---|
| `setrlimit(RLIMIT_AS / RLIMIT_DATA / RLIMIT_RSS)` | Rejected with `EINVAL` when lowered from infinity, as a soft-only or a soft-and-hard limit. A 2 GB allocation then succeeds. In the same process, `RLIMIT_NOFILE` and `RLIMIT_STACK` lower normally. These limits are also per process, not per tree. |
| cgroups, job objects | Do not exist. |
| Kernel per-process memory limits (memorystatus) | Private interfaces, not a supported API for an unprivileged tool. |
| Running tests in a VM or container | Changes the platform under test and costs far more than the protection is worth. |
| Sampling from the supervisor | Works unprivileged: `proc_listpids(PROC_PGRP_ONLY, pgid)` lists a group's members, and `proc_pid_rusage(pid, RUSAGE_INFO_V4)` reads each member's `ri_phys_footprint`. |

`ri_phys_footprint` is the metric jetsam and Activity Monitor use. It includes
compressed memory, so it does not fall while the machine compresses.

## Design

### Mechanism

For each launch on macOS whose memory request is measuring, a sampler owned by
the launch's boundary does the following every `SAMPLE_INTERVAL`:

1. Lists the members of the launch's process group.
2. Sums their `ri_phys_footprint`.
3. Records the sum as the running peak.
4. If enforcing and the sum exceeds the ceiling, immediately takes a second
   pass, with no wait. Only if that pass also exceeds the ceiling does it kill
   the group and record the reading that crossed it.

The confirming pass exists because the members are read one after another, not
at the same instant, so a pass's sum is not a true point-in-time total. If one
member releases memory just as another allocates it, a single pass can count
the same memory twice and sum above the ceiling, although the group never held
that much at once. A false breach would report a healthy mutant as `outofmem`.
A real runaway is still over the ceiling on the second pass, which costs only
the time of one more pass. Confirmation makes a false breach much less likely,
but it does not rule one out; see "Non-simultaneous reads" under the stated
limits.

The kill reuses the existing process-group sweep, including the macOS
zombie-leader handling. The verdict reuses `outofmem` and its note: the binary,
the peak reached, and the ceiling passed.

The baseline uses the same sampler to measure the peak that calibrates the
ceiling, so the default `--memory enforce` becomes available on macOS without
any new configuration.

### Sampling interval

`SAMPLE_INTERVAL` is a fixed 10 ms constant, not a user setting. A prototype
measured the overshoot against a runaway growing faster than the incident's:

| Interval | 1 process, ~12 GB/s | 4 processes, ~40 GB/s |
|---|---|---|
| **10 ms** | **+47 MB**, stopped at 0.13 s | **+96 MB**, stopped at 0.04 s |
| 50 ms | +360 MB | +801 MB |
| 100 ms | +887 MB | +2,950 MB |

The overshoot is about the growth rate times the interval. At 10 ms it is well
inside the default 128 MiB headroom. That compares with more than 90 GB in the
incident. One sample costs one listing call plus one usage call per group
member, which is negligible next to running a test binary.

### Stated limits

- **Soft bound.** The overshoot above is inherent. The `outofmem` note reports
  the reading that crossed the ceiling, which can exceed the ceiling.
- **Startup window.** The child is already running when its sampler starts,
  because the sampler needs the child's process group, which exists only after
  the spawn. The first reading therefore comes as soon as the sampler thread
  is running, normally well under one interval after launch. That is longer
  under heavy host load. A runaway that starts allocating at once can overshoot
  by its growth rate times that delay as well. Holding the child suspended
  until the sampler is ready would close the window. That would change the
  launch path for every macOS launch, and it is not done for a gap normally
  shorter than the interval itself.
- **Non-simultaneous reads.** Members are read one after another, so a pass's
  sum is not a point-in-time total. A breach must be seen on two consecutive
  passes, which filters out a single transient shift between members. A group
  that hands the same memory from one member to another between the reads of
  both passes can still sum above the ceiling twice and be stopped as
  `outofmem`, although it never held that much at once. That needs the
  handoff to coincide with the reads twice in a row. Ruling it out would need
  a stable snapshot, for example stopping the group while it is read. That
  would pause every macOS launch at every sample, so it is not done.
- **Group escape.** A descendant that calls `setsid` or `setpgid` leaves the
  group and is neither counted nor killed. This is the same best-effort
  containment macOS already has for termination. It does not newly weaken
  anything.
- **Unreadable members.** An unprivileged reader gets `EPERM` for a process
  owned by another user. For example, a test may launch a setuid tool such as
  `top`, `sudo` or `crontab`. Such a member is skipped like a vanished one, so
  the bound covers what this user can read. Stopping the launch instead would
  make every such suite unjudged on macOS. Before this design, those suites ran
  normally. Any other reading failure, including a failure to list the group,
  means the guard is lost. That stops the launch, and it is reported as unjudged
  rather than as a measurement.
- **Short-lived peaks.** A process that peaks and exits between two samples is
  not seen. This is acceptable for runaway detection, because a runaway is by
  definition still alive and still growing. It matters only for baseline
  calibration, where an underestimated peak would tighten the ceiling.
  The headroom and multiplier absorb it. A baseline launch is marked as
  calibrating in its memory request. When it is, each sample also sums the
  members' `ri_lifetime_max_phys_footprint`, and one final reading is taken
  after the leader exits and before it is reaped. This depends on the
  calibrating mark, not on whether a ceiling is present, so
  `--baseline-memory-limit` does not turn the recovery off. On macOS 15.7.2, an exited, unreaped
  process still reports its lifetime maximum, while its current footprint reads
  zero. This sum can exceed the true simultaneous peak, which errs toward a
  looser ceiling. Enforcement compares only the current footprint, never
  historical maxima.

### Support and demand

On macOS, `support()` reports that memory control is available in its sampled
form, instead of reporting unsupported. Consequences:

- Inherited `--memory enforce` now enforces on macOS rather than degrading to
  `off`. The run states once that enforcement is sampled, so the user is not led
  to believe that a kernel boundary exists.
- A stated `--memory enforce` or `--memory-limit` is honored on macOS instead of
  failing the run.
- `--memory off` remains the user's explicit choice of no control.

**Not with nextest.** With `--nextest`, cargo-gamma launches `cargo-nextest`,
which puts every test process in a process group of its own (on cargo-nextest
0.9.138, a test's pid equals its pgid, distinct from the runner's group). A
group sampler would then measure the runner, not the tests, and would neither
count nor stop a runaway test. So macOS with nextest is treated as a host
without memory control, through the existing degrade-or-refuse path:

- Inherited control degrades to `off`, and the run states that the reason is
  nextest's per-test process groups. The sampled-enforcement note is not shown.
- Stated control or a stated ceiling is refused, with advice to drop
  `--nextest` or pass `--memory off`.

Linux and Windows are unaffected, because the cgroup or job contains nextest's
test processes too. Covering nextest on macOS would require walking the process
tree by parentage instead of by group. That is left to a later change.

The existing contract says that support is claimed only where it exists. It is
kept by naming the kind of support: kernel-enforced, or sampled.

### Where the code goes

| Crate | Change | Reason |
|---|---|---|
| `cargo-gamma-unsafe` | Safe wrappers for `proc_listpids` and `proc_pid_rusage` | FFI belongs here; every other crate forbids unsafe code |
| `cargo-gamma-process` | The macOS sampler behind the existing accounting and ceiling interfaces | This crate already owns memory accounting for Linux and Windows |
| `cargo-gamma-lib` | Rewording of the memory module's rationale; the run-start note naming sampled enforcement | Policy is unchanged; only the description of the platform changes |

This design changes one existing rationale. The memory module says that polling
is not used because it describes one process and races short-lived
descendants. That remains true where a kernel boundary exists. On macOS, no such
boundary exists. Polling the whole process group is the strongest available
guard, and the race affects only processes that are not runaways.

### Testing

Following the repository's test-review rules, unit tests do not spawn
processes:

- The sampler is driven by an injected reading source. Tests cover the
  following: a sum crossing the ceiling kills exactly once; the peak tracks the
  maximum across samples; members that disappear between listing and reading
  are skipped, not treated as errors; and a measuring but not enforcing policy
  never kills.
- A single macOS-only integration test, ignored by default and failing when
  requested by name on a host that cannot run it, launches a real runaway
  allocator under a small explicit ceiling. It asserts that the outcome is
  `outofmem` and that the recorded peak is within a stated bound of the
  ceiling.

## Relation to reference-value mutants

The leak-specific limiting proposed in PR #243 was a response to this same gap,
seen from one mutator: a `Box::leak` replacement on an unmetered host. With
sampled enforcement, macOS is no longer unmetered. A leaking mutant is held to
the same ceiling as every other mutant on every supported platform. A
per-mutator counter would not have stopped the incident above, while this
design does.

## Out of scope

- Sealed containment on macOS, which would need a boundary a descendant cannot
  leave. No unprivileged mechanism exists.
- Host-wide memory pressure policy, such as pausing workers when the machine is
  low on memory regardless of ceilings.
- Changing the ceiling arithmetic, the `outofmem` outcome, or its suppression
  defaults.
