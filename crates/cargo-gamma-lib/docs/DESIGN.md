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

### Progress and dashboard

The ordinary live display uses one active phase line at a time. Workspace
discovery completes `Analyzing the workspace` as `Analyzed the workspace`.
Compiler convergence hides Cargo's invocation-local unit counter and reports
the monotonic number of unviable mutants found; `--show-build` exposes Cargo's
raw narration for troubleshooting. Planning and testing use the ordinary
cargo-style progress line. `--dashboard` replaces those lines, when progress
display is eligible, with a multiline view of mutant verdicts, explicit and
inferred hint effectiveness, and test-process cost. Both modes share the same
lifecycle and finish before the durable summary and artifact notices are
written.

The testing progress display reports completed and total mutants plus observed
verdict counts. It does not predict completion time: scheduling, contention,
timeouts, and learned test selection make a live ETA misleading.

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

Source-declared test resources are recovered from ignored harness markers
before baseline execution. A function annotation maps a resource to one
qualified test name; an inline module annotation maps it to every launch of
that test target. The attribute macro emits the ignored harness markers that
the coordinator enumerates to recover those declarations.
Campaign configuration supplies capacities, defaulting each declared resource
to one. Every launch path uses one shared admission coordinator, including
baseline, census, mutant probes, whole-binary fallbacks, and confirmations.
Custom harnesses have no libtest marker registry and therefore contribute no
source-declared resources. Resource-bearing nextest launches additionally limit
the runner to one concurrent test process.

When `--show-build` is enabled, Cargo's leading erase control is consumed
rather than rendered visibly and its color styling remains intact. Without
that flag, Cargo progress and convergence diagnostics remain hidden behind the
stable cargo-gamma phase line.

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
removes stale baseline records. On failure it writes one structured record for
each of at most 64 unique failed tests across the whole campaign beneath
`baseline-failures/<package>/<target>/tests/<test>/failure.json`; unnamed
binary-level failures use categorical leaves. Binaries are folded in plan order
and each binary's retained tests are sorted for deterministic selection. Every
record carries the retained identities for its binary in
`observedFailedTests` and the number beyond the cap in
`observedFailedTestsOmitted`. The campaign-wide 64-record cap applies only to
per-test `testFailure` artifacts; a timeout, stall, memory limit, enumeration
failure, or infrastructure failure remains a single categorical record and
retains its own bounded failed-test evidence even after that cap is exhausted.
Components are cross-platform safe and
length-bounded, and colliding readable paths receive a short identity-derived
digest. Each failure directory also receives `diags.json`, and the ordinary
canonical diagnostics bundle is retained at its configured path. Only
environment values cargo-gamma explicitly controls are eligible for diagnostic
records, never the inherited process environment.

Counted baseline summaries state that the observed tests ran across the measured
test binaries; custom harnesses that announce no count instead report that the
suite passed.

Completed runs publish the five ordinary `gamma-report.json`, HTML, SARIF,
performance-advice, and diagnostics artifacts. An early baseline failure
instead publishes nested `failure.json` and `diags.json` artifacts for up to 64
retained failed tests, plus the canonical diagnostics bundle, before the
scratch workspace is removed. The baseline record uses `schemaVersion: 1` and
records the failure kind and reason; package, target, runner, executable, and
working directory; cargo-gamma's explicit environment overrides; retained
failing-test identities, the omitted failing-test count, and the last observed
test; termination, elapsed time, budget, peak, and memory limit; and
control-character-encoded stdout and stderr tails with a truncation flag.

All selected target packages are scanned in dependency order and instrumented
before compilation begins. Schema convergence runs `cargo check` over every
package with pending mutations, including a package with no runnable test
target, while the final code-generating build retains the reachability-based
test-package selection. This keeps the complete mutation population visible to
one convergence and lets Cargo expose independent failures together without
generating or linking test binaries in every round. Runs whose original Cargo
selection is a package subset retain their narrowed graph throughout.

