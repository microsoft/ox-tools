# Continuous Validation Strategy

This document defines how `cargo-ox-ci` is kept correct over time. The headline mechanism
is dogfooding — the `microsoft/ox-tools` repo, where cargo-ox-ci itself lives, uses
`cargo ox-ci update` to manage its own CI. Every PR that touches the catalog or the
emitters produces a visible diff in `.github/` and `justfiles/ox-ci/`, then runs through
the regenerated CI on the same commit. A broken emitter or catalog fails the PR's own
checks immediately.

See also:

- [design/](./design/) — the tool's design.
- [design/updates.md](./design/updates.md) — the state machine validated by fixture tests.
- [design/checks.md](./design/checks.md) — the catalog dogfooded by ox-tools.

## 1. Goals

- **Detect regressions on the PR that introduces them.** No "this broke a downstream repo"
  surprises after a release.
- **Validate the whole pipeline**, not just unit-level behavior: catalog → templates →
  manifest → emitted CI → CI actually running.
- **Cover the state machine in [updates.md §5](./design/updates.md#5-the-decision-algorithm)
  exhaustively** — every row of the decision table is exercised by some test.
- **Keep validation cheap** — most of it runs in the PR pipeline; nothing requires a
  bespoke test environment.

## 2. Layers

### 2.1 Self-hosting (primary)

`microsoft/ox-tools` is the canonical adopter of `cargo-ox-ci`. Its `.github/workflows/`,
`.github/actions/`, `justfiles/ox-ci/`, `[workspace.lints]` region in `Cargo.toml`, etc.
are all emitted by `cargo ox-ci update` against the in-repo version of the binary. There
is no manual maintenance of these files after the initial migration.

Every PR runs (via a small bootstrap workflow described in §3):

1. `cargo build --locked -p cargo-ox-ci` — build the binary from source.
2. `target/debug/cargo-ox-ci ox-ci update` — regenerate every owned file and managed region.
3. `git diff --exit-code` — fail with a clear message if regeneration produced changes the
   PR didn't commit.
4. Continue into the normal `ox-ci-pr` workflow, which is itself the freshly regenerated
   workflow file.

What this validates end-to-end:

- The catalog renders to valid YAML / TOML / `just`.
- The manifest's three-checksum state machine produces idempotent output (rerunning
  `update` with no changes is a no-op).
- Every emitted CI building block actually runs — broken composite actions, broken
  reusable workflows, broken step templates surface immediately.
- The full default check catalog is exercised on every PR. ox-tools deliberately enables
  every catalog check (no opt-out stubs) and the default cross-OS matrix (Linux +
  Windows for test groups).

What this doesn't catch — see §2.4.

### 2.2 Fixture-based integration tests

Under `crates/cargo_ox_ci/tests/fixtures/`, a small set of fixture repos covers shapes
ox-tools doesn't have. Each fixture is a directory tree plus an `expected/` snapshot.
The test runner copies the fixture to a tmpdir, runs `cargo ox-ci update`, and asserts
byte-equal output against `expected/`.

Initial fixture set:

| Fixture            | Purpose                                                                                   |
|--------------------|-------------------------------------------------------------------------------------------|
| `fresh/`           | Empty repo. Exercises the "first run, no manifest" creation paths.                        |
| `single-crate/`    | Non-workspace repo. Validates the `[lints]` (vs `[workspace.lints]`) branch.              |
| `simple-workspace/`| Two-member workspace. The everyday case mirroring ox-tools at small scale.                |
| `opt-outs/`        | Empty owned files, empty region bodies. Validates the "no proposal until template change" rule. |
| `customized/`      | Dirty owned files and dirty regions. Validates the dirty-flow including `.ox-ci-proposed` emission. |
| `migration/`       | A repo with an old manifest schema and pre-existing emitted content. Validates the on-load migration logic. |

Each fixture is exercised by at least three independent assertions:

- **Idempotence** — running `update` twice in a row produces zero diff on the second run.
- **Determinism** — running `update` against two identical clones produces identical
  output (no time-, env-, or path-dependence).
