# cargo-anvil Implementation Guide

This guide describes internal implementation constraints that keep the emitted behavior aligned
with the user-visible contract in [design](./design/README.md).

## Pull request title validation

The `pr-title.just` template owns both title validation and its failure diagnostic. The ordered
human-readable pattern list is the source of truth: the PowerShell recipe expands its placeholders
into regular-expression fragments and uses the same list verbatim in the error message. Allowed
types are likewise defined once and used for both case-insensitive matching and diagnostics.

Changes to accepted title syntax must therefore modify the pattern or type data rather than adding
a separate regular expression. Pattern expansion escapes the display syntax before injecting the
trusted regular-expression fragments; the reverse order would escape the fragments themselves and
match them literally.

The skip path is shared by an unset and an explicitly empty `PR_TITLE`, because cloud backends
publish an empty value outside a pull request context. Snapshot tests pin the emitted template,
while the focused integration test executes the generated recipe and verifies accepted titles,
rejected titles, both skip cases, and corrective output.

## Workspace formatting

Formatting uses the catalog-pinned cargo-each release to fan out one rustfmt
child per workspace member. The canonical recipe owns the `{manifest}`
substitution and aggregate failure propagation; generated prerequisite recipes
own installation and version validation. Generated copies and snapshots keep
those pieces synchronized.

The modified impact category is only a run-or-skip gate. It never supplies
package arguments to the formatter, so cargo-each's workspace-member selection
remains the formatter's input boundary.

## Semantic-version candidate selection

The SemVer recipe builds candidates from Cargo metadata before intersecting them
with the affected package set. Cargo serializes unrestricted publication as
`null`, forbidden publication as an empty array, and named-registry restrictions
as a nonempty array. Only the empty-array form is excluded; library targets in
the other two forms remain candidates.

The canonical recipe owns this mapping, baseline availability checks, and
per-package execution. Focused contract tests cover every publication form,
while generated copies and snapshots detect drift from the template.

## Impact scoping and tier routing

Impact scoping lets a clean PR run each check against only the cargo packages its committed
diff touches, while unscoped checks, scheduled/full runs, and a dirty working tree still cover
the whole workspace. The *user-visible* contract for this — commands, `ANVIL_IMPACT` values,
per-check policy, and CI shape — lives in the design chapters ([local](./design/local.md) §4,
[checks](./design/checks.md) §5, [github](./design/github.md), [ado](./design/ado.md),
[containers](./design/containers.md)). This section records only the internal architecture and
the synchronization boundaries that keep those promises true across the three backends.

### Source-of-truth boundaries

The subsystem is deliberately split so each fact has exactly one owner:

- **The recipe is the algorithm.** `templates/justfiles/anvil/impact.just` owns *all* impact
  logic — base-ref resolution, the two snapshots, cache-key composition, the throwaway
  worktree, dirty-tree widening, tier projection, and the cargo-delta-identifier → Cargo
  package-selector mapping. The emitted `justfiles/anvil/impact.just` is a generated copy; it
  is never hand-edited, and the snapshot tests (`tests/snapshots/`) pin the emitted text so a
  template change that isn't regenerated fails CI.