Structured compiler messages carry package, target, diagnostic, and primary
span context. Direct generated-text blame withdraws a mutant immediately.
Otherwise isolation considers diagnostic-file mutants, then the failing
package, then its transitive dependency cone; checks only the failing target;
and admits at most 4,096 candidates and 32 proof checks per target. Each Cargo convergence
invocation additionally admits at most 64 diagnostic contexts and therefore at most 2,048 proof
checks in total. The total is derived from those two local bounds so every admitted context can use
its complete allowance and one target cannot exhaust the isolation budget needed by a later target;
only contexts with a nonempty tier inside the candidate limit consume a context slot. Contexts with
the largest dependency cones are investigated first so Cargo's diagnostic order cannot spend the
shared ceiling on small targets and strand most of the population. Unresolved
contexts become `notbuilt` rather than extending compiler work without bound. Independent
failing targets are isolated in one global round. A minimal interaction group
excludes one deterministic member as `notbuilt` rather than calling any member
individually unviable. Proofs activate exactly their requested subset and restore every unrelated
pending mutant. They preserve the failed invocation's selected package roots and Cargo feature
graph while narrowing mutation activity and graph-equivalent target work. No isolation path expands
to the whole workspace. Cargo compiler-message rows do not carry artifact profiles. For a failed
invocation that can compile both ordinary targets and test harnesses, ambiguous library,
proc-macro, and binary diagnostics retain the exact failed Cargo verb rather than guessing a mode
or duplicating proof contexts. A context that compiles with the complete dependency cone is
discarded rather than producing unresolved evidence. Direct compiler blame dominates overlapping
unresolved or interaction results from another diagnostic context.

Narrow-to-wide fallback is transactional for verdict state. Before a narrowed check or build, the
coordinator snapshots withdrawals, compiler reasons, abandoned and unavailable populations,
interaction and unresolved evidence, probes, ordering counters, and compiled-source evidence. A
stuck narrow attempt restores that snapshot before the wider graph runs, so only blame confirmed
under the successful graph survives.

The isolation subsystem lives under `exec/build/isolation.rs`; the coordinator owns stage
transitions, while the subordinate module owns diagnostic contexts, tiers, campaign budgets, proof
memoization, and interaction minimization. Splice invalidation uses the symmetric withdrawal delta,
and its guard index is updated only for dirty files.

Cargo convergence evidence is decoded only for event consumers that explicitly request it.
Diagnostic consumers retain the full opt-in evidence, exposed with standard-library path views;
the normal console retains only monotonic compiler-unviability counts.

Target-frontier acceptance remains deliberately absent. Deterministic command-count fixtures show
that frontiers followed by mandatory global confirmation add invocations to clean, direct,
downstream, and interaction cases. Omitting confirmation is unsound under Cargo feature unification
and cross-target interactions, so no package-at-a-time acceptance is permitted.

Instrumentation reads the synchronized scratch tree rather than re-reading the
live checkout after discovery. Each copied source is checked against the
generation digest recorded when its mutants were discovered after that source
is scanned and before the complete population is instrumented. The comparison
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

Checked-in hints use grouped YAML schema version 4. `cargo gamma hints` reads
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
record it replaced. Version-12 campaign
records persist workspace-relative paths for exact identities and keep ambiguous
grouped-probe guesses in score-neutral scheduling hints rather than observed
killer identities. Unsupported
campaign-record versions are discarded for automatic reuse and rejected by
state-consuming commands. The hints context
contains only the generating HEAD commit and UTC date because hints are
revalidated scheduling advice, not context-gated evidence. The independently
versioned generalized schema is version 3. It distinguishes seed observations
from cross-mutant transfer hits and misses, interns repeated killing-test and
binary identities, and persists stable reach sites with the engine-owned site
digest rather than normalized source text. Version-1 generalized hints migrate
with normalized seeds and reset observations because their meanings are not
comparable; version-2 observations are preserved. Other generalized schema
versions remain unsupported. Promotion output reports only records added,
updated, removed, and preserved; aggregate hint and mutant counts remain
available from the artifact rather than being repeated in the command status
line.

