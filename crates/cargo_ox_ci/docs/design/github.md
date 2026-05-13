# GitHub Actions Integration

This document describes what `cargo ox-ci update --backend github|both` emits for GitHub
Actions, and how a repo wires those building blocks into workflows. ox-ci emits **only
building blocks** — never workflow files. The user owns triggers, runners, permissions,
concurrency, and any non-ox-ci jobs.

See also:

- [design.md](./design.md) for the overall principles and the "building blocks only" stance.
- [checks.md](./checks.md) for what each group runs.
- [local.md](./local.md) for the `just` recipes the building blocks invoke.
- [ado.md](./ado.md) for the ADO counterpart.

## 1. Why only building blocks

- **Workflows reflect repo policy.** Triggers, branch protection, required-status-checks
  policy, concurrency groups, and the choice of GH-hosted vs self-hosted runners are all
  repo-level decisions ox-ci has no business making.
- **Compliance parity with ADO.** The ADO side cannot emit workflows for compliance reasons
  ([ado.md](./ado.md)). Keeping GH on the same model keeps the user-facing mental model
  uniform: "ox-ci ships building blocks; you wire them in."
- **Toolchain install is the user's responsibility.** `rustup` is pre-installed on
  GH-hosted runners and `rust-toolchain.toml` triggers auto-install on first `cargo`
  invocation. On self-hosted or pre-baked-image runners the user installs Rust however they
  prefer. An ox-ci-emitted Rust install step would be either wrong or redundant.

## 2. Emitted artifacts

All under `.github/actions/` (one composite action per directory):

```text
.github/actions/
├── ox-ci-setup/action.yml                # installs just + catalog tools
├── ox-ci-impact/action.yml               # cargo-delta scoping; omitted if .delta.toml disabled
├── ox-ci-pr-fast/action.yml              # one composite action per group
├── ox-ci-pr-test/action.yml
├── ox-ci-pr-mutants/action.yml
├── ox-ci-nightly-test/action.yml
├── ox-ci-nightly-advisories/action.yml
├── ox-ci-nightly-runtime/action.yml
└── ox-ci-nightly-exhaustive/action.yml
```

What ox-ci does **not** emit:

- Workflow files (`.github/workflows/*.yml`).
- Job templates, reusable workflows.
- Rust toolchain install steps.

### 2.1 `ox-ci-setup`

Composite action that installs `just` (`cargo install just --locked --version >=<min>`)
and runs `just ox-ci-tools-install-missing`. Does not install Rust; expects `cargo` on
PATH. The user's workflow file is responsible for any prior Rust install step.

### 2.2 Per-group composite actions

One composite action per group (3 PR + 4 nightly = 7 actions). Each declares the inputs it
needs and invokes `just ox-ci-<tier>-<group>` with them wired to env vars.

Example `ox-ci-pr-fast/action.yml`:

```yaml
name: ox-ci-pr-fast
description: ox-ci PR fast group
inputs:
  pr_title:
    description: PR title for the pr-title check
    required: false
    default: ""
  exclude_not_modified:
    description: cargo-excludes string from ox-ci-impact (--modified). Empty = full workspace.
    required: false
    default: ""
runs:
  using: composite
  steps:
    - uses: ./.github/actions/ox-ci-setup
    - shell: bash
      env:
        PR_TITLE: ${{ inputs.pr_title }}
        OX_CI_EXCLUDE_NOT_MODIFIED: ${{ inputs.exclude_not_modified }}
      run: just ox-ci-pr-fast
```

The per-group composite actions accept these inputs (each optional, default `""` = full
workspace):

| Group                  | Inputs                                                                  |
|------------------------|-------------------------------------------------------------------------|
| `ox-ci-pr-fast`        | `pr_title`, `exclude_not_modified`                                      |
| `ox-ci-pr-test`        | `exclude_not_affected`, `exclude_not_required`                          |
| `ox-ci-pr-mutants`     | `base_ref`, `exclude_not_affected`                                      |
| `ox-ci-nightly-test`   | (none — always full workspace)                                          |
| `ox-ci-nightly-advisories` | (none)                                                              |
| `ox-ci-nightly-runtime`    | (none)                                                              |
| `ox-ci-nightly-exhaustive` | (none)                                                              |

## 3. Example user-owned workflow

The user writes one workflow file per tier. Below is a typical hand-written
`.github/workflows/ox-ci-pr.yml`:

