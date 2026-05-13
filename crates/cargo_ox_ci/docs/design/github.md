# GitHub Actions Integration

This document describes what `cargo ox-ci update --backend github|both` emits for GitHub
Actions, and how a repo wires those files into its own CI.

ox-ci emits three layers, all owned by ox-ci with the standard owned-file flow (edit →
dirty → `.ox-ci-proposed` sibling on next update). The split is by what users actually
need to change:

1. **Root workflows** (`ox-ci-pr.yml`, `ox-ci-nightly.yml` at `.github/workflows/`).
   Triggers, `permissions`, runner choice, any secret pass-through. ox-ci ships an
   opinionated default; users who need to customize edit in place and accept the
   proposal-on-update flow.
2. **Reusable workflows** (`ox-ci-pr-impl.yml`, `ox-ci-nightly-impl.yml`), containing the
   impact job and the per-group jobs with all the `needs.impact.outputs.*` plumbing.
   These change when ox-ci's groups or impact wiring evolve; most users won't ever edit
   them.
3. **Per-group composite actions** (`.github/actions/ox-ci-*/`). Each is a multi-step
   composite that runs setup + the matching `just ox-ci-<tier>-<group>` recipe.

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
  `.ox-ci-proposed` sibling when the ox-ci defaults evolve — which is rare, since the
  root workflow is intentionally minimal.
- The reusable-workflow seam ([`workflow_call`][1]) is GitHub's first-class mechanism for
  exactly this: a workflow can call another workflow in the same repo, passing inputs and
  secrets. We use it so the root workflow stays ~10 lines.

[1]: https://docs.github.com/en/actions/sharing-automations/reusing-workflows

## 2. Emitted artifacts

```text
.github/
├── actions/
│   ├── ox-ci-setup/action.yml         owned   (install just + catalog tools)
│   ├── ox-ci-impact/action.yml        owned   (cargo-delta; omitted if .delta.toml disabled)
│   ├── ox-ci-pr-fast/action.yml       owned   (one composite action per group)
│   ├── ox-ci-pr-test/action.yml       owned
│   ├── ox-ci-pr-mutants/action.yml    owned
│   ├── ox-ci-nightly-test/action.yml  owned
│   ├── ox-ci-nightly-advisories/action.yml  owned
│   ├── ox-ci-nightly-runtime/action.yml     owned
│   └── ox-ci-nightly-exhaustive/action.yml  owned
└── workflows/
    ├── ox-ci-pr-impl.yml              owned   (reusable workflow doing the wiring)
    ├── ox-ci-nightly-impl.yml         owned   (reusable workflow for nightly)
    ├── ox-ci-pr.yml                   owned   (root workflow; triggers/permissions/runner)
    └── ox-ci-nightly.yml              owned
```

