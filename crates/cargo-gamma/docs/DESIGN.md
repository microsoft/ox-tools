# cargo-gamma architecture

cargo-gamma measures whether selected Rust tests detect small source changes.
Its unit of evidence is one mutation, activated in a fresh test process against
a prepared set of instrumented binaries. A campaign reports both the test gaps
it observed and the mutations it could not validly judge.

This document defines the campaign contract and its correctness boundaries.
Option syntax belongs in [CMDLINE.md](CMDLINE.md), configuration in
[CONFIG.md](CONFIG.md), operators in [MUTATORS.md](MUTATORS.md), and execution
ordering in [SCHEDULING.md](SCHEDULING.md). The
[implementation guide](IMPLEMENTATION.md) records mechanics; the
[migration guide](MIGRATION.md) records compatibility changes.

## Architectural decisions

The central representation is a **mutant schema**: source containing guarded
alternatives for many mutations. A process-local selector activates one mutant;
every other guard executes the original source. With no selector, the same
instrumented binaries execute the unmutated baseline.

Compilation is therefore shared rather than repeated for every mutant. This
does not promise one successful compiler invocation: type errors and interactions
can require multiple checks, isolation proofs, rewrites, and final build attempts.
The optimization removes compilation from the ordinary per-mutant testing loop,
not from campaign preparation.

Nested guards avoid exponential expansion but can still grow superlinearly
because enclosing alternatives repeat original expressions.

Five boundaries keep the evidence interpretable:

- **Coordinator:** resolves policy, chooses the population and test oracle,
  manages reusable knowledge, schedules work, and produces verdicts and reports.
- **Source engine:** discovers sites, interprets source policy, assigns stable
  identities, and rewrites source. It does not decide test outcomes.
- **Scratch workspace and Cargo:** preserve the selected build context while
  keeping instrumentation out of the checkout.
- **Process supervisor:** owns launches, observations, resource enforcement,
  cancellation, and process-tree cleanup. It does not define mutation scoring.
- **Guard runtime:** selects the active mutation and observes guard reach.
  It is dependency-free so injection does not introduce another dependency graph
  or build script into the project under test.

All instrumented packages use the same vendored runtime. Existing dependencies
on that runtime are redirected while retaining feature settings; an unrelated
dependency occupying its required crate name is refused.

## Campaign pipeline

A normal measured campaign proceeds through these dependent stages:

1. **Resolve and discover.** Read configuration and Cargo metadata, determine
   active build conditions, discover source and mutations, apply population
   selection and policy, and validate reusable compiler evidence.
2. **Prepare the workspace.** Synchronize a private source copy, preserve Cargo
   path/configuration semantics, inject the runtime, and validate pristine source
   under the selected compilation scope.
3. **Converge the schema.** Instrument the admitted population, withdraw proven
   compiler failures or identified interactions, and check again until the
   complete schema is accepted. Generate the test binaries.
4. **Establish the oracle.** Retain relevant admitted test binaries, reconstruct
   their execution environment, discover shared-resource declarations, and run
   the unmutated baseline.
5. **Plan and test.** Load candidate killers, schedule unresolved mutants, and
   obtain fresh outcomes. Ordinary active-mutant launches also observe reach;
   there is no separate reach-sampling campaign.
6. **Publish.** Persist outcomes and learning, render reports from the measured
   source generation, and evaluate requested gates and source expectations.

Target-linkage capture is part of preparation, not a substitute for baseline or
runtime reach evidence. An empty selection still publishes empty reports rather
than leaving an earlier campaign's reports in place.

### Population and source semantics

Package, file, mutator, name, diff, and shard selection define which mutations
the campaign is answering for. Selection must be explicit in reports so that a
partial population cannot masquerade as a complete one.

Discovery follows the configured target, features, profile, and compiler flags.
Definitely inactive source is excluded; unknown conditions remain eligible.
File and diff filters restrict mutations, not the package declarations needed
to reason about those mutations. Unanalyzable files are reported, not silently
represented as files with no candidates.

The engine uses parsed structure and byte spans but edits the original text.
Inactive guards must preserve observable evaluation order, moves, borrows,
temporary lifetimes, and destruction order. Only one mutant may be active in a
process. In particular, nested replacements need not activate together, but
rustc still checks their combined guarded representation.

