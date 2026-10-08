# Check architecture

Checks are generated Just recipes, not duplicated cloud scripts. Each check owns
its invocation, selection rules and prerequisites. Groups compose checks; tiers
compose groups. Cloud backends provide parallel jobs, matrices and reporting.
The executable inventory is in [checks](../../templates/justfiles/anvil/checks),
[groups](../../templates/justfiles/anvil/groups) and
[tiers.just](../../templates/justfiles/anvil/tiers.just).

## 1. Groups and tiers

| Tier | Groups | Scope |
|---|---|---|
| `anvil-pr` (`anvil` alias) | `pr-fast` and `pr-slow` | Impact-aware |
| `pr-slow` | `pr-test`, `pr-msrv`, `pr-runtime-analysis`, `pr-mutants` | Local convenience composition; four separate cloud groups |
| `anvil-scheduled` | Four `scheduled-*` groups below | Full workspace |
| `anvil-full` | PR plus scheduled | Full workspace |

Every public scheduled group also forces `ANVIL_IMPACT=off` when called directly.
This is not dependent on entering through the scheduled tier. The wrapper sets
the environment before invoking its private dependency graph; setting it in a
group body would be too late for Just dependencies.

PR groups aim to catch change-related failures within a PR budget. Scheduled
groups cover externally changing inputs (advisories and repository metadata),
repeat tests without impact narrowing, and run expensive analyses. Repetition
across tiers is intentional, not limited to Miri.

All five GitHub PR groups run on Linux/Windows x86_64/ARM64. GitHub scheduled
groups use the same matrix except `scheduled-exhaustive`, which uses x86_64.
ADO uses Linux/Windows x86_64 throughout. Recipes own unsupported-platform skips;
matrices need not duplicate individual tool support rules.

## 2. Checks by group

Names below omit the `anvil-` prefix. Exact flags and pins remain in templates.

| Group | Checks |
|---|---|
| `pr-fast` | `fmt`, `clippy`, `check-all-targets`, `cargo-sort`, `license-headers`, `ensure-no-cyclic-deps`, `ensure-no-default-features`, `doc-build`, `readme-check`, `spellcheck`, `pr-title`, `deny`, `audit`, `udeps`, `semver-check`, `external-types` |
| `pr-test` | `llvm-cov`, `doc-test`, `examples` |
| `pr-msrv` | `msrv-test` |
| `pr-runtime-analysis` | `miri`, `careful`, `loom`, `bolero` |
| `pr-mutants` | `mutants-diff` |
| `scheduled-test` | `llvm-cov`, `doc-test`, `examples` |
| `scheduled-advisories` | `deny`, `audit`, `aprz`, `clippy` |
| `scheduled-runtime-analysis` | `miri`, `miri-tree-borrows`, `miri-strict-provenance`, `miri-race-coverage` |
| `scheduled-exhaustive` | `mutants-full`, `cargo-hack`, `bench` |

`aprz` is scheduled-only. A group's “advisories” name does not make every command
advisory: recipes normally propagate failures. Semver has the explicit exception
described below.

## Check contracts that affect correctness

### Formatting, documentation and API checks

- [`pr-title`](../../templates/justfiles/anvil/checks/pr-title.just) validates
  `PR_TITLE`; both GitHub and ADO obtain a known pull request's current title
  through their REST APIs. A failed lookup is not an empty-title skip.
- [`fmt`](../../templates/justfiles/anvil/checks/fmt.just) uses pinned nightly
  rustfmt, even when the adopter's options happen to work on stable. `cargo each`
  iterates workspace members with keep-going behavior instead of formatting all
  local path dependencies or constructing one unbounded command line.
- [`udeps`](../../templates/justfiles/anvil/checks/udeps.just) runs all features
  twice: first default targets, then all targets. The first pass catches regular
  dependencies used only by tests/examples/benches; the second catches unused
  dev-dependencies. Nightly is required by the tool; missing prerequisites are
  not permission to skip it.
