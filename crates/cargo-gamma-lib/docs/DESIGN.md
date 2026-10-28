# cargo-gamma-lib — Design

> Status: **Implemented**.
> Crate name: `cargo-gamma-lib`.

This is the crate's top-level design document.

The provisioned real-toolchain acceptance boundary is specified in
[System tests](design/system-tests.md).

## Purpose

This crate coordinates cargo-gamma campaigns: configuration, discovery,
scratch workspaces, instrumented builds, test selection, process supervision,
verdicts, incremental reuse, reporting, and command dispatch.

## Boundaries

- Rust parsing and instrumentation are delegated to `cargo-gamma-engine`.
- Process-tree mechanics are delegated to `cargo-gamma-process` and
  `cargo-gamma-unsafe`.
- Build-stage metadata captures and nextest inventory commands run through the
  contained process-output lifecycle used by builds and tests. Their stdout and
  stderr are drained concurrently, and descendants are swept before inherited
  pipe handles are allowed to keep capture open.
- Ordinary and offline discovery metadata, and compiler cfg, host, and version
  probes, are trusted setup helpers rather than campaign-owned subtrees. They
  have no campaign build deadline or guaranteed descendant sweep. A hung helper
  can block setup indefinitely; cancellation does not guarantee that its
  descendants terminate. `--build-timeout` bounds compiler stages, not all
  discovery or the whole command.
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
  because surviving descendants can interfere with later mutants.
- The injected guard protocol is provided by dependency-free
  `cargo-gamma-rt`. Its package-local source bundle is exposed only through an
  internal feature used by the coordinator, so published `cargo-gamma-lib`
  packages never depend on repository-relative source paths.
- Ordinary mutant launches record active-guard reach observations through
  `GAMMA_REACH`. Exact-hint and inferred-test probes remain checked guesses;
  missing reach observations mean whole-binary work, never uncovered code.
  Only sealed, passing whole-binary observations may exclude another replacement
  at the same stable source site while deterministic reach narrowing is enabled.
- A reachable exact killer hint is probed before inferred tests, regardless of
  its binary's canonical position. Confirmed kills settle immediately.
  Confirmation and containment requirements apply to every phase.
- A direct-harness filter that excludes a hinted test makes that probe
  inconclusive before any process starts. Inferred and canonical selection
  remain eligible; refusing an empty selection is not a containment failure.
- Runtime derived selections are called inferred tests. Persisted knowledge
  retains its `generalized` schema and existing checkpoint and diagnostic
  `generalized*` field names for compatibility.
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

Mutation selection and declaration evidence have separate scopes. `--file`,
`--exclude-file` (including configuration exclusions), and `--in-diff` restrict
which sources produce mutants, not which package declarations can inform them.
The survey reads excluded sources for module declarations and `Default`
evidence before emitting candidates, preserving implementations in other files
without losing negative evidence for unrelated local types. Evidence remains
package-local. An unanalyzable source disables negative evidence only for its
own package and is reported even when excluded from mutation.
Source labels additionally bind declarations to unique active crate roots and
module paths before collection. Cross-file aliases and re-export chains use
that identity, so selecting only a consumer file still retains evidence about
its declarations. Literal `#[path]` modules use physical file identities,
including parent-relative components and symlink resolution, rather than raw
joined path spellings. A path-resolution failure is reported rather than
silently discarding the crate's declaration evidence. Conditional,
missing, duplicate, or ambiguous module evidence stays unknown rather than
borrowing another same-named type's `Default` implementation or its absence.
The test-module exclusion graph uses the same physical identity for roots,
nodes, edges, and walked-file membership. Aliased references combine their
edges; a genuinely live alternative reference prevents test-only reachability
from excluding that source or its live descendants.

Dependency feature propagation requires activation of optional edges before
their declared features or defaults can become definite evidence. Weak
forwarding does not activate an optional dependency. Conditional or otherwise
uncertain feature contributions remain unknown during cfg evaluation, including
under `not` and nested `all`/`any`; an overapproximation is not treated as exact
absence or presence.

