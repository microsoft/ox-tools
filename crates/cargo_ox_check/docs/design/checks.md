# Check Catalog

This document defines the opinionated default profile: which checks ship, how they're
grouped, which tier they belong to, and how the tool-version policy works. It is the
canonical source for "what does ox-check actually run?"

See also:

- [design.md](./design.md) for the overall principles and CLI shape.
- [local.md](./local.md) for how the catalog is exposed as `just` recipes.
- [github.md](./github.md) / [ado.md](./ado.md) for how groups map to CI building blocks.

## 1. Groups and tiers

The check catalog is hardcoded in the binary. Each check belongs to one or more *groups*, and
each group belongs to exactly one *tier*. Groups are the unit of CI parallelization (one CI
job per group) and the unit of local invocation through `just` (one `just` recipe per group).
A user (or CI) never has to enumerate individual checks — they operate at the group level.

The **single-tier-per-group** rule is deliberate: if you see `just ox-check-pr-fast` in CI logs,
you know it is a PR-tier check; if you see `just ox-check-scheduled-runtime`, you know it is
scheduled-only. This makes "what gets executed" trivially answerable from the group name.

A consequence is that some checks must appear in two groups — one PR group and one scheduled
group — when the check should run in both tiers. The two invocations may differ (e.g.
`mutants` runs diff-scoped in PR and full-workspace in scheduled) or be identical (e.g. `tests`
runs the same way in both, but the scheduled run catches flakes/environmental drift on `main`).

Group recipes follow the pattern `ox-check-<tier>-<group>` (e.g. `ox-check-pr-fast`,
`ox-check-scheduled-runtime`). The tier prefix removes the need to pick distinct names for groups
in different tiers and makes the tier of any failing job obvious from its name alone.

### PR tier (3 groups)

| Group              | OS scope                              | Purpose                                                                                                              |
|--------------------|---------------------------------------|----------------------------------------------------------------------------------------------------------------------|
| `pr-fast`          | Linux x86_64 + Windows x86_64 + Linux aarch64 + Windows aarch64 (GH) / Linux x86_64 + Windows x86_64 (ADO) | All static analysis (including `udeps` and `semver-check`). Cross-OS because clippy, doc-build, udeps, and semver-check all compile per host target. Text/metadata checks (fmt, license-headers, …) run on every leg too; the redundancy cost is negligible compared to a separate job's setup overhead. `external-types` lives in `scheduled-advisories` instead because it pins a specific nightly rustdoc JSON schema and breaks frequently on toolchain drift. |
| `pr-test`          | Same default as `pr-fast`             | Code execution: tests (instrumented for coverage), doctests, examples. Coverage reporting is folded in via `cargo llvm-cov nextest`. |
| `pr-mutants`       | Linux x86_64 + Windows x86_64 (GH) / Linux x86_64 + Windows x86_64 (ADO) | Diff-scoped mutation testing on the change in this PR. Cross-OS to match `oxidizer`'s policy — mutations on cfg-gated code matter. **x86_64-only**: cargo-mutants currently doesn't build on `aarch64-pc-windows-msvc` (upstream `winapi` crate incompat), and the value of mutation testing on the ARM legs doesn't justify the extra wall-clock. |

### scheduled tier (4 groups)

