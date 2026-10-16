# cargo-coverage-gate — Design

> Status: **Draft**.
> Crate name: `cargo-coverage-gate`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

PR-time coverage gating in Rust workspaces is hard for three reasons:

1. **Global thresholds are wrong for delta builds.** With impact-scoping (e.g.
   cargo-delta) a PR runs tests only for affected packages, so the overall
   coverage percentage is computed against a different denominator than the
   nightly run on `main`. A meaningful comparison requires the same denominator.
2. **Diff coverage misses real regressions.** A PR showing 90% diff coverage
   can still drop overall coverage if the displaced code was at 100%. The "did
   this PR move the codebase backwards?" question isn't answered.
3. **External services don't always reach.** Codecov is unavailable for many
   Microsoft-internal repos; Azure DevOps' native coverage UI shows totals but
   has no built-in gating mechanism that correlates with the source of truth.

The unifying observation: **the right unit for coverage policy is the package**,
not the diff, not the workspace. each package has a stable set of files and a
stable measured percentage; thresholds attached to packages remain comparable
across full and impact-scoped builds, and they catch the displaced-code case
because the per-package number drops when high-coverage code is replaced with
lower-coverage code.

## 2. Goals

1. **One opinionated gating mechanism** that works identically locally, on
   GitHub Actions, and on Azure DevOps Pipelines — no external service
   dependency.
2. **per-package thresholds** stored next to the code they describe
   (`[package.metadata.coverage-gate]` in each `Cargo.toml`).
3. **Delta-build safe**: the gate accepts a subset of packages to check, so PR
   builds running impact-scoped tests gate only against the packages whose
   tests actually ran. Each comparison stays apples-to-apples.
4. **Single visible verdict** across local CLI output, GitHub workflow
   summary, and ADO build summary tab. Reviewers see the same per-package
   table no matter where they look.
5. **Threshold lifecycle is git-native**: thresholds change via Cargo.toml
   edits that show up in PR diffs — no hidden state, no separate baseline
   service.
6. **Portable collection orchestration**: an optional mode runs the supported
   cargo-llvm-cov/nextest sequence and evaluates the result, so repositories do
   not carry platform-specific coverage scripts.
7. **Released-surface measurement**: tests still execute and cover production
   code, but source that can only belong to test/example/bench/build-script
   targets, plus item regions that provably require `cfg(test)`, never enters
   the denominator or the LCOV report `run` publishes.
8. **Open source**: ships from `github.com/microsoft/ox-tools` to crates.io.

## 3. Non-Goals

- Replacing cargo-llvm-cov, nextest, or LLVM. The optional `run` mode
  orchestrates those tools and owns their portable handoff, but coverage
  instrumentation, test execution, and raw export remain their implementations.
- Diff coverage (per-line annotation of changed lines). Different concern,
  different consumers (review tooling like Codecov); this tool answers a
  separable question.
- Uploading coverage to Codecov / ADO Coverage UI. The surrounding CI owns the
  upload step. Integrations should upload the byte-exact filtered LCOV files
  `run` publishes and evaluates.