Resource-effect evidence is separate from package-local nominal type evidence.
It may read required workspace dependencies without adding them to mutation or
instrumentation selections. Those evidence-only reads participate in source
generation validation, using digests of the exact bytes read. Discovery and
package-scoped scans follow linked workspace library roots to a fixed point,
without a depth cutoff; repeated roots and cycles are visited only once.
Failure to resolve offline resource-dependency metadata stops discovery with the
original Cargo error and recovery guidance, rather than supplying an empty graph.
Ordinary discovery uses that single full offline metadata result for workspace
identity, targets, feature selection, and resource dependencies. Cache-enabled
discovery retains its separate workspace-target and external-input provenance
passes.
Dependency helpers must provide unconditional/configuration-invariant evidence; propagated feature
sets are not treated as exact compilation-unit cfg. Exact dependency identities
gate supported external API contracts, and unresolved effects remain unknown.

The default storage layout keeps the synchronized source, vendored runtime, and
stable workspace lock under the platform cache home. Cargo artifacts, reach-observation
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

`--re` compiles one or more Rust regular expressions into a single alternative match. Discovery
matches them against the same full name printed by `list mutants`, including the workspace-relative
file and location, qualified item path, mutation summary, and any suppression marker. A malformed pattern is a usage error.
The filter runs after source and diff discovery but before exact-ID selection, sharding, and ordinal
assignment. `list files` under this filter shows only files with selected mutants. `unsuppress`
rejects `--re`, since a partial mutant population cannot establish that a directive is idle.
Reports produced with `--re` record `config.nameFiltered` so merge can combine their verdicts
without treating mutants outside the regex as withdrawn. When a filtered report has newer source
text, merge retains an available unfiltered source presentation so unmatched historical verdicts
remain renderable and counted; a current unfiltered listing supplies current presentations.
If a filtered source introduces a mutant absent from that selected presentation, merge reports an
error and produces no staged document until a complete listing for the current source is included.
Otherwise staging would discard that mutant's verdict permanently.
Staged merges retain this per-file source property in merge provenance, so a second merge cannot
mistake a filtered source for a complete presentation, and retain whether each verdict came from a
name-filtered source. They also retain each mutant's latest source sighting separately from its
verdict and the rendered file source. A complete population
can withdraw that mutant only when it is at least as recent as the sighting. Merged outputs carry
sightings, but not the withdrawal authority of the complete populations they absorbed: merged
inputs never act as complete populations. Staged and direct merges make the same withdrawal
decision only when the complete populations remain among the final merge inputs. Otherwise, a
later filtered input can be refused or kept rather than withdrawn.
The merged report timestamp includes the latest sighting even if it contributed only a pending
listing, preserving its generation when a later merge chooses report metadata.
The completed campaign record also treats a name-filtered run as partial: for unchanged files it
retains valid earlier outcomes and killer hints outside the selected mutant IDs. The latest
campaign population remains the filtered set for hint promotion.
Older reports without the field retain
their existing interpretation.

### Compiler exclusion policy

A campaign validates its pristine scratch workspace once and freezes the package,
target, feature, profile, and compiler context for all subsequent exclusion work.
Runtime dependency links are installed before this validation, not between
validation and instrumented compilation.
Checks and isolation proofs use that same metadata-check invocation; the final
build adds code generation without narrowing the target or package selection.
Isolation reuses the initial pristine validation instead of launching new
pristine controls. Compilation failures are published as they are established,
not deferred until a package-selected convergence finishes.

`compiler-exclusions.json` in the campaign cache is a cumulative inventory of
exact mutant IDs previously established as individually unviable. On first use,
it imports compiler failures from completed and incomplete campaign records and
`gamma-hints.yaml`. Mutation discovery retains matching mutants as compile-error
results, without assigning executable ordinals or emitting their mutation
branches. These historical exclusions count as unviable immediately and remain
part of the population reported by the `Mutated` summary, so subtracting the
`Excluded` total yields the viable population when every remaining mutant builds.
Only executable mutants determine whether compilation and testing are needed.
The policy
is independent of incremental verdict reuse and deliberately ignores changes to
compiler context: an inventory entry remains excluded until explicitly removed.
The existing inventory is authoritative, so removing an entry is not undone by
reimporting older records. Raw mutant listings still describe source candidates.

