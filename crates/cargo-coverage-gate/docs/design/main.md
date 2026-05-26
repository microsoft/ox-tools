# cargo-coverage-gate — Design

> Status: **Draft**.
> Crate name: `cargo-coverage-gate`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

PR-time coverage gating in Rust workspaces is hard for three reasons:

1. **Global thresholds are wrong for delta builds.** With impact-scoping (e.g.
   cargo-delta) a PR runs tests only for affected crates, so the overall
   coverage percentage is computed against a different denominator than the
   nightly run on `main`. A meaningful comparison requires the same denominator.
2. **Diff coverage misses real regressions.** A PR showing 90% diff coverage
   can still drop overall coverage if the displaced code was at 100%. The "did
   this PR move the codebase backwards?" question isn't answered.
3. **External services don't always reach.** Codecov is unavailable for many
   Microsoft-internal repos; Azure DevOps' native coverage UI shows totals but
   has no built-in gating mechanism that correlates with the source of truth.

The unifying observation: **the right unit for coverage policy is the crate**,
not the diff, not the workspace. Each crate has a stable set of files and a
stable measured percentage; thresholds attached to crates remain comparable
across full and impact-scoped builds, and they catch the displaced-code case
because the per-crate number drops when high-coverage code is replaced with
lower-coverage code.

## 2. Goals

1. **One opinionated gating mechanism** that works identically locally, on
   GitHub Actions, and on Azure DevOps Pipelines — no external service
   dependency.
2. **Per-crate thresholds** stored next to the code they describe
   (`[package.metadata.coverage-gate]` in each `Cargo.toml`).
3. **Delta-build safe**: the gate accepts a subset of crates to check, so PR
   builds running impact-scoped tests gate only against the crates whose
   tests actually ran. Each comparison stays apples-to-apples.
4. **Single visible verdict** across local CLI output, GitHub workflow
   summary, and ADO build summary tab. Reviewers see the same per-crate
   table no matter where they look.
5. **Threshold lifecycle is git-native**: thresholds change via Cargo.toml
   edits that show up in PR diffs — no hidden state, no separate baseline
   service.
6. **Open source**: ships from `github.com/microsoft/ox-tools` to crates.io.

## 3. Non-Goals

- Running tests or producing coverage data. The tool consumes JSON produced
  by [`cargo-llvm-cov`][cargo-llvm-cov]; it does not invoke the toolchain
  itself.
- Diff coverage (per-line annotation of changed lines). Different concern,
  different consumers (review tooling like Codecov); this tool answers a
  separable question.
- Uploading coverage to Codecov / ADO Coverage UI. Those uploads use the
  byte-exact lcov / cobertura files cargo-llvm-cov emits and are wired by
  the surrounding CI templates. This tool stays out of upload paths.