- **Manifest consistency** — after a clean run, every entry in `.ox-ci.lock` matches the
  checksum of the corresponding on-disk content.

The fixture corpus grows when a bug is fixed: the bug's repro becomes a fixture before
the fix lands.

### 2.3 Schema validation

Run as part of `ox-ci-pr-fast` against ox-tools's emitted output (and as part of each
fixture's assertion suite):

- **`actionlint`** on every emitted `.github/workflows/*.yml` and
  `.github/actions/*/action.yml`. Catches GitHub-Actions-specific errors that plain
  YAML validation misses.
- **`just --summary --unstable`** on every `justfiles/ox-ci/*.just`. Verifies recipes
  parse and dependency graph is well-formed.
- **`taplo check`** on every TOML file ox-ci writes to. Verifies the post-edit file is
  still parsable TOML and conforms to the cargo schema where applicable.
- **ADO YAML**: no widely-available local validator. The fixture snapshots are the
  contract; the manual release checklist (§2.4) covers semantic verification against
  real ADO. We accept this gap because ox-tools cannot dogfood ADO emission anyway.

### 2.4 Manual release verification

Three things ox-tools dogfooding doesn't catch, addressed by a pre-release checklist
maintained in `docs/release-checklist.md`:

- **Compliance-extending ADO pipelines** (1ESPT, SubstratePT, CloudBuild). Validated
  manually by running `cargo ox-ci update --dry-run` against `oxidizer`,
  `assistants-oxide`, and `ox-docs` (internal mirrors) and inspecting the diff. If the
  diff looks right, queue a buddy build to confirm the regenerated pipeline still
  passes.
- **Cross-repo migrations**. Each release that bumps the manifest schema or renames a
  catalog item runs `update --dry-run` against every surveyed repo and confirms the
  proposed migration is correct.
- **Self-hosted runners and non-default matrices**. Spot-checked against `oxidizer`'s
  Microsoft-pool builds before each release.

The release checklist is a literal markdown file in the repo; checking it off is part
of the publish PR.

## 3. The PR-time workflow

ox-tools's `.github/workflows/ox-ci-pr.yml` (post-migration) wraps the regenerated
`ox-ci-pr-impl.yml` with a small self-validation gate. Sketch:

```yaml
name: ox-ci-pr
on:
  pull_request: {}
  merge_group: {}
permissions: { contents: read }
jobs:
  regenerate-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --locked -p cargo-ox-ci
      - name: Regenerate emitted files
        run: ./target/debug/cargo-ox-ci ox-ci update
      - name: Assert no drift
        run: |
          if ! git diff --exit-code; then
            echo "::error::cargo-ox-ci changed files. Run 'cargo ox-ci update' locally and commit the diff."
            exit 1
          fi

  ox-ci:
    needs: regenerate-check
    uses: ./.github/workflows/ox-ci-pr-impl.yml
```

The `regenerate-check` job runs first. If a PR changes the catalog or emitter without
also committing the regenerated output, this fails with an actionable message. After
that, the standard `ox-ci-pr-impl.yml` reusable workflow runs every group, exactly as
in any consumer repo.

The wrapper workflow above is the **one** hand-written workflow in ox-tools — it
bootstraps the dogfood loop. Every other CI artifact is regenerated.

## 4. Bootstrap and breaking changes

### Initial bootstrap

The very first cargo-ox-ci PR cannot dogfood itself — the binary doesn't exist yet.
Bootstrap plan:

1. PR #1: lands the binary's skeleton (current state, no `update` logic yet) plus
   hand-written workflows for the unit and integration tests. ox-tools's CI is still
   hand-written.
2. PR #N (first usable `update`): lands the emitter implementation. Run
   `cargo ox-ci update` locally, commit the diff, push. From this point forward
   ox-tools is self-hosted.
3. PR #N+1 onward: every PR runs the regenerate-check gate.

### Breaking changes inside cargo-ox-ci

Two flavors require care:

- **Manifest-schema bumps.** Migration logic must be in place before any release that
  needs it. The `migration/` fixture exercises old-schema-to-new-schema upgrades.
  Release notes call out the bump.
