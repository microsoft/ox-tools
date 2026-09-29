# cargo-gamma — Implementation guide

This guide records executable and end-to-end mechanics behind
[`DESIGN.md`](DESIGN.md).

The [validation-improvement stack plan](implementation-plans/0000/README.md)
organizes population-scope correctness, mutation defaults, flaky-result gating,
library target selection, external artifacts, exact replay, and const-function
execution into linked implementation layers.

## Executable boundary

The binary implements the real terminal host and calls
`cargo_gamma_lib::run`. Installed-binary tests launch that executable in both
direct and Cargo subcommand argument shapes, then compare its complete version
output with the package version.

The command layer normalizes Cargo's inserted `gamma` argument and inserts
`run` only for a bare option-first invocation. Clap owns syntax and generated
help; configuration is resolved afterward so split settings such as shard count
in `gamma.toml` and shard index on the command line can be validated as one
effective value. The top-level boundary maps help and success to `0`, usage to
`1`, failed flaky-result, score or source-expectation gates to `2`, inability to proceed to
`3`, and an uncaught internal panic to `70`.

## Mutation discovery

The engine registry declares `relational.gt_to_eq` as default-on with the `ROR` alias.
The `BinOp::Gt` replacement table appends equality after `gt_to_ge` and `gt_to_lt`: their
replacement indices remain identity-significant and unchanged. Generic selector resolution
supplies family, alias, and preset membership without a separate path.

The binary collector rewrites the whole expression with parenthesized operands and uses
ordinary no-op and identical-edit filtering. Selection precedes deduplication, so an exact
mutator selection still emits its edit when a broader selection coalesces identical edits.
Runtime guards evaluate only the chosen expression branch, without hoisting operands into
temporary bindings. The compiler decides viability; the collector does not infer operand
types for this transformation.

## Exact identity resolution

The command layer snapshots report inputs through the bounded regular-file/schema reader.
The input-byte BLAKE3 digest identifies the parent even when publication overwrites that path.
Report paths and character coordinates are validated only as data; embedded source is parsed
without consulting `projectRoot`. Replay requires the explicit current identity scheme and
original Gamma producer, including per-verdict producer metadata for merged documents.

`commands::exact::Request` deduplicates input IDs in sorted order, indexes a complete current
scan, reports duplicate current identities and accounts for every requested ID before filtering.
Historical comparison checks file, mutator and replacement, normalized site tokens, and the
containing item's tokens with module/implementation headers, and its token-indexed source
position. Sibling items do not participate. The containing body and site position prevent
occurrence renumbering or different edit coalescing from redirecting historical selections.
Exact lookup preserves ordinary coalescing and identity construction.

`Survey` retains the resolved scan and its source digests for exact requests. Measurement
validates those digests against the synchronized tree before preflight and consumes the
retained candidates by package, assigning execution ordinals through the existing staged
pipeline. Ordinary runs continue to discover package by package. Exact requests do not adopt
cached outcomes; current build and execution controls decide the results.

The existing population scope carries `selection = exactIds`, a reduction, the audited
`exact.ids` set and optional `exact.parentReport` digest. Complete-file assertions are cleared,
so both direct and staged merges preserve omitted findings under the existing authority rules.
Historical explanation uses retained report source and verdict provenance, labels it as
historical, and does not infer current correspondence or execute Cargo.

## Scratch layout

The coordinator synchronizes sources and vendors the dependency-free guard
runtime under an external per-workspace scratch base. By default, Cargo
artifacts and campaign state live under
`<resolved-target>/cargo-gamma/cache/<workspace-identity>`. An explicit cache
directory keeps the all-in-one layout. Published reports remain under the
artifact directory rather than reusable cache state.

The external cache also carries `campaign-location`, an atomically written
pointer to the campaign base used by the latest completed run. Completion
writes and serializes the merged record first, retains that merged value in
memory, publishes the locator second, and enables postprocessing notes only
when the same locator lookup used by `hints` and `suppress` resolves the
record.

