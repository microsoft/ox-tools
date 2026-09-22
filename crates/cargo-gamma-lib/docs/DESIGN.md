# cargo-gamma-lib — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-lib`.

This is the crate's top-level design document.

## Purpose

This crate coordinates cargo-gamma campaigns: configuration, discovery,
scratch workspaces, instrumented builds, test selection, process supervision,
verdicts, incremental reuse, reporting, and command dispatch.

## Boundaries

- Rust parsing and instrumentation are delegated to `cargo-gamma-engine`.
- Process-tree mechanics are delegated to `cargo-gamma-process` and
  `cargo-gamma-unsafe`.
- Cargo metadata and nextest inventory commands run through the same contained
  process-output lifecycle as later builds and tests. Their stdout and stderr
  are drained concurrently, and descendants are swept before inherited pipe
  handles are allowed to keep capture open.
- After the final build, Cargo metadata and the successful JSON build stream
  are combined into one immutable test environment. Direct binary listing,
  baselines, and mutant attempts receive Cargo's package, manifest,
  build-script, binary-executable, loader-path, Cargo-home, and selected
  rustup-toolchain variables. Nextest receives that Cargo context and adds its
  own `NEXTEST_*` runtime contract. This reconstruction happens once rather
  than invoking Cargo in the per-mutant execution loop.
- Build and verdict supervision surface a failure to terminate a timed-out or
  otherwise abandoned subtree instead of continuing as though cleanup
  succeeded. Verdict cleanup failure abandons the remaining mutation campaign
  because surviving descendants can interfere with later mutants. Census
  remains deliberately fail-open: it is an optimization, and its bounded
  output drain converts cleanup failure into a missing census so ordinary
  discovery can still proceed.
- The injected guard protocol is provided by dependency-free
  `cargo-gamma-rt`. Its package-local source bundle is exposed only through an
  internal feature used by the coordinator, so published `cargo-gamma-lib`
  packages never depend on repository-relative source paths.
- The guard census is disabled unless `--optimize-test-execution` is set.
  Disabling it leaves exact and generalized probes active and treats missing
  case-level reachability as whole-binary work, never as uncovered code.
- The `internals` feature exists only for this crate's integration tests and
  is not a supported downstream API.
- The private rustc-wrapper entry point preserves the wrapped compiler's
  representable process exit code, and the executable returns that `ExitCode`
  directly to Cargo. Launch failures and processes without a representable
  exit code become failure; successful captures remain success. Compiler
  capture stands down when Cargo configuration declares `build.rustc-wrapper`
  or `build.rustc-workspace-wrapper`, because a relative configured path cannot
  be moved safely into the scratch workspace.
- The agreement tests use `cargo-gamma-attrs-impl` through a versionless path
  dev-dependency. Cargo omits that test-only edge from published packages, so
  it adds no downstream dependency or release-order constraint.
- The crate forbids unsafe code.

Replaceable facade, cache, supervision, and test mechanics are recorded in the
[implementation guide](../IMPLEMENTATION.md).

The default storage layout keeps the synchronized source, vendored runtime, and
stable workspace lock under the platform cache home. Cargo artifacts, census
data, and reusable campaign records live under
`<resolved-target>/cargo-gamma/cache/<workspace-identity>`. An explicit
`--cache-dir` keeps all of those entries together at the selected path.

## Public contract

The primary public contract is the `cargo gamma` command surface and its
configuration, reports, diagnostics, and exit codes. The Rust API is an
implementation detail used by the thin executable crate. Its rustdoc is hidden,
and its hand-written README warns downstream users not to depend on it.

Reports that omit `config.mutantIdVersion` use the current identity scheme,
preserving compatibility with reports written before that field was persisted.
An explicit different version is excluded from a merge because its identifiers
cannot safely share a population with the current scheme. A merge may still
produce reports and diagnostics for the compatible population, but
`--min-score` fails when any requested input was excluded this way rather than
grading an incomplete population.

Runtime startup failures are infrastructure failures, not mutant kills. This
includes both failure to acquire the startup environment and a guard reached
before the runtime constructor installed its selection; either fixed marker
disqualifies the process as mutation-score evidence.

Interactive build progress reuses Cargo's progress text while cargo-gamma owns
the terminal redraw. Cargo's leading erase control is consumed rather than
rendered visibly, and its color styling remains intact.

Each Cargo output stream is retained up to 256 MiB for artifact and diagnostic
processing, and each logical line is bounded at 1 MiB. The buffers grow with
observed output rather than reserving those ceilings for every invocation.

