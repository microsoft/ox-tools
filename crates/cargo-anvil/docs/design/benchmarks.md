# cargo-anvil benchmark regression detection

This document describes `cargo-anvil`'s support for detecting performance
regressions from a repo's benchmarks over time. It wraps
[`cargo-bench-history`][cbh] (cbh) — which stores each benchmark run as an
immutable record, reconstructs per-benchmark series in git first-parent order,
partitions series by a hardware machine key, and reports level shifts and drift
with noise-aware, false-discovery-controlled statistics — and integrates it into
the opinionated catalog across both cloud-workflow backends.

The intended audience is `cargo-anvil` maintainers and downstream catalog
authors. The phased build-out is tracked in
[implementation-plans/0003.md](../implementation-plans/0003.md).

## 1. Problem

A repo's benchmarks are only useful as a regression signal if a slowdown shows
up as a break in a *trend*, well before it would reach any expensive,
bespoke-hardware load test. Turning per-run numbers into a reliable trend is the
hard part: results must be ordered by how the code evolved (not by when a
benchmark happened to run), compared only against like hardware (CI runs on a
heterogeneous, rotating pool whose machine-to-machine variance dwarfs the
measurement), and judged with noise-aware statistics (a fixed percentage
threshold on that noise fires constantly). Every anvil repo has benchmarks and
none of them gets this today — the existing `bench` check only compiles them.

## 2. Design principles

- **Detection is cbh's, not anvil's.** anvil installs and drives the tool; it
  does not reimplement change-point detection, machine-key normalization, or
  history storage. The opinionated contribution is *where* the tool runs, *how*
  its history persists, and *how* a regression is surfaced and accepted.
- **Regression detection is a scheduled concern.** It obeys the catalog's
  standing rule — a check belongs in scheduled iff its outcome can change without
  a commit to this repo (see [checks.md §4](./checks.md)). Benchmark results
  accrue over time and the trend verdict at the tip changes as history grows, so
  detection runs on the scheduled tier. On pull requests the benchmark check
  stays **compile-only**; running benches per-PR is too costly and, on shared
  runners, too noisy to gate a merge.
- **History is cross-run state, kept in CI-native storage.** cbh's local backend
  is an immutable, key-addressed directory that round-trips between scheduled runs
  through each backend's native build **artifacts**, whose retention and
  fetch-the-latest-from-the-default-branch semantics keep the subsystem
  self-contained in the pipeline with no external store to provision by default.
- **A regression fails the scheduled build.** Detection happens after the
  offending change merged, so there is no pull request to annotate; the signal is
  a failed scheduled build, which each backend's native failure notifications
  carry. This sits within the "advisory, never fail" tenet, which governs *PR
  gating* — a scheduled build blocks no one's merge.
- **Intentional changes are accepted through a reviewed file.** A regression is
  cleared by fixing it or by *blessing* it; blessing is expressed as a committed,
  reviewed entry rather than an out-of-band action, so accepting a slowdown is an
  audited decision.
- **One catalog, both backends.** As with every other check, the capability is
  generated for GitHub Actions and Azure DevOps from the same source, so adding
  it once reaches every consuming repo.

## 3. Place in the catalog

The compile-only `bench` check is unchanged. A new analyzing check —
`bench-history` — runs the benchmarks, records this commit's results into the
history, applies any pending blessings, and analyzes the accumulated
series. It lives in its own scheduled group, `scheduled-benchmarks`, so its
history round-trip and failure semantics stay isolated, and it **exits non-zero
when cbh reports an active regression**; locally and in cloud it behaves
identically (always writes its findings; only the exit code gates), matching the
local-vs-cloud parity every recipe keeps.

## 4. History as cross-run state

Each scheduled run checks out with full history (analysis reads the commit graph
to order series and locate the base merge-base), restores the latest history
**artifact from the default branch**, runs collect → apply-blessings → analyze,
and publishes the updated history as this run's artifact. Because each run
republishes the whole accumulated directory, only the newest snapshot is ever
needed.

