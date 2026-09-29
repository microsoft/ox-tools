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
`1`, failed score or source-expectation gates to `2`, inability to proceed to
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