| Group                | OS scope                  | Purpose                                                                                                                                |
|----------------------|---------------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `scheduled-test`       | Same default as `pr-test` | Re-runs the test suite on `main` (with coverage instrumentation) to catch flakes/environment-dependent failures and to publish a full coverage snapshot of the current `main`. |
| `scheduled-advisories` | Same default as `pr-fast` | Re-runs every check whose outcome can change without a commit to this repo: `deny`, `audit`, `aprz` (external databases), `clippy` (lint set evolves with toolchain), plus `external-types` (which needs nightly rustdoc and is gated to nightly to avoid blocking PRs on JSON schema drift). Cross-OS because clippy compiles per host. |
| `scheduled-runtime`    | Same default as `pr-fast` | Tests under stricter runtimes that catch UB and timing/threading bugs: `miri`, `careful`. Both tools work on every Tier 1 Rust target; the surveyed repos (`oxidizer`, `oxidizer-github`) both run them cross-OS, so ox-check does too. |
| `scheduled-exhaustive` | Linux x86_64 + Windows x86_64 | The expensive whole-workspace permutations that don't fit the PR budget: full `cargo mutants`, `cargo-hack --feature-powerset`, and `cargo bench --no-run` plus a single-iteration smoke run per bench target. Cross-OS to match `oxidizer`'s policy and to give cargo-hack / bench compile coverage for cfg-gated code. **x86_64-only**: same `cargo-mutants` / `winapi` constraint as `pr-mutants`. Adopters who can't afford the full matrix (mutants-full can run for hours per leg) override the matrix in their root workflow / pipeline. |

**Backend asymmetry on ARM coverage.** The GitHub backend ships a four-leg default matrix
(Linux/Windows × x86_64/aarch64) because GH has Microsoft-hosted ARM runners
(`ubuntu-24.04-arm`, `windows-11-arm`). The ADO backend ships a two-leg default
(x86_64 only) because ADO has no hosted ARM agents; adopters with self-hosted ARM pools
extend the stages template themselves. The catalog and recipes are identical across
backends — the asymmetry is purely in the wiring layer's default OS matrix.