The default cache name is a pinned BLAKE3-derived physical-workspace identity
used by both locations. The stable process-held lock remains under the platform
cache home, so deleting the Cargo target cannot create a second lock domain.
Ownership markers prevent two workspaces from sharing mutable state
accidentally. An explicit cache directory is validated before use and must be
empty when first claimed.

Cargo metadata resolves `CARGO_TARGET_DIR` or `build.target-dir` before `campaign_base` derives
the workspace-specific artifact location. `Workspace::cargo` passes that campaign's `target/`
directory to every Cargo build. `gamma_base`, its source/runtime tree and the stable workspace
lock do not depend on the target setting. The existing VCS exposure is shared by local-target
and external-target runs; artifact placement adds no Git interception or redirection.

The installed-binary artifact-placement fixtures compare equivalent Gamma campaigns against
ordinary and linked worktrees. Their build scripts query revision, branch and tag metadata,
and their test harnesses consume the embedded results. Build-script and test-launch markers
distinguish warm Cargo artifacts from freshly executed verdicts. The fixtures also exercise
the persisted locator and workspace-owned cleanup next to unrelated target contents.

Completed runs always publish JSON, self-contained HTML, SARIF, Markdown
performance advice, and a versioned diagnostics bundle. Before a build,
cargo-gamma clears stale `baseline-failures/` records. Each named baseline test
failure receives a readable
`baseline-failures/<package>/<target>/tests/<test>/` directory containing
`failure.json` and `diags.json`; categorical leaves represent unnamed
binary-level failures. Filesystem sanitization and length limits are
collision-checked, with a short identity digest added only when needed.
Artifact publication is separate from the cache, and `--artifact-dir` moves
the complete user-facing set.

## Library target policy

`CargoOptions::lib` carries the effective shared CLI/config policy into discovery and every build.
`Survey` retains Cargo target identities, library classification across crate kinds, `test`
eligibility and custom `harness` declarations. The mutation file inventory remains independent.
Before copying, measurement validates name patterns and the requested oracle against that inventory.

Mode-specific Cargo verbs distinguish ordinary checks, production stages and full harness builds.
The shared invocation path intersects every package selection, including unrestricted widening,
with eligible library roots. Production stages may compile libraries with disabled test harnesses;
harness invocations cannot request them. Library convergence does not use optional ordering probes
or diagnostic-free isolation builds: each counted round is one aggregate invocation, and only
returned compiler diagnostics authorize withdrawal. The final library build retains preflight's
package feature scope rather than performing optional target-name narrowing.

Artifacts retain Cargo package IDs and carry target kinds and workspace-relative source roots.
They are associated with metadata before any execution. Persisted hint identities combine package
and target names with kind and relative source root, without recording checkout-specific package
IDs or executable paths. Nextest inventories the final build's package roots with `--lib`, even
when linkage or oracle-package filtering omits some built binaries from execution.

Killer and generalized binary hints optionally carry the complete logical identity. Legacy
package/name hints are admitted only when the declared inventory contains one matching test
target, including targets excluded from the current oracle. All hints remain fresh checked
probes, never verdict reuse. Custom harnesses bypass libtest enumeration.
Identity-enriched hints remain readable alongside legacy name-only hints by this reader;
older readers that reject the additional identity field cannot consume them.

The effective target policy is part of the population shaping record and its canonical key.
It also participates in build/execution cache policy and test-selection terms. Diagnostic binary
records include redacted package identity and source root alongside Cargo target kinds.

## Runtime protocol

The injected runtime uses fixed static buffers and native startup-environment
access because it has no allocator or production dependencies during
construction. Startup acquisition failures use a fixed marker and reserved exit
status so the coordinator cannot mistake an unselected mutant for a baseline
run.

The same guard protocol records sealed reach observations. The opt-in census
groups and subdivides test scopes under an economic deadline; incomplete
observations are positive checked hints only. During the sweep, workers claim
mutants from an assignment-time scheduler that tracks active files and items.
Cold same-item siblings wait for their scout to publish exact-test, file, and
safe same-site negative reach learning before they become eligible.