The persistence is a **rolling window** on CI-native artifacts — portable and
zero-config, and on eviction it degrades to a harmless cold start, since
detection is advisory-by-design and gates nobody's merge. The backend-specific
restore/save building blocks live in [github.md](./github.md) and
[ado.md](./ado.md). cbh's own durable backends (for example Azure Blob) are
outside anvil's scope: the artifact rolling window is the supported store.

## 5. Surfacing: failing the scheduled build

An active regression fails the scheduled build; the findings — each benchmark,
its magnitude, and the commit cbh attributes the change-point to — are written to
the build summary and to a findings file the backend wiring consumes. The emitted
findings include cbh's topology-accurate trend chart, so the reviewer's surface is
self-contained — enough to decide *fix or bless* without reproducing the run.

- **GitHub Actions** — the failure feeds the repo's create-issue-on-failure path;
  the issue is **updated in place** each run from the findings file, so
  concurrent regressions and the authors of their attributed commits surface even
  while the build is already red.
- **Azure DevOps** — existing failed-build notification subscriptions fire; the
  findings live in the build summary.

A sustained regression re-fails every run until it is fixed or blessed, so red
stays meaningful only under the discipline that the build is always returned to
green by one of those two actions.

## 6. Local behavior

The `bench-history` recipe is the same command locally and in cloud, but its
*input* differs: the history lives only in the CI artifact store, so a local run
has no shared trend to analyze. Locally the recipe runs the benches, records them
into a gitignored local store, and analyzes that store — which on a fresh checkout
is empty, so analysis is a clean no-op reporting that there is no local history
yet. A developer thus gets the current run's numbers (and a single-machine local
trend if they run it repeatedly), not the shared regression signal.

Regression detection is therefore a scheduled, shared concern, and the failure
surface (§5) — cbh's finding and trend chart in the issue or build summary — is
the interface a developer acts on. Reproducing a CI finding locally is not a
first-class workflow: it would require downloading that run's `bench-history`
artifact into the local store and running cbh's `examine` with the run's machine
key (a developer's machine has a different hardware fingerprint). That remains a
documented manual escape hatch rather than a generated recipe.

## 7. Accepting intentional changes: bless

cbh's blessing re-baselines a series from a commit forward via an append-only
sidecar written into the *history store*. To keep the store single-writer and to
make acceptance reviewable, blessing is expressed as a committed entry — the
benchmark, the attributed commit, and a human reason — that the scheduled job
applies (idempotently) before analyzing. The workflow is therefore: red build →
a reviewed pull request accepting the change → the next scheduled run applies it
and the build returns to green. The accumulated entries are an audit trail of
every deliberate tradeoff.

## 8. Boundaries and caveats

- **Hosted-runner machine-key density.** cbh partitions by a hardware
  fingerprint; a heterogeneous hosted pool can split a series into per-key
  partitions too sparse to analyze. Whether a hosted pool stays dense enough
  depends on its hardware homogeneity; self-hosted or dedicated runners avoid the
  concern.
- **Attribution is coarse under sparse benchmarking.** Benches do not run on
  every commit, so the attributed commit is the first *benchmarked* one after a
  regression and may bundle several changes — an honest range, not always a
  single culprit.
- **The scheduled status is one bit.** It collapses several concurrent
  regressions into one red; GitHub recovers per-regression detail through the
  updated issue, ADO through the build summary (its native notification is
  coarser while already red).
- **Uncalibrated thresholds.** cbh's gating thresholds are defaults rather than
  values calibrated to every consumer's data; pinning the tool version contains
  the resulting risk, and the signal only gates the scheduled build.
- **History horizon.** CI-native artifacts hold a rolling window, not unbounded
  history; there is no anvil-supported durable store, so the horizon is the
  artifact retention.

[cbh]: https://github.com/folo-rs/folo/tree/main/packages/cargo-bench-history