Source-visible proofs may omit invalid or equivalent candidates. A separate
resource-safety policy omits mutations proven to introduce stalled loops or
unbounded retained growth. These omitted candidates have no verdict or score
contribution; they are not kills or source suppressions. Such proofs require
bounded, resolved evidence, not familiar names or historical timeout results.
Unknown effects or exhausted proof budgets retain the candidate. Some noisy
optimistic candidate classes are deliberately default-off unless selected
explicitly; the mutator catalog defines that selection policy.

Statement deletion uses concrete names such as `stmt.delete_call` and
`stmt.delete_assign` for selection and suppression.

`--in-diff` selects changed mutation sites, not tests affected by a change.
A test-only edit can select nothing even if it removes protection for unchanged
production code. A passing diff campaign makes no claim about those other sites.
An exact, existing, workspace-relative Rust file outside every workspace package
directory is understood but contributes no mutation sites when no package
in the selected declaration inventory declares it with `mod` or `#[path]`.
A diff with such a file still counts as understood if its only other paths
are unmatched non-Rust files.
An exact mutation candidate takes precedence, but an exact external file
checked for module ownership is never redirected to another file by fuzzy
matching. For a Git rename, the `rename to` header gives the destination
without a diff prefix. A guessed prefix can help find a mutation candidate
but cannot justify skipping an unresolved Rust file.
The exception does not apply to unsafe or absolute paths, missing
files, or files under a package directory. If declaration analysis is
incomplete or finds a module with a conditional `#[path]` it cannot resolve,
Gamma refuses the exception rather than assuming the file is unowned.
Declaration evidence is limited to the parsed workspace inventory: it does
not resolve `include!` arguments or recursively follow declarations from files
outside that inventory.

### Source policy

Source suppressions and configured exclusions are reviewed policy, distinct from
test-derived evidence. Suppressed mutants remain visible as `ignored`.
Attributes and equivalent comment directives support statements and expressions
where stable Rust cannot accept custom attributes. Malformed directives and
unmatched configured selectors are errors rather than silent policy changes.

Source expectations are assertions, not suppressions: governed mutants still
run and count normally. `expect_killed` requires `killed`; `expect_survived`
accepts score-bearing undetected outcomes, including resource outcomes and
`uncovered`. Excluded or pending outcomes prove neither assertion. Per-site
timeout multipliers change execution budgets, not scoring.

## Compiler convergence and proof boundaries

Pristine validation fixes the package roots, target selection, features, and
compiler options. Ordinary campaigns use global test-target checks followed by
code generation; library-only campaigns retain Cargo's library-test selection.
Mutated packages without runnable tests still require compiler validation.
Doctests, examples, and benchmarks are not independent campaign test oracles.

Convergence repeatedly performs three operations:

1. Emit the currently admitted schema and ask Cargo to compile the selected scope.
2. Attribute structured diagnostics to generated replacement text where that is
   sufficient evidence; otherwise isolate the failure with bounded proof builds.
3. Withdraw confirmed causes, regenerate changed files, and check the whole
   remaining schema again.

A diagnostic near a guard is not necessarily blame. Ownership, lifetime,
inference cascades, and deletions can fail away from the responsible edit.
These cases require isolation rather than broad positional attribution.
An individually established compiler failure becomes `unviable`, with a bounded
reason. If a group fails only in combination, one deterministic member is
removed as `notbuilt`; the interaction does not prove individual unviability.

Isolation changes the active mutation subset, not Cargo's validated roots or
feature-unification graph. Mutations elsewhere in the failing connected
components are restored for the proof; disconnected successful components may
remain instrumented. Clean partitions do not prove their members individually
viable or rule out interactions across partitions. A globally successful schema
is still required before tests run.

Proof work and rollback rounds are bounded separately. Diagnostic-local and
dependency-scoped searches may reduce cost, but unresolved causes do not justify
excluding an entire dependency population as unviable. Exhaustion without safe
progress stops the campaign; a stuck build is an incomplete answer, not a
successful campaign containing thousands of inferred exclusions.

The final code-generating build can expose failures absent from checking and
uses the same convergence discipline. Instrumentation can itself prevent a
combination of individually valid edits from compiling; compiler convergence is
not proof that every rejected schema member is an invalid standalone mutation.

### Conditional compiler optimizations

Proof memoization is scoped to the exact active/background schema and compiler
context. Cross-round reuse additionally requires a frozen source and mutation
generation, identical roots and targets, compiler/toolchain, configuration,
environment, dependency artifacts, and attribution scope. Missing identity
evidence disables reuse. Incomplete results and timeouts cannot become
definitive cached proofs.