- **Renames in the emitted CI surface** (e.g., `ox-ci-pr-fast` → `ox-ci-pr-static`).
  Treated as major-version bumps. The PR introducing the rename is split into two
  commits: (a) implement, (b) `cargo ox-ci update` to regenerate. Downstream repos do
  the same two-step on adoption.

Both flavors are caught by ox-tools's own regenerate-check: a missing migration or a
rename that doesn't round-trip will produce drift on the second run, failing the gate.

### Recovering from a self-inflicted breakage

If a PR lands that breaks the regenerate-check (because reviewers missed it), the
breakage is **not stuck** — every PR builds cargo-ox-ci from source on its own branch,
so the fix PR is free to either revert the offending change or land the missing
regenerated output, and its own CI will pass cleanly.

What is affected: unrelated PRs that branched off the broken commit will fail their
regenerate-check, because they inherit the drift through the merge base. They recover
by rebasing past the fix.

Procedure:

1. Open a fix PR. Either: (a) revert the offending commit, or (b) commit the missing
   `cargo ox-ci update` output. Either flavor builds the binary from the fix branch
   and produces a clean `git diff` against its own tree, so CI passes.
2. Merge.
3. In-flight PRs rebase to pick up the fix; their regenerate-check passes once their
   merge base is past the fix commit.

ox-tools never depends on the published crates.io version of cargo-ox-ci for its own
checks — it always builds from source. So a broken release on crates.io doesn't
cascade into ox-tools's CI; only a broken `main` does, and only for unrelated
in-flight PRs.

## 5. Coverage gaps

Acknowledged limits of this strategy:

- **1ESPT/SubstratePT/CloudBuild composition** — ox-tools is OSS; it cannot dogfood
  internal compliance harnesses. Manual release checklist covers this.
- **Self-hosted runner pools** — ox-tools uses GH-hosted; the `runs_on` input is set
  to defaults. Self-hosted shapes are documented but exercised only by the manual
  release checklist.
- **macOS** — not in the default matrix (see [design.md §8.3](./design/design.md#83-cross-os-test-matrices));
  not dogfooded.
- **Very deep workspaces or unusual layouts** — covered only by fixtures, not by
  real-world traffic. New layouts that adopters surface become new fixtures.
- **Long-lived divergence** — a repo that's been on an old cargo-ox-ci version for
  many releases is only validated by the cross-repo migration step in §2.4.

## 6. Files and locations

| Path                                                | Purpose                                                                 |
|-----------------------------------------------------|-------------------------------------------------------------------------|
| `crates/cargo_ox_ci/tests/fixtures/`                | Integration test fixtures (one directory per shape).                    |
| `crates/cargo_ox_ci/tests/update.rs`                | Test runner: per-fixture idempotence/determinism/consistency assertions. |
| `crates/cargo_ox_ci/tests/schema.rs`                | actionlint / taplo / just-parse wrappers run against ox-tools and fixtures. |
| `.github/workflows/ox-ci-pr.yml`                    | Hand-written self-validation wrapper (the one bootstrap file).          |
| `.github/workflows/ox-ci-pr-impl.yml` (and friends) | Regenerated by `cargo ox-ci update`. Subject to the regenerate-check.    |
| `justfiles/ox-ci/*.just`                            | Regenerated. Subject to the regenerate-check.                           |
| `Cargo.toml` (ox-ci-workspace-lints region)         | Regenerated. Subject to the regenerate-check.                           |
| `.ox-ci.lock`                                       | The manifest itself. Diffed on every PR.                                |
| `docs/release-checklist.md`                         | Pre-publish checks for things dogfooding misses.                        |

## 7. Future work

- A dedicated `cargo ox-ci verify` subcommand that runs `update --dry-run` + all
  schema validators + the manifest-consistency check in one step, for local
  pre-commit use.
- A small set of "downstream canary" repos in `microsoft/` org that pin cargo-ox-ci
  to `main` (not to a release) and report failures via issues. Catches regressions
  earlier than the manual release checklist.
- Pre-release smoke runs of every adopter's CI against a release candidate, gated by
  a draft pre-release tag.