New individual compiler failures are atomically added before another convergence
round starts, so interruption does not lose them. Mutation-interaction exclusions,
unresolved candidates, and unrelated build failures never enter the inventory.
An unreadable or unsupported inventory is an actionable error, not an empty
inventory. No speculative historical-failure build is needed.

Source updates preserve byte-identical files and their timestamps. Isolation
retains the instrumented source of unaffected, successfully compiled dependency
components; connected components may legitimately need edits while establishing
individual failure or interaction evidence. Successful compilation does not
unconditionally freeze a dependency that a later failure implicates.

### Exact-first adaptive sweep

Every campaign first drains exact hinted killing tests across the population.
Each worker then takes an unresolved mutant through the remaining stages to a
final verdict before claiming another mutant:

1. Run exact hinted killing tests. Every confirmed kill settles immediately,
   regardless of its binary's position in reachable order.
2. Admit inferred named cases from same-item killing-test learning and
   historical site-reach candidates, then rank the combined eligible cases
   strictly by descending global distinct-mutant kills for each binary/test
   pair. Package, qualified target identity, and test name break ties
   deterministically; transfer misses and runtime do not affect this order.
   Historical per-mutant exact-killer hints seed these counts, including
   completed or resumed kills; generalized candidate seed frequencies and
   transfer counters do not. Confirmed final named kills from every stage,
   including exact probes and new full-binary discoveries, update the counts
   immediately. Each mutant contributes once, moving its credit if its named
   killer changes. Unnamed whole-binary failures, enumeration failures, and
   resource or inconclusive outcomes do not credit named tests. A confirmed
   unnamed single-case failure credits the selected case.
   For both direct libtest and nextest, retain
   the first six eligible distinct cases and run them individually in priority
   order, stopping on a confirmed kill. Libtest does not group cases by binary.
3. Run every distinct inferred full binary, best-first and sequentially per
   mutant. Inference includes binaries identified by the named cases, item/file
   reach observations, and file-local kills. There is no binary-candidate cap.
4. Run the remaining reachable full binaries. Package dependency reachability
   is refined by compiler-proven source linkage where available; proven
   unlinked binaries are excluded, while incomplete linkage remains conservative
   rather than becoming evidence of no coverage.

Fallback retains own-package precedence, then orders each package tier by
descending distinct-mutant kill count. Baseline runtime and stable binary
identity break ties. Historical exact-killer hints seed the counts; confirmed
kills from all stages update them before the next fallback snapshot. A known
mutant is credited only once, moving its credit if a different binary kills it.
Unnamed confirmed kills count too, while passing or inconclusive runs do not.
This changes execution order, not the baseline-sorted reachable order used to
choose the final non-killing outcome.

Only stage 1 is a campaign-wide barrier. Stages 2 through 4 execute end-to-end
for each assigned mutant. Selections fan out across mutants, never across binaries
for the same mutant. Stage transitions are internal and do not emit additional Testing messages.
Nextest also runs at most one test process at a time for each mutant while
retaining process-per-test isolation. Every phase uses the configured mutant job
count without a CPU-count cap or aggregate memory reservation. The user controls
overall memory pressure by reducing the job count when necessary. Named-resource
reservations apply to the actual selection, not to a mutant's entire future work.
Independent mutants need not wait for a same-item learning scout.

The phase-2 six-case limit counts mutant-active probes, not control
confirmations or nextest-configured retries. Only passing named probes suppress
duplicate cases in later inference; inconclusive named probes remain eligible.
Duplicate cases and binaries are removed without changing their ranked order.
Unreachable, filtered, duplicate, and already-passing cases do not consume the
six-case allowance. This cap does not remove candidates from full-binary inference.

Per-process memory enforcement and named-resource admission still apply to every
run, including named-test probes and their confirmations. Baseline and sweep use
the same harness-thread configuration.