Checked-in hints use version-3 YAML grouped by source file, with repeated killer
identities interned per file and generalized schema-v2 identities interned
globally. Promotion projects workspace-relative identities directly from the
persisted campaign record; it performs no population rediscovery. Incremental
promotion upserts that campaign's knowledge and preserves other scopes;
`--replace` rebuilds from the campaign population. Both modes publish against
the exact generation read and verify the replacement. YAML generations are
scanned for forbidden references before strict deserialization begins.
Incremental generalized merging and change accounting use keyed Fx tables,
then restore canonical ordering before publication.

## Population evidence in reports

`Survey` resolves shaping inputs once, and each scan adds the actual sorted mutator membership.
`Plan` accumulates successful per-file assertions separately from candidate outcomes. Declaration
or selected-file analysis skips conservatively withhold completeness across the plan. Normal
suppressed candidates stay visible. Report construction includes complete empty files and verifies
their retained source generation exactly as it does files containing mutants.

`config.population` uses a versioned envelope in the standard schema's free-form configuration.
Its context table stores complete JSON shaping records keyed by BLAKE3 over recursively
key-sorted JSON. Arrays describing sets are sorted before writing. Unknown additive shaping
fields survive decoding and digest validation; unsupported envelope versions supply no authority.
Error replacement expressions retain their identity-significant ordering. Arbitrary Cargo flags,
build configuration, resolved compiler cfg evidence and discovery-affecting environment inputs are digests,
not ambient values. Unrelated process environment and the Gamma package version do not shape this
key. Source-file bytes remain outside it as well.

Merge indexes complete ID sets by file and context, ranked by original time, origin and lineage.
Only the newest assertion for each pair survives. Original observations are flattened,
deduplicated and retained in merge provenance, including observations currently excluded by
retirement or source presentation. Keeping only an intermediate winning verdict would lose the
alternate context's evidence when a subsequent assertion retires that winner. These records carry
source digests rather than duplicate source text, retain original verdict timestamps, and are
subject to the same bounded report-input budgets as the standard document. They are not scored
unless selected into the standard file/mutant projection.
Before publication the merged JSON is size-checked against the reader's per-report budget.
An oversized history fails explicitly rather than publishing an artifact that cannot be remerged
or silently discarding provenance. Independent contexts can be kept in separate histories.

The reader validates recognized metadata and cross-record references rather than discarding
failed parses. The writer preserves source authority even for empty projected files; staged
merges therefore retain empty snapshots independently of mutant provenance.

## Flaky evidence and gating

Run configuration resolves `no-confirm` and `no-fail-on-flaky` before discovery/build and rejects
disabled confirmation without explicit gate opt-out. Completed observations are graded separately
from their execution: infrastructure/build inability takes precedence, then flaky, expectation,
pending and percentage checks. The reports already exist when grading returns an exit code.

`elements::gamma_outcome` decodes Gamma's status/reason pair without granting reuse eligibility.
The merge retains producer identity and optional confirmation policy in both winning-verdict
provenance and original staged observations. Its real-verdict-before-pending ranking otherwise
uses original timestamps, so neither listings nor intermediate publication refresh the evidence.
Flaky and known unconfirmed detection counts are taken only after retirement and presentation
compatibility. Unknown producer or confirmation metadata is never synthesized from a merged
document's top-level framework. Each merge records its own effective gate policy.

Confirmation settlement retains the test identity and categorical mutated/unmutated observations.
Resource-limited confirmation is inconclusive rather than an assertion failure. Published notes
encode control characters and never contain raw subprocess output or ambient environment values.
Existing bounded local diagnostics and baseline failure artifacts remain separate from these notes.

## Test fixtures

Cross-process tests use owned temporary directories and explicit environment
markers. Concurrency tests observe channels, process completion, or ownership
state where those transitions are controllable; watchdogs remain only as
last-resort harness protection outside mutation campaigns.

Tests requiring a host capability are ignored with an explicit reason when the
suite is run generally and fail when invoked specifically without that
capability. Registry and cgroup mechanics use isolated registries and recording
killers. Tests that must exercise the production signal handler run in child
processes, so they cannot leak process-global interrupt state or direct a
fabricated process group from the main test process.
