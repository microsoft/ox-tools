# cargo-gamma — Implementation guide

This guide records executable and end-to-end mechanics behind
[`DESIGN.md`](DESIGN.md).

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