Full-binary outcomes from phase 3 are reused in phase 4, including inconclusive
outcomes; selected-case passes do not replace a full-binary run. A confirmed
failure in any binary takes precedence over earlier timeout, memory, or flaky
observations. Without a kill, the first non-passing full-binary result in stable
reachable order determines the reported resource/flakiness/inconclusive outcome.
Only mutants whose required full-binary runs all pass are survivors. An empty
reachable set, including one established by sealed whole-binary evidence at the
site, is no coverage. Loss of containment aborts immediately.

Settled kills publish learning before their assignment is released. Named
inference can use every exact-pass kill and full-binary discoveries from earlier
assignments. Candidates are read from live learning at each stage, not frozen
for the population.

Assignment preserves package-turn fairness, then favors functions with fresh
named killers whose seed-plus-transfer-hit count is at least their transfer-miss
count. Repeated misses can remove this priority boost without retiring any
candidate. Historical candidates alone do not establish freshness. Among the
remaining work, independent files and functions are preferred while discoveries
are in flight, followed by sibling learning benefit per estimated cost,
longest-work-first priority, and deterministic ties. Remaining sibling counts
and full-binary discovery costs are recomputed after exact hints finish.
There is no same-function scout gate or additional worker cap.

Intermediate probe state is not durable: resume
repeats unfinished probes, preserving completed verdicts and cumulative execution
accounting. A frozen scheduler version rejects snapshots from earlier policies
with an instruction to start a new campaign rather than mixing verdict policies.

### Progress and dashboard

The ordinary live display uses one active phase line at a time. Workspace
discovery completes `Analyzing the workspace` as `Analyzed the workspace`.
Compiler convergence starts both displays at
`Excluding unviable mutants (0 found)` and retains the monotonic confirmed
compiler-unviability count through ordinary checks and isolation proofs.
Proof subset sizes and interaction-only exclusions never increase this count.
Both displays show only confirmed counts, without check or proof counters.
Historical exclusions appear immediately; each newly proved singleton is published
before isolation continues. There is no speculative scope widening or deferred compiler tally.
Cargo's invocation-local unit counter remains hidden; `--show-build` exposes Cargo's
raw narration for troubleshooting. Planning and testing use the ordinary
cargo-style progress line. `--dashboard` replaces those lines, when progress
display is eligible, with a multiline view of mutant verdicts, explicit and
inferred hint effectiveness, and test-process cost. Both modes share the same
lifecycle and finish before the durable summary and artifact notices are
written.

The testing progress display reports completed and total mutants plus observed
verdict counts. It does not predict completion time: scheduling, contention,
timeouts, and learned test selection make a live ETA misleading.

Current reports advertise mutant-ID scheme 7, which includes normalized
stated-value replacement expressions. Explicit scheme-6 reports cannot be merged
with scheme-7 reports, even when they represent different shards.
Reports that omit `config.mutantIdVersion`, including reports without run metadata,
use historical identity scheme 5, not the scheme of the reader. They can merge
with other scheme-5 reports but cannot contribute IDs to a scheme-7 population.
Survivor-only selection likewise rejects unstamped historical reports, and
`explain` identifies their historical scheme.
An explicit different version is excluded from a merge because its identifiers
cannot safely share a population with the current scheme. A merge may still
produce reports and diagnostics for the compatible population, but
`--min-score` fails when any requested input was excluded this way rather than
grading an incomplete population.
Reports explicitly serialize `config.populationComplete`. Only an explicitly
complete, non-sharded, non-merged population can withdraw older identities absent
from a represented file. Exact-ID, diff-filtered, and mutator-selected reports have no such
authority; legacy reports without the field are conservatively incomplete.
Any explicit mutator selector, including `all`/`@all` and configuration-supplied
selectors, disables authority. Whole-file/package selection alone preserves
authority for successfully analyzed included files.
Successfully analyzed files, backed by retained analyzed source and its digest,
remain represented even when they contain no mutants, so a complete empty
population can retire the last obsolete identity.
Unanalyzed files do not acquire this authority. Merged `notBuilt` counts reflect
the retained, deduplicated outcomes after supersession, not the sum of input
report observations.

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
baseline, mutant probes, whole-binary fallbacks, and confirmations.
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
Test output retains a 4 MiB prefix per stream and, for failure diagnostics, a
64 KiB tail. Individual line reads are bounded even without a newline; oversized
lines keep draining and count as activity, but their fragments cannot supply
authoritative harness verdicts. Runtime startup markers remain detectable across
fragment boundaries. A capped prefix's unfinished final line is diagnostic
text only, not a failure announcement.