All files are regular owned files (carry an `ox-ci-checksum` first line, governed by
[updates.md §5](./updates.md#5-the-decision-algorithm)). Users who customize the root
workflow take ownership through the standard dirty-file flow.

## 3. Root workflows

The default `ox-ci-pr.yml` ox-ci emits is the minimum needed to call the reusable
workflow:

```yaml
# .github/workflows/ox-ci-pr.yml
name: ox-ci-pr
on:
  pull_request: {}
  merge_group: {}
permissions:
  contents: read
jobs:
  ox-ci:
    uses: ./.github/workflows/ox-ci-pr-impl.yml
```

The nightly root workflow adds a schedule and `workflow_dispatch`:

```yaml
# .github/workflows/ox-ci-nightly.yml
name: ox-ci-nightly
on:
  schedule: [{ cron: '0 6 * * *' }]
  workflow_dispatch: {}
permissions:
  contents: read
jobs:
  ox-ci:
    uses: ./.github/workflows/ox-ci-nightly-impl.yml
```

Common edits users make to the root workflow (these flip the file to "dirty" and produce
a `.ox-ci-proposed` sibling on the next `update` — see
[updates.md §5](./updates.md#5-the-decision-algorithm)):

- **Self-hosted runners**: pass `with: { runs_on: 'self-hosted-rust' }` to the reusable
  workflow.
- **Trim or expand the test matrix**: pass `with: { test_os: '["ubuntu-latest"]' }` to
  run tests on Linux only, or `'["ubuntu-latest","windows-latest","macos-latest"]'` to
  add macOS. See §4 for the input contract.
- **Required secrets**: add `secrets: inherit` (or specific secrets) under the `ox-ci:`
  job.
- **Different schedule** for nightly.
- **Concurrency groups**: add a `concurrency:` block.
- **Path filters** to skip the workflow on docs-only PRs (though ox-ci's own
  `.delta.toml` trip-wire patterns already do impact-scoped skipping).

## 4. Owned reusable workflows

`ox-ci-pr-impl.yml` is where the wiring lives. Every per-group composite action takes
the same three impact-exclude inputs unconditionally; which ones a group's checks
actually consume is the catalog's concern, not the wiring layer's. Moving a check
between groups never changes the reusable workflow.

Approximate shape (ox-ci writes this verbatim; users never edit it):

```yaml
# .github/workflows/ox-ci-pr-impl.yml   (owned by cargo-ox-ci)
on:
  workflow_call:
    inputs:
      runs_on:
        type: string
        default: ubuntu-latest
        description: Runner for single-OS jobs (impact, pr-fast, pr-mutants).
      test_os:
        type: string
        default: '["ubuntu-latest","windows-latest"]'
        description: JSON array of runners for the cross-OS pr-test matrix.

jobs:
  impact:
    runs-on: ${{ inputs.runs_on }}
    outputs:
      exclude_not_modified: ${{ steps.impact.outputs.exclude_not_modified }}
      exclude_not_affected: ${{ steps.impact.outputs.exclude_not_affected }}
      exclude_not_required: ${{ steps.impact.outputs.exclude_not_required }}
      skip: ${{ steps.impact.outputs.skip }}
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }
      - id: impact
        uses: ./.github/actions/ox-ci-impact
        with:
          base_ref: ${{ github.event.pull_request.base.sha }}

  pr-fast:
    needs: impact
    runs-on: ${{ inputs.runs_on }}
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-fast
        with:
          pr_title:             ${{ github.event.pull_request.title }}
          exclude_not_modified: ${{ needs.impact.outputs.exclude_not_modified }}
          exclude_not_affected: ${{ needs.impact.outputs.exclude_not_affected }}
          exclude_not_required: ${{ needs.impact.outputs.exclude_not_required }}
          impact_skip:          ${{ needs.impact.outputs.skip }}

  pr-test:
    needs: impact
    strategy:
      fail-fast: false
      matrix:
        os: ${{ fromJSON(inputs.test_os) }}
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-test
        with:
          exclude_not_modified: ${{ needs.impact.outputs.exclude_not_modified }}
          exclude_not_affected: ${{ needs.impact.outputs.exclude_not_affected }}
          exclude_not_required: ${{ needs.impact.outputs.exclude_not_required }}
          impact_skip:          ${{ needs.impact.outputs.skip }}

  pr-mutants:
    needs: impact
    runs-on: ${{ inputs.runs_on }}
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-mutants
        with:
          base_ref:             origin/${{ github.event.pull_request.base.ref }}
          exclude_not_modified: ${{ needs.impact.outputs.exclude_not_modified }}
          exclude_not_affected: ${{ needs.impact.outputs.exclude_not_affected }}
          exclude_not_required: ${{ needs.impact.outputs.exclude_not_required }}
          impact_skip:          ${{ needs.impact.outputs.skip }}
```

The wiring never short-circuits jobs on `skip=true`. Each group always runs; the
recipes inside the group decide whether a given check can no-op. This matters because
several PR-tier checks (`fmt`, `deny`, `audit`, `aprz`, `pr-title`, `spellcheck`) don't
scope to workspace members and must run on every PR, including docs-only PRs where
nothing in the workspace is "affected." See
[local.md §4](./local.md#4-impact-scoping-pass-through-env-vars) for the recipe-side
contract.

The nightly reusable workflow is simpler — it omits the `impact` job and runs each group
full-workspace. The exclude inputs are still passed (defaulted to empty) so the composite
actions have a uniform interface across tiers:

```yaml
# .github/workflows/ox-ci-nightly-impl.yml  (owned)
on:
  workflow_call:
    inputs:
      runs_on:
        type: string
        default: ubuntu-latest
      test_os:
        type: string
        default: '["ubuntu-latest","windows-latest"]'
jobs:
  test:
    strategy:
      fail-fast: false
      matrix: { os: ${{ fromJSON(inputs.test_os) }} }
    runs-on: ${{ matrix.os }}
    steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-ci-nightly-test } ]
  advisories:  { runs-on: ${{ inputs.runs_on }}, steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-ci-nightly-advisories } ] }
  runtime:     { runs-on: ${{ inputs.runs_on }}, steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-ci-nightly-runtime } ] }
  exhaustive:  { runs-on: ${{ inputs.runs_on }}, steps: [ { uses: actions/checkout@v4 }, { uses: ./.github/actions/ox-ci-nightly-exhaustive } ] }
```

Nightly composite actions don't receive `exclude_*` at all — their inputs default to
empty (full workspace) and the reusable workflow omits the passthrough. Threading them
through is purely a PR-tier optimization; nightly never benefits.

If `.delta.toml`'s managed region is disabled
([updates.md §opt-out](./updates.md#6-opting-out-in-file-stubs)),
`ox-ci-pr-impl.yml` is regenerated **without** the `impact` job: each group job becomes
unconditional and the `exclude_*` inputs default to empty, so every group runs
full-workspace. `.github/actions/ox-ci-impact/` is not emitted in that mode.

The reusable workflow declares a small input set so the root workflow can pass overrides:

| Input     | Type   | Default                                | Meaning                                                |
|-----------|--------|----------------------------------------|--------------------------------------------------------|
| `runs_on` | string | `ubuntu-latest`                        | Runner for single-OS jobs (impact, pr-fast, pr-mutants). |
| `test_os` | string | `'["ubuntu-latest","windows-latest"]'` | JSON array of runners for the cross-OS `pr-test` matrix. Override to drop Windows for OSS-only repos, or to add `macos-latest`, `windows-2022`, a self-hosted label, etc. |

The nightly reusable workflow has the same two inputs; its `nightly-test` job uses
`test_os` and every other job uses `runs_on`.

We deliberately keep this input surface minimal. Anything more elaborate (e.g.
per-job runner overrides) lives in the user's own workflow, which can compose its own
`uses:`-of-reusable-workflow shape.

## 5. Per-group composite actions

Each per-group composite action has the **same** uniform input surface — the three
impact-exclude variables plus a per-action handful of PR-context strings. This means
the reusable workflow doesn't need to know which excludes a group's checks consume; it
threads all three to every action. Moving a check between groups is a pure catalog
change.

```yaml
# .github/actions/ox-ci-pr-fast/action.yml  (owned)
name: ox-ci-pr-fast
description: ox-ci PR fast group
inputs:
  pr_title:
    description: PR title for the pr-title check.
    required: false
    default: ""
  exclude_not_modified:
    description: cargo-excludes string from ox-ci-impact (--modified). Empty = full workspace.
    required: false
    default: ""
  exclude_not_affected:
    description: cargo-excludes string from ox-ci-impact (--affected). Empty = full workspace.
    required: false
    default: ""
  exclude_not_required:
    description: cargo-excludes string from ox-ci-impact (--required). Empty = full workspace.
    required: false
    default: ""
  impact_skip:
    description: '"true" when no workspace member is in any impact tier. Recipes that scope to workspace members may early-return; non-scoping recipes ignore this.'
    required: false
    default: "false"
runs:
  using: composite
  steps:
    - uses: ./.github/actions/ox-ci-setup
    - shell: bash
      env:
        PR_TITLE: ${{ inputs.pr_title }}
        OX_CI_EXCLUDE_NOT_MODIFIED: ${{ inputs.exclude_not_modified }}
        OX_CI_EXCLUDE_NOT_AFFECTED: ${{ inputs.exclude_not_affected }}
        OX_CI_EXCLUDE_NOT_REQUIRED: ${{ inputs.exclude_not_required }}
        OX_CI_IMPACT_SKIP: ${{ inputs.impact_skip }}
      run: just ox-ci-pr-fast
```

Uniform input set on every per-group composite action:

| Input                     | Default     | Notes                                              |
|---------------------------|-------------|----------------------------------------------------|
| `exclude_not_modified`    | `""`        | Forwarded as `OX_CI_EXCLUDE_NOT_MODIFIED`.         |
| `exclude_not_affected`    | `""`        | Forwarded as `OX_CI_EXCLUDE_NOT_AFFECTED`.         |
| `exclude_not_required`    | `""`        | Forwarded as `OX_CI_EXCLUDE_NOT_REQUIRED`.         |
| `impact_skip`             | `"false"`   | Forwarded as `OX_CI_IMPACT_SKIP`. Recipes that consume the excludes may early-return when `"true"`; non-scoping recipes (fmt, deny, audit, …) ignore it. See [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars). |

Per-action additions (only where the action consumes PR-context strings the recipe needs):

| Action                       | Extra inputs                                                            |
|------------------------------|-------------------------------------------------------------------------|
| `ox-ci-pr-fast`              | `pr_title`                                                              |
| `ox-ci-pr-mutants`           | `base_ref`                                                              |
| `ox-ci-pr-test`              | —                                                                       |
| `ox-ci-nightly-*`            | —                                                                       |

The recipes themselves consume only the env vars they need; the catalog records the
mapping (see [checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping)).
Threading all three to every action costs a few lines per composite but is the right
separation: wiring is about "which jobs depend on impact and feed it forward", not about
"which check needs which env var."

These actions are consumed primarily by ox-ci's own reusable workflow. Users who want to
plug individual groups into an unrelated workflow can `uses:` them directly.

### `ox-ci-setup`

`ox-ci-setup` installs `just` (`cargo install just --locked --version >=<min>`) and runs
`just ox-ci-tools-install-missing`. Does not install Rust; expects `cargo` on PATH (see
§7). `ox-ci-impact` is described in §6 below.

## 6. Impact scoping

`.github/actions/ox-ci-impact/action.yml` is a composite action with input `base_ref`. It
runs:

1. `git checkout $base_ref` and `cargo delta -c .delta.toml snapshot > baseline.json`.
2. `git checkout $head` and `cargo delta -c .delta.toml snapshot > current.json`.
3. `cargo delta impact -c .delta.toml --baseline baseline.json --current current.json -f
   cargo-excludes --modified|--affected|--required` once per tier.

Outputs:

| Output                  | Meaning                                                                                                  |
|-------------------------|----------------------------------------------------------------------------------------------------------|
| `exclude_not_modified`  | `--exclude X --exclude Y …` string for the complement of cargo-delta's `modified` tier.                  |
| `exclude_not_affected`  | Same, for the `affected` tier.                                                                           |
| `exclude_not_required`  | Same, for the `required` tier.                                                                           |
| `skip`                  | `"true"` when no workspace member is in any tier (no PR-relevant change). Propagated via `impact_skip` to every composite action; recipes that scope to workspace members may use it to early-return, but the wiring never gates whole jobs on it (see §4). |

The reusable workflow handles consumption — users never wire these outputs themselves.

The check → tier mapping is in
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping). The recipe-side
mechanics are in [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars).

## 7. Rust toolchain

ox-ci does not install Rust on GitHub. The composite actions assume `cargo` is on PATH.
On GH-hosted runners (the default), `rustup` is pre-installed and
`rust-toolchain.toml` triggers auto-install on first `cargo` invocation; the cache hit on
subsequent runs is good. No explicit install step is needed.

On self-hosted runners or pre-baked images without rustup, the user adds a Rust install
step to their root workflow before the `uses:` of the reusable workflow:

```yaml
jobs:
  ox-ci:
    uses: ./.github/workflows/ox-ci-pr-impl.yml
    # Self-hosted? Add a setup workflow that runs first and uploads
    # toolchain to a shared cache, then reference it here.
```

Since reusable workflows can't accept "previous step" handoff, self-hosted users usually
forgo the reusable-workflow shape and write a single workflow that calls the composite
actions directly. ox-ci's composite actions are exposed for that use case.

`_ox-ci-require` (invoked by every check recipe) validates the installed `rustc` against
the catalog minimum at recipe time; missing or below-minimum `rustc` produces a clean
failure message.

## 8. Caching

The `ox-ci-setup` composite action computes a cache key from: OS, rustc version (read
from `rust-toolchain.toml`), `Cargo.lock`, `.cargo/config.toml`, and the binary's
embedded catalog hash. Uses `actions/cache` natively. `CARGO_HOME` is pinned to a
workspace-scratch location to keep cache scoping predictable.

The cache covers:

- The `cargo install`-ed tools from `ox-ci-tools-install-missing`.
- The `target/` directory (per ox-ci recipe; a per-recipe cache scope means a `pr-test`
  cache hit doesn't have to wait on a `pr-fast` cache miss).

## 9. Security

The composite actions do nothing privileged on their own — they just install tools and
invoke `just`. The reusable workflow propagates only what the root workflow passes (and
only the inputs explicitly declared).

Recommended root workflow shape:

- `permissions: contents: read` at the workflow level. ox-ci's default ships with
  this.
- No `pull-requests: write` (the PR-title check only needs the title from the event
  payload, which is already in `${{ github.event.pull_request.title }}`).
- Nightly secrets, if any, live on `ox-ci-nightly.yml` only — never on `ox-ci-pr.yml`.
- All cargo-tool installs done by `ox-ci-setup` use `--locked`. No `cargo-binstall`.