Complete cross-round identities currently require a sealed successful check of
a dependency-free source graph, optionally including the exact tool-owned
runtime. Build scripts, proc macros, arbitrary dependencies, unsupported source
expansion, or incomplete capture disable this optimization.

A selective compiler wrapper may replay supported successful metadata/dep-info
units only while their complete commands, source/search-directory inventories,
dependencies, toolchain, environment, and outputs remain valid. Unsupported or
changed units invoke the real compiler; code-generation and linking units do
too. Cargo retains scheduling and global confirmation. Fewer compiler starts
do not guarantee lower elapsed time because validating evidence also costs work.

## Test oracle and runtime observations

By default, only tests belonging to a mutant's own package may judge it.
`--test-package` selects other oracle packages; `--test-workspace` admits the
workspace. Reverse dependents do not improve another package's score unless
policy admits them. Library-only and target/test filters further constrain the
oracle; impossible selections are errors.

Package dependency reachability is conservative. Exact compiler-captured source
linkage may narrow it, but missing, opaque, or ambiguous capture retains the
broader package-level selection. A failed exact-target build falls back to the
admitted target kinds, not a broader package or feature scope.

### Baseline

The baseline runs the retained instrumented binaries with no mutant active.
It establishes that the selected tests pass and measures duration, longest
harness silence, and available process-tree memory peaks. It does **not** prove
which mutation guards individual tests reach.

Every retained baseline binary settles once, within its budgets. A failing
binary is not retried into acceptance, and a failure does not cancel the other
retained binaries. Failure stops mutation testing and publishes bounded local
failure records and diagnostics.

Direct libtest and nextest runs use the prepared Cargo package, build-script,
toolchain, working-directory, and loader environment. Nextest reuses the built
binaries rather than invoking Cargo for each mutant. Its runner-specific
environment remains nextest's responsibility.

`--no-baseline` deliberately weakens attribution: an already-red test can be
credited as a kill, and there is no baseline-derived timeout, stall threshold,
or memory ceiling. Explicit memory enforcement then requires an explicit limit.

### Selection and reach

Exact killer hints are attempted first. Unresolved mutants proceed through
bounded ranked named probes, inferred full binaries, and remaining reachable
full binaries. Passing a named probe never establishes survival. Whole-binary
results are reused within that mutant, and every required full-binary check
must pass before survival is reported. Scheduling details live in
[SCHEDULING.md](SCHEDULING.md).

An accepted test failure settles immediately; ordinary kills have no unmutated
confirmation run. A green baseline cannot rule out intermittent failures later,
so those failures can count as kills. Nextest enumeration failures are different:
an unmutated control must establish attribution. A failed or inconclusive control
produces `flaky`, not a kill.

Reach observations accompany active-mutant launches. A process-owned sealed
stream distinguishes reached, not reached, and unknown. Missing, malformed,
truncated, or descendant-appended streams cannot establish absence. Observation
failure does not convert an otherwise valid test outcome into a reach conclusion;
runtime selector acquisition failure instead prevents a trustworthy verdict.

A passing whole-binary non-reach observation may avoid a later launch only for
another replacement at the exact same stable source site, under deterministic
reach policy. Negative reach cannot substitute for execution when the current
attempt has a timeout, stall budget, or memory metering: non-reach alone cannot
prove that an unexecuted attempt meets its resource policy.

If no reachable binary remains, the mutant is `uncovered`. This includes an
empty admitted reachable test set, not just learned non-reach. If a required
whole binary actually runs and passes, its unknown or negative reach observation
does not itself turn that mutant into `uncovered`. Unknown reach runs more tests;
it never means no test reached the site.

`--whole-test-binaries` disables case selection and deterministic reach narrowing.
It is the conservative mode for reach affected by threads, time, or randomness,
but cannot manufacture runtime tests for code the oracle does not exercise.

## Verdicts, scoring, and exit behavior

Verdicts describe evidence, not merely whether a subprocess returned nonzero.