- [`doc-test`](../../templates/justfiles/anvil/checks/doc-test.just) runs all
  features and default features. It intersects scope with Cargo's `doctest`
  capability, not a guessed list of library kinds, avoiding false failures when
  only bin-only packages are affected. Empty capable scope is a successful skip.
- [`external-types`](../../templates/justfiles/anvil/checks/external-types.just)
  iterates affected library manifests because the tool is per-manifest. It uses
  a dedicated nightly pin compatible with the tool's rustdoc JSON schema.
  Findings and schema/tool errors fail; accepting a newer installed binary does
  not guarantee schema compatibility.
- [`semver-check`](../../templates/justfiles/anvil/checks/semver-check.just)
  compares affected publishable libraries against the PR/base revision, not the
  last registry release. Bin-only and nonpublishable packages are excluded;
  named-registry publication remains eligible.

Semver preflight failures (such as metadata failure or an unavailable base
commit) fail the recipe. Once `cargo-semver-checks` runs, deny-level findings
(exit 100), inconclusive operational failures (exit 101), and unexpected nonzero
exits produce an advisory rather than failing the recipe. Recognized unavailable
baselines—new/moved manifest, renamed package, bin-to-library transition or a
yanked dependency in the baseline—skip without treating that absence as an API
regression. This distinction prevents an unbuildable historical baseline from
being reported as proof of a breaking change.

### Tests, coverage and MSRV

[`check-all-targets`](../../templates/justfiles/anvil/checks/check-all-targets.just)
uses the selected stable compiler and `cargo each --keep-going` to check every
affected package independently with `cargo check --package '{spec}' --all-targets
--locked`, first with default features and again with `--no-default-features`.
An unscoped run selects every workspace member. Package isolation avoids features
enabled by other selected packages masking missing test dependencies or example
feature requirements. This compiles targets without linking or executing tests.

[`llvm-cov`](../../templates/justfiles/anvil/checks/llvm-cov.just) runs nextest
under pinned nightly with **all features** and **no default features**. Each
configuration produces LCOV and passes through `cargo-coverage-gate`. Nightly
enables `cfg(coverage_nightly)` exclusions; uploading reports is not the gate.

With an explicit affected-package selection, packages declaring
`[package.metadata.coverage-gate] min-lines-percent = 0` are routed through plain
nextest. They still run tests, but do not make an otherwise empty coverage export
fail. Full-workspace mode retains workspace instrumentation. Windows ARM64 uses
plain nextest for the entire selection because its LLVM profile tooling cannot
reliably merge the data.

Reports are `target/coverage/lcov-all-features.info` and
`target/coverage/lcov-no-default.info`. GitHub uploads available pairs on every
leg except Windows ARM64; ADO publishes both OS legs. Backend publication does
not replace or weaken the recipe's threshold enforcement.

[`msrv-test`](../../templates/justfiles/anvil/checks/msrv-test.just) resolves the
declared root MSRV and runs `cargo test --tests` twice: all features and default
features. This includes lib/bin unit tests and integration tests, not
doctests/benchmarks/examples. With no root MSRV it skips; it does not invent a
version. Setup installs the declared compiler, while prerequisite validation does
not auto-install it.

Examples and benchmarks are compile-only in their validation groups. The
developer-facing examples recipe can explicitly run examples, but that is not
the default CI contract.

### Runtime analysis

[`miri`](../../templates/justfiles/anvil/checks/miri.just) compiles the selected
packages together with `cargo miri test --no-run --tests --all-features`, then
runs the resulting test executables concurrently. One compilation preserves
feature unification; one libtest process per artifact avoids repeating Miri's
initialization for every test. `ANVIL_MIRI_JOBS` controls artifact concurrency,
defaulting to logical processors.

`[package.metadata.anvil.miri] exclude = true` excludes a package's own test
targets, not its compilation as a dependency. An explicit example invocation
requires a package and uses `miri run`; test filtering and example selection are
mutually exclusive. Scheduled profiles add Tree Borrows, strict provenance or a
day-based multi-seed window while using the same driver.