- Cross-crate aggregation policies (e.g. "workspace total must stay above
  N"). If a repo wants that, run the existing
  `cargo llvm-cov --fail-under-lines` flag in parallel; the two are
  independent.
- Mixed-language coverage (C/C++ + Rust). `cargo-llvm-cov` doesn't support
  it either; repos that need it switch to `grcov` and out of this tool's
  scope.

## 4. Guiding Principle

> **one package, one number, one threshold, one verdict.**

Corollaries:

- Each workspace member gets a base policy via the three-layer resolution
  (per-package → workspace → built-in `100.0`). A matching package-scoped
  target table replaces that base policy to produce the effective policy.
  Opting a package out of gating is explicit: set `min-lines-percent = 0.0`.
  A package that legitimately has **no coverable lines** instead declares
  `expect-no-coverable-lines = true` — a self-validating assertion that
  passes while the package stays empty and fails if coverable lines
  appear (see §5.3).
- The gate compares **measured percentage for package X against the threshold
  for package X**, in isolation. There is no cross-package dependency.
- Whether the build that produced the coverage data ran one package's tests
  or all of them does not affect the verdict logic — only which packages are
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
cargo coverage-gate [EVALUATION OPTIONS]
cargo coverage-gate run [SELECTION] [COLLECTION OPTIONS] [EVALUATION OPTIONS]
```

The bare command retains its existing invocation shape. In 0.7 it filters one
or more cargo-llvm-cov lcov tracefiles to the released surface before resolving
policy, computing per-package percentages, and emitting a verdict.

`run` first collects coverage through cargo-llvm-cov and nextest, then calls the
same evaluator in-process. Exit `0` if collection succeeds and every in-scope
package meets policy; exit `1` for a policy failure; exit `2` for invalid
configuration, missing tools, failed collection/export, or malformed coverage.

Flags:

- `--lcov <path>` — path to a cargo-llvm-cov lcov tracefile. May be
  repeated (`--lcov a.info --lcov b.info`); the tracefiles are merged at
  the line level after each is filtered to released Cargo lib/bin source
  (line sets combined, coverage retained when any input has a hit) before
  gating, so multiple feature-config exports (`--all-features`,
  `--no-default-features`) can be gated together without a separate,
  platform-specific merge step (`lcov -a` is Linux-only). Defaults to a
  single `target/coverage/lcov.info` when omitted (matching the
  recommended `cargo llvm-cov report --lcov --output-path <path>`
  invocation).
- `-p` / `--package <spec>` — restrict the operation to one or more
  package selectors. Accepts the same idiom as `cargo build`: repeat
  the flag (`-p foo -p bar`) and/or use Unix shell glob patterns
  (`-p 'tokio-*'`, `-p '*macros'`). Default: every workspace
  member. CI integrations pass the impacted-package list from their
  test-impact step so that impact-scoped runs only gate the packages
  whose tests actually ran. A selector that matches no member is a
  configuration error (exit 2). A malformed glob is reported separately as
  invalid selector syntax rather than as a valid selector with no matches.
- `--target <triple>` — collect coverage and evaluate target-specific package
  policies for the supplied Rust target. For standalone evaluation, omission
  resolves the rustc host only when target policies exist. `run` always
  resolves the rustc host when omitted and passes it explicitly to collection.
- `--summary-file <path>` — write a Markdown verdict table to this file.
  When unset, the tool honors the environment variables
  `GITHUB_STEP_SUMMARY` (GitHub Actions) and
  `COVERAGE_GATE_SUMMARY` (any CI that pipes the file content through
  `##vso[task.uploadsummary]` or equivalent) automatically.
- `--quiet` — suppress stdout output (the summary file, if any, is still
  written). In `run` mode this also suppresses stdout inherited or forwarded
  from Cargo, cargo-llvm-cov, nextest, and LLVM. Stderr diagnostics and
  operational errors remain visible.

The tool never writes to `Cargo.toml`. All threshold values are set by
hand, so every change appears in a PR diff and is reviewed.

`run` additionally accepts:

- `--configuration <all-features|no-default-features>` — feature
  configuration to collect. Repeatable; when omitted, `run` collects both
  configurations and merges them through the existing multi-LCOV evaluator.
- `--jobs <N>` — positive concurrency forwarded consistently to nextest's
  build and test execution.
- `--coverage-dir <path>` — artifact directory, default
  `target/coverage`. Each configuration gets a distinct released-surface LCOV
  file. Those exact files feed evaluation and are the intended CI upload
  inputs.
- `--no-coverage-target <triple>` — explicitly run plain nextest without
  coverage collection or gating when the effective target matches. Repeatable;
  the default list is empty. The effective target is the explicit `--target`
  or the rustc host resolved by `run`.

Both instrumented cargo-llvm-cov/nextest runs and plain-nextest no-gate runs
always pass `--locked`. Collection therefore measures the dependency graph
recorded by the committed `Cargo.lock` and fails rather than updating it.

The package selection controls which tests run and which packages are gated.
Every selected package is instrumented regardless of its effective policy:
all-zero and mixed selections use the same cargo-llvm-cov path as positive
thresholds because their tests may cover source owned by another selected
package. Plain nextest is used only for an explicit
`--no-coverage-target` match. That path covers every selected package and
requested feature configuration, emits explicit no-coverage and no-gate
diagnostics, and creates no LCOV output.

Released-surface discovery remains workspace-wide even with package selection:
every workspace target graph is validated before reports are filtered. A source
layout the filter cannot represent therefore fails explicitly even when its
package is not selected for the verdict.

### 5.3 Policy metadata

A package's base policy is resolved in three layers, in priority order:

1. **per-package**: `[package.metadata.coverage-gate]` in the package's
   `Cargo.toml`.
2. **Workspace default**: `[workspace.metadata.coverage-gate]` in the
   root `Cargo.toml`. Applies to every member without per-package
   metadata.
3. **Built-in default**: `min-lines-percent = 100.0`. Applied when neither of
   the above is present.

```toml
# per-package (highest priority):
[package.metadata.coverage-gate]
min-lines-percent = 75.0
```

```toml
# Workspace-wide default (applies to all members that don't override):
[workspace.metadata.coverage-gate]
min-lines-percent = 80.0
```

The base schema accepts `min-lines-percent`, an integer or float percentage
(`0.0`–`100.0` inclusive), or the package-only
`expect-no-coverable-lines` assertion described below. Future extensions can
add `min-functions` and `min-regions` symmetrically.
Workspace metadata additionally accepts the source-classification
`test-attributes` list described below.

The built-in default of `100.0` means **gating is on by default**: a new
package with no metadata anywhere will only pass if every measured line is
covered. To opt a package out of gating, set `min-lines-percent = 0.0` explicitly
(at the package or workspace level). There is no implicit opt-out.

#### Qualified test attributes

The compiler-owned `#[test]` and `#[bench]` attributes are always test-only.
Qualified or custom attribute macros are never inferred from their final name:
an attribute such as `#[company::test]` may generate released code.

Workspaces explicitly list framework attributes that are known to expand to
test-only functions:

```toml
[workspace.metadata.coverage-gate]
test-attributes = ["my_runtime::test"]
```

The entries are exact Rust attribute paths. The list is workspace-scoped so a
source file shared by multiple packages receives one classification;
released-surface filtering rejects package-scoped `test-attributes` as a
configuration error. The explicit `evaluate_unfiltered*` escape hatch does not
consume or validate this filtering-only key. There are no qualified built-in
defaults.

#### `expect-no-coverable-lines`: empty crates vs. opt-out

`min-lines-percent = 0.0` is a deliberately loose opt-out: "I don't
require any coverage for this package." It says nothing about whether the
package *has* coverable code — it always passes, and it suppresses the
no-data configuration error (§6.3) so a package no test binary touches
still passes. That conflates two distinct intents:

- **"This package is not covered by tests"** — it has real, executable
  code, we simply choose not to gate it. `min-lines-percent = 0.0`.
- **"This package has no coverable lines at all"** — pure re-exports,
  trait/type definitions, a thin `main` shim, a build-script-only crate.
  There is genuinely nothing to cover, and that fact should be asserted
  and re-checked, not silently waved through.

For the second intent, declare it explicitly:

```toml
[package.metadata.coverage-gate]
expect-no-coverable-lines = true
```

Semantics:

- The package **passes** as long as it has zero coverable lines (which
  includes the "no attributed coverage data at all" case — that is the
  expected state, not the §6.3 configuration error).
- If coverable lines ever appear, the package **fails the gate**
  (exit `1`), exactly like a coverage regression. The author either
  removes the code, or — if the new code is meant to stay — replaces the
  assertion with a real `min-lines-percent` floor in the same PR. The
  change is visible in the diff and reviewed.

This is the key difference from `min-lines-percent = 0.0`: the assertion
is **self-validating**. `0.0` keeps passing forever no matter what the
package grows into; `expect-no-coverable-lines = true` actively re-checks
the claim on every run.

Rules:

- `expect-no-coverable-lines` is **package-scoped only**. Setting it to
  `true` in `[workspace.metadata.coverage-gate]` is a configuration
  error (exit `2`) — a workspace-wide "no coverable lines" default is
  nonsensical. (An explicit `expect-no-coverable-lines = false` there is
  accepted and ignored, since `false` is everywhere equivalent to
  omitting the key.)
- It is **mutually exclusive** with `min-lines-percent` on the same
  package: setting both is a configuration error (exit `2`). A numeric
  floor describes code that should be covered; the assertion declares
  there is no such code.
- A non-boolean value is a configuration error (exit `2`). An explicit
  `false` is identical to omitting the key.

#### Target-specific policy

Packages whose coverage policy varies by compilation target use Cargo-style
target selectors nested under package metadata:

```toml
[package.metadata.coverage-gate]
min-lines-percent = 100

[package.metadata.coverage-gate.target.'cfg(not(windows))']
expect-no-coverable-lines = true
```

Target keys use the target-derived subset of Cargo's target-specific dependency
selector grammar: an exact target triple (`x86_64-pc-windows-msvc`) or a quoted
`cfg(...)` expression composed from target configuration options
(`cfg(windows)`, `cfg(target_os = "linux")`,
`cfg(all(unix, target_arch = "x86_64"))`). Coverage-gate parses selectors with
`cargo-platform` and matches cfg expressions against
`rustc --print cfg --target <triple>`. Build-context configuration
options such as `feature`, `test`, `debug_assertions`, and `proc_macro` are
configuration errors because a standalone target query cannot evaluate them.

`min-lines-percent = 0` disables gating for that package on the matching
target. It does not disable tests or coverage instrumentation. Test binaries
from zero-threshold packages remain part of the instrumented run because they
may contribute coverage to other workspace packages. This is distinct from
`expect-no-coverable-lines = true`, which asserts that the selected package
itself owns no coverable lines.

Target tables are package-scoped; declaring one in workspace metadata is a
configuration error. A selected table replaces the base policy with either
`min-lines-percent` or `expect-no-coverable-lines = true`, producing the final
effective policy. The two behaviors are mutually exclusive. An exact target
that overrides a broader cfg policy repeats its desired policy.

Resolution follows Cargo's precedence:

1. An exact target-triple table wins over every matching `cfg(...)` table.
2. Otherwise one matching `cfg(...)` table supplies the target policy.
3. Multiple matching `cfg(...)` tables are a configuration error rather than
   depending on TOML declaration order.
4. With no matching target table, the base package → workspace → built-in
   policy remains effective.

The CLI accepts `--target <triple>`. When omitted, standalone evaluation
obtains the rustc host target from `rustc -vV` only if a package declares
target policies. `run` instead resolves the host once on every omitted-target
invocation, passes that triple explicitly to collection and evaluation, and
reuses the same `rustc -vV` output for nightly validation.

### 5.4 The verdict table

The tool prints a table to stdout (and to the summary file when
configured). The output is deterministic: byte-identical input
produces byte-identical output, so diffs across runs are stable.

```text
coverage-gate

  Package  Lines       Threshold   Δ vs threshold   Status   Source
  ───────  ──────────  ──────────  ───────────────  ───────  ─────────
  alpha    82.1%        80.0%        +2.1pp           OK       package
  beta     74.5%        80.0%        −5.5pp           FAIL     workspace
  gamma    91.0%       100.0%        −9.0pp           FAIL     default
  ───────  ──────────  ──────────  ───────────────  ───────  ─────────
  Result: 2 package(s) below threshold.
```

The `Source` column reports which layer supplied the threshold: `package`,
`workspace`, or `default` (the built-in `100.0`).

The `Status` column is `OK` / `FAIL` for normally-gated packages,
`NO DATA` for a gated package with no attributed coverage data (a
configuration error, §6.3), and — for packages declaring
`expect-no-coverable-lines` (§5.3) — `EMPTY` when the assertion holds
(passing) or `NOT EMPTY` when coverable lines were found (failing). For
the `EMPTY` / `NOT EMPTY` rows the `Threshold` and `Δ vs threshold`
columns render `(no lines)` and `—`, since there is no percentage floor.

Markdown variant uses the same columns and a leading `### coverage-gate`
header so it renders cleanly in GitHub job summaries and ADO build summaries.

When the verdict is not a pass, both renderers append **failure details**.
A **coverable line** is a distinct LCOV `DA:` record: a source line for which
LLVM emitted line-coverage instrumentation, whether or not a test executed it.
For a package below its numeric threshold, the details show exact
covered/coverable line counts and the uncovered package-relative source
locations. For a package that unexpectedly contains coverable lines, they
show those locations instead. A `NO DATA` package gets an explicit statement
that no coverage records were attributed to it.

Locations are ordered by package, file, and line, and contiguous lines are
rendered as ranges. To keep CI logs and summaries bounded, at most the first
100 relevant line locations are shown per package; the renderer reports how
many additional locations were omitted. The aggregate counts always describe
the complete input, not the displayed subset. One hundred locations is large
enough to show several failure clusters while keeping a pathological package
to a few kilobytes of diagnostic output; it should be reevaluated if typical
failures routinely omit the first useful cluster or materially inflate CI
summaries.

The [implementation guide](../implementation.md) records the parser,
attribution, status-selection, ordering, and rendering pipeline behind this
contract.

```text
Failure details:
  beta: 60/100 lines covered; 40 uncovered.
    src/lib.rs: 61-100
  gamma: 91/100 lines covered; 9 uncovered.
    src/lib.rs: 12, 24-27, 83-86
```

The details are part of the gate output rather than delegated to an external
coverage service or a transient CI artifact. A failed local command and a
failed CI stage therefore retain enough information to identify what must be
covered even when a later upload step is skipped.

### 5.5 Local invocation

```sh
# Collect both supported feature configurations and apply the gate.
cargo coverage-gate run
```

Evaluation of externally produced files remains available:

```sh
cargo coverage-gate \
  --lcov target/coverage/lcov-all-features.info \
  --lcov target/coverage/lcov-no-default.info
```

Library callers get released-surface semantics from the ordinary `evaluate*`
and `filter_lcov*` APIs. Callers that intentionally need every attributed LCOV
line, including tests and non-release targets, use the explicitly named
`evaluate_unfiltered*` escape hatch. Unfiltered evaluation is useful for inspecting
instrumentation itself, debugging target ownership, or building a report whose
policy deliberately includes test harness source; it is not the production
gate's default.

### 5.6 Collection sequence

For each requested feature configuration, `run`:

1. resolves one effective Rust target from explicit `--target` or the rustc
   host and uses it for no-coverage matching, test execution, reporting, and
   policy evaluation;
2. allocates a fresh private coverage target for each feature configuration
   beneath a unique per-invocation scratch directory;
3. invokes cargo-llvm-cov with nextest, `--no-report`, `--no-tests=pass`, and
   `--locked` for the selected package set;
4. invokes `cargo llvm-cov report --lcov` for the same package and target
   selection, delegating raw-profile merging, ordinary and nested trybuild
   object discovery, and the complete default filename exclusions to
   cargo-llvm-cov;
5. reconstructs Cargo target ownership from metadata and Rust module
   declarations, removes non-release target sources and provably test-only
   line records, omits function and branch records because ownership proof is
   line-granular, and fails explicitly if a source cannot be resolved, read,
   parsed, or safely owned;
6. atomically publishes the filtered report to its stable consumer path under
   `--coverage-dir` and passes that exact path to the evaluator.

The selected package set is resolved to exact `name@version` specs before
subprocess execution. With no selection options, the collector passes
`--workspace`.

Feature configurations and concurrent invocations never share instrumented
build/profile state: each configuration uses
`target/coverage-gate/run-<pid>-<nonce>/cargo-target/<configuration>` (under
Cargo's resolved target directory). A private target starts empty, so `run`
does not invoke `cargo llvm-cov clean`; it removes only the enclosing
per-invocation scratch directory on success or failure. Shared cargo-llvm-cov
HTML/text reports, trybuild targets, UI test targets, and ordinary Cargo target
state are never cleanup inputs. `CARGO_TARGET_DIR` is set to the same private
configuration directory as cargo-llvm-cov's target and build overrides, so
metadata-derived trybuild, UI, and report paths are private as well.

LCOV consumer paths under `--coverage-dir` remain stable. Instrumented-state
isolation does not provide concurrent-writer semantics for those paths:
callers must not run simultaneous invocations against the same
`--coverage-dir`.

Scratch cleanup has explicit result precedence. An evaluation error remains the
primary error and carries any cleanup failure as additional context. A rendered
policy failure keeps exit `1`; cleanup failure is reported as a warning instead
of replacing the verdict. A passing evaluation cannot claim complete success
when cleanup fails, so that case exits `2`. Successful cleanup never changes
the evaluation outcome.

cargo-llvm-cov normally launches LLVM with its discovered object set and full
filename filter. If that launch exceeds the Windows command-line limit,
`run` retries through an LLVM response file. The `cargo llvm-cov report` child
has `MSYSTEM` removed so cargo-llvm-cov renders the failed command with the
Windows argument dialect. `run` parses that diagnostic as data, validates that
it describes an `llvm-cov export`, and re-quotes each argument for LLVM's
Windows response-file tokenizer; it never executes the diagnostic as shell
syntax. This preserves upstream object discovery and exclusions instead of
maintaining a second, necessarily incomplete implementation.

The exceptional response-file export writes to per-invocation scratch and
captures stderr. A successful export and the same complete paired
no-coverage-data conversion used by the ordinary report path both pass through
released-surface filtering before stable publication. Every other fallback
failure removes temporary files, leaves any previously completed stable
artifact unchanged, and exits `2`.

Failure in any collection phase is operational failure, not missing coverage,
and exits `2`. A successful, structurally valid empty LCOV export continues to
the evaluator so zero-threshold and `expect-no-coverable-lines` policies retain
their existing meaning. When LLVM reports the complete paired no-coverage-data
diagnostic for discovered objects with no coverage maps, `run` writes the same
valid empty LCOV result and continues. Missing profiles, failed object
discovery, and every other cargo-llvm-cov failure remain operational errors.

## 6. Inputs & Outputs in Detail

### 6.1 The cargo-llvm-cov lcov tracefile

`cargo llvm-cov report --lcov` emits the [LCOV tracefile][lcov]
format. The tool reads its per-source-file sections:

```text
TN:
SF:/abs/.../crates/alpha/src/lib.rs
DA:1,5
DA:2,3
DA:3,0
...
LF:100
LH:82
end_of_record
```

Before counting, the CLI and ordinary `evaluate*` APIs filter every section to
the released Cargo surface:

- target roots classified as libraries (`lib`, `rlib`, `dylib`, `cdylib`,
  `staticlib`, and `proc-macro`) or `bin` are released roots;
- test, example, bench, custom-build, unknown, and every other target kind are
  non-release roots;
- out-of-line modules are followed through normal Rust resolution, inline
  module directories, and `#[path = "..."]`;
- conditional module paths expressed through `cfg_attr(..., path = "...")`
  are rejected explicitly because selecting the released file requires the
  complete Cargo build configuration;
- a file reached by both classes remains released;
- within a released file, `#[test]`, `#[bench]`, `#[cfg(test)]`, and
  predicates such as `#[cfg(all(test, unix))]` are removed, while
  `cfg(any(test, feature = "test-util"))`, `cfg(feature = "test-util")`, and
  `cfg(not(test))` remain. Correlated occurrences of up to twelve distinct
  non-`test` cfg atoms are evaluated exactly; larger or unsupported predicate
  shapes fall back to conservative retention rather than hiding released code.

Filtering removes matching line records so downstream coverage UIs see the same
released line surface as the gate. Function and branch records are omitted
entirely because the proof is line-granular. A source/path/parse state the tool
cannot classify safely is exit `2`, not a silent exclusion.

For each remaining `SF:` section the tool counts:

- `lines_total` — number of distinct `DA:` records (executable lines
  the instrumentation knows about).
- `lines_covered` — number of those with a non-zero hit count.
- The line numbers of all `DA:` records and of the zero-count subset,
  retained for failure diagnostics.

Function (`FN:`/`FNDA:`) and branch (`BRDA:`) records are removed from filtered
LCOV. The filter's proof is line-based and cannot safely disambiguate mixed
released/test-only syntax that shares one source line. The resulting artifact
is deliberately line-coverage-only, matching the gate's policy surface.

#### Multiple tracefiles

`--lcov` may be supplied more than once. The tool filters each tracefile
independently, then parses and merges them at the line level **before** computing per-package
aggregates: the `DA:` line set is the union across inputs, and a line is
`covered` if it was hit in *any* input. Execution-count magnitudes are not
combined because the gate consumes only the covered/uncovered predicate.
When `run` collects multiple configurations, malformed raw LCOV diagnostics
identify the feature configuration and scratch path that failed validation.
This produces the same line-coverage result as the merge `cargo-llvm-cov`
performs internally on its `.profraw` set, so passing the `--all-features` and
`--no-default-features` exports here yields the same per-package line
coverage as a single merged report. The motivation is to avoid a separate
merge step in the producing recipe: `cargo-llvm-cov`'s union *report*
step shells one `--object=` per test binary and overflows the Windows
`CreateProcess` command-line limit on large workspaces, while the
Linux-only `lcov -a` merger is not portable. Letting the gate consume the
two per-config exports directly sidesteps both.

#### Why lcov, not the JSON

`cargo-llvm-cov` can also export the same data as JSON v2/v3, cobertura
XML, or Codecov's custom JSON. We pick lcov because it matches every
other coverage UI fed by the same data:

- Codecov ingests lcov uploads directly.
- ADO's coverage task can ingest lcov directly.
- `cargo llvm-cov report --codecov` emits Codecov's custom JSON, also
  derived from lcov.

The JSON export uses a stricter line-coverage semantics ("every region
on a line must be hit"); lcov uses the lenient "any region hit" rule
that everything else understands. Picking lcov keeps the gate's
numbers aligned with what adopters see in codecov / ADO when they
calibrate their `min-lines-percent` thresholds.

The tool computes its own per-package aggregates so that
`--package` filtering, plus any `--ignore-filename-regex` passed
upstream to cargo-llvm-cov, doesn't desync from the displayed verdict.

[lcov]: https://github.com/linux-test-project/lcov

### 6.2 File-to-package attribution

Released-surface filtering requires every `SF:` path to be reachable from a
Cargo target root through supported module declarations. A source reached only
from a non-release target is removed. An unresolvable, unreadable, unparseable
source, a source with no Cargo target owner, or a stale source reference is a
configuration/operational error (exit `2`) rather than a counter the tool
silently hides. Explicit `evaluate_unfiltered*` APIs bypass this filtering.

The module graph does not infer ownership through `include!`, macro-generated
module declarations, or build-script-generated source inclusion. If such a
path appears in LCOV without a supported Cargo target or module edge, filtering
fails with the offending source path rather than hiding its counters.
Additionally, a statically recognizable non-test built-in `include!`
encountered while walking a released source fails immediately: a separate
test-target edge to the included file must not make released counters look
test-only.

Rust macro expansion is not available to the source walker. A module generated
by `macro_rules!`, a procedural macro, or a renamed `include!` can therefore be
classified as non-release when the same file is also reached through a
supported non-release edge. Repositories using those shared generated-source
patterns must use explicit `mod` declarations for released-surface filtering,
or choose the unfiltered library API.

Canonical source identity is kept separate from Rust's module-resolution
location. If a Cargo target or module source is reached through a file symlink,
child modules resolve from the declared file name in its resolved parent
directory, rather than from the canonical file target. The canonical file
remains the identity used for deduplication and workspace ownership.

For each retained `SF:` path, the tool canonicalizes the source and determines
which workspace member owns it by **longest-prefix match** against canonical
workspace-member directories. It then writes the path relative to the owning
member's directory as reported by `cargo metadata`, so the filtered LCOV and
policy attribution use the same path namespace. A file under
`workspace_root/crates/alpha/src/lib.rs` belongs to the member whose
manifest is `workspace_root/crates/alpha/Cargo.toml`.

During Cargo-target/module graph construction, every released source is
validated against workspace-member directories even if no tracefile mentions
it. Released files outside every member are rejected because the package policy
they should affect cannot be determined safely. Separately, upstream
cargo-llvm-cov filename exclusions should keep dependency/sysroot sources out
of the report; if one leaks through without a supported graph edge, the gate
fails loudly with the offending path.

### 6.3 Aggregation

Per package, the tool sums `lines.count` and `lines.covered` over every
attributed file, then computes percentage as
`100.0 * sum(covered) / sum(count)`.

A package that is in the gated set (either listed in `--package`, or
implicitly via the default of "every workspace member") but has **zero
attributed files** in the lcov tracefile is a configuration error: it
means no test binary that touched that package's source actually ran.
The tool reports such packages in the table as `(no data)` and exits
with code `2`. This converts a silent gap — a typo in `--package`,
a broken impact tool, a `--ignore-filename-regex` mismatch — into a
loud failure that surfaces immediately in CI.

The exception is a package that declares `expect-no-coverable-lines =
true` (§5.3): for that package, zero attributed lines is the *expected*
state and classifies as a pass (`EMPTY` / `➖`), not the no-data
configuration error. Conversely, if such a package *does* have attributed
coverable lines, it fails the gate (exit `1`) rather than passing.

A package whose effective target policy sets `min-lines-percent = 0` passes,
including when it has no attributed data. Its tests remain instrumented so
they can contribute coverage to other packages.

An empty lcov tracefile is valid input. Each in-scope package is classified
from its effective policy: zero-threshold and `expect-no-coverable-lines`
packages pass, while a package with a positive threshold reports `NO DATA`
and makes the result a configuration error. When every discovered object lacks
a coverage map, LLVM exits before cargo-llvm-cov can write that valid empty
report, with the paired diagnostics `no coverage data found` and
`could not load coverage information`. `run` recognizes only that complete
case, writes an empty LCOV file, and evaluates it normally. It does not remove
test binaries from instrumentation preemptively; if those tests produce
coverage for another package, the normal export succeeds and that coverage
remains visible.

### 6.4 Cross-package test attribution

Per-package aggregation groups measurements by **source-file ownership**
(longest-prefix match against workspace member paths), but the
measurements themselves are produced by **whichever test binaries
executed**. In a workspace those two views diverge: package `B`'s
integration tests can — and routinely do — exercise package `A`'s public
API, and the lines marked covered are then attributed to `A` even
though the test that produced them lives in `B`.

This matters at PR time under impact scoping. Consider:

1. **PR1** modifies `A` and `B`. The impact-scoped run includes both,
   so `B`'s integration tests run and incidentally cover much of `A`.
   `A`'s measured percentage is high; the author commits a high
   `min-lines-percent` for `A`.
2. **PR2** modifies only `A`. If the impact-scoped run includes only
   `A`'s own tests, `A`'s measured percentage drops sharply — not
   because `A` regressed, but because the test binary that contributed
   most of `A`'s coverage didn't run. The gate fails for reasons the
   author cannot fix in this PR.

The fix is a **contract on the impact tool**, not on the gate:

> **For every package `X` in the impacted set, the impact tool must also
> include every package that depends on `X` (directly or transitively,
> through normal, dev, and build dependencies), so that every test
> binary capable of exercising `X` runs.**

This is the reverse-dependency closure. Most impact tools — including
`cargo-delta` — already compute it, because they have to in order to
catch downstream breakage from API changes. The argument that it
suffices for coverage is the same: the only way another package's tests
can contribute coverage to `X` is if those tests link against `X`,
which requires a (transitive) dependency edge from that package to `X`.
The reverse-dep closure includes every such package by construction, so
the set of test binaries that exercise `X` is the same in PR1 and PR2,
and so is `X`'s measured percentage.

The tool cannot verify the contract directly — the lcov tracefile
doesn't record which test binaries ran. The mitigation is the §6.3
rule above: a package listed in `--package` but with no attributed files
is treated as a configuration error, so the most common
contract-violating shape (the impact tool omits A package's
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
### coverage-gate

| Package | Lines  | Threshold | Δ vs threshold | Status | Source     |
|---------|-------:|----------:|---------------:|:------:|:-----------|
| alpha   | 82.1%  | 80.0%     | +2.1pp         | ✅     | package    |
| beta    | 74.5%  | 80.0%     | −5.5pp         | ❌     | workspace  |
| gamma   | 91.0%  | 100.0%    | −9.0pp         | ❌     | default    |

**Result:** 2 packages below threshold.
```

## 7. Threshold Lifecycle

### 7.1 Adoption

A new repo adopts the gate by enabling `cargo coverage-gate` in CI and
watching the first run fail loudly. every package inherits the built-in
default of `100.0`, so until the maintainer says otherwise, every package
is required to be fully covered.

To shape the policy, the maintainer either:

- sets a `[workspace.metadata.coverage-gate]` default for the repo,
- adds per-package `[package.metadata.coverage-gate]` overrides for
  packages whose realistic floor differs, or
- explicitly opts packages out with `min-lines-percent = 0.0`, or
  declares genuinely empty packages with
  `expect-no-coverable-lines = true` (§5.3).

All three live in `Cargo.toml` files, so the policy lands as a normal
reviewed change. From that point on, every PR runs the gate.

### 7.2 Intentional improvement

After a PR meaningfully improves coverage in some package, the maintainer
ratchets the threshold up by editing the relevant `min-lines-percent` value in
`Cargo.toml` (per-package or workspace-level). The change appears in the
PR diff and is reviewed alongside the code that justifies it.

There is intentionally no automated "ratchet" command: every threshold
change — up or down — is a code-reviewed edit, so the policy and the
code that satisfies it land together and stay together in the git log.

### 7.3 Regression

A PR that drops a package's coverage below its threshold fails the gate.
The author either:

- Improves test coverage in that package to clear the threshold, **or**
- Edits `min-lines-percent` downward in the same PR, making the lowered floor
  visible in the PR diff. The reviewer judges whether the lowering is
  acceptable.

There is no mechanism for "temporarily skip this gate" — the second option
is the intentional way to bypass, and it leaves a permanent record.

### 7.4 package addition and removal

A new package added in a PR inherits the workspace default (or the
built-in `100.0` if no workspace default is set). To set a different
floor, the same PR adds `[package.metadata.coverage-gate]` to the new
package's `Cargo.toml`. A removed package's threshold disappears with its
`Cargo.toml`; the gate ignores it automatically.

## 8. CI Integration

### 8.1 GitHub Actions

The intended call site is from a reusable workflow that already publishes
the filtered lcov artifacts. After the test step:

```yaml
- name: Coverage gate
  shell: bash
  run: cargo coverage-gate $PACKAGE_FLAGS
```

`$PACKAGE_FLAGS` is whatever the surrounding pipeline produces from its
test-impact step (e.g., from `cargo-delta` or an equivalent), formatted
as repeated `-p` arguments (`-p alpha -p beta`). If you don't do impact
scoping, drop the variable and gate every workspace member every run.

The job picks up `$GITHUB_STEP_SUMMARY` automatically and writes the
verdict table to the workflow-run page above the job log.

Coverage services must receive the filtered LCOV files without a second
test-path or `test-util` ignore list. Test-only source is already absent, and a
service-side ignore would make its displayed denominator differ from the local
gate.

The generated Anvil recipe cannot provide that artifact contract until it pins
a published cargo-coverage-gate release containing this filter. During the
0.7.0 publication PR it still uploads raw cargo-llvm-cov output; the follow-up
Anvil rollout must update the pin and switch the gate and uploads to the same
published filtered files.

### 8.2 Azure DevOps

```yaml
- bash: |
    summary="$(mktemp).md"
    cargo coverage-gate \
        $(PACKAGE_FLAGS) \
        --summary-file "$summary"
    echo "##vso[task.uploadsummary]$summary"
  displayName: Coverage gate
```

The summary file is uploaded as a tab on the build run, alongside the
existing "Code Coverage" tab. The job's exit code is set by
`cargo coverage-gate`, so the build fails when any package is below
threshold.

### 8.3 Coexistence with native UIs

The tool gates; Codecov and ADO Coverage UI continue to handle navigation
(which lines, which files, trends). They may show warnings on
workspace-aggregate drops the tool doesn't gate against; users who want
those warnings gating set the corresponding status check as required in
branch protection / branch policy. The gate is the source of truth for
the per-package verdict; the native UIs are the source of truth for
line-level navigation.

## 9. Customization

Three escape valves, in increasing severity:

1. **Set, raise, or remove `min-lines-percent`** in A package's `Cargo.toml`. The
   normal flow.
2. **Limit scope with `--package`** to gate only a subset of packages per
   run. Useful for impact-scoped builds; the CI templates use this by
   default.
3. **Stop running the gate.** Remove the `cargo coverage-gate` step
   from the CI template. The thresholds in `Cargo.toml`s remain as
   static documentation; nothing enforces them.

## 10. Cross-cutting Concerns

### 10.1 Determinism

per-package aggregation must produce the same percentage byte-for-byte given
the same lcov input, regardless of file iteration order. This holds for
free because the aggregation step sums integer line counters (commutative
and associative), and the f64 percentage is computed once at the end.
The displayed measured percentage rounds down to one decimal place.
Configured thresholds display to one decimal place using normal numeric
formatting. Pass/fail evaluation uses the unrounded values so presentation
precision cannot weaken the configured threshold — see §10.5.

### 10.2 Security

Released-surface evaluation reads `Cargo.toml` files, coverage lcov tracefiles,
and reachable Rust sources for canonicalization, module discovery, parsing,
and line classification; explicit `evaluate_unfiltered*` APIs do not inspect
Rust sources. Evaluation writes only the selected summary file. `run`
additionally writes beneath
`--coverage-dir`, executes cargo-llvm-cov, nextest, Cargo, rustc, and LLVM, and
deletes only temporary response/staging files and its per-invocation coverage
scratch. It never invokes workspace-wide cargo-llvm-cov cleanup.
cargo-coverage-gate itself performs no network calls or privileged operations.
Its Cargo and nextest children follow the caller's Cargo configuration and may
fetch locked dependencies when they are not cached.
The effective target is the deliberate exception: `run` always supplies
`--target <triple>`, so `CARGO_BUILD_TARGET` and Cargo `build.target`
configuration cannot select a different collection target. Callers that
require network isolation must configure Cargo offline mode outside this tool.

Workspace discovery invokes the read-only `cargo metadata` command through
`cargo_metadata::MetadataCommand::exec()` to enumerate workspace members and
resolve the workspace root. When any package declares target-specific policy,
evaluation also invokes the executable selected by `RUSTC` (or `rustc` when
unset). It runs `rustc -vV` when it must discover the host target, then runs
`rustc --print cfg --target <triple>` for both explicit and discovered targets.
When `--target` is omitted, `run` always reads the host triple once and reuses
that version output for instrumentation validation. It passes the resulting
triple explicitly to instrumented nextest, `cargo llvm-cov report`, plain
nextest on a `--no-coverage-target` match, and policy evaluation.

Child processes receive arguments directly rather than through a shell.
Package selectors are resolved against workspace metadata before execution;
they are never interpreted as command fragments. Collection uses the
environment's effective Cargo and rustc and inherits `RUSTUP_TOOLCHAIN`,
`PATH`, and other caller configuration unchanged, except that Windows removes
`MSYSTEM` from the `cargo llvm-cov report` child to make its diagnostic
argument dialect deterministic.

### 10.3 Monorepo / multi-workspace

v1 supports one workspace per invocation, located by walking up from
CWD to the nearest `Cargo.toml` with a `[workspace]` table. Repos with
multiple workspaces invoke the tool once per workspace.

### 10.4 lcov tracefile compatibility

The LCOV tracefile format has no version number. It is a line-oriented
record format that has been stable across the LCOV project for over a
decade, with new record types added over time (function `FN:` /
`FNDA:`, branch `BRDA:`, etc.). The line gate reads `SF:` and `DA:`.
Released-surface publication removes function and branch records because its
ownership proof is line-granular. Summary records are recomputed when filtered
LCOV is serialized.

If a tracefile contains no `SF:` sections, every in-scope package is evaluated
as having no attributed data. The effective package policies determine the
result as described in §6.3. Structural parse errors (e.g., malformed `DA:`
records) are hard errors with exit code 2.

The released-surface default is an intentional public-API break: the next
publication containing this design must be `0.7.0`, not another `0.6.x`
release. Existing library callers that deliberately relied on raw LCOV
attribution migrate from `evaluate*` to the corresponding
`evaluate_unfiltered*` function. CLI behavior and the ordinary `evaluate*`
family then share one
opinionated production-coverage contract.

#### Tooling requirements

To get faithful numbers, run the tracefile-producing step on **nightly
Rust with `cargo-llvm-cov ≥ 0.9`**. Four reasons:

- `#[coverage(off)]` is gated behind `feature(coverage_attribute)`,
  which is nightly-only. On stable, files annotated with
  `#[cfg_attr(coverage_nightly, coverage(off))]` are still measured
  and intentionally excluded non-testable paths inflate the denominator.
- cargo-llvm-cov 0.6.x did not propagate `#[coverage(off)]` into its
  JSON / lcov output even on nightly. Versions 0.7+ fix this. Older
  versions silently report inflated line counts, which then surface
  as low percentages in the gate.
- cargo-llvm-cov 0.8.0 added support for the exact `name@version` package
  selectors that `run` resolves from workspace metadata and passes to both
  test and report commands.
- cargo-llvm-cov 0.9.0 added `--workspace` support to the `report` subcommand,
  which `run` uses when no explicit package selection is supplied.

`cargo coverage-gate run` enforces these prerequisites before instrumented
collection. Invoke the command through the intended nightly Cargo/toolchain;
the collector honors inherited `CARGO`, `RUSTC`, `RUSTUP_TOOLCHAIN`, `PATH`,
and related configuration except that its explicit effective `--target`
overrides Cargo build-target configuration. Only an explicit
`--no-coverage-target` match takes the plain-test no-gate path and avoids the
cargo-llvm-cov requirement.

### 10.5 Float comparison

Percentage comparisons use the unrounded measured value:
`pct >= threshold`. Display rounding is presentation-only and never relaxes
the configured floor. In particular, a `100.0` threshold passes only when
every coverable line is covered.

A measured percentage is rounded down to one decimal place directly from the
integer covered/coverable line counts, so binary floating-point representation
cannot push an exact decimal tenth downward. A near-boundary failure remains
visibly below its threshold: `1999 / 2000 = 99.95%` displays as `99.9%` and
fails a `100.0%` threshold. Non-zero deltas whose raw magnitude is below
`0.1pp` retain their direction as `+<0.1pp` or `-<0.1pp`; exact `0.1pp`
boundaries, including values within `1e-12pp` of that boundary to account for
binary floating-point representation, use ordinary one-decimal rendering.
Only an exact match displays as `0.0pp`. The status and exact
covered/coverable line counts remain authoritative.

## 11. Out-of-Scope, Possible Extensions

These are intentionally not part of v1; called out so the design doesn't
preclude them.

- **Function and region thresholds** in parallel to `min-lines-percent`. Same
  data, additive keys, no architectural change.
- **cobertura input** in addition to the cargo-llvm-cov lcov tracefile.
  cargo-llvm-cov derives cobertura from the same lcov data, so the
  numbers would be identical; a small parser wrapper if a real need
  surfaces.
- **Per-file thresholds.** Rejected for v1: at the file level, thresholds
  become noisy and brittle (renames, splits, new files). the package is
  the right granularity unit.
- **Reporting the absolute number of lines** (covered/total) alongside
  the percentage. Useful for small packages where 1 line moves the
  percentage by 10pp. Could be a `--verbose` flag.
- **Optional baseline tracefile for delta display**. The gate is comparing
  observed vs. threshold, not observed vs. previous-run, by design.
  Trend reporting is the dashboard's job, not the gate's.

[cargo-llvm-cov]: https://github.com/taiki-e/cargo-llvm-cov
