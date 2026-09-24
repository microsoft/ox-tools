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
`--cache-dir` keeps all of those entries together at the selected path. A
`campaign-location` file under the external per-workspace cache points
state-consuming commands at the latest successfully published campaign base.
The record is published first; footer advice is enabled only after that locator
resolves back to the published record. State-consuming commands accept a
workspace member as `--dir`: they resolve its current owning workspace through
Cargo metadata, then accept only a persisted locator or cache owner for that
workspace identity. If current workspace identity cannot be resolved, they fail
rather than adopting potentially stale state.

Every measured campaign, including `--incremental no`, publishes completed
campaign state and therefore holds the process-held workspace lock for its
lifetime. Measured campaigns for one workspace are serialized. Dry runs publish
no state and do not take that lock.

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
generation digest recorded when its mutants were discovered after that source
is scanned and before its current stage is instrumented. Earlier stages are not
revalidated while their guards remain in the synchronized tree. The comparison
omits a leading UTF-8 byte-order mark, matching discovery and parsing. A
mismatch stops the campaign and asks the user to rerun after edits settle rather
than attributing build or mutation results to the earlier source generation.
Before completed evidence is published, the
same discovery digests are checked against the pre-execution input snapshot;
this prevents a source edit between snapshot capture and synchronization from
giving a different generation the snapshot's provenance. A pre-execution test
declaration scan also gives completed killed outcomes the workspace-relative
file identity of an unambiguous killer, so later validation does not accept a
same-named test moved elsewhere. Instrumentation
repeats the generation check before splicing; a later mismatch gives affected
mutants the explicit `notbuilt` outcome, and splicing at plausible-but-wrong
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

Checked-in hints use grouped YAML schema version 3. `cargo gamma hints` reads
the persisted campaign record directly: after resolving and validating the
current workspace identity, it performs no source walk, parse, or mutation
discovery, and therefore accepts no mutant selection flags: it promotes the
completed campaign population exactly as recorded. The completed ledger may
retain unchanged outcomes from earlier narrow campaigns for incremental reuse,
but it separately records the latest
campaign population so promotion cannot remove or republish hints for those
carried outcomes. Incremental promotion upserts exact and generalized knowledge
while preserving unrelated entries and exact hints for `pending`, `notbuilt`,
or `ignored` mutants that the campaign did not judge; explicit `--replace` is
the destructive whole-artifact operation and requires a valid completed
campaign record before changing an existing artifact. Generalized item, file,
and reach
knowledge is projected onto the completed record's persisted file and site
identities before either merge mode, so replacement cannot republish scheduling
knowledge inherited from scopes outside that campaign. Promotion holds the
workspace lock across a second campaign-locator resolution, campaign-record
selection, artifact publication, and verification, so a
concurrently completing campaign cannot be followed by hints derived from the
record it replaced. Version-10 campaign
records persist workspace-relative paths for exact identities. Version-9
records retain safely usable generalized and compiler-ordering knowledge but
omit exact probes whose path was never recorded rather than inventing one. The hints context
contains only the generating HEAD commit and UTC date because hints are
revalidated scheduling advice, not context-gated evidence. The independently
versioned generalized schema is version 2. It distinguishes seed observations
from cross-mutant transfer hits and misses, interns repeated killing-test and
binary identities, and persists stable reach sites with the engine-owned site
digest rather than normalized source text. Version-1 generalized data is read
conservatively: candidate identities become seeds, while conflated transfer
statistics and measured costs are reset before the in-memory schema advances
to version 2. Promotion output reports only records added, updated, removed,
and preserved; aggregate hint and mutant counts remain available from the
artifact rather than being repeated in the command status line.

Campaign records use schema version 10 and contain a complete outcome ledger:
stable mutant ID, outcome, workspace-relative file, mutator, source-site
identity and location, replacement identity, existing suppression state, and
the discovered file digest. Incremental execution still reuses only compiler
unviability. `cargo gamma suppress` normally resolves the persisted ledger
after using Cargo metadata to validate the current workspace identity. It
performs no workspace synchronization, builds, baselines, or tests. Explicit
package, file, mutator, diff, shard, feature, and configuration selections are
rejected because they cannot narrow an already persisted campaign ledger. The inherited run
`--dry-run` flag is also rejected rather than ignored; only
`--dry-run-suppress` previews source edits. The command parses only
affected current source files, relocates a uniquely matching unchanged site, reports missing or
ambiguous sites as stale, and verifies the resulting source policy
transactionally. Persisted campaign locators are accepted only after Cargo resolves the selected
directory's current workspace and the located cache's owner marker names that
workspace. If current workspace identity cannot be resolved, the command fails
rather than adopting potentially stale state. Record source paths containing
roots or parent traversal are rejected before edit planning.
Verification unions separately tagged directives that share a source line and
rejects any edit whose syntactic scope would suppress an ineligible mutant. A
rejected edit reports source locations, mutation descriptions, and prior
verdicts rather than internal mutant IDs, then restores every changed file. The
workspace lock is held from ledger selection through source publication and
verification, so a finishing campaign cannot replace the evidence part-way
through a suppression. It never rewrites the campaign
record or progress log.

Reports use a retained source snapshot in a fresh sidecar under cargo-gamma's
private cache, outside the synchronized campaign workspace. The snapshot is
populated from the original text discovery read, including a leading UTF-8
byte-order mark when present. Generation digests and mutation spans use the
normalized text discovery parsed. The snapshot is validated against those
digests before publication and used even when the original checkout changes or
deletes files. Report identities remain rooted at the original workspace and
use original workspace-relative paths;
cache paths never enter JSON, HTML, SARIF, annotations, or diagnostics. Runs
that never build retain the discovered source in the plan and publish from it.

The completed console footer is a single ordered block: `Summary:`, `Stats  :`
for an executed campaign, an actionable `Note   :` for idle directives
followed by indented entries, conditional suppression and hints-promotion
notes, and `Wrote  :` artifact notices. The hints reminder depends on an
addition, correction, or removal in exact killer or compiler-ordering
knowledge learned by this campaign, not on whether `gamma-hints.yaml` already
exists or on generalized counter churn.

### Redirected cache security

On Unix, cargo-gamma creates a previously absent redirected cache with
permissions limited to the invoking user, independently of the process umask.
It does not change permissions on a pre-existing directory: the directory and
its physical ancestry must already be owned by the invoking user or root and
must not permit another user to replace entries. Sticky shared ancestors such
as `/tmp` are accepted, but the cache directory itself must not be writable by
group or other.
