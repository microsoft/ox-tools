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
you know it is a PR-tier check; if you see `just ox-check-nightly-runtime`, you know it is
nightly-only. This makes "what gets executed" trivially answerable from the group name.

A consequence is that some checks must appear in two groups — one PR group and one nightly
group — when the check should run in both tiers. The two invocations may differ (e.g.
`mutants` runs diff-scoped in PR and full-workspace in nightly) or be identical (e.g. `tests`
runs the same way in both, but the nightly run catches flakes/environmental drift on `main`).

Group recipes follow the pattern `ox-check-<tier>-<group>` (e.g. `ox-check-pr-fast`,
`ox-check-nightly-runtime`). The tier prefix removes the need to pick distinct names for groups
in different tiers and makes the tier of any failing job obvious from its name alone.

### PR tier (3 groups)

| Group              | OS scope           | Purpose                                                                                                              |
|--------------------|--------------------|----------------------------------------------------------------------------------------------------------------------|
| `pr-fast`          | Linux only         | All static analysis: nothing here compiles user tests or examples through to execution. Fast feedback, fail-fast.    |
| `pr-test`          | Linux + Windows    | Code execution: tests (instrumented for coverage), doctests, examples. Coverage reporting is folded in via `cargo llvm-cov nextest`. |
| `pr-mutants`       | Linux only         | Diff-scoped mutation testing on the change in this PR.                                                               |

### Nightly tier (4 groups)

| Group                | OS scope        | Purpose                                                                                                                                |
|----------------------|-----------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `nightly-test`       | Linux + Windows | Re-runs the test suite on `main` (with coverage instrumentation) to catch flakes/environment-dependent failures and to publish a full coverage snapshot of the current `main`. |
| `nightly-advisories` | Linux only      | Re-runs every check whose outcome can change without a commit to this repo: `deny`, `audit`, `aprz` (external databases), `clippy` (lint set evolves with toolchain), `udeps` (uses `cargo +nightly`, which evolves). |
| `nightly-runtime`    | Linux only      | Tests under stricter runtimes that catch UB and timing/threading bugs: `miri`, `careful`. (Both tools are Linux-only.)                  |
| `nightly-exhaustive` | Linux only      | The expensive whole-workspace permutations that don't fit the PR budget: full `cargo mutants`, `cargo-hack --feature-powerset`, and `cargo bench --no-run` plus a single-iteration smoke run per bench target. |

