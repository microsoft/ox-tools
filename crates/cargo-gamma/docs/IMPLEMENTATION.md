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

The coordinator synchronizes sources, vendors the dependency-free guard
runtime, and places Cargo artifacts and campaign state under the selected cache
base. The default base is selected from a stable physical workspace identity;
published reports remain under the artifact directory rather than reusable
cache state.

The default cache name is a pinned BLAKE3-derived physical-workspace identity
under the platform cache home. Ownership markers and process-held locks prevent
two workspaces or commands from sharing mutable state accidentally. An explicit
cache directory is validated before use and must be empty when first claimed.

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
identities interned per file. Promotion joins the run record to the current
selected population. Incremental promotion upserts that knowledge and preserves
other scopes; `--replace` rebuilds from the selection. Both modes publish
against the exact generation read, verify the replacement, and remove legacy
JSON only if its generation is unchanged.

## Test fixtures

Cross-process tests use owned temporary directories and explicit environment
markers. Concurrency tests observe channels, process completion, or ownership
state where those transitions are controllable; watchdogs remain only as
last-resort harness protection outside mutation campaigns.

Tests requiring a host capability are ignored with an explicit reason when the
suite is run generally and fail when invoked specifically without that
capability. Registry mechanics use isolated instances where the call path
permits injection. The remaining tests that call the production interrupt
handler against process-global state are recorded as correctness work in
[`TODO.md`](TODO.md#t1-isolate-tests-from-the-production-interrupt-registry),
rather than being described as isolated before they are.