- Cross-crate aggregation policies (e.g. "workspace total must stay above
  N"). If a repo wants that, run the existing
  `cargo llvm-cov --fail-under-lines` flag in parallel; the two are
  independent.
- Mixed-language coverage (C/C++ + Rust). `cargo-llvm-cov` doesn't support
  it either; repos that need it switch to `grcov` and out of this tool's
  scope.

## 4. Guiding Principle

> **One crate, one number, one threshold, one verdict.**

Corollaries:

- Each workspace member gets its own threshold or no threshold at all
  (silent opt-out).
- The gate compares **measured percentage for crate X against the threshold
  for crate X**, in isolation. There is no cross-crate dependency.
- Whether the build that produced the coverage data ran one crate's tests
  or all of them does not affect the verdict logic — only which crates are
  in scope.

## 5. User Experience

### 5.1 Installation

```sh
cargo install --locked cargo-coverage-gate
```

Required at PR time on CI runners and (optionally) on developer machines that
want to reproduce the gate locally.

### 5.2 CLI surface

```text
cargo coverage-gate check  [--json <path>] [--crates <name>,<name>,...]
                           [--summary-file <path>] [--quiet]
cargo coverage-gate init   [--json <path>] [--crates <name>,<name>,...]
                           [--margin <pp>] [--dry-run]
```

Two subcommands, deliberately small surface:

- **`check`** — the gating command. Reads coverage JSON, resolves the
  effective per-crate threshold (per-crate metadata, then workspace
  default, then the built-in default of `100.0`), computes per-crate
  percentages, emits a verdict table. Exit `0` if every in-scope crate
  meets its threshold; exit `1` if any in-scope crate fails; exit `2` on
  configuration error.
- **`init`** — adds `[package.metadata.coverage-gate]` to crates that
  don't yet have one, seeded from observed coverage as
  `floor((observed - margin) * 10) / 10`. Idempotent: never overwrites
  an existing per-crate threshold. After `init`, any subsequent
  threshold change is a manual `Cargo.toml` edit so that the change
  appears in the PR diff and is reviewed.

Shared flags:

- `--json <path>` — path to the cargo-llvm-cov JSON report. Defaults to
  `target/coverage/coverage.json` (matching the recommended
  `cargo llvm-cov report --json --output-path <path>` invocation).
- `--crates <list>` — restrict the operation to a comma-separated list of
  package names. Default: every workspace member. CI integrations pass
  the impacted-crate list from their test-impact step (e.g., a
  comma-separated env var set by the surrounding pipeline) so that
  impact-scoped runs only gate the crates whose tests actually ran.
- `--summary-file <path>` — write a Markdown verdict table to this file.
  When unset, `check` honors the environment variables
  `GITHUB_STEP_SUMMARY` (GitHub Actions) and
  `COVERAGE_GATE_SUMMARY` (any CI that pipes the file content through
  `##vso[task.uploadsummary]` or equivalent) automatically.
- `--margin <pp>` — (`init` only) percentage points subtracted from the
  observed value before writing it as a threshold, to absorb measurement
  noise. Default: `1.0`.
- `--dry-run` — (`init` only) print the proposed Cargo.toml edits
  without writing.
- `--quiet` — suppress stdout output (the summary file, if any, is still
  written).

### 5.3 The threshold metadata

A crate's threshold is resolved in three layers, in priority order:

1. **Per-crate**: `[package.metadata.coverage-gate]` in the crate's
   `Cargo.toml`.
2. **Workspace default**: `[workspace.metadata.coverage-gate]` in the
   root `Cargo.toml`. Applies to every member without per-crate
   metadata.
3. **Built-in default**: `min-lines = 100.0`. Applied when neither of
   the above is present.

```toml
# Per-crate (highest priority):
[package.metadata.coverage-gate]
min-lines = 75.0
```

```toml
# Workspace-wide default (applies to all members that don't override):
[workspace.metadata.coverage-gate]
min-lines = 80.0
```

The schema today is one key, `min-lines`, an integer or float percentage
(`0.0`–`100.0` inclusive). Future extensions can add `min-functions`,
`min-regions` symmetrically.

The built-in default of `100.0` means **gating is on by default**: a new
crate with no metadata anywhere will only pass if every measured line is
covered. To opt a crate out of gating, set `min-lines = 0.0` explicitly
(at the crate or workspace level). There is no implicit opt-out.

### 5.4 The verdict table

`check` prints a deterministic table to stdout (and to the summary file
when configured):

```text
ox coverage-gate

  Crate              Lines       Threshold   Δ vs threshold   Status   Source
  ─────────────────  ──────────  ──────────  ───────────────  ───────  ─────────
  crates/alpha       82.1%        80.0%        +2.1pp           OK       crate
  crates/beta        74.5%        80.0%        −5.5pp           FAIL     workspace
  crates/gamma       91.0%       100.0%        −9.0pp           FAIL     default
  ─────────────────  ──────────  ──────────  ───────────────  ───────  ─────────
  Result: 2 crate(s) below threshold.
```

The `Source` column reports which layer supplied the threshold: `crate`,
`workspace`, or `default` (the built-in `100.0`).

Markdown variant uses the same columns and a leading `### ox coverage-gate`
header so it renders cleanly in GitHub job summaries and ADO build summaries.

### 5.5 Local invocation

```sh
# Run coverage tests (cargo-llvm-cov produces target/coverage/coverage.json).
cargo llvm-cov nextest --workspace --all-features --locked --no-report
cargo llvm-cov report --json --output-path target/coverage/coverage.json

# Apply the gate.
cargo coverage-gate check
```

Both commands chain naturally and can be wrapped in a single recipe by
whatever task-runner the repo uses (`just`, `make`, `cargo xtask`, …).

## 6. Inputs & Outputs in Detail

### 6.1 The cargo-llvm-cov JSON

`cargo llvm-cov report --json` emits the LLVM coverage JSON v2 schema:

```json
{
  "data": [
    {
      "files": [
        {
          "filename": "/abs/.../crates/alpha/src/lib.rs",
          "summary": {
            "lines":     { "count": 100, "covered": 82, "percent": 82.0 },
            "functions": { "count": 12,  "covered": 11, "percent": 91.67 },
            "regions":   { "count": 140, "covered": 110, "percent": 78.57 }
          }
        }
      ],
      "totals": { ... }
    }
  ]
}
```

The tool reads `data[*].files[*]`. The top-level `totals` is ignored — the
tool computes its own per-crate aggregates so opt-in/opt-out via
`--crates` and `--ignore-filename-regex` (passed to cargo-llvm-cov
upstream) doesn't desync from the displayed verdict.

### 6.2 File-to-crate attribution

For each file path in the JSON, the tool determines which workspace member
owns it by **longest-prefix match** against the workspace members'
canonicalized manifest directories. A file under
`workspace_root/crates/alpha/src/lib.rs` belongs to the member whose
manifest is `workspace_root/crates/alpha/Cargo.toml`.

Files that match no member (typically generated code outside the workspace
tree, or proc-macro expansions with synthesized paths) are dropped with a
single warning per `check` run. They are not folded into any crate's totals.

### 6.3 Aggregation

Per crate, the tool sums `lines.count` and `lines.covered` over every
attributed file, then computes percentage as
`100.0 * sum(covered) / sum(count)`.

A crate that is in the gated set (either listed in `--crates`, or
implicitly via the default of "every workspace member") but has **zero
attributed files** in the coverage JSON is a configuration error: it
means no test binary that touched that crate's source actually ran.
`check` reports such crates in the table as `(no data)` and exits with
code `2`. This converts a silent gap — a typo in `--crates`, a broken
impact tool, a `--ignore-filename-regex` mismatch — into a loud failure
that surfaces immediately in CI.

### 6.4 Cross-crate test attribution

Per-crate aggregation groups measurements by **source-file ownership**
(longest-prefix match against workspace member paths), but the
measurements themselves are produced by **whichever test binaries
executed**. In a workspace those two views diverge: crate `B`'s
integration tests can — and routinely do — exercise crate `A`'s public
API, and the lines marked covered are then attributed to `A` even
though the test that produced them lives in `B`.

This matters at PR time under impact scoping. Consider:

1. **PR1** modifies `A` and `B`. The impact-scoped run includes both,
   so `B`'s integration tests run and incidentally cover much of `A`.
   `A`'s measured percentage is high; the author commits a high
   `min-lines` for `A`.
2. **PR2** modifies only `A`. If the impact-scoped run includes only
   `A`'s own tests, `A`'s measured percentage drops sharply — not
   because `A` regressed, but because the test binary that contributed
   most of `A`'s coverage didn't run. The gate fails for reasons the
   author cannot fix in this PR.

The fix is a **contract on the impact tool**, not on the gate:

> **For every crate `X` in the impacted set, the impact tool must also
> include every crate that depends on `X` (directly or transitively,
> through normal, dev, and build dependencies), so that every test
> binary capable of exercising `X` runs.**

This is the reverse-dependency closure. Most impact tools — including
`cargo-delta` — already compute it, because they have to in order to
catch downstream breakage from API changes. The argument that it
suffices for coverage is the same: the only way another crate's tests
can contribute coverage to `X` is if those tests link against `X`,
which requires a (transitive) dependency edge from that crate to `X`.
The reverse-dep closure includes every such crate by construction, so
the set of test binaries that exercise `X` is the same in PR1 and PR2,
and so is `X`'s measured percentage.

`check` cannot verify the contract directly — the coverage JSON
doesn't record which test binaries ran. The mitigation is the §6.3
rule above: a crate listed in `--crates` but with no attributed files
is treated as a configuration error, so the most common
contract-violating shape (the impact tool omits a crate's
reverse-dep that owns the only test binary covering it) surfaces as a
hard failure rather than a quietly mis-gated number.

Repos that do not impact-scope their coverage runs are unaffected:
running every test binary every time trivially satisfies the contract.

One residual case is intentional, not a bug: a PR that *removes* a
test in `B` which had been covering parts of `A` will cause `A`'s
measured percentage to drop, and the gate will fail on `A`. This is
exactly the displaced-coverage case the gate is designed to catch
(§1.2). The author either restores equivalent coverage, lowers `A`'s
threshold in the same PR (visible in the diff), or finds another way
to cover the affected paths.

### 6.5 Markdown output

Markdown rendering uses GitHub-flavored tables, which both
`$GITHUB_STEP_SUMMARY` and ADO's `task.uploadsummary` render correctly:

```markdown
### ox coverage-gate

| Crate         | Lines  | Threshold | Δ vs threshold | Status | Source     |
|---------------|-------:|----------:|---------------:|:------:|:-----------|
| crates/alpha  | 82.1%  | 80.0%     | +2.1pp         | ✅     | crate      |
| crates/beta   | 74.5%  | 80.0%     | −5.5pp         | ❌     | workspace  |
| crates/gamma  | 91.0%  | 100.0%    | −9.0pp         | ❌     | default    |

**Result:** 2 crates below threshold.
```

## 7. Threshold Lifecycle

### 7.1 Adoption

A new repo adopts the gate in one of two ways:

- **Start strict, opt out as needed.** Do nothing: every crate inherits
  the built-in default of `100.0` and the first `check` run fails
  loudly. The maintainer either sets a `[workspace.metadata.coverage-gate]`
  default for the repo, adds per-crate `min-lines` overrides, or
  explicitly opts crates out with `min-lines = 0.0`. The whole picture
  is visible in the same PR.
- **Seed from observed.** Run coverage once
  (`cargo llvm-cov nextest --no-report && cargo llvm-cov report --json …`),
  then `cargo coverage-gate init` to populate per-crate
  `[package.metadata.coverage-gate]` with values seeded from observed
  coverage (`floor((observed - margin) * 10) / 10`). The maintainer
  commits the resulting `Cargo.toml` diffs.

From that point on, every PR runs `check` and gates.

### 7.2 Intentional improvement

After a PR meaningfully improves coverage in some crate, the maintainer
ratchets the threshold up by editing the relevant `min-lines` value in
`Cargo.toml` (per-crate or workspace-level). The change appears in the
PR diff and is reviewed alongside the code that justifies it.

There is intentionally no automated "ratchet" command: every threshold
change — up or down — is a code-reviewed edit, so the policy and the
code that satisfies it land together and stay together in the git log.

### 7.3 Regression

A PR that drops a crate's coverage below its threshold fails the
`check` step. The author either:

- Improves test coverage in that crate to clear the threshold, **or**
- Edits `min-lines` downward in the same PR, making the lowered floor
  visible in the PR diff. The reviewer judges whether the lowering is
  acceptable.

There is no mechanism for "temporarily skip this gate" — the second option
is the intentional way to bypass, and it leaves a permanent record.

### 7.4 Crate addition and removal

A new crate added in a PR inherits the workspace default (or the
built-in `100.0` if no workspace default is set). To set a different
floor, the same PR adds `[package.metadata.coverage-gate]` to the new
crate's `Cargo.toml`. A removed crate's threshold disappears with its
`Cargo.toml`; the gate ignores it automatically.

## 8. CI Integration

### 8.1 GitHub Actions

The intended call site is from a reusable workflow that already publishes
the lcov / cobertura artifacts. After the test step:

```yaml
- name: Coverage gate
  shell: bash
  run: cargo coverage-gate check --crates "$IMPACTED_CRATES"
```

`$IMPACTED_CRATES` is whatever comma-separated list the surrounding
pipeline produces from its test-impact step (e.g., from `cargo-delta`
or an equivalent). If you don't do impact scoping, drop the `--crates`
flag and gate every workspace member every run.

The job picks up `$GITHUB_STEP_SUMMARY` automatically and writes the
verdict table to the workflow-run page above the job log.

### 8.2 Azure DevOps

```yaml
- bash: |
    summary="$(mktemp).md"
    cargo coverage-gate check \
        --crates "$(IMPACTED_CRATES)" \
        --summary-file "$summary"
    echo "##vso[task.uploadsummary]$summary"
  displayName: Coverage gate
```

The summary file is uploaded as a tab on the build run, alongside the
existing "Code Coverage" tab. The job's exit code is set by
`coverage-gate check`, so the build fails when any crate is below
threshold.

### 8.3 Coexistence with native UIs

The tool gates; Codecov and ADO Coverage UI continue to handle navigation
(which lines, which files, trends). They may show warnings on
workspace-aggregate drops the tool doesn't gate against; users who want
those warnings gating set the corresponding status check as required in
branch protection / branch policy. The gate is the source of truth for
the per-crate verdict; the native UIs are the source of truth for
line-level navigation.

## 9. Customization

Three escape valves, in increasing severity:

1. **Set, raise, or remove `min-lines`** in a crate's `Cargo.toml`. The
   normal flow.
2. **Limit scope with `--crates`** to gate only a subset of crates per
   run. Useful for impact-scoped builds; the CI templates use this by
   default.
3. **Stop running the gate.** Remove the `coverage-gate check` step from
   the CI template. The thresholds in `Cargo.toml`s remain as static
   documentation; nothing enforces them.

## 10. Cross-cutting Concerns

### 10.1 Determinism

Per-crate aggregation must produce the same percentage byte-for-byte given
the same JSON input, regardless of file iteration order. The
implementation sorts files by path before aggregation and rounds to one
decimal place (matching cargo-llvm-cov's default text-summary precision)
for display; the underlying comparison uses the unrounded `f64`.

### 10.2 Security

The tool reads `Cargo.toml` files and a coverage JSON file. It writes
`Cargo.toml` files only when `init` is invoked. No network
access, no shell-out, no privileged operations.

### 10.3 Monorepo / multi-workspace

v1 supports one workspace per invocation, located by walking up from
CWD to the nearest `Cargo.toml` with a `[workspace]` table. Repos with
multiple workspaces invoke the tool once per workspace.

### 10.4 Versioning of the JSON schema

LLVM coverage JSON has a `type` and `version` field at the top level. The
tool accepts `version: "2.0.x"` (the current schema as of llvm-tools
1.93). Newer schemas: refuse to run with a clear error pointing at the
expected version. Older schemas: support is not implemented in v1; users
upgrade `cargo-llvm-cov` to a current release.

### 10.5 Float comparison

Percentage comparisons use a small epsilon (`1e-6`) to avoid "82.0% <
82.0%" failures from floating-point representation of either side. The
displayed value is rounded to one decimal; the underlying compare uses
the raw value plus epsilon.

## 11. Out-of-Scope, Possible Extensions

These are intentionally not part of v1; called out so the design doesn't
preclude them.

- **Function and region thresholds** in parallel to `min-lines`. Same
  data, additive keys, no architectural change.
- **lcov / cobertura input** in addition to the cargo-llvm-cov JSON. lcov
  has the data we need but a different schema; small wrapper if a real
  need surfaces.
- **Per-file thresholds.** Rejected for v1: at the file level, thresholds
  become noisy and brittle (renames, splits, new files). The crate is
  the right granularity unit.
- **Reporting the absolute number of lines** (covered/total) alongside
  the percentage. Useful for small crates where 1 line moves the
  percentage by 10pp. Could be a `--verbose` flag.
- **Optional baseline JSON for delta display**. The gate is comparing
  observed vs. threshold, not observed vs. previous-run, by design.
  Trend reporting is the dashboard's job, not the gate's.

[cargo-llvm-cov]: https://github.com/taiki-e/cargo-llvm-cov
