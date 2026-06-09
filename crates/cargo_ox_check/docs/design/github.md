# GitHub Actions Integration

This document describes what `cargo ox-check update --backend github` emits for GitHub
Actions, and how a repo wires those files into its own CI.

ox-check emits three layers, all owned by ox-check with the standard owned-file flow (edit →
dirty → `.ox-check-proposed` sibling on next update). The split is by what users actually
need to change:

1. **Root workflows** (`ox-check-pr.yml`, `ox-check-nightly.yml` at `.github/workflows/`).
   Triggers, `permissions`, runner choice, any secret pass-through. ox-check ships an
   opinionated default; users who need to customize edit in place and accept the
   proposal-on-update flow.
2. **Reusable workflows** (`ox-check-pr-impl.yml`, `ox-check-nightly-impl.yml`), containing the
   impact job and the per-group jobs with all the `needs.impact.outputs.*` plumbing.
   These change when ox-check's groups or impact wiring evolve; most users won't ever edit
   them.
3. **Per-group composite actions** (`.github/actions/ox-check-*/`). Each is a multi-step
   composite that runs setup + the matching `just ox-check-<tier>-<group>` recipe.

See also:

- [design.md §6](./design.md#6-repo-layout) for the file-category model.
- [checks.md](./checks.md) for what each group runs.
- [local.md](./local.md) for the `just` recipes the composite actions invoke.
- [ado.md](./ado.md) for the ADO counterpart.

## 1. Why three layers

- **Frequently-changing wiring** (group set, impact computation, fan-out, `needs:` graph)
  lives in the reusable workflows. Updates apply automatically; users don't have to merge
  changes.
- **Per-repo customization** (triggers, permissions, runner pool, secret scoping) lives
  in the root workflows. Users who customize them accept the cost of merging the
  `.ox-check-proposed` sibling when the ox-check defaults evolve — which is rare, since the
  root workflow is intentionally minimal.
- The reusable-workflow seam ([`workflow_call`][1]) is GitHub's first-class mechanism for
  exactly this: a workflow can call another workflow in the same repo, passing inputs and
  secrets. We use it so the root workflow stays ~10 lines.

[1]: https://docs.github.com/en/actions/sharing-automations/reusing-workflows

## 2. Emitted artifacts

```text
.github/
├── actions/
│   ├── ox-check-setup/action.yml         owned   (install just + catalog tools)
│   ├── ox-check-impact/action.yml        owned   (cargo-delta; omitted if .delta.toml disabled)
│   ├── ox-check-pr-fast/action.yml       owned   (one composite action per group)
│   ├── ox-check-pr-test/action.yml       owned
│   ├── ox-check-pr-mutants/action.yml    owned
│   ├── ox-check-nightly-test/action.yml  owned
│   ├── ox-check-nightly-advisories/action.yml  owned
│   ├── ox-check-nightly-runtime/action.yml     owned
│   └── ox-check-nightly-exhaustive/action.yml  owned
└── workflows/
    ├── ox-check-pr-impl.yml              owned   (reusable workflow doing the wiring)
    ├── ox-check-nightly-impl.yml         owned   (reusable workflow for nightly)
    ├── ox-check-pr.yml                   owned   (root workflow; triggers/permissions/runner)
    └── ox-check-nightly.yml              owned
```

All files are regular owned files tracked by the sidecar `.ox-check.lock` manifest
(no in-file checksum line; see [updates.md §1](./updates.md#1-the-manifest)). Users
who customize the root workflow take ownership through the standard dirty-file
flow.

## 3. Root workflows

The default `ox-check-pr.yml` ox-check emits is the minimum needed to call the reusable
workflow:

```yaml
# .github/workflows/ox-check-pr.yml
name: ox-check-pr
on:
  pull_request: {}
  merge_group: {}
permissions:
  contents: read
jobs:
  ox-check:
    uses: ./.github/workflows/ox-check-pr-impl.yml
```

The nightly root workflow adds a schedule and `workflow_dispatch`:

```yaml
# .github/workflows/ox-check-nightly.yml
name: ox-check-nightly
on:
  schedule: [{ cron: '0 6 * * *' }]
  workflow_dispatch: {}
permissions:
  contents: read
jobs:
  ox-check:
    uses: ./.github/workflows/ox-check-nightly-impl.yml
```

Common edits users make to the root workflow (these flip the file to "dirty" and produce
a `.ox-check-proposed` sibling on the next `update` — see
[updates.md §5](./updates.md#5-the-decision-algorithm)):

- **Self-hosted runners**: pass `with: { linux_runner: 'self-hosted-rust', windows_runner: 'self-hosted-rust-win', linux_arm_runner: 'self-hosted-rust-arm', windows_arm_runner: 'self-hosted-rust-win-arm' }`
- **Different OS matrix scope**: not a workflow input. The matrices are part of the
  workflow's identity — adopters who want to add macOS, drop ARM, or otherwise change
  the OS axis fork the emitted `ox-check-pr-impl.yml` / `ox-check-nightly-impl.yml`
  in their own repo and dirty-file-flow takes over from there. Surveyed-repo precedent
  (`oxidizer-github`, `oxidizer`) does the same.
  to the reusable workflow. The runner inputs are CSV-keyed by OS (see §4 for the
  exact contract).
- **Different OS matrix scope**: not a workflow input. The matrices are part of the
  workflow's identity — adopters who want to add macOS, drop ARM, or otherwise change
  the OS axis fork the emitted `ox-check-pr-impl.yml` / `ox-check-nightly-impl.yml`
  in their own repo and dirty-file-flow takes over from there. Surveyed-repo precedent
  (`oxidizer-github`, `oxidizer`) does the same.
  (`linux`/`windows`/`macos`), not runner labels — runner labels come from the separate
  `*_runner` inputs.
- **Different schedule** for nightly.
- **Path filters** to skip the workflow on docs-only PRs (though ox-check's
  `cargo delta impact` step already produces a `--skip` sentinel for the include lists
  when nothing relevant changed).

ox-check ships two defaults in the root workflow that adopters typically keep but can
remove if they have specific reasons:

- `concurrency: { group: ox-check-pr-${{ github.head_ref || github.ref }}, cancel-in-progress: true }`
  on `ox-check-pr.yml`. Prevents two ox-check runs from racing on the same PR
  branch — the newer push cancels the older. Removing it costs CI minutes but
  is otherwise harmless.
- `secrets: inherit` on the `ox-check:` job. Forwards the calling repo's
  secrets (notably `CODECOV_TOKEN`) into the reusable workflow without each
  adopter having to enumerate them. Removing it disables Codecov uploads
  for private repos but doesn't affect anything else.

## 4. Owned reusable workflows

`ox-check-pr-impl.yml` is where the wiring lives. Every per-group composite action takes
the same three impact-exclude inputs unconditionally; which ones a group's checks
actually consume is the catalog's concern, not the wiring layer's. Moving a check
between groups never changes the reusable workflow.

Approximate shape (ox-check writes this verbatim; users never edit it):

```yaml
# .github/workflows/ox-check-pr-impl.yml   (owned by cargo-ox-check)
on:
  workflow_call:
    inputs:
      linux_runner:       { type: string, default: ubuntu-latest }
      windows_runner:     { type: string, default: windows-latest }
      linux_arm_runner:   { type: string, default: ubuntu-24.04-arm }
      windows_arm_runner: { type: string, default: windows-11-arm }

jobs:
  impact:
    runs-on: ${{ inputs.linux_runner }}
    outputs:
      include_modified: ${{ steps.delta.outputs.include_modified }}
      include_affected: ${{ steps.delta.outputs.include_affected }}
      include_required: ${{ steps.delta.outputs.include_required }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - id: delta
        uses: ./.github/actions/ox-check-impact

  pr-fast:
    needs: impact
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows, linux-arm, windows-arm]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-check-pr-fast
        with:
          include_modified: ${{ needs.impact.outputs.include_modified }}
          include_affected: ${{ needs.impact.outputs.include_affected }}
          include_required: ${{ needs.impact.outputs.include_required }}
        env:
          PR_TITLE: ${{ github.event.pull_request.title }}

  pr-test:
    needs: impact
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows, linux-arm, windows-arm]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-check-pr-test
        with:
          include_modified: ${{ needs.impact.outputs.include_modified }}
          include_affected: ${{ needs.impact.outputs.include_affected }}
          include_required: ${{ needs.impact.outputs.include_required }}

  pr-mutants:
    # x86_64 only — cargo-mutants doesn't build on aarch64-pc-windows-msvc.
    needs: impact
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner || inputs.windows_runner }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - uses: ./.github/actions/ox-check-pr-mutants
        with:
          include_modified: ${{ needs.impact.outputs.include_modified }}
          include_affected: ${{ needs.impact.outputs.include_affected }}
          include_required: ${{ needs.impact.outputs.include_required }}
        env:
          BASE_REF: ${{ github.event.pull_request.base.sha }}
```

Every multi-OS job hardcodes its OS axis as an inline YAML array. Per-leg runner
*labels* are inputs (so adopters can swap in self-hosted runners), but the OS axis
itself is part of the workflow's identity. Adopters who need a different shape (add
macOS, drop ARM, mix in exotic targets) fork the reusable workflow and let
dirty-file-flow take over. The previously-considered `fromJSON(inputs.X)` pattern
was rejected because it added a silent failure mode (mis-formatted inputs produced
empty matrices that GitHub Actions silently treats as "no legs to run") without
meaningfully expanding what adopters could customize — anyone who wants to change
the OS axis is almost certainly making other changes too.

The wiring never gates whole jobs on impact output. Each group always runs; recipes
inside the group decide whether a given check no-ops, by testing for the literal sentinel
`--skip` in the relevant include var. This matters because unscoped checks (`fmt`, `deny`,
`audit`, `aprz`, `pr-title`, `mutants-full`) must run on every PR, including docs-only
PRs where every tier comes back `--skip`. See
[local.md §4](./local.md#4-impact-scoping-pass-through-env-vars) for the recipe-side
contract.

The nightly reusable workflow is simpler — it omits the `impact` job and runs each group
full-workspace. The include inputs default to empty strings, so recipes fall through to
their local-default behavior (`--workspace`):

```yaml
# .github/workflows/ox-check-nightly-impl.yml  (owned)
on:
  workflow_call:
    inputs:
      linux_runner:       { type: string, default: ubuntu-latest }
      windows_runner:     { type: string, default: windows-latest }
      linux_arm_runner:   { type: string, default: ubuntu-24.04-arm }
      windows_arm_runner: { type: string, default: windows-11-arm }
jobs:
  nightly-test:
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows, linux-arm, windows-arm]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}
    steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-check-nightly-test } ]
  nightly-advisories:
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows, linux-arm, windows-arm]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}
    steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-check-nightly-advisories } ]
  nightly-runtime:
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows, linux-arm, windows-arm]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}
    steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-check-nightly-runtime } ]
  nightly-exhaustive:
    # x86_64 only — cargo-mutants constraint.
    strategy:
      fail-fast: false
      matrix:
        os: [linux, windows]
    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner || inputs.windows_runner }}
    steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-check-nightly-exhaustive } ]
```

Nightly composite actions don't receive any `include_*` inputs at all — their inputs
default to empty strings (recipes default to `--workspace`) and the reusable workflow
omits the passthrough. Threading them through is purely a PR-tier optimization;
nightly never benefits.

If `.delta.toml`'s managed region is emptied
([updates.md §opt-out](./updates.md#6-opting-out-in-file-stubs)),
`cargo delta impact` runs with its own defaults — the file is optional configuration, not
a feature gate — and the `impact` job still emits include lists that recipes interpret
normally. The user has opted out of *ox-check's curated cargo-delta config*, not out of
impact scoping itself.

The reusable workflow declares a small input set so the root workflow can pass overrides:

| Input                | Type   | Default              | Meaning                                                |
|----------------------|--------|----------------------|--------------------------------------------------------|
| `linux_runner`       | string | `ubuntu-latest`      | Runner label for x86_64 Linux jobs and the single-leg `impact` job. |
| `windows_runner`     | string | `windows-latest`     | Runner label for x86_64 Windows jobs.                  |
| `linux_arm_runner`   | string | `ubuntu-24.04-arm`   | Runner label for aarch64 Linux jobs.                   |
| `windows_arm_runner` | string | `windows-11-arm`     | Runner label for aarch64 Windows jobs.                 |

The input surface is intentionally narrow: only per-leg *runner labels* are exposed,
because swapping in self-hosted runners is the one common need that doesn't require
otherwise touching the workflow. The OS matrix shape (which legs run) is fixed in the
workflow source — see the discussion under the PR snippet above.

The reusable workflows also declare an optional `workflow_call` secret
`CODECOV_TOKEN`. See §10 (Coverage upload) for how it's used.

We deliberately keep this input surface minimal. Anything more elaborate (e.g.
per-job runner overrides) lives in the user's own workflow, which can compose its own
`uses:`-of-reusable-workflow shape.

## 5. Per-group composite actions

Each per-group composite action has the **same** uniform input surface — the three
impact-include variables plus a per-action handful of PR-context strings. This means
the reusable workflow doesn't need to know which include vars a group's checks consume;
it threads all three to every action. Moving a check between groups (or between
buckets) is a pure catalog change.

```yaml
# .github/actions/ox-check-pr-fast/action.yml  (owned)
name: ox-check-pr-fast
description: ox-check PR fast group
inputs:
  pr_title:
    description: PR title for the pr-title check.
    required: false
    default: ""
  include_modified:
    description: |
      Pre-formatted --package args from ox-check-impact for the modified
      tier, or "--skip" when the modified set is empty. Empty string =
      local invocation; recipes default to --workspace.
    required: false
    default: ""
  include_affected:
    description: Same shape as include_modified, for the affected tier.
    required: false
    default: ""
  include_required:
    description: Same shape as include_modified, for the required tier.
    required: false
    default: ""
runs:
  using: composite
  steps:
    - uses: ./.github/actions/ox-check-setup
    - shell: bash
      env:
        PR_TITLE: ${{ inputs.pr_title }}
        OX_CHECK_INCLUDE_MODIFIED: ${{ inputs.include_modified }}
        OX_CHECK_INCLUDE_AFFECTED: ${{ inputs.include_affected }}
        OX_CHECK_INCLUDE_REQUIRED: ${{ inputs.include_required }}
      run: just ox-check-pr-fast
```

Uniform input set on every per-group composite action:

| Input              | Default | Notes                                                                                                                                  |
|--------------------|---------|----------------------------------------------------------------------------------------------------------------------------------------|
| `include_modified` | `""`    | Forwarded as `OX_CHECK_INCLUDE_MODIFIED`. `--skip` → recipe exits 0. Empty → recipe defaults to `--workspace`.                          |
| `include_affected` | `""`    | Forwarded as `OX_CHECK_INCLUDE_AFFECTED`. Same semantics.                                                                              |
| `include_required` | `""`    | Forwarded as `OX_CHECK_INCLUDE_REQUIRED`. Same semantics.                                                                              |

Per-action additions (only where the action consumes PR-context strings the recipe needs):

| Action                       | Extra inputs                                                            |
|------------------------------|-------------------------------------------------------------------------|
| `ox-check-pr-fast`              | `pr_title`                                                              |
| `ox-check-pr-mutants`           | `base_ref`                                                              |
| `ox-check-pr-test`              | —                                                                       |
| `ox-check-nightly-*`            | —                                                                       |

The recipes themselves consume only the env vars they need; the catalog records the
mapping (see [checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping)).
Threading all three to every action costs a few lines per composite but is the right
separation: wiring is about "which jobs depend on impact and feed it forward", not about
"which check needs which env var."

These actions are consumed primarily by ox-check's own reusable workflow. Users who want to
plug individual groups into an unrelated workflow can `uses:` them directly.

### `ox-check-setup`

`ox-check-setup` installs `just` (`cargo install just --locked --version >=<min>`) and runs
`just ox-check-tools-install-missing`. Does not install Rust; expects `cargo` on PATH (see
§7). `ox-check-impact` is described in §6 below.

## 6. Impact scoping

`.github/actions/ox-check-impact/action.yml` is a composite action with input `base_ref`. It
runs:

1. `cargo install --locked cargo-delta`.
2. `cargo delta impact --base $base_ref --format json` once, capturing the JSON tier
   sets in a single invocation.
3. For each of the three tiers (`modified`, `affected`, `required`), format the crate
   list into a pre-built `--package X --package Y …` string, or emit the sentinel
   `--skip` when the tier is empty.

Outputs:

| Output             | Meaning                                                                                                                                                                |
|--------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `include_modified` | `--package X --package Y …` for cargo-delta's `modified` tier, or `--skip` when empty.                                                                                  |
| `include_affected` | Same shape, for the `affected` tier (modified ∪ workspace rev-deps).                                                                                                    |
| `include_required` | Same shape, for the `required` tier (affected ∪ workspace-internal transitive deps).                                                                                    |

The wiring never gates jobs on these outputs — every job runs regardless of `--skip`
status. Per-recipe interpretation lives in the recipes themselves (see [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars)).
This is intentional: unscoped checks (`deny`, `audit`, `aprz`, `pr-title`,
`mutants-full`) must run on every PR even when every tier reports `--skip`.

The check → bucket mapping is in
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping).

## 7. Rust toolchain

ox-check does not install Rust on GitHub. The composite actions assume `cargo` is on PATH.
GH-hosted runners ship with a recent stable Rust and `rustup` pre-installed; if your
`rust-toolchain.toml` pins a different channel, the first `cargo` invocation in a job
triggers `rustup` to download the pinned toolchain. For a published stable channel this
typically takes 10–30 seconds on Linux (somewhat longer on Windows and longer still for
nightly with components). The auto-install runs once per job and is not cached across
jobs by ox-check — `~/.rustup` has high invalidation churn and the install cost is small
relative to the cached cargo registry / `target/` paths (§8). Repos that want to skip
even this per-job overhead can add their own toolchain-install step (e.g.
`dtolnay/rust-toolchain@stable`) before the ox-check composite action runs.

On self-hosted runners or pre-baked images without rustup, the user adds a Rust install
step to their root workflow before the `uses:` of the reusable workflow:

```yaml
jobs:
  ox-check:
    uses: ./.github/workflows/ox-check-pr-impl.yml
    # Self-hosted? Add a setup workflow that runs first and uploads
    # toolchain to a shared cache, then reference it here.
```

Since reusable workflows can't accept "previous step" handoff, self-hosted users usually
forgo the reusable-workflow shape and write a single workflow that calls the composite
actions directly. ox-check's composite actions are exposed for that use case.

`_ox-check-require` (invoked by every check recipe) validates the installed `rustc` against
the catalog minimum at recipe time; missing or below-minimum `rustc` produces a clean
failure message.

## 8. Caching

The `ox-check-setup` composite action computes a cache key from: OS, rustc version (read
from `rust-toolchain.toml`), `Cargo.lock`, `.cargo/config.toml`, and the binary's
embedded catalog hash. Uses `actions/cache` natively. `CARGO_HOME` is pinned to a
workspace-scratch location to keep cache scoping predictable.

The cache covers:

- The `cargo install`-ed tools from `ox-check-tools-install-missing`.
- The `target/` directory (per ox-check recipe; a per-recipe cache scope means a `pr-test`
  cache hit doesn't have to wait on a `pr-fast` cache miss).

## 9. Security

The composite actions do nothing privileged on their own — they just install tools and
invoke `just`. The reusable workflow propagates only what the root workflow passes (and
only the inputs explicitly declared).

Recommended root workflow shape:

- `permissions: contents: read` at the workflow level. ox-check's default ships with
  this.
- No `pull-requests: write` (the PR-title check only needs the title from the event
  payload, which is already in `${{ github.event.pull_request.title }}`).
- Nightly secrets, if any, live on `ox-check-nightly.yml` only — never on `ox-check-pr.yml`.
- All cargo-tool installs done by `ox-check-setup` use `--locked`. No `cargo-binstall`.

## 10. Coverage upload

After `pr-test` (and `nightly-test`) runs the `ox-check-llvm-cov` recipe, the reusable
workflow uploads the resulting `target/coverage/lcov.info` to Codecov on the Linux leg
of the matrix only (the other legs produce equivalent data; uploading once avoids
double-counting in the Codecov UI).

The upload step:

```yaml
- name: Upload coverage to Codecov
  if: matrix.os == 'linux' && needs.impact.outputs.skip != 'true'
  uses: codecov/codecov-action@v5
  with:
    files: target/coverage/lcov.info
    token: ${{ secrets.CODECOV_TOKEN }}
    fail_ci_if_error: false
```

The reusable workflow declares `CODECOV_TOKEN` as an optional `workflow_call` secret;
the root workflow's default `secrets: inherit` (see §3) forwards it without each adopter
having to enumerate. Public repos with Codecov OIDC trust configured need no token at
all; private repos set `CODECOV_TOKEN` at the repo level. `fail_ci_if_error: false`
keeps the build green when Codecov is unreachable (typical for internal repos that
can't reach `codecov.io`).

On the nightly upload the step additionally passes `flags: nightly` so the two streams
(PR vs nightly) stay distinguishable in the Codecov UI.

ox-check does not gate the PR on coverage. The lcov upload is informational; Codecov's
own status check is the gating layer when the adopter wants one (configured in Codecov,
visible as a separate required check in branch protection).
