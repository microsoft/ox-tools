# Check Catalog

The catalog defines the checks exposed by `anvil-<check>` recipes and their
groups. Local and CI runs use the same recipes.
See [local.md](./local.md) for setup, toolchain selection, and configuration.
Supported repositories require **Rust and Cargo 1.95+**; repository and tool
requirements may be higher. Prerequisite validation does not currently enforce
that floor universally.

## 1. Groups and tiers

| Group | Checks |
|-------|--------|
| `pr-fast` | fmt, clippy, cargo-sort, license-headers, ensure-no-cyclic-deps, ensure-no-default-features, doc-build, readme-check, spellcheck, pr-title, deny, audit, udeps, semver-check, external-types |
| `pr-test` | llvm-cov, doc-test, examples |
| `pr-msrv` | msrv-test |
| `pr-runtime-analysis` | miri, careful, loom |
| `pr-fuzzing` | bolero |
| `pr-mutants` | mutants-diff |
| `release-profile` | release-profile Clippy, nextest, and doctests |
| `scheduled-test` | check-all-targets, llvm-cov, doc-test, examples, Bolero scheduled campaign |
| `scheduled-advisories` | deny, audit, aprz, clippy |
| `scheduled-runtime-analysis` | miri, miri-tree-borrows, miri-strict-provenance, miri-race-coverage |
| `scheduled-exhaustive` | mutants-full, cargo-hack, bench |

`anvil-pr` runs all PR groups; `anvil` is its alias. `anvil-pr-slow` runs every
PR group except `pr-fast`. `anvil-scheduled` runs the scheduled groups, and
`anvil-full` runs both tiers. Local groups run sequentially; CI runs independent
groups in parallel. Checks within a group run sequentially and a failure stops
the group. `release-profile` is an opt-in group outside every tier.

### Default CI platforms

GitHub uses Linux and Windows on x86_64 and ARM64. ADO uses Linux and Windows
x86_64. Exceptions:

- Fuzzing runs only on Linux.
- GitHub PR mutation campaigns run on Windows x86_64; the other three matrix
  legs report successful no-op contexts.
- Scheduled exhaustive checks run on Linux and Windows x86_64.
- Windows ARM runs tests without coverage measurement or upload.

Locally, recipes run on the host OS. See [github.md](./github.md) and
[ado.md](./ado.md) for runner configuration.

## 2. Checks by group

### `pr-fast`

| Check | Contract |
|-------|----------|
| `fmt` | Checks every workspace member with pinned-nightly rustfmt. Non-member path dependencies are excluded. Local `--fix` applies formatting. |
| `clippy` | Checks selected packages with all targets and features, locked dependencies, and warnings as errors. |
| `cargo-sort` | Checks dependency ordering and manifest formatting, preserving blank-line-separated dependency groups. |
| `license-headers` | Checks workspace license headers with cargo-heather. |
| `ensure-no-cyclic-deps` | Rejects workspace dependency cycles. |
| `ensure-no-default-features` | Checks the workspace's default-feature policy. |
| `doc-build` | Builds all-features documentation for selected packages without dependency documentation; rustdoc warnings fail. Local `--open` opens the result. |
| `readme-check` | Checks generated READMEs for publishable crates except `[package.metadata.ox-gen-readme]` opt-outs. Library/proc-macro rustdoc takes precedence over binary rustdoc. |
| `spellcheck` | Checks spelling with cargo-spellcheck and repository configuration. |
| `pr-title` | Validates `PR_TITLE`; skips when it is unset or empty. CI retrieves the current title and fails if a known PR's title cannot be read. |
| `deny` | Enforces cargo-deny policy. |
| `audit` | Checks the lockfile against security advisories. |
| `udeps` | Checks unused dependencies with all features, both for default targets and for all targets. This catches unused dev-dependencies and regular dependencies used only by tests, benches, or examples. |
| `semver-check` | Compares affected publishable libraries against the PR target; API findings and inconclusive comparisons are advisory. |
| `external-types` | Checks exposed dependency types in selected libraries with a schema-matched nightly. Bin-only packages are skipped; tool/nightly incompatibility fails. |