Each completed baseline observation records at least one nanosecond. Zero
remains reserved for binaries for which no baseline was measured, including
`--no-baseline` runs, even when the platform clock cannot distinguish the start
and end of a very fast observation.

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
before instrumented compilation begins. Historical compiler exclusions are filtered
before ordinal assignment and are never instrumented. Schema convergence runs
`cargo check --tests --keep-going` with the exact roots, features, and compiler
options already validated by pristine preflight. Proofs repeat that command;
the final code-generating build uses `cargo build --tests --keep-going` with
the same selection. `--test-lib` instead preserves its library-harness-only
`cargo test --no-run --lib --no-fail-fast` command throughout.
This keeps the complete mutation population visible to one convergence and lets
Cargo expose independent failures together without generating or linking test
binaries in every round. Runs whose original Cargo selection is a package subset
retain their narrowed graph throughout.

Structured compiler messages carry package, target, diagnostic, and primary
span context. Direct generated-text blame withdraws a mutant immediately.
Otherwise diagnostics first consider the enclosing item's likely mutations,
then the whole item. Ownership failures prioritize control-flow and state-update
mutations. Item envelopes use the guards'
instrumented coordinates, never original source line numbers. This is ordering
evidence only: missing or misleading locations still fall back to diagnostic-file
mutants, then the failing package, then its transitive dependency cone. Isolation
checks the complete validated target work
and admits at most 4,096 candidates per search and 32 proof checks per target. Diagnostic-local
item searches apply to type and lifetime errors as well as ownership errors, using
item envelopes captured from the original failed schema before any proof rewrites
its guards. Oversized tiers are partitioned deterministically into bounded searches;
passing partitions do not establish
that a cross-partition interaction is absent. Each Cargo convergence
invocation additionally admits at most 64 diagnostic contexts and therefore at most 2,048 proof
checks in total. The total is derived from those two local bounds so every admitted context can use
its complete allowance and one target cannot exhaust the isolation budget needed by a later target;
only contexts with a nonempty tier inside the candidate limit consume a context slot. Contexts with
the largest dependency cones are investigated first so Cargo's diagnostic order cannot spend the
shared ceiling on small targets and strand most of the population. Unresolved
contexts never justify abandoning a package or dependency population. Proven causes are
withdrawn and convergence retries the remaining schema; if no cause can be proved within
the budget, the campaign fails with diagnostic and source-selection advice rather than
publishing unjudged candidates as `notbuilt`. Pristine validation runs once,
before instrumentation; isolation never launches another pristine control. Independent
failing targets and multiple independent failures within one context are isolated
in one global round. Once all diagnostic-local tiers have been searched and
yielded proven progress, broader file/package/dependency tiers are deferred to
the next ordinary convergence check. Clean broader-tier proof builds are not
required to accept a proved local cause. Partitioned tiers and contexts without
diagnostic locality retain their bounded searches. After each proof, its blamed
members or deterministic interaction exclusion are removed from the remaining
search population. Every
retry must strictly shrink that population. Exact-active-subset results
are reused across diagnostic targets that issue identical
Cargo commands within one failed invocation. Local proof allowances remain per target;
cache reuse never crosses source, roots, target-mode, configuration, or invocation
boundaries. If further isolation cannot proceed after making
progress, the remaining neighbors return to the next ordinary convergence
check instead of being abandoned on the strength of an inconclusive extra
probe. A minimal interaction group
excludes one deterministic member as `notbuilt` rather than calling any member
individually unviable. Proofs activate their requested subset within the failed
dependency-connected components, restoring other mutations in those components
to prevent interaction-only failures from becoming individual blame. Disconnected
successful components remain instrumented, preserving their source bytes and mtimes.
Proofs preserve the failed invocation's selected package roots, target selectors, and Cargo feature
graph while narrowing only mutation activity. No isolation path expands
to the whole workspace. Cargo compiler-message rows do not carry artifact profiles. For a failed
invocation that can compile both ordinary targets and test harnesses, ambiguous library,
proc-macro, and binary diagnostics retain the exact failed Cargo verb rather than guessing a mode
or duplicating proof contexts. A context that compiles with the complete dependency cone is
discarded rather than producing unresolved evidence. Direct compiler blame dominates overlapping
unresolved or interaction results from another diagnostic context.