OS-scope is an opinion ox-check ships and the user overrides per-repo through the
backend-specific knobs ([github.md §4](./github.md#4-owned-reusable-workflows) for
the per-leg runner-label inputs and forking the workflow when the matrix shape itself
needs to change, [ado.md §4](./ado.md#4-owned-stages-templates) for
`linuxPool`/`windowsPool`).
Locally there is no OS matrix; `just ox-check-pr-test` runs against whatever OS the
developer is on. See [design.md §8.3](./design.md#83-cross-os-test-matrices) for the
overall rationale.

The `scheduled-exhaustive` group's checks are independent and could in principle live in
separate parallel jobs; they're folded into one group because each individually is just
one check, and scheduled tolerates the longer wall-clock that serial execution within one
job implies. Repos that want to parallelize them can split the recipe into separate group
recipes locally.

## 2. Checks by group

The cell format is `cargo invocation (short rationale)`. "Source" cites the surveyed repo
that provided the strongest version of the check.

### `pr-fast`

| Check                          | Invocation                                                | Source |
|--------------------------------|-----------------------------------------------------------|--------|
| `fmt`                          | `cargo +<pinned-nightly> fmt --all --check`               | all |
| `clippy`                       | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | all |
| `cargo-sort`                   | `cargo sort --workspace --check`                          | oxidizer-github |
| `license-headers`              | `cargo heather --workspace`                               | oxidizer (`heather`), oxidizer-github |
| `ensure-no-cyclic-deps`        | `cargo ensure-no-cyclic-deps --workspace`                 | oxidizer-github (sibling crate in `ox-tools-gh`) |
| `ensure-no-default-features`   | `cargo ensure-no-default-features --workspace`            | oxidizer-github |
| `doc-build`                    | `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | oxidizer-github |
| `readme-check`                 | `cargo doc2readme --check` for each crate that opts in (presence of a `[package.metadata.doc2readme]` table) | oxidizer-github |
| `spellcheck`                   | `cargo spellcheck check --code 1`                         | oxidizer-github |
| `pr-title`                     | Conventional-Commits regex applied to the title in the `PR_TITLE` env var, with a fallback to `git log -1 --pretty=%s HEAD` when unset. Written as a `[script("pwsh")]` recipe (the one check that needs scripting; see [design.md §8.3](./design.md#83-cross-os-test-matrices)). The CI emitter sets `PR_TITLE` on the pr-fast job: GitHub Actions reads `${{ github.event.pull_request.title }}`; ADO reads `$(System.PullRequest.Title)`. Local `just ox-check-pr-fast` works without setup via the git fallback. | oxidizer-github |
| `deny`                         | `cargo deny check`                                        | all |
| `audit`                        | `cargo audit`                                             | oxidizer |
| `udeps`                        | `cargo +nightly udeps --workspace --all-targets --all-features` | oxidizer, oxidizer-github |
| `semver-check`                 | `cargo semver-checks --workspace`                         | oxidizer-github |
| `external-types`               | `cargo check-external-types --workspace`                  | oxidizer-github |
| `aprz`                         | `cargo aprz check` — third-party risk analysis published on crates.io | oxidizer |

### `pr-test`

| Check        | Invocation                                                                  | Source |
|--------------|-----------------------------------------------------------------------------|--------|
| `llvm-cov`   | Three steps from one instrumented `cargo llvm-cov nextest --no-report` run: `report --lcov` → `target/coverage/lcov.info`, `report --cobertura` → `target/coverage/cobertura.xml`, `report --html` → `target/coverage/html/` (local viewer). The nextest run produces the test pass/fail signal; the three `report` invocations re-render the cached `.profraw` data in each format without re-running tests. lcov feeds Codecov on GitHub; cobertura feeds `PublishCodeCoverageResults@2` on ADO; HTML is purely a local affordance. No threshold enforcement at the check level. | oxidizer, oxidizer-github |
| `doc-test`   | `cargo test --doc --workspace --all-features --locked` (nextest does not run doctests, so this is a separate cargo-test invocation) | oxidizer, oxidizer-github |
| `examples`   | `cargo build --workspace --examples --all-features --locked` — verifies that example targets compile. Running each example is intentionally not part of the check (examples are not test scaffolding; their runtime behavior isn't part of what we gate on). | oxidizer, oxidizer-github |

### `pr-mutants`

| Check     | Invocation                                                                 | Source |
|-----------|----------------------------------------------------------------------------|--------|
| `mutants` | `cargo mutants --in-diff <base>…HEAD --no-shuffle --jobs 0` (diff-scoped)  | oxidizer-github |

The PR mode requires a base ref. Locally, the recipe resolves `BASE_REF` (if set), then
`origin/main`, then `origin/master`, then errors out. In GitHub Actions the workflow
passes `${{ github.event.pull_request.base.sha }}`; in ADO the impact step exports
`$(System.PullRequest.TargetBranch)` as `BASE_REF` and the recipe picks it up.

### `scheduled-test`

Same three checks as `pr-test` — `llvm-cov`, `doc-test`, `examples` — and the same
recipe invocations, with the same output paths (`target/coverage/lcov.info` and
`target/coverage/cobertura.xml`). The recipe is shared between tiers; only the CI
wiring around it changes (PR uploads lcov to Codecov / cobertura to ADO from each
PR run; scheduled does the same against `main` plus flags the upload as `scheduled` in
Codecov so the two streams stay distinguishable in the UI). Two purposes for re-running
on scheduled: catch flakes/environmental sensitivities that didn't trip in PR, and
publish a full-coverage snapshot for the current state of `main`.

### `scheduled-advisories`

| Check    | Invocation                                                          | Source |
|----------|---------------------------------------------------------------------|--------|
| `deny`   | `cargo deny check`                                                  | all |
| `audit`  | `cargo audit`                                                       | oxidizer |
| `aprz`   | `cargo aprz check`                                                  | oxidizer |
| `clippy` | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | all |
| `udeps`  | `cargo +nightly udeps --workspace --all-targets --all-features`     | oxidizer, oxidizer-github |

These checks share a property: their outcome can change without a commit to this repo.
`deny`/`audit`/`aprz` consult external databases (RustSec advisory DB, license registries,
Azure risk indices). `clippy` reflects whatever lint set ships with the currently-installed
toolchain — even when `rust-toolchain.toml` is pinned, repos using floating channels
(`stable`, or msrustup channel pointers like `ms-prod-1.93`) can pick up new lints when the
pointer is bumped upstream. `udeps` runs on `cargo +nightly` and reflects whatever nightly
is installed on the runner. Re-running these on the scheduled tier turns "something landed upstream
yesterday" into a tracked failure rather than an invisible regression discovered next time
someone opens an unrelated PR.

### `scheduled-runtime`

| Check     | Invocation                                                                          | Source |
|-----------|-------------------------------------------------------------------------------------|--------|
| `miri`    | `cargo +nightly miri nextest run --workspace`                                       | oxidizer, oxidizer-github |
| `careful` | `cargo +nightly careful test --workspace --all-features --locked`                   | oxidizer-github |

### `scheduled-exhaustive`

| Check                 | Invocation                                                                                                   | Source |
|-----------------------|--------------------------------------------------------------------------------------------------------------|--------|
| `mutants-full`        | `cargo mutants --workspace --no-shuffle --jobs 0`                                                            | oxidizer-github, oxidizer (sharded cross-OS) |
| `cargo-hack` powerset | `cargo hack --workspace --feature-powerset --depth 2 check`                                                  | oxidizer, oxidizer-github |
| `bench`               | `cargo bench --workspace --all-features --no-run` ＋ a single-iteration smoke benchmark for each bench target | oxidizer |

## 3. Per-check vs grouped CI execution

Each *group* is one CI job. Within a job, the checks belonging to the group run sequentially
as the `just` recipe defines them. A failure in any check fails the group; the per-check log
lines are visible in the job log but the CI surface (the green/red pill in the PR view) is
per-group.

This is the deliberate middle ground between "one giant CI step running `just ox-check-pr`"
(loses all per-check structure, one red X for any failure) and "twenty-five individual CI
steps" (unmaintainable YAML, fragile, and the tool would have to re-emit the workflow file
every time the catalog changes). Groups are stable units of meaning the user can talk about;
checks are implementation details that can churn.

## 4. What scheduled does and does not re-run

The rule is simple: **a check belongs in scheduled iff its outcome can change without a
commit to this repo.** Re-running everything else on the scheduled tier would just burn CI time
duplicating PR signal.

What that means concretely:

- **Re-run in scheduled** (in addition to PR):
  - `llvm-cov`, `doc-test`, `examples` (in `scheduled-test`) — non-determinism, environment
    sensitivity, runner drift can produce flakes that the PR run missed.
  - `deny`, `audit`, `aprz`, `clippy`, `udeps` (in `scheduled-advisories`) — see §2.
- **Run only in PR** — checks whose outcome is fully determined by the source tree and
  the pinned tool versions, so re-running on the same `main` commit can't surface anything
  new: `fmt`, `cargo-sort`, `license-headers`, `ensure-no-cyclic-deps`,
  `ensure-no-default-features`, `doc-build`, `readme-check`, `spellcheck`, `pr-title`,
  `semver-check`, diff-scoped `mutants`.
- **Run only in scheduled** — the expensive whole-workspace work that can't fit a PR
  budget: `miri`, `careful` (in `scheduled-runtime`); full `mutants`,
  `cargo-hack --feature-powerset`, `bench` (in `scheduled-exhaustive`).
  Also `external-types` (in `scheduled-advisories`) — it requires nightly rustdoc
  + compiles cargo-check-external-types from source against nightly + runs
  rustdoc JSON generation per package, which together exceeded the PR-tier
  time budget when dogfooded on ox-tools-gh.

The single-tier-per-group rule still holds: when a check appears in both tiers it lives in
two different groups (one PR group, one scheduled group). Repos that want a
belt-and-suspenders cron run of `just ox-check-pr` on `main` can wire one up in their own
workflow/pipeline file alongside the ox-check composite actions / step templates.

## 5. Impact-scoping check → env-var mapping

The tool uses [`cargo-delta`](https://crates.io/crates/cargo-delta) to skip checks for
unaffected workspace members on PR runs. cargo-delta computes three concentric impact tiers
(`required ⊇ affected ⊇ modified`) and emits each as a list of crate names. The
`ox-check-impact` building block formats each tier into a pre-built `--package X --package Y`
string (or the literal sentinel `--skip` when the tier is empty), publishes the result as
`OX_CHECK_INCLUDE_MODIFIED`, `OX_CHECK_INCLUDE_AFFECTED`, and `OX_CHECK_INCLUDE_REQUIRED`
env vars, and the recipes in `checks.just` consume them.

Each catalog check is tagged with one of four buckets:

| Bucket    | Env var consumed              | Behavior in CI                                                              | Behavior locally (env unset)        |
|-----------|-------------------------------|-----------------------------------------------------------------------------|--------------------------------------|
| modified  | `OX_CHECK_INCLUDE_MODIFIED`   | If `--skip`: exit 0. Otherwise run unconditionally (tool is workspace-wide). | Run unconditionally.                 |
| affected  | `OX_CHECK_INCLUDE_AFFECTED`   | If `--skip`: exit 0. Otherwise splice the value into the cargo invocation.   | Default to `--workspace`.            |
| required  | `OX_CHECK_INCLUDE_REQUIRED`   | If `--skip`: exit 0. Otherwise splice the value into the cargo invocation.   | Default to `--workspace`.            |
| unscoped  | *(none)*                       | Always run.                                                                  | Always run.                          |

Bucket assignments per check:

| Bucket    | Checks                                                                                                                |
|-----------|-----------------------------------------------------------------------------------------------------------------------|
| modified  | `fmt`, `cargo-sort`, `license-headers`, `ensure-no-cyclic-deps`, `ensure-no-default-features`, `readme-check`, `spellcheck` |
| affected  | `clippy`*, `llvm-cov`, `doc-test`, `examples`, `mutants` (diff and full), `miri`, `careful`, `semver-check`, `external-types`, `bench` |
| required  | `doc-build`, `udeps`, `cargo-hack` (feature powerset)                                                                  |
| unscoped  | `pr-title`, `deny`, `audit`, `aprz`, `mutants-full`                                                                    |

\* cargo-delta's README recommends `clippy` with the modified tier. ox-check deliberately
runs it on the affected set instead: a change in a crate's API can introduce clippy lints
(trait-bound mismatches, obviously-truthy-condition warnings keying off changed types) in a
dependent crate, so downstream rev-deps need to lint too. The cost is small — clippy is
incremental — and the recall benefit avoids a class of merge surprises.

`required` is `affected ∪ workspace-internal transitive deps`, not "the whole workspace".
For a small PR it can still be much narrower than `--workspace`. It is used for tools
whose correctness resolves through the dep graph: `cargo doc` (intra-doc links walk into
deps), `cargo udeps` (unused-deps detection needs the resolved graph), `cargo hack
--feature-powerset` (feature combinations cascade through dep features).

`unscoped` is for checks that have nothing to do with workspace-member identity:
`deny`/`audit` read `Cargo.lock`, `pr-title` reads PR metadata, `aprz` consults an
external risk DB. These ignore the env vars and always run.

The sentinel `--skip` is a magic string that cannot be a valid cargo argument, so there
is no collision with real package names. Recipes test for it with
`[ "$VAR" = "--skip" ]` and exit 0 to keep the CI job green while signalling that
nothing in that tier needed to run.

The recipe-side mechanics are in
[local.md §4](./local.md#4-impact-scoping-pass-through-env-vars). The CI-side wiring (the
`ox-check-impact` building block, how downstream jobs consume the include vars) is in
[github.md](./github.md#impact-scoping) and [ado.md](./ado.md#impact-scoping).

Trade-off acknowledged: the risk cargo-delta introduces is that a misconfigured analysis
silently skips checks that should have run, leaving "all green" on a PR that actually broke
something. The design mitigates this with: (1) trip-wire patterns in `.delta.toml` that
bias toward full runs whenever config changes; (2) `unscoped` checks (`deny`, `audit`,
`aprz`, `pr-title`, `mutants-full`) always run regardless of impact analysis;
(3) scheduled always runs full-workspace, catching anything the PR-scoping missed within 24
hours;
