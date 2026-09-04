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
- The `internals` feature exists only for this crate's integration tests and
  is not a supported downstream API.
- The agreement tests use `cargo-gamma-attrs-impl` through a versionless path
  dev-dependency. Cargo omits that test-only edge from published packages, so
  it adds no downstream dependency or release-order constraint.
- The crate forbids unsafe code.

Replaceable facade, cache, supervision, and test mechanics are recorded in the
[implementation guide](../IMPLEMENTATION.md).

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

Each failed baseline observation retains a bounded 64 KiB, 200-line tail from
stdout and stderr, along with the process exit code or signal when available.
The baseline error identifies the package, target, runner, executable, working
directory, elapsed time, resource failure, and failing or last-observed test.
It points to the generated diagnostics instead of printing captured test output
to the console. Successful observations retain none of this output. Before
early failure cleanup, the command writes the structured record, including
safely encoded output tails, to `baseline-failure.json` and writes the ordinary
`gamma-diagnostics.json` bundle; only environment values cargo-gamma explicitly
controls are eligible for diagnostic records, never the inherited process
environment.

When Cargo's resolved package selection covers the whole workspace, every stage
checks mutation viability with that constant Cargo root set. Packages with no
mutable files may be omitted from preflight, but do not narrow the staged
checks. This keeps dependency feature unification identical across stages
instead of compiling a new dependency variant for each downstream package
selection. Only the current stage's mutants are instrumented; mutants belonging
to other stages are restored before each ordinary, probe, or isolation build.
Diagnostic blame, isolation, and withdrawal therefore remain limited to the
current stage even though Cargo checks the wider graph. The final test-target
build retains its reachability-based package selection; runs whose original
Cargo selection is a package subset retain their narrowed graph throughout.

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