- **The catalog registers the file.** `src/anvil/artifacts/justfile.rs::impact()` registers
  the recipe as an owned artifact; `src/anvil/artifacts/mod.rs` wires it into the built-in
  registry and holds the shared tier→mode policy (`impact_mode`). The ADO renderer (`ado.rs`)
  substitutes that policy's value into the `__IMPACT_MODE__` token of each per-group job.
  GitHub does not template the mode per group: its reusable-workflow YAML fixes it statically
  (`pr-impl-workflow.yml` passes `impact_mode: consume`; the scheduled workflow omits it, so
  `run-group-action`'s default `off` applies). Both paths encode the same tier rule — PR
  groups consume, scheduled groups run full — so a group's mode can never diverge between
  backends.
- **Design docs are the contract.** They describe observable behavior; they are not consulted
  by the code and must not be cited from it (see the root `AGENTS.md`).

The `every_check_matches_its_declared_impact_policy` test in `justfile.rs` is the guard that
keeps the per-check policy table, the emitted recipes, and the documented mapping in agreement:
each check's structural `_anvil-impact-include <category>` call (or its absence) is pinned to an
expected category, so a check silently gaining, losing, or changing scoping fails there.

### Cache identity and invalidation

The recipe keeps two independently keyed snapshots under `target/anvil/impact/snapshots/`:

- `baseline.json` (base ref) — expensive (needs a throwaway git worktree). Keyed on the
  **composite** of the base commit sha *and* the effective `.delta.toml` identity, persisted in
  `baseline.key` as `<base-sha> <config-hash>`. Folding the config hash in is a correctness
  requirement, not an optimization: the config governs *what* every snapshot captures, so a
  warm cache keyed on the base sha alone could diff a stale-config baseline against a
  new-config current snapshot and silently mis-scope.
- `current.json` (working tree) — cheap. Keyed on the HEAD sha alone (`current.key`); the
  dirty-tree guard means a snapshotted tree corresponds exactly to HEAD.

The composite baseline key is why the base worktree is retaken when the base moves *or*
`.delta.toml` changes; `baseline_regenerates_when_delta_config_changes_without_moving_the_base`
pins that second input. The worktree lives under the runner-managed scratch dir
(`RUNNER_TEMP` → `AGENT_TEMPDIRECTORY` → system temp) and is named with `$PID` so concurrent
runs never collide or remove each other's checkout; it is always removed in a `finally`.

### Fail-closed mapping and the dirty-tree safety net

cargo-delta reports library/target identifiers that do not uniquely name a Cargo package, so
`_anvil-impact-format` reverse-maps them to version-qualified package selectors using three
Ordinal (case-sensitive) lookups (package name, lib/proc-macro target, manifest-dir leaf). When
a reported identifier resolves to **zero** packages (an unmapped gap) or **more than one** (an
ambiguity), the recipe fails hard rather than guessing — under-scoping would silently skip
affected work. Complementarily, a dirty working tree (any uncommitted change outside `target/`,
detected via a git `:(exclude)` pathspec) widens *every* tier to `--workspace` locally, because
cargo-delta scopes on the committed diff and cannot see working-tree edits. Cloud checkouts are clean, so this
only affects local runs.

### Mode routing before dependency evaluation

`ANVIL_IMPACT` has three modes — producer (unset/compute), `consume` (read a downloaded cache),
and `off` (full workspace). The critical invariant is that the mode must be established in the
shell **before** `just` evaluates a recipe's dependencies, because scoped checks take a
`: anvil-impact` dependency. `helpers.just`'s `_anvil-unscoped` therefore exports
`ANVIL_IMPACT=off` before re-invoking the private tier or group, so those recipes are
never scoped. `container.just` sets the same variable through the engine's `-e` when a
containerized run needs it. `justfile.rs` asserts the off-before-dependencies ordering.

### Cross-backend CI handoff

Both backends compute the impact set once per OS family and transport it to the consuming jobs
as a per-OS artifact, rather than threading it through stage/output variables:

- **GitHub** (`pr-impl-workflow.yml`): `impact-linux` / `impact-windows` jobs run the
  `anvil-impact` composite action and upload `anvil-impact-<os>`; group jobs download it and run
  under `ANVIL_IMPACT=consume`. The impact jobs check out without `lfs: true` (they read only
  path/metadata inputs), and the setup action excludes `target/` from its cache so the
  downloaded impact cache is neither dwarfed nor clobbered.
- **ADO** (`steps/impact.yml`, `steps/job.yml`): the impact step publishes the cache as a
  pipeline artifact; `job.yml`'s `inputArtifacts` parameter defaults to `DownloadPipelineArtifact@2`
  but is overridable so 1ESPT-compliant pipelines can substitute their own download mechanism.
  The impact step does not set `CARGO_INCREMENTAL` because it compiles no workspace code.

The producer/consumer split means the same recipe code runs locally (produce + consume in one
process) and in CI (produce in one job, consume in many), which is what lets the behavioral
tests in `tests/impact.rs` exercise the real recipe rather than a CI-only path.

## GitHub group execution and status reporting

The generated `anvil-run-group` composite action owns the capture-before-failure
protocol. Its inline Bash step invokes Just through `tee`, temporarily disables
immediate exit, and reads `PIPESTATUS[0]` so the saved result belongs to Just
rather than `tee`. It selects the final standard Just failed-recipe diagnostic,
including the optional line-number form, and falls back to the group recipe
when a tool exits without that diagnostic. The step writes the recipe and exit
code as outputs without failing so the reporter can consume them. After
best-effort reporting, a final guarded step propagates the captured failure.

The status reporter is an inline `actions/github-script` body. It validates the
pull-request head SHA, reads same-commit status history newest-first, and keeps
only the newest value for each context. An encoded group marker in the workflow
run URL identifies statuses owned by the current group. The visible context
also contains the group so identical recipe failures reached from different
groups remain independent. A new failure is published before old contexts are
superseded, preventing a temporary failure-free rollup. Clean runs only
supersede active failures; they do not add a fresh supplemental status.

The reporter's API errors are ignored by the composite action because the
native workflow job is authoritative. The generated root workflow grants the
required status permission only to the same-repository pull-request caller.
Merge-group and fork execution retain annotations and the named failure step
without a write-capable status token.

Tests extract and execute the exact YAML-embedded Bash and JavaScript bodies.
The Bash harness covers success, both Just diagnostic forms, and a failure
without a diagnostic. The JavaScript harness mocks status-history pagination
and publication to cover setup and recipe failures, cleanup, ownership,
deduplication, ordering, truncation, and missing event data. Snapshot tests pin
the complete emitted artifacts.