```yaml
name: ox-ci-pr
on: { pull_request: {}, merge_group: {} }
permissions: { contents: read }
jobs:
  impact:
    runs-on: ubuntu-latest
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
  lint:
    needs: impact
    if: needs.impact.outputs.skip != 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-fast
        with:
          pr_title: ${{ github.event.pull_request.title }}
          exclude_not_modified: ${{ needs.impact.outputs.exclude_not_modified }}
  test:
    needs: impact
    if: needs.impact.outputs.skip != 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-test
        with:
          exclude_not_affected: ${{ needs.impact.outputs.exclude_not_affected }}
          exclude_not_required: ${{ needs.impact.outputs.exclude_not_required }}
  mutants:
    needs: impact
    if: needs.impact.outputs.skip != 'true'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-mutants
        with:
          base_ref: origin/${{ github.event.pull_request.base.ref }}
          exclude_not_affected: ${{ needs.impact.outputs.exclude_not_affected }}
```

A typical `.github/workflows/ox-ci-nightly.yml` is shorter — nightly always runs
full-workspace, so it omits the `impact` job entirely:

```yaml
name: ox-ci-nightly
on:
  schedule: [{ cron: '0 6 * * *' }]
  workflow_dispatch: {}
permissions: { contents: read }
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-nightly-test
  advisories:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-nightly-advisories
  runtime:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-nightly-runtime
  exhaustive:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-nightly-exhaustive
```

The README that ox-ci writes on first run includes both snippets (plus an impact-free PR
variant for repos that disabled `.delta.toml`) as copy-paste starting points.

## 4. Impact scoping

ox-ci emits `.github/actions/ox-ci-impact/action.yml` as a composite action with input
`base_ref`. It runs:

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
| `skip`                  | `true` when no workspace member is in any tier (no PR-relevant change); use to short-circuit downstream jobs via `if: needs.impact.outputs.skip != 'true'`. |

The check → tier mapping is in
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping). The recipe-side
mechanics are in [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars).

If `.delta.toml`'s managed region is disabled
([updates.md §opt-out](./updates.md#opting-out-in-file-stubs)), ox-ci suppresses emission
of `ox-ci-impact/action.yml` entirely. The per-group composite actions still accept the
`exclude_*` inputs for compatibility; with no `impact` job feeding them they default to
empty and every group runs full-workspace.

Nightly workflows always run full-workspace and don't use the `ox-ci-impact` action.

## 5. Rust toolchain

ox-ci does not install Rust on GitHub. The composite actions assume `cargo` is on PATH.
The user is responsible for installing Rust before invoking any ox-ci action:

- **GH-hosted runners (default)**: `rustup` is pre-installed; `rust-toolchain.toml`
  triggers auto-install on first `cargo` invocation; the cache hit on subsequent runs is
  good. No explicit install step is needed.
- **Self-hosted runners or pre-baked images without rustup**: add whatever install step
  fits your environment (`dtolnay/rust-toolchain`, `actions-rust-lang/setup-rust-toolchain`,
  msrustup, a pre-baked image, …) as the first step of each job, before the ox-ci
  composite action.

`_ox-ci-require` (invoked by every check recipe) validates the installed `rustc` against
the catalog minimum at recipe time; missing or below-minimum `rustc` produces a clean
failure message.

## 6. Caching

The `ox-ci-setup` composite action computes a cache key from: OS, rustc version (read from
`rust-toolchain.toml`), `Cargo.lock`, `.cargo/config.toml`, and the binary's embedded
catalog hash. Uses `actions/cache` natively. `CARGO_HOME` is pinned to a workspace-scratch
location to keep cache scoping predictable.

The cache covers:

- The `cargo install`-ed tools from `ox-ci-tools-install-missing`.
- The `target/` directory (per ox-ci recipe; a per-recipe cache scope means a `pr-test`
  cache hit doesn't have to wait on a `pr-fast` cache miss).

## 7. Security

The composite actions do nothing privileged on their own — they just install tools and
invoke `just`. The user's workflow controls permissions.

Recommended user-workflow shape:

- `permissions: contents: read` at the workflow level.
- No `pull-requests: write` (the PR-title check only needs the title from the event
  payload, which is already in `${{ github.event.pull_request.title }}`).
- Nightly secrets, if any, live on `ox-ci-nightly.yml` only — never on `ox-ci-pr.yml`.
- All cargo-tool installs done by `ox-ci-setup` use `--locked`. No `cargo-binstall`.
