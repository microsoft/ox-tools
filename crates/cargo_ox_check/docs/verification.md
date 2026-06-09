# Continuous Validation Strategy

This document defines how `cargo-ox-check` is kept correct over time. The headline mechanism
is dogfooding — the `microsoft/ox-tools` repo, where cargo-ox-check itself lives, uses
`cargo ox-check update` to manage its own CI. Every PR that touches the catalog or the
emitters produces a visible diff in `.github/` and `justfiles/ox-check/`, then runs through
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

`microsoft/ox-tools` is the canonical adopter of `cargo-ox-check`. Its `.github/workflows/`,
`.github/actions/`, `justfiles/ox-check/`, `[workspace.lints]` region in `Cargo.toml`, etc.
are all emitted by `cargo ox-check update` against the in-repo version of the binary. There
is no manual maintenance of these files after the initial migration.

Every PR runs (via a small bootstrap workflow described in §3):

1. `cargo build --locked -p cargo-ox-check` — build the binary from source.
2. `target/debug/cargo-ox-check ox-check update` — regenerate every owned file and managed region.
3. `git diff --exit-code` — fail with a clear message if regeneration produced changes the
   PR didn't commit.
4. Continue into the normal `ox-check-pr` workflow, which is itself the freshly regenerated
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

### 2.2 Snapshot tests (shipping today)

Under `crates/cargo_ox_check/tests/snapshots.rs`, three integration tests drive the full
emitter against a bare-workspace tempdir for representative input combinations
(`--no-backends`, `--backend github`, `--backend ado`) and snapshot the full collection of
emitted files via [`insta`][insta]. Template edits then surface as reviewable diffs in
PRs — `cargo insta review` accepts them.

Snapshot files live committed under `tests/snapshots/`, one per backend combination.
The `.ox-check.lock` manifest is filtered out of the snapshot input to keep the snapshots
stable across version bumps (the manifest carries `rendered_by = "cargo-ox-check <ver>"`
which would otherwise churn on every release).

### 2.3 Fixture-based integration tests

Alongside the snapshot tests, a `tests/fixtures/` corpus covers
directory-tree scenarios that benefit from being reviewable as real
files on disk:

| Fixture            | What it pins                                                                                                                        |
|--------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `single-crate/`    | Non-workspace repo. Validates the `[lints]` (vs `[workspace.lints]`) branch and that the full `justfiles/ox-check/` tree is written. |
| `opt-outs/`        | A user-emptied managed region stays empty across re-runs (steady-state opt-out, `LeaveAlone` decision).                              |
| `customized/`      | A user edit inside a managed region is preserved verbatim across re-runs when the template is unchanged (`LeaveAlone` decision).     |
| `migration/`       | A repo with pre-existing hand-written `Justfile`, `deny.toml`, and `[profile.release]` in `Cargo.toml`. Ox-check splices its regions without losing the user content. |

`tests/update.rs` stages each fixture into a tempdir (via `walkdir` +
`std::fs::copy`), runs `run_update`, and asserts the scenario-specific
invariants above. The single-crate and migration scenarios additionally
assert idempotence — a second run produces an empty plan.

The fixtures are complementary to the imperative scenarios in
`src/run.rs`, which seed equivalent setups inline. The on-disk fixtures
are easier to review and to copy when designing new migration paths.

[insta]: https://crates.io/crates/insta

### 2.4 Schema validation

Run as part of `ox-check-pr-fast` against ox-tools's emitted output:

- **`actionlint`** on every emitted `.github/workflows/*.yml` and
  `.github/actions/*/action.yml`. Catches GitHub-Actions-specific errors that plain
  YAML validation misses.
- **`just --summary --unstable`** on every `justfiles/ox-check/*.just`. Verifies recipes
  parse and dependency graph is well-formed.
- **`taplo check`** on every TOML file ox-check writes to. Verifies the post-edit file is
  still parsable TOML and conforms to the cargo schema where applicable.
- **ADO YAML**: no widely-available local validator. The snapshot tests
  are the contract; the manual release checklist (§2.5) covers
  semantic verification against real ADO. We accept this gap because
  ox-tools cannot dogfood ADO emission anyway.

A small in-process schema-validation suite at
`crates/cargo_ox_check/tests/schemas.rs` covers the subset that can be
checked without external tooling (TOML parseability of every emitted
TOML region/file, the `.ox-check.lock` schema, etc.).

### 2.5 Manual release verification

Three things ox-tools dogfooding doesn't catch, addressed by a pre-release checklist
maintained in `docs/release-checklist.md`:

- **Compliance-extending ADO pipelines** (1ESPT, SubstratePT, CloudBuild). Validated
  manually by running `cargo ox-check update --dry-run` against `oxidizer`,
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

ox-tools's `.github/workflows/ox-check-pr.yml` (post-migration) wraps the regenerated
`ox-check-pr-impl.yml` with a small self-validation gate. Sketch:

```yaml
name: ox-check-pr
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
      - run: cargo build --locked -p cargo-ox-check
      - name: Regenerate emitted files
        run: ./target/debug/cargo-ox-check ox-check update
      - name: Assert no drift
        run: |
          if ! git diff --exit-code; then
            echo "::error::cargo-ox-check changed files. Run 'cargo ox-check update' locally and commit the diff."
            exit 1
          fi

  ox-check:
    needs: regenerate-check
    uses: ./.github/workflows/ox-check-pr-impl.yml
```