The checked-in `tests/fixtures/isolation-graph` workspace demonstrates why roots
alone do not prove target-graph equivalence. Its application receives a dependency
API through a development dependency: checking its test targets succeeds, but
checking only its ordinary library loses that API and produces E0425. Its ordinary
package has no development dependencies and supports the existing ordinary
library-only proof reduction. A Windows cold-directory sample, using identical
package roots and `--all-features`, measured:

| Context | Original rebuilt units / elapsed | Library-only rebuilt units / elapsed | Outcome |
| --- | --- | --- | --- |
| Ordinary library and binary | 3 / 1,162 ms | 2 / 710 ms | Both pass; retained library/dependency features agree |
| Library and test targets | 4 / 1,168 ms | 1 / 724 ms before failure | Narrowing changes dependency features and fails |

These single samples establish unit counts and the feature-graph counterexample,
not a campaign speedup. Ambiguous harness contexts therefore keep their original
target work; they are not narrowed from diagnostic wording or an ordinary
artifact's presence. Tests exercise this routing in process without launching
Cargo; the checked-in workspace permits independent manual reproduction.

There is no narrow-to-wide fallback during exclusion. An unattributed failure is
reported in its validated context rather than retried with a different feature graph.
Only source files whose active mutation sets change are regenerated; byte-identical
results are not written. Connected dependencies may legitimately change and rebuild
their consumers, but unrelated successful crates are not restored and reapplied.

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

Scratch synchronization invalidates its consistency marker before modifying an
existing tree. If delta synchronization fails, a fresh copy requires successful
removal of the old tree; cleanup failure stops preparation rather than overlaying
current sources onto stale destination-only files.

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
completed or interrupted testing campaign population exactly as recorded. The completed ledger may
retain unchanged outcomes from earlier narrow campaigns for incremental reuse,
but it separately records the latest
campaign population so promotion cannot remove or republish hints for those
carried outcomes. Incremental promotion upserts exact and generalized knowledge
while preserving unrelated entries and exact hints for `pending`, `notbuilt`,
or `ignored` mutants that the campaign did not judge; explicit `--replace` is
the destructive whole-artifact operation and requires a valid persisted
completed campaign record before changing an existing artifact. It never uses
an interrupted checkpoint to replace the whole artifact. Generalized item, file,
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
unviability. Before expensive measured-campaign work, publication checks the
compatibility of an existing completed record even when incremental reuse is
disabled. An unsupported readable generation is preserved and produces an early,
actionable refusal rather than becoming a publication surprise after testing.
Compiler-evidence policy participates in the reuse context independently of the
package version. A policy change prevents fresh runs from adopting older compiler
exclusions without making their ledgers unreadable. Publishing a frozen checkpoint
preserves its original context; exact killer and ordering hints do not become
compiler proof. These context checks govern ordinary cache reuse; they do not
override the independently authoritative historical compiler-exclusion inventory.
`cargo gamma suppress` normally resolves the persisted ledger
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
verification populations include all current candidates in affected sources,
including unrecorded candidates treated as pending. A partial ledger cannot
authorize collateral suppression of those unmeasured candidates. The
workspace lock is held from ledger selection through source publication and
verification, so a finishing campaign cannot replace the evidence part-way
through a suppression. It never rewrites the campaign
record or progress log.