Each failed baseline observation retains a bounded 64 KiB, 2,000-line tail from
stdout and stderr, along with the process exit code or signal when available.
Direct libtest baselines continue after a failure announcement, within their
existing budgets, so the retained evidence includes every announced failure
and libtest's trailing panic details. The terminal reports each published
canonical diagnostics file and each failure and diagnostics file with a `Wrote`
line using the platform's native path separator, followed by the aggregate count and artifact directory.
Baseline-failure diagnostics use the settled post-build plan, preserving the complete population,
compiler-withdrawn outcomes, pending viable mutants, and their mutator and package breakdowns.
Successful observations retain none of this output. Before a build, the command
removes stale baseline records. On failure it writes one structured record per failed test beneath
`baseline-failures/<package>/<target>/tests/<test>/failure.json`; unnamed
binary-level failures use categorical leaves. Components are cross-platform
safe and length-bounded, and colliding readable paths receive a short
identity-derived digest. Each failure directory also receives `diags.json`,
and the ordinary canonical diagnostics bundle is retained at its configured
path. Only environment values cargo-gamma explicitly controls are eligible for
diagnostic records, never the inherited process environment.

Completed runs publish the five ordinary `gamma-report.json`, HTML, SARIF,
performance-advice, and diagnostics artifacts. An early baseline failure
instead publishes every failure's nested `failure.json` and `diags.json`, plus
the canonical diagnostics bundle, before the scratch workspace is removed.
The baseline record uses `schemaVersion: 1`
and records the failure kind and reason; package, target, runner, executable,
and working directory; cargo-gamma's explicit environment overrides; failing
and last-observed tests; termination, elapsed time, budget, peak, and memory
limit; and control-character-encoded stdout and stderr tails with a truncation
flag.

When Cargo's resolved package selection covers the whole workspace, every stage
checks mutation viability with that constant Cargo root set, and preflight
validates the same roots even when some packages contain no mutable files. This
keeps dependency feature unification identical across validation and stages
instead of compiling a new dependency variant for each downstream package
selection. Only the current stage's mutants are instrumented; mutants belonging
to other stages are restored before each ordinary, probe, or isolation build.
Diagnostic blame, isolation, and withdrawal therefore remain limited to the
current stage even though Cargo checks the wider graph. The final test-target
build retains its reachability-based package selection; runs whose original
Cargo selection is a package subset retain their narrowed graph throughout.

Instrumentation reads the synchronized scratch tree rather than re-reading the
live checkout after discovery. Each copied source is checked against the
generation digest recorded when its mutants were discovered. If they differ,
those mutants receive the explicit `notbuilt` outcome: their spans do not
describe the tree being tested, so emitting no guard is not treated as an
internal instrumentation failure and splicing them at plausible-but-wrong
offsets is never attempted.

Diff paths are resolved to the workspace-relative Rust files discovered by the
survey. Absolute or rooted paths inside the workspace are normalized to those
candidates; one from another checkout is normalized only when its suffix
uniquely identifies one candidate. Paths that traverse outside the workspace
are never accepted as source selections; this includes parent traversal and
Windows drive-relative prefixes. Non-source paths count as understood only
when they name regular workspace files. Diffs, checked-in hints, and
incremental records are read under a 256 MiB bound. An oversized diff is a
usage error, while oversized optimization artifacts are ignored under the same
fail-open contract as corrupt or foreign-version artifacts.

Checked-in hints use grouped YAML schema version 3. Incremental promotion
upserts exact and generalized knowledge from the selected population while
preserving every absence that partial discovery cannot prove stale; explicit
`--replace` is the destructive whole-artifact operation. The hints context
contains only the generating HEAD commit and UTC date because hints are
revalidated scheduling advice, not context-gated evidence. JSON schemas 1 and
2 are read-only migration inputs and are removed only after the YAML
replacement has been published and verified. Generalized schema version 1
interns repeated killing-test and binary identities, and stable reach sites
persist the engine-owned site digest rather than normalized source text.
Readers still accept the pre-interning version-1 representation so an existing
artifact can be promoted without discarding its knowledge.

Reports use the same source generation from which their mutant spans were
derived. If an analyzed source changes before report construction, the run
refuses to publish reports that would combine the completed verdicts with the
new source.

### Redirected cache security

On Unix, cargo-gamma creates a previously absent redirected cache with
permissions limited to the invoking user, independently of the process umask.
It does not change permissions on a pre-existing directory: the directory and
its physical ancestry must already be owned by the invoking user or root and
must not permit another user to replace entries. Sticky shared ancestors such
as `/tmp` are accepted, but the cache directory itself must not be writable by
group or other.