PR titles accept these forms:

```text
<type>: <description>
<type>(<scope>): <description>
<type>!: <description>
<type>(<scope>)!: <description>
```

Types are case-insensitive: `feat`, `fix`, `chore`, `docs`, `refactor`, `test`,
`build`, `ci`, `perf`, and `revert`. Additional lines of title content are rejected.

#### Locked SemVer snapshots

Both revisions use their own checked-in lockfiles and the current checkout's
selected stable compiler and build target. Missing or unusable lockfiles are
reported as inconclusive rather than freshly resolved. Dependency changes can
therefore contribute to API findings.

Non-publishable and bin-only crates are skipped, as are new, moved, or renamed
crates and bin-to-library transitions without a comparable baseline.
Default features remain enabled. Other features are included except `unstable`,
`nightly`, `bench`, `no_std`, and names starting with `_`, `unstable-`, or
`unstable_`; each revision supplies its own features.

The comparison uses the checker's built-in lint policy, not Cargo metadata lint
overrides. Anvil warns when those overrides are present. Multi-target Cargo
configuration must select one target with `CARGO_BUILD_TARGET`. Artifacts go to
`target/anvil/semver/` regardless of `CARGO_TARGET_DIR`.

Build failures and failed comparisons are reported in
`target/anvil/comments/semver.md` without failing the check. Preflight failures
that prevent identifying current packages, compiler, target, or baseline do
fail. See [advisory comments](#6-advisory-pr-comments).

### `pr-test`

| Check | Contract |
|-------|----------|
| `llvm-cov` | Runs selected tests through cargo-coverage-gate on the catalog nightly, with all features and with no default features. Measures released library/binary surfaces and enforces the collector's coverage policy. |
| `doc-test` | Runs doctest-capable selected packages with all features and default features, using locked dependencies. An empty capable set skips successfully. |
| `examples` | Builds and runs selected examples with all features. See [example options and exclusions](./local.md#examples). |

Coverage reports are `target/coverage/lcov-all-features.info` and
`target/coverage/lcov-no-default.info`. The local gate and CI uploads use the
same filtered reports. Zero thresholds disable their gates without skipping
tests; an all-zero/no-coverable selection can produce valid empty reports.
Windows ARM uses plain nextest without coverage measurement.

Coverage always measures the invoking compiler's native host, overriding Cargo
cross-target configuration. A non-host coverage target requires a native runner
or a repository-specific check.

Released-surface source validation covers every workspace member, including
packages outside the affected test set. Ambiguous or unsupported layouts fail;
see [cargo-coverage-gate's accepted layouts](../../../cargo-coverage-gate/docs/design/README.md).
Workspaces owning the `cargo-coverage-gate` package and binary use that checkout's
collector; others use the installed tool.

### `pr-msrv`

The declared root MSRV runs affected library/binary unit tests and integration
tests with all features and default features. Benches, examples, and doctests
belong to other checks. A root toolchain file does not suppress MSRV testing.
Without a root MSRV, the check skips. Setup installs the declared compiler when
needed; see [toolchain selection](./local.md#the-stable-rust-toolchain).

### `pr-runtime-analysis`

| Check | Contract |
|-------|----------|
| `miri` | Runs all-features unit and integration tests under the catalog nightly's interpreter. Supports targeted local runs, concurrency control, and package/test exclusions. |
| `careful` | Runs all-features tests with debug-instrumented standard libraries. A compiler or cargo-careful executable change invalidates its build cache and triggers `cargo clean`. |
| `loom` | Runs declared Loom test targets in release mode with all features, `cfg(loom)`, and one test thread. |

See [Miri configuration](./local.md#5-miri-execution-customization) for supported
overrides. Loom harnesses must declare `loom` in `required-features`:

```toml
[[test]]
name = "concurrency"
required-features = ["loom"]
```

Loom tests run per package; ordinary tests are excluded. A package declaring
Loom support without a matching test target fails. No Loom targets means a
successful skip. Anvil supplies no global exploration bound; each model must
bound its own search.

#### Miri workload scope

For Anvil itself, repeated substring scans of embedded pipeline and recipe text are
native/`cargo careful` contracts, not execution of the generated scripts. Expensive
instances have individual Miri ignores; artifact construction, rendering, and focused
recipe checks remain interpreted. Parser byte boundaries, Unicode handling, region
splicing, and in-memory update planning retain Miri coverage in every profile.
These exclusions do not disable native tests, mutation testing, or any unsafe/concurrency
tests in the workspace.

The planner's exhaustive resolver-failure, multi-deletion adoption, neighbor-order,
ownership-migration and cross-host policy scenarios run natively and under
`cargo careful`, rather than repeating complete parse/plan/settle cycles in every
Miri profile. Focused cache/error, lexical, adoption, TOML/JSON splicing and
in-memory planning tests remain interpreted. The TOML planner lifecycle remains
complete; the JSON lifecycle uses CRLF under Miri and all three newline forms
natively. Dedicated lexical and byte-boundary tests still interpret LF, CRLF,
bare CR and Unicode inputs.

Dependabot's large merge-policy and formatting matrices also remain native/
`cargo careful` contracts. Small block and flow patching round trips, including
BOM/Unicode/CRLF preservation, malformed input, alias cycles and completed-lookup
cache cleanup stay in Miri. Catalog-builder tests use a small nonempty catalog
instead of repeatedly cloning and hashing every embedded workflow; native tests
still verify the full built-in catalog. No Miri profile or workspace unsafe/
concurrency test is removed. The scheduled job target is under two hours on each
supported platform, measured across the complete job rather than interpreter-
reported per-test time.

### `pr-fuzzing`

Bolero runs discovered libfuzzer targets on Linux with the catalog nightly and
release profile. Non-Linux hosts skip fuzzing, including its setup and
prerequisite validation; harnesses can still run as ordinary tests.
Adopters disabling Bolero's default features must enable `std` for libfuzzer.

```console
just anvil-bolero
just anvil-bolero scheduled
```

The default PR campaign uses 30,000 runs and seed 2027 per target. The scheduled
campaign uses 90 seconds per target and lets libfuzzer generate a seed unless
`BOLERO_RANDOM_SEED` is set. Campaign selection does not change impact scope;
the scheduled group supplies full-workspace scope.

Empty successful discovery skips. Invalid campaign names and failed target,
metadata, or harness discovery fail.

### `pr-mutants`

`mutants-diff` tests changed-line mutation sites against the resolved base,
including working-tree edits. It requires a locally available base ref.
Both mutation recipes require a 100% assertion-killed score and zero unresolved
flaky outcomes, then merge learned hints even after a failed campaign.
See [Gamma configuration and outcomes](./local.md#9-mutation-testing-with-cargo-gamma).

### `release-profile`

This opt-in group validates optimization-sensitive behavior across the full
workspace. It runs Clippy with all targets and all features, then nextest and
doctests with all features. Every command uses locked dependencies and Cargo's
`release` profile.

The group never uses impact analysis. It writes nextest JUnit output to
`target/anvil/test-results/release-profile.xml`. Generated ADO steps publish
that file to Test Results, and the shared GitHub group action uploads it as a
workflow artifact.

Use this group when release-only build scripts, `cfg(debug_assertions)`, native
linking, or optimization can change behavior. It is not part of `anvil-pr`,
`anvil-scheduled`, or `anvil-full` because it duplicates expensive compilation.

### `scheduled-test`

The group runs `check-all-targets`, the PR test checks, and finally the Bolero
scheduled campaign, all over the full workspace.

`check-all-targets` compiles each package independently with default features,
then with no default features. This catches dependencies accidentally supplied
by feature unification with other workspace members. All targets are checked,
including tests, examples, and benches, but none are executed by this check.

Targets requiring features must declare `required-features` or gate the relevant
code. Cargo skips targets whose requirements are disabled; libraries must build
with defaults disabled. Dependency defaults still apply. All failing packages
within a configuration are reported; a failed configuration stops the check
before the next one.

### `scheduled-advisories`

`deny`, `audit`, and `aprz` check external policy, vulnerability, and dependency
risk data; `clippy` checks the selected compiler's lint set. Despite the group
name, failures are enforcing. APRZ uses `GITHUB_TOKEN` or an authenticated `gh`
CLI when available. Without either, it warns and proceeds unauthenticated,
which can exhaust GitHub's rate limit and wait for quota renewal.

### `scheduled-runtime-analysis`

All Miri profiles cover the full workspace. Profiles run sequentially within
each platform job; executables within a profile run concurrently.

| Check | Additional behavior | Per-test exclusion cfg |
|-------|---------------------|------------------------|
| `miri` | Standard interpreter checks. | `miri` |
| `miri-tree-borrows` | Tree Borrows aliasing checks. | `miri_tree_borrows` |
| `miri-strict-provenance` | Strict-provenance checks. | `miri_strict_provenance` |
| `miri-race-coverage` | Two seeds per day, rotated by day of month. | `miri_race_coverage` |

For example:

```rust
#[cfg_attr(miri_tree_borrows, ignore = "exceeds interpreter memory budget")]
```

### `scheduled-exhaustive`

- `mutants-full`: whole-workspace Gamma campaign with strict score/flakiness gates.
- `cargo-hack`: feature powerset compilation to depth 2.
- `bench`: all-features benchmark compilation without execution.

## 3. Execution and prerequisites

Each check can run independently. Groups and tiers validate required tools
up front; a missing prerequisite fails with setup guidance. Validation never
installs tools. See [setup](./local.md#3-tool-versions-toolchains-and-installation).

CI reports group-level status, with individual check output in the job log.
SemVer findings are advisory; other check failures fail their group.

## 4. Scheduled scope

Scheduled groups run full-workspace checks regardless of the current diff.
They provide coverage and runtime results for untouched packages, fresh
external-service checks, time-budgeted fuzzing, and exhaustive checks omitted
from the PR tier. `anvil-full` also disables impact scoping for its PR checks.

## 5. Impact-scoping check → include mapping

Cargo-delta selects packages from the committed diff against the base.
`modified` contains changed packages; `affected` adds workspace dependents;
`required` also includes their workspace dependencies.

| Bucket | Checks | Behavior |
|--------|--------|----------|
| modified | fmt, cargo-sort, license-headers, ensure-no-cyclic-deps, ensure-no-default-features | Skip when empty; otherwise check the tool's normal workspace/input domain. |
| affected | clippy, check-all-targets, llvm-cov, doc-test, examples, msrv-test, mutants-diff, all Miri profiles, careful, loom, bolero, semver-check, external-types, bench | Select affected packages; mutation testing further selects changed lines across the workspace. |
| required | doc-build, udeps, cargo-hack | Include affected packages and their workspace dependencies. |
| unscoped | pr-title, deny, audit, aprz, mutants-full, readme-check, spellcheck, release-profile | Run regardless of impact. |

README templates and spelling dictionaries can be repository-level inputs, so
their checks are unscoped. Empty categories skip scoped checks successfully;
invalid reports, unknown package names, and failed metadata/discovery commands
fail rather than silently skipping work.

Coverage currently receives `name@version` package selectors. A registry
dependency sharing both a workspace member's name and version can make that
selection ambiguous. Other Cargo checks use workspace exclusions or
source-qualified per-package selectors.

Dirty local trees widen to the full workspace. `ANVIL_IMPACT=off` disables
scoping; `consume` reads an existing CI artifact. See
[impact controls and failures](./local.md#4-impact-scoping-via-the-anvil-impact-recipe).

## 6. Advisory PR comments

SemVer is the only advisory-emitting check. API findings and inconclusive
comparisons are written to `target/anvil/comments/semver.md`; a clean result
removes the file. Local runs only write or remove this report.

CI uses the Linux x86_64 leg to update one sticky PR comment, or clear a
previous comment when the report is absent:

- GitHub requires pull-request write permission and skips posting on fork PRs.
- ADO uses `System.AccessToken` and requires the build identity's
  **Contribute to pull requests** permission.

Advisory comparison failures do not block the PR, but failures identifying
current packages, compiler, target, or baseline still fail preflight.