| Verdict | Evidence | Score treatment |
| --- | --- | --- |
| `killed` | An accepted mutant-caused test failure, including confirmed nextest enumeration failure | Detected |
| `survived` | Required whole-binary executions passed | Undetected |
| `uncovered` | No reachable runtime test remains to execute for the site | Undetected |
| `timeout` | A confirmed time or stall budget violation | Undetected |
| `outofmem` | A confirmed memory ceiling violation | Undetected |
| `unviable` | Individual compiler failure or an applicable historical compiler exclusion | Excluded |
| `ignored` | Explicit source or configuration policy | Excluded |
| `notbuilt` | Source/build unavailable or a schema-interaction exclusion without individual blame | Excluded |
| `flaky` | An unmutated nextest control failed or was inconclusive, or a retained historical flaky result | Excluded |
| `pending` | No trustworthy judgment, including never-run or locally refused work | Excluded |

The score is `killed / (killed + survived + uncovered + timeout + outofmem)`.
Resource exhaustion changes behavior but is not assertion-based detection.
Unviable, never-built, unknown, and never-run work must not become survivors or
kills merely to complete the table.

Requested score and flaky gates fail on pending work or an empty gradeable
population. `--max-flaky` independently limits inconclusive flaky outcomes; it
is not general intermittent-test detection. The exception is an empty final
`--in-diff` selection, which succeeds even with gates. A nonempty population
containing only excluded outcomes is not that exception.

| Exit | Meaning |
| ---: | --- |
| `0` | Command and requested gates/expectations passed |
| `1` | Invalid usage, configuration, or selection |
| `2` | Completed command failed a gate or expectation, including an incomplete population for a requested gate |
| `3` | The requested answer could not be completed |
| `70` | Internal panic reached the executable boundary |

Without a requested gate, reportable survivors or pending outcomes do not
implicitly introduce grading policy. Operational failures remain failures.

## Identity, reuse, and invalidation

Stable mutant identities derive from workspace-relative source, enclosing item,
mutator, normalized site/replacement content, and occurrence disambiguation.
They are independent of run-local ordinals and line numbers. Comments and
insignificant whitespace do not change identity; meaningful token or literal
changes can. Occurrence and `replacement_index` distinguish repeated forms;
the digest is rendered as twelve hex characters. Identity schemes are versioned
namespaces, not interchangeable hashes.

Reusable knowledge has deliberately different trust levels:

- **Killer and inferred hints** affect order only. Candidate tests execute again.
  Missing, stale, or unsupported automatic hints fall back to colder scheduling.
- **Ordinary compiler evidence** requires matching source and complete compilation
  context, including the compiler-evidence policy epoch. Undeclared build-script
  inputs can make that context unknowable and disable reuse.
- **Historical compiler exclusions** are a separate explicit policy inventory,
  `compiler-exclusions.json`, applied by exact ID before guards and ordinals.
  They persist across toolchain/features changes and `--incremental no`.
- **Test verdicts** are not adopted by a new campaign. Every score-bearing
  outcome is re-established, even when source and environment are unchanged.
  Resume continues the same campaign and is the deliberate exception.

The compiler inventory is seeded from prior completed/incomplete evidence and
checked-in hints only when absent. Existing entries are authoritative, subject
to evidence-policy revalidation after attribution or encoding changes.
Individually proved failures are saved during preparation; interaction-only
removals are not imported as compiler failures. Retrying a repaired entry
requires removing that entry and disabling ordinary incremental reuse; deleting
the whole inventory permits historical evidence to be imported again.

Deleting hint/proof caches may change cost only, but deleting this inventory can
change the selected population. A blanket claim that all cached state is
score-neutral would therefore be incorrect. Reviewed source suppressions remain
separate durable project policy.

Completed ledgers retain observed source and outcomes even when conservative
reuse eligibility is unavailable. Explicit postprocessing validates workspace
identity and affected current sites. Suppression edits are previewed by default;
apply verifies their effect transactionally. Hint promotion preserves unrelated
knowledge unless replacement is explicitly requested, and refuses unreadable
future formats rather than silently destroying them.

## Workspace, persistence, and reports

Normal runs instrument only the synchronized workspace. The default layout
separates external source/runtime scratch from campaign state and compiler
artifacts beneath Cargo's resolved target directory. `--cache-dir` selects an
owned all-in-one layout; `--artifact-dir` relocates published outputs separately.
One process-held workspace lock serializes measured and source-changing commands.
Ownership markers and a second redirected-cache lock prevent accidental adoption
or concurrent mutation of another workspace's state.

Synchronization preserves unchanged bytes and timestamps, updates changed inputs,
removes deleted inputs, and keeps generated output separate. Relative external
path dependencies stay anchored to their original locations. These are build
correctness constraints, not just copy optimizations.