OS-scope is an opinion ox-check ships and the user overrides per-repo through the
backend-specific knobs ([github.md §4](./github.md#4-owned-reusable-workflows) for
`test_os`, [ado.md §4](./ado.md#4-owned-stages-templates) for `linuxPool`/`windowsPool`).
Locally there is no OS matrix; `just ox-check-pr-test` runs against whatever OS the
developer is on. See [design.md §8.3](./design.md#83-cross-os-test-matrices) for the
overall rationale.

The `nightly-exhaustive` group's checks are independent and could in principle live in three
parallel jobs; they're folded into one group because each individually is just one check,
and nightly tolerates the longer wall-clock that serial execution within one job implies.
Repos that want to parallelize them can split the recipe into three group recipes locally.

## 2. Checks by group

The cell format is `cargo invocation (short rationale)`. "Source" cites the surveyed repo
that provided the strongest version of the check.

### `pr-fast`

| Check                          | Invocation                                                | Source |
|--------------------------------|-----------------------------------------------------------|--------|
| `fmt`                          | `cargo fmt --all --check`                                 | all |
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
| `llvm-cov`   | `cargo llvm-cov nextest --workspace --all-features --locked --lcov --output-path target/coverage/lcov.info` ＋ HTML report ＋ enforced minimum threshold. The instrumented `nextest` run produces both the test pass/fail signal and the coverage artifacts in a single pass. | oxidizer, oxidizer-github |
| `doc-test`   | `cargo test --doc --workspace --all-features --locked` (nextest does not run doctests, so this is a separate cargo-test invocation) | oxidizer, oxidizer-github |
| `examples`   | `cargo run --example <name>` for each example target                        | oxidizer, oxidizer-github |

### `pr-mutants`

| Check     | Invocation                                                                 | Source |
|-----------|----------------------------------------------------------------------------|--------|
| `mutants` | `cargo mutants --in-diff <base>…HEAD --no-shuffle --jobs 0` (diff-scoped)  | oxidizer-github |

The PR mode requires a base ref. Locally, the recipe defaults to `origin/main` (or `master`)
and can be overridden via a `BASE_REF` env var; in GitHub Actions the workflow passes
`${{ github.event.pull_request.base.sha }}`; in ADO the template parameter `prBaseRef` is
wired to `System.PullRequest.TargetBranch`.

### `nightly-test`

| Check        | Invocation                                                                  | Source |
|--------------|-----------------------------------------------------------------------------|--------|
| `llvm-cov`   | `cargo llvm-cov nextest --workspace --all-features --locked --lcov --output-path target/llvm-cov/nightly.lcov` | oxidizer, oxidizer-github |
| `doc-test`   | `cargo test --doc --workspace --all-features --locked`                      | oxidizer, oxidizer-github |
| `examples`   | `cargo run --example <name>` for each example target                        | oxidizer, oxidizer-github |

The same checks as `pr-test`, run on `main`. Two purposes: catch flakes/environmental
sensitivities that didn't trip in PR, and publish a full coverage snapshot for the current
state of `main` (the PR `llvm-cov` upload only reflects diffed code; this one reflects the
whole codebase). The CI emitter wires the lcov artifact upload step in the nightly workflow
only.

### `nightly-advisories`

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
is installed on the runner. Re-running these nightly turns "something landed upstream
yesterday" into a tracked failure rather than an invisible regression discovered next time
someone opens an unrelated PR.

### `nightly-runtime`

| Check     | Invocation                                                                          | Source |
|-----------|-------------------------------------------------------------------------------------|--------|
| `miri`    | `cargo +nightly miri nextest run --workspace`                                       | oxidizer, oxidizer-github |
| `careful` | `cargo +nightly careful test --workspace --all-features --locked`                   | oxidizer-github |

### `nightly-exhaustive`

| Check                | Invocation                                                                                                   | Source |
|----------------------|--------------------------------------------------------------------------------------------------------------|--------|
| `mutants-full`       | `cargo mutants --workspace --no-shuffle --jobs 0`                                                            | oxidizer-github |
| `cargo-hack` powerset| `cargo hack --workspace --feature-powerset --depth 2 check`                                                  | oxidizer, oxidizer-github |
| `bench`              | `cargo bench --workspace --all-features --no-run` ＋ a single-iteration smoke benchmark for each bench target | oxidizer |

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

## 4. What nightly does and does not re-run

The rule is simple: **a check belongs in nightly iff its outcome can change without a
commit to this repo.** Re-running everything else nightly would just burn CI time
duplicating PR signal.

What that means concretely:

- **Re-run in nightly** (in addition to PR):
  - `llvm-cov`, `doc-test`, `examples` (in `nightly-test`) — non-determinism, environment
    sensitivity, runner drift can produce flakes that the PR run missed.
  - `deny`, `audit`, `aprz`, `clippy`, `udeps` (in `nightly-advisories`) — see §2.
- **Run only in PR** — checks whose outcome is fully determined by the source tree and
  the pinned tool versions, so re-running on the same `main` commit can't surface anything
  new: `fmt`, `cargo-sort`, `license-headers`, `ensure-no-cyclic-deps`,
  `ensure-no-default-features`, `doc-build`, `readme-check`, `spellcheck`, `pr-title`,
  `semver-check`, `external-types`, diff-scoped `mutants`.
- **Run only in nightly** — the expensive whole-workspace work that can't fit a PR
  budget: `miri`, `careful` (in `nightly-runtime`); full `mutants`,
  `cargo-hack --feature-powerset`, `bench` (in `nightly-exhaustive`).

The single-tier-per-group rule still holds: when a check appears in both tiers it lives in
two different groups (one PR group, one nightly group). Repos that want a
belt-and-suspenders cron run of `just ox-check-pr` on `main` can wire one up in their own
workflow/pipeline file alongside the ox-check composite actions / step templates.

## 5. Impact-scoping check → env-var mapping

The tool uses [`cargo-delta`](https://crates.io/crates/cargo-delta) to skip checks for
unaffected workspace members on PR runs. cargo-delta computes three concentric impact tiers
(`required ⊇ affected ⊇ modified`) and emits each as a string of `--exclude X --exclude Y …`
flags (the workspace complement of the relevant tier), which composes cleanly with `cargo
--workspace`. Each catalog check is tagged with the tier it consumes:

| Env var                       | cargo-delta source                                     | Checks that consume it                               |
|-------------------------------|--------------------------------------------------------|------------------------------------------------------|
| `OX_CHECK_EXCLUDE_NOT_MODIFIED`  | `cargo delta impact -f cargo-excludes --modified`      | clippy, udeps                                        |
| `OX_CHECK_EXCLUDE_NOT_AFFECTED`  | `cargo delta impact -f cargo-excludes --affected`      | llvm-cov, doc-test, examples, miri, careful, semver-check, mutants (diff and full), cargo-hack powerset, bench |
| `OX_CHECK_EXCLUDE_NOT_REQUIRED`  | `cargo delta impact -f cargo-excludes --required`      | doc-build, readme-check, external-types              |

Checks with no per-crate scope ignore the vars: `fmt` (always all files), `pr-title`,
`spellcheck`, `deny`, `audit`, `aprz`, `cargo-sort`, `license-headers`,
`ensure-no-cyclic-deps`, `ensure-no-default-features`. The mapping is hardcoded in the
catalog alongside each check's invocation.

The recipe-side mechanics are in [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars)
(including the `OX_CHECK_IMPACT_SKIP` early-return hint). The CI-side wiring (the
`ox-check-impact` building block, how downstream jobs consume the excludes) is in
[github.md](./github.md#impact-scoping) and [ado.md](./ado.md#impact-scoping).

Trade-off acknowledged: the risk cargo-delta introduces is that a misconfigured analysis
silently skips checks that should have run, leaving "all green" on a PR that actually broke
something. The design mitigates this with: (1) trip-wire patterns in `.delta.toml` that
bias toward full runs whenever config changes; (2) the `skip` flag is advisory only and
the CI wiring never gates whole jobs on it — non-scoping checks (`fmt`, `deny`, `audit`,
`aprz`, `pr-title`, `spellcheck`) always run regardless of impact analysis; (3) nightly
always runs full-workspace, catching anything the PR-scoping missed within 24 hours; and
(4) any repo can disable scoping wholesale by emptying `.delta.toml`'s region.