Campaign records use schema version 12 and contain a complete outcome ledger:
stable mutant ID, outcome, workspace-relative file, mutator, source-site
identity and location, replacement identity, existing suppression state, and
the discovered file digest. A killing test is recorded in that ledger only when
the harness names it or a single-test selection proves it; ambiguous grouped
failures remain scheduling hints. Incremental execution still reuses only compiler
unviability. `cargo gamma suppress` normally resolves the persisted ledger
after using Cargo metadata to validate the current workspace identity. It
performs no workspace synchronization, builds, baselines, or tests. Explicit
package, file, mutator, diff, shard, feature, configuration, and execution settings are not offered
because they cannot narrow an already persisted campaign ledger. Like `unsuppress`, the command
previews source edits by default and requires `--apply` to write them. The command parses only
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

After baseline measurement, `--dashboard` replaces the ordinary single-line
progress bar with a multiline in-place view of outcome counts, selection-tier
hit rates, and test-process launch costs. It begins in `PLANNING` before
reachability, census, projection, and queue preparation, then transitions to
`TESTING` at the sweep-planned event. It is enabled only when the resolved
progress policy permits terminal output, redraws at most once per second, and
selects small, medium, or large rendering from the current terminal width.
The width is sampled again before every eligible repaint, so resizing the
window changes layout without restarting the campaign. Refreshes erase and
replace the display in one synchronized terminal update rather than exposing
an empty intermediate frame. While workers produce no events, the sweep
coordinator emits a one-second heartbeat that flushes rate-limited state and
refreshes wall-clock-derived metrics. The dashboard records the visible width
of each row it draws. A repaint moves upward by the resulting physical row
count and erases to the end of the screen before writing the replacement; it
does not depend on the terminal's global saved-cursor slot, which other
programs may overwrite or some terminals may not implement. Recomputing the
physical row count at the current terminal width removes obsolete rows after
terminal reflow. Survivor,
timeout, out-of-memory, and flaky verdicts are still written as individual
lines above the display. Clearing, exceptional output, finalization, and
failure abandonment all restore the cursor before ordinary diagnostics or the
final summary are written.

All three layouts use Unicode separators and box drawing. The same resolved
color policy used by ordinary console reporting is carried into the dashboard:
section hierarchy and outcome semantics are colored under `--color=always` (or
automatic terminal color) and emit no style escapes under `--color=never`.

During testing the dashboard reuses the ordinary progress renderer as its
header, preserving its progress bar and verdict totals. The
large layout puts equal-height Mutants and Test Execution panels side by side
and centers Hints beneath them. Panel widths are derived from their longest
rendered row with one space of inner padding, so short panels do not retain a
fixed right margin and long metrics cannot cross the border. Label and value
columns use the same three-space gutter in every panel. Mutants uses one cell per mutant through 100
displayed outcomes and apportions larger populations over a 10-by-10 waffle,
so each cell then represents approximately one percent. Exact legend counts
remain authoritative for classifications too small to receive a cell.

Hints deliberately includes only explicit persisted hints and inferred
generalized hints, each with an attempt count and hit rate. Test Execution
reports test-binary count, current worker occupancy, five-minute mutant
throughput, whole versus filtered process launches, mean and maximum launch
runtime, launches per mutant completed during this sweep, the largest
test-binary baseline memory peak when the platform measured one, and the
baseline-runtime estimate avoided by successful explicit or inferred hints.

### Redirected cache security

On Unix, cargo-gamma creates a previously absent redirected cache with
permissions limited to the invoking user, independently of the process umask.
It does not change permissions on a pre-existing directory: the directory and
its physical ancestry must already be owned by the invoking user or root and
must not permit another user to replace entries. Sticky shared ancestors such
as `/tmp` are accepted, but the cache directory itself must not be writable by
group or other.