[`loom`](../../templates/justfiles/anvil/checks/loom.just) discovers integration
test targets structurally through `required-features = ["loom"]`. It combines
feature activation with `--cfg loom`, selects the owning package/test, and runs
release tests single-threaded. There is no global exploration bound. No Loom
support is a successful skip; declared Loom support without a matching harness
is an error, not an empty green run.

[`bolero`](../../templates/justfiles/anvil/checks/bolero.just) provides a short
Linux-only fuzzing pass. [`careful`](../../templates/justfiles/anvil/checks/careful.just)
adds its own runtime checks. Unsupported environments are handled explicitly in
the recipes rather than being interpreted as generic tool failures to ignore.

### Mutation and feature-space checks

PR mutation testing uses the base-to-head diff and affected package scope.
Scheduled mutation testing is full-workspace. Both use the same
[platform configuration rule](./local.md#9-platform-specific-mutation-configuration):
an existing `.cargo/mutants.<os>.toml` replaces, rather than overlays, the default
configuration. Invalid selected configuration fails.

`cargo-hack` supplies feature-powerset coverage in the scheduled exhaustive group;
benchmark compilation is separate from running benchmark measurements. These
checks belong outside the default PR budget, not outside correctness validation.

## 5. Impact scoping: check → include mapping

The projection answers two different questions: whether work is needed, and
which Cargo package specs to select. The complete protocol is in
[local execution](./local.md#4-impact-scoping-via-the-anvil-impact-recipe).

| Projection | Meaning | Consumers |
|---|---|---|
| `modified` | No changed packages: `--skip`; otherwise keep the tool's native domain | fmt, cargo-sort, license-headers, ensure-no-cyclic-deps, ensure-no-default-features |
| `affected` | Changed packages and reverse dependencies, as `--package name@version` pairs | clippy, check-all-targets, semver-check, external-types, llvm-cov, doc-test, examples, msrv-test, miri, careful, loom, bolero, mutants-diff, bench |
| `required` | Affected packages and required dependencies, as version-qualified pairs | doc-build, udeps, cargo-hack |
| Unscoped | Always retain the check's native domain | readme-check, spellcheck, pr-title, deny, audit, aprz, mutants-full |

The `modified` bucket does **not** restrict formatting/license checks to changed
package directories. It is an all-or-nothing skip gate. This preserves their
workspace or repository-wide semantics. README and spelling remain unscoped
because root files, dictionaries and other shared inputs are not reliably
represented by per-crate deltas.

Projection maps cargo-delta results through current Cargo metadata. Unknown or
ambiguous package mappings fail, rather than silently dropping names. Scoped
recipes capture and propagate resolver failures before interpreting the returned
selection. An error must never be mistaken for empty scope.

Dirty local trees and a base preceding the workspace can deliberately widen to
full workspace. Invalid modes, missing consume artifacts and failed computation
are errors, not widening rules. Scheduled/full wrappers turn scoping off
explicitly. Even an empty successful PR impact result still permits cloud groups
to run and report terminal outcomes.

## 6. Advisory PR comments

An advisory recipe writes a complete Markdown body to
`target/anvil/comments/<name>.md` when it has findings and removes it on a clean
or skipped run. The body includes the stable marker `<!-- anvil-<name> -->`.
Today the publishing integrations enumerate `semver`.

The recipe's return code and the publication step have separate responsibilities:

- Local callers receive the same advisory file without needing a PR API.
- GitHub upserts/deletes a sticky comment on the canonical Linux PR leg, only
  for same-repository pull requests.
- ADO updates a marker-owned thread or closes it when the file is absent.

Publication wiring is explicit, not directory auto-discovery. Adding or retiring
an advisory therefore requires coordinating the recipe and backend publisher;
otherwise old comments can outlive the check that produced them.

## Extending a check

A check change should preserve its execution boundary: body, setup and
`validate-prereqs` stay together. Update group membership and prerequisite graphs
with the check, and classify new cloud groups in the
[shared impact-mode policy](../../src/anvil/artifacts/mod.rs). Choose scope based
on the tool's actual input domain, not the desire to make every check package
scoped. Backend changes should transport results and artifacts, not reimplement
check commands.