Output publication resolves physical destinations by following each encountered
symlink before applying later parent components. Staging and collision checks
therefore use the same destination the operating system would resolve.
If the final pre-replacement comparison cannot read the destination, publication
removes its staged sibling and reports the comparison failure without replacing
the destination.

### Testing-only resume

`cargo gamma run --resume-testing` continues only a campaign interrupted in its
mutant-testing sweep. It uses the existing copied, instrumented workspace and
compiled binaries without rediscovery, synchronization, instrumentation,
building or baseline measurement. Selection, ordinals,
oracle, test arguments, budgets, resource declarations, launch environment,
and learned reach evidence remain frozen. Live checkout edits, including
promoted hints and suppression edits, do not change the resumed campaign.
Reporting flags and score gates apply to the resumed result. Live `gamma.toml`
is loaded only for `min-score` and `max-flaky`, with command-line gates taking
precedence; execution settings remain frozen. Repeated interrupts and resumes
are supported.
The reporting-only configuration schema recognizes the same top-level keys as
a fresh run but ignores the values of known execution settings, even when they
would be invalid for a fresh run. Unknown keys, reporting gates and TOML syntax
still require valid input.
Resume announces `Planning` and completes with `Planned` for the remaining
mutants before testing starts; it does not print an unlabelled population count.
The testing running total and outcome counters include verdicts retained from
earlier testing segments. Planning still counts only the remaining work.
The resume announcement uses the same restored progress counters as the status
numerator, excluding ignored, compiler-unviable, and not-built outcomes.
The total is retained test outcomes plus the remaining pending mutants, not
the entire discovered population.
The initial testing status is painted immediately before worker setup and
test execution, without waiting for a mutant to finish.

The private campaign cache retains a versioned `testing-session.json` snapshot,
an authoritative `testing-verdicts.jsonl` journal, and `testing-phase.json`.
Prepared snapshot schema version 2 rejects earlier reachability-policy snapshots,
whose settled no-coverage verdicts cannot be safely reused. Those campaigns
require a fresh run.
A fresh measured run invalidates resumability before changing its workspace;
the testing phase becomes resumable only after the prepared snapshot and empty
journal are durably published. The coordinator appends and syncs each completed
verdict and exact probe, without signal-handler I/O or worker checkpoint writes.
Every completed outcome is retained, including survivors, resource failures,
and flakes. Unfinished work has no verdict and remains pending. An interrupted
final journal append is discarded; corruption in a complete entry is an error.
The phase marker binds the snapshot's digest, and each complete journal entry
has its own integrity checksum.
The human-readable progress and bounded selection journals are auxiliary.
Resume recreates either missing log, preserves complete existing entries, and
discards an interrupted, unterminated selection-log tail before appending.
An existing selection retention marker remains effective. Real filesystem errors
are reported rather than silently ignored. Finishing the sweep remains
resumable until the completed ledger is durable; recovery after the last
verdict launches no mutants. Completed ledger publication ends resumability.
Checkpoint publication failures stop testing, wake resource-admission waiters,
and join the workers; no failed or subsequently drained verdict is publicly
reported as settled. Only the durable prefix is retained by the next resume.
An exhausted sweep may still report pending mutants when no trustworthy judgement
was possible. Its finished-testing marker does not turn those into settled
verdicts: if ledger publication fails or is interrupted, resume retains the
settled outcomes and retries the pending mutants.

On Linux, a frozen metered campaign reacquires systemd cgroup delegation when
the invoking shell lacks it, before claiming the prepared testing workspace.
The state-read lock is released before relaunching. Live memory configuration
does not override frozen metering, and an already-delegated invocation, an explicit
relaunch opt-out, or the relaunch recursion guard prevents another relaunch.