The `regenerate-check` job runs first. If a PR changes the catalog or emitter without
also committing the regenerated output, this fails with an actionable message. After
that, the standard `ox-check-pr-impl.yml` reusable workflow runs every group, exactly as
in any consumer repo.

The wrapper workflow above is the **one** hand-written workflow in ox-tools — it
bootstraps the dogfood loop. Every other CI artifact is regenerated.

## 4. Bootstrap and breaking changes

### Initial bootstrap

The very first cargo-ox-check PR cannot dogfood itself — the binary doesn't exist yet.
Bootstrap plan:

1. PR #1: lands the binary's skeleton (current state, no `update` logic yet) plus
   hand-written workflows for the unit and integration tests. ox-tools's CI is still
   hand-written.
2. PR #N (first usable `update`): lands the emitter implementation. Run
   `cargo ox-check update` locally, commit the diff, push. From this point forward
   ox-tools is self-hosted.
3. PR #N+1 onward: every PR runs the regenerate-check gate.

### Breaking changes inside cargo-ox-check

Two flavors require care:

- **Manifest-schema bumps.** Migration logic must be in place before any release that
  needs it. The `migration/` fixture exercises old-schema-to-new-schema upgrades.
  Release notes call out the bump.
- **Renames in the emitted CI surface** (e.g., `ox-check-pr-fast` → `ox-check-pr-static`).
  Treated as major-version bumps. The PR introducing the rename is split into two
  commits: (a) implement, (b) `cargo ox-check update` to regenerate. Downstream repos do
  the same two-step on adoption.

Both flavors are caught by ox-tools's own regenerate-check: a missing migration or a
rename that doesn't round-trip will produce drift on the second run, failing the gate.

### Recovering from a self-inflicted breakage

If a PR lands that breaks the regenerate-check (because reviewers missed it), the
breakage is **not stuck** — every PR builds cargo-ox-check from source on its own branch,
so the fix PR is free to either revert the offending change or land the missing
regenerated output, and its own CI will pass cleanly.

What is affected: unrelated PRs that branched off the broken commit will fail their
regenerate-check, because they inherit the drift through the merge base. They recover
by rebasing past the fix.

Procedure:

1. Open a fix PR. Either: (a) revert the offending commit, or (b) commit the missing
   `cargo ox-check update` output. Either flavor builds the binary from the fix branch
   and produces a clean `git diff` against its own tree, so CI passes.
2. Merge.
3. In-flight PRs rebase to pick up the fix; their regenerate-check passes once their
   merge base is past the fix commit.

ox-tools never depends on the published crates.io version of cargo-ox-check for its own
checks — it always builds from source. So a broken release on crates.io doesn't
cascade into ox-tools's CI; only a broken `main` does, and only for unrelated
in-flight PRs.

## 5. Coverage gaps

Acknowledged limits of this strategy:

- **1ESPT/SubstratePT/CloudBuild composition** — ox-tools is OSS; it cannot dogfood
  internal compliance harnesses. Manual release checklist covers this (§2.5).
- **Self-hosted runner pools** — ox-tools uses GH-hosted; the `runs_on` input is set
  to defaults. Self-hosted shapes are documented but exercised only by the manual
  release checklist.
- **macOS** — not in the default matrix (see [design.md §8.3](./design/design.md#83-cross-os-test-matrices));
  not dogfooded.
- **Very deep workspaces or unusual layouts** — covered only by fixtures, not by
  real-world traffic. New layouts that adopters surface become new fixtures.
- **Long-lived divergence** — a repo that's been on an old cargo-ox-check version for
  many releases is only validated by the cross-repo migration step in §2.5.

## 6. Files and locations

| Path                                                | Purpose                                                                 |
|-----------------------------------------------------|-------------------------------------------------------------------------|
| `crates/cargo_ox_check/tests/snapshots.rs`             | Snapshot tests over the three backend combinations (insta).             |
| `crates/cargo_ox_check/tests/snapshots/`               | Committed snapshot files (one per backend combination).                 |
| `crates/cargo_ox_check/tests/fixtures/`                | Integration test fixtures (one directory per shape).                    |
| `crates/cargo_ox_check/tests/update.rs`                | Per-fixture assertions for opt-outs, customizations, migrations, single-crate. |
| `crates/cargo_ox_check/tests/schemas.rs`               | actionlint / taplo / just-parse wrappers run against the emitted output. |
| `.github/workflows/ox-check-pr.yml`                    | Hand-written self-validation wrapper (the one bootstrap file).          |
| `.github/workflows/ox-check-pr-impl.yml` (and friends) | Regenerated by `cargo ox-check update`. Subject to the regenerate-check.    |
| `justfiles/ox-check/*.just`                            | Regenerated. Subject to the regenerate-check.                           |
| `Cargo.toml` (ox-check-workspace-lints region)         | Regenerated. Subject to the regenerate-check.                           |
| `.ox-check.lock`                                       | The manifest itself. Diffed on every PR.                                |
| `docs/release-checklist.md`                         | Pre-publish checks for things dogfooding misses.                        |

## 7. Future work

- A dedicated `cargo ox-check verify` subcommand that runs `update --dry-run` + all
  schema validators + the manifest-consistency check in one step, for local
  pre-commit use.
- A small set of "downstream canary" repos in `microsoft/` org that pin cargo-ox-check
  to `main` (not to a release) and report failures via issues. Catches regressions
  earlier than the manual release checklist.
- Pre-release smoke runs of every adopter's CI against a release candidate, gated by
  a draft pre-release tag.
