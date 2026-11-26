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

The executable integrates `rallocator` on Linux and Windows; other hosts,
including macOS, use the system allocator without adding another platform
dependency. This selection affects only the coordinator, not the binaries under
test. Cargo-gamma maintains no allocator benchmarks or performance comparisons,
so the platform selection is not a relative-performance claim.

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

## Source evidence

One survey retains declaration evidence for its lifetime, keyed by source path
and package (whose selected configuration is fixed). Package scans merge this
evidence rather than repeatedly parsing shared dependency declarations.
Mutation scans still read their selected source to produce edits; cached
evidence carries its original digest and is rejected if that source changed.
Scratch-source validation also compares every participating declaration digest
before instrumentation. Source-directory inventories and per-scan physical
path identities are reused without changing population or ordinal order.
The complete metadata graph supplies both resource dependencies and incremental
cache provenance; incremental discovery does not load it a second time.
Non-incremental graph resolution retains its offline requirement.

Nested method inference carries the receiver's frozen type, declaration scope and
import snapshot together. These facts live only for the current query, and method
visitors reuse their receiver type across substitution and predicate consumers
before descending into argument closures or changing lexical bindings. There is
no file-wide inference cache to invalidate when a name is rebound.

Resource cursor proofs use a separate closed typed context rather than reopening
the legacy scalar interpreter across opaque control flow. Dominating declarations
identify standard vectors, scalar fields and immutable string slices; shadowing
kills earlier evidence, and shift closures are certified at their declaration
before captures can be shadowed. The frame backedge permits only an idempotent
minimum on a vector disjoint from the branch selectors. The paired-scan backedge
remains idempotent even when its frontier read and write select the same cell.
Both proofs require a source-supported mutation-induced recurrence and retain
unknown effects, guards, alternate progress and exhausted contexts.

Nonempty record-grouping evidence resolves private local layouts and helper
declarations in the bound source graph. The supported grammar accounts for
derived record effects, standard dependency-backed path values, tuple ordering
keys, deduplication and nested membership searches. Replaying a successful
iteration preserves checked indices without assuming a maximum artifact size.
Unknown implementations, escaping aliases, conditional declarations and incomplete
cross-file effects withhold the certificate.

Population retry evidence resolves local record layouts, ordinal keys, cache-hit
prefixes, scalar budget guards and constant entry construction. Its bounded
interpreter carries exact populations, cache snapshots and counters through
chunking, complements, collection updates and fallible calls. The complete state
history recognizes both fixed points and multi-partition cycles; unknown or
exhausted execution withholds a proof. This path has its own closed entry analysis
and does not relax the legacy interpreter's opaque-context restrictions.

Ownership continuations share reference-counted slices of expressions borrowed
from the file AST. Exit prefixes refer to the evaluated subexpressions rather
than copying their syntax trees; restoration checks for multiple moved places
share the same suffix. The source outlives the evidence, and the existing
32-expression continuation limit and unknown-flow fallback remain unchanged.

## Compiler convergence

The converger retains a bounded cache of definitive, unattributed compiler proofs
across failed rounds. Each isolation pass freezes a source/mutation generation;
each reusable subset also carries its complete compilation identity, roots, verb,
attribution scope and active/background ordinals. Missing source evidence,
unavailable mutations, changed compilation identity, harvested blame and incomplete
outcomes withhold cross-round reuse. The existing per-context memoization and
progress assertions remain independent of this optional cache.

Selective proofs use the ordinary Cargo command and containment runner, with
wrapper-level reuse enabled only for supported metadata/dep-info units. Successful
captures bind actual compiler commands and complete diagnostic streams to validated
source, directory, dependency, toolchain and output snapshots. Missing or changed
evidence invokes rustc normally. Closed source-only graphs can additionally seal a
complete identity for cross-round proof reuse; unsupported dependencies, build
scripts, proc macros and unbounded expansion withhold that seal. Snapshot validation
currently repeats per eligible unit and can outweigh the avoided compiler work.

The library filters historical compiler exclusions during discovery, before
assigning ordinals or writing guards. The cumulative `compiler-exclusions.json`
inventory is independent of incremental reuse and is atomically extended as new
individual failures are proved. Interaction-only removals are never added.

Pristine validation fixes the package roots, targets, features, and compiler
options once. Every exclusion and proof check repeats `cargo check --tests
--keep-going`; the final `cargo build --tests --keep-going` retains that scope.
Library-only campaigns preserve their `cargo test --no-run --lib --no-fail-fast`
invocation instead. Unattributed failures enter diagnostic-scoped isolation with
exact active subsets in the failed dependency-connected components. Other
mutations in those components are restored; disconnected successful components
remain instrumented. Identical source selections and bytes are not rewritten. Local
limits are 4,096 candidates per search and 32 proofs per context. Diagnostic-local
searches precede deterministically partitioned oversized tiers; passing partitions
do not disprove cross-partition interactions. After the complete diagnostic-local
tier group yields proven progress, the next ordinary convergence check replaces
broader clean-tier proof builds. All local partitions are still considered;
contexts without diagnostic locality retain the broader search. Each Cargo invocation
admits 64 contexts and therefore at most 2,048 proofs, preserving a complete
local proof allowance for every admitted context. Larger dependency cones run
first. Exhausted contexts preserve unjudged candidates. Confirmed causes permit
another convergence round; no-progress exhaustion stops the campaign with an
actionable error instead of abandoning a package or dependency population.
The initial validation is the pristine control: isolation launches no additional
controls, historical probes, target reductions, or scope-widening retries.
Confirmed singleton counts are published during isolation, before subsequent proofs.
Isolation and interaction minimization live in
`cargo-gamma-lib/src/exec/build/isolation.rs`; splice deltas and the incremental
guard index remain in `build/splices.rs`. Ordinary reporters do not request
Cargo evidence decoding, while diagnostic reporters opt in through `Events`.

Proof-cost ordering lazily indexes immutable mutant metadata and reverse package
reach once per isolation pass. The index borrows the plan and is discarded with
that pass. Each score still uses the current active and background populations;
only metadata lookup is reused, not final scores or compiler verdicts.

Target-frontier acceptance was rejected by deterministic invocation-count
fixtures: retaining global confirmation always adds work, while omitting it is
unsound for feature-unified downstream and cross-target interaction cases.

## Runtime protocol

The injected runtime uses fixed static buffers and native startup-environment
access because it has no allocator or production dependencies during
construction. Startup acquisition failures use a fixed marker and reserved exit
status so the coordinator cannot mistake an unselected mutant for a baseline
run.

The same guard protocol records sealed active-mutant reach observations through
`GAMMA_REACH`, without separate sampling launches. Incomplete observations never
establish absence. During the sweep, workers claim
mutants from an assignment-time scheduler that tracks active files and items.
The four execution phases share the configured mutant concurrency; same-item
siblings remain runnable while related work is active. Completed mutants publish
exact-test, file, and safe same-site negative reach learning before releasing
their assignments.

Checked-in hints use version-4 YAML grouped by source file, with repeated killer
identities interned per file and generalized schema-v3 identities interned
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