Resume validates the checkpoint's tool version, platform, workspace identity,
snapshot integrity, and every complete verdict-journal entry. If mutants remain
pending, it additionally requires unchanged prepared sources, runtime, test
executables, dynamic libraries, build-script output trees, package executables,
and nextest metadata.
The copied workspace's and package directories' `target` trees are generated
output, not frozen source (package directories have a sibling `Cargo.toml`):
tests may create, change, or remove temporary workspaces there. It is excluded
from the source-tree scan, including when reading older checkpoints that
captured such files. Explicitly captured build-script output trees, package
executables, and test binaries remain checked even when located under that tree.
Other source directories named `target` remain part of the frozen source tree.
Resume checks the recorded files, not whether the tree gained new files:
tests may create temporary inputs anywhere in their working directory.
New files are permitted without rediscovery or rebuilding; recorded prepared
inputs still fail validation if missing or changed.
Missing, incompatible, corrupt, completed, or pre-testing state is rejected
with an actionable error, never replaced with a fresh campaign. Workspace and
redirected-cache locks protect resume just as they protect a fresh run.
The snapshot retains Cargo-derived test environments, including build-script
values, in the private cache; arbitrary inherited process environment is not
copied. Their saved digest must match the current inherited environment before
any pending tests run. A changed or unrecorded inherited environment refuses
resume with instructions to restore it or start a new campaign. The digest is
persisted even when the compiler identity is unavailable and incremental reuse
is disabled. When no mutants remain pending, ledger-publication recovery launches
no tests and requires neither the prepared execution artifacts nor host execution
capabilities or a matching inherited environment. It still validates the frozen
campaign evidence and captured report-source provenance; current checkout bytes
never substitute for the source generation that produced the verdicts.

After interruption, ordinary `hints` merging and `suppress` prefer the active campaign's partial
ledger over any older completed record. Pending entries are not treated as
verdicts; only recorded evidence is actionable. These commands retain their
existing source-generation and transactional-edit safeguards. `unsuppress`
continues to inspect current source policy independently. Final reports are
published only after the sweep completes; a partial checkpoint is not a
completed mutation score. Sweep timing and launch diagnostics on a resumed
run combine the durable active-time prefix with the current testing segment;
package spans exclude downtime. Verdict elapsed times are retained for all
completed mutants.

Sweep diagnostics additionally expose optional `phases.sweep.accounting` in
diagnostic schema 7. Before assignment, the campaign freezes the post-build,
post-baseline Pending/nonzero-ordinal population and its initial exact hints.
`initialPopulation` and `initialExactHints` describe that same population, not
the number of hint-file entries or assigned workers. `initialKillerConfirmed`
counts distinct mutants for which the initially recorded test produces an
accepted failure, regardless of the execution tier or which failure later
wins canonical verdict precedence. The private checkpoint retains the original
hint identities; diagnostic reports expose counts, not that private map.

Separate `exact`, `inferred`, `fallback`, and `confirmation` buckets
contain `launchAttempts`, `executionMs`, `hits`, `misses`, and `inconclusive`.
Attempts count runner invocations, not physical child creations; a refusal
before spawning is inconclusive. Confirmation work is disjoint from the other
buckets, and reusable executions add no attempts. `duplicateLaunchesAvoided`
records deduplication. Summed execution milliseconds include concurrent worker
effort and are not campaign wall-clock time.

The private campaign base also contains `gamma-sweep-live.json`, atomically
refreshed with `schemaVersion: 1`, `sweepWallMs`, and `accounting`. These metrics
affect only generated files and logs, never terminal display or dashboard
layout. Cumulative checksummed accounting-only journal snapshots replace
earlier counters during replay rather than being added twice. Confirmation
checkpoints carry only the newly confirmed identity; other checkpoints do not
copy the growing identity set. Replay unions this evidence with the prepared
snapshot and any legacy full-set entries, preserving deduplication across
resume. Final summaries retain the complete set. Each journal entry is
serialized once, checksummed in the existing format, and durably synced;
identity storage grows with confirmations rather than with confirmations
multiplied by checkpoint count. Existing journals are not rewritten. Legacy
coverage that cannot be reconstructed remains unknown, `complete` marks incomplete
accounting, and historical counters remain intact. Interrupted attempts can
lack finalized durations or results. Live-refresh failures increment
`liveRefreshFailures` and persist their full error chain in journal diagnostics;
journal publication failures propagate instead of silently losing accounting.

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
reachability, hints, and queue preparation, then transitions to
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