Reports use retained source bytes from the measured generation, not later
checkout contents, while preserving original project-relative identities.
Console, JSON, HTML, SARIF, diagnostics, and gates share one outcome model.
Default publication is under `target/cargo-gamma`; baseline failures have
separate bounded records. Reusable ledgers and journals are not published reports.
`clean` removes owned scratch/campaign state, not published reports or source policy.

The mutation-testing-elements JSON mapping preserves scoring: timeouts and
memory exhaustion export as `Survived` with distinguishing reasons; `notbuilt`
and `flaky` export as `Ignored`. `--only-survivors` selects genuine survivors
from the prior report, intersects current discovery, and obtains fresh verdicts.
It does not accidentally select resource outcomes sharing the interchange status.

### Resume and merge compatibility

Testing resume uses the frozen prepared campaign without discovery, copying,
building, or baseline. It retains durable verdicts and retries unfinished work.
Selection, ordinals, oracle, resources, budgets, environment, and scheduling policy
remain frozen; reporting controls and score/flaky gates may change.

Resume validates snapshot/journal integrity, tool/platform/schema compatibility,
campaign identity, and captured source provenance. Pending execution additionally
requires unchanged prepared inputs/artifacts, inherited environment, and host
capabilities. Symlink targets and executable identity are part of validation.
If all verdicts were recorded before publication was interrupted, recovery may
publish without those execution prerequisites. Completed, incompatible, corrupt,
or pre-testing state is an error, never permission to start a fresh run silently.

Shard reports merge by stable identity and source freshness. Incompatible
identity namespaces are not counted together. Missing/inconsistent shards or
excluded incompatible inputs fail requested score/flaky gates. A shard or
name-filtered population cannot prove that absent historical mutants were removed;
withdrawal requires authoritative complete-population evidence. Source
presentations and latest sightings are tracked separately so filtering cannot
silently erase retained results.

## Execution safety and limitations

Each mutant executes in fresh processes sharing immutable binaries. Launches
own process trees, not just leader PIDs. Cleanup must finish before releasing
resource reservations. Named source resources receive configured capacities and
the same admission policy during baseline, probes, fallback, and controls.
Concurrency is bounded by jobs and declared resources, not an aggregate memory
reservation; users must size jobs for the host.

Timeouts and stalls use baseline observations and receive larger-budget
confirmation. The last test the harness announced is a landmark rather than a diagnosis:
parallel libtest harnesses announce completion, not the still-running test.
Memory ceilings use baseline peaks plus policy and receive
resource-specific retries. Enforcement is kernel-backed on usable Linux cgroups
and Windows jobs; macOS sampling is a soft bound. Unsupported inherited defaults
may degrade explicitly, while an unavailable explicitly requested guarantee fails.
Where no sealed process-tree boundary exists, process groups are best effort
and escaping descendants can outlive cleanup.

This is not a sandbox for hostile projects: build scripts and tests execute
user code. Metadata, compiler-version, and cfg discovery helpers are trusted,
unbounded setup commands outside campaign containment and build timeout policy.
A whole-command deadline or guaranteed cleanup of those helpers needs external
supervision. Redirected caches must be trusted executable storage; Windows does
not provide the Unix ownership checks used for cache ancestors.

Repository-controlled terminal text is control-character encoded. Published
reasons use observed outcomes, not raw child output that may contain secrets.
Local failure artifacts can retain bounded output and execution context.
The HTML viewer is embedded, with no external viewer code able to read report
source. Reports themselves contain project source and require appropriate handling.

The remaining limits are intentional:

- Guards enlarge source/binaries, perturb layout and optimization, and may expose
  compiler or stack limits. Instrumented binaries are not application benchmarks.
- Survivors require complete relevant execution; hints cannot remove that cost.
- Runtime reach assumes determinism when used negatively. Whole-binary mode
  disables that optimization, not nondeterminism itself.
- Doctests are outside the model: Rust compiles them as separate programs,
  which would reintroduce per-mutant compilation.
- Compile-time behavior and opaque macro expansion are not equivalent
  to runtime guard execution. Static source analysis is deliberately incomplete.
- Containment, memory readings, and environment capture depend on the host.
  Linux uses its immutable startup environment image; other Unix hosts require
  earlier native initializers not to mutate the environment concurrently.

The governing rule is to preserve uncertainty: build or run more work when
evidence is incomplete, report unjudged work honestly, and never manufacture a
better mutation score from missing observations.
