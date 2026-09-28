# Layer 6: exact mutant replay and explanation

[Stack root](README.md) | [Previous layer](05-external-artifacts.md) |
[Next layer](07-const-functions.md)

## Outcome and interface

Execute exactly named current candidates, optionally corroborated against a
previous report, and explain either current or historical findings.

| Invocation | Meaning |
| --- | --- |
| `run --mutant-id ID` repeated | Execute the deduplicated named current candidates. |
| `run --from-report PATH` | Execute the named report's candidate population under current configuration. |
| `run --from-report PATH --mutant-id ID` repeated | Execute that report's explicitly named subset after historical/current corroboration. |
| `list mutants --mutant-id ID` | Show the exact resolved current population without building. |
| `explain ID` with workspace selection context | Explain a current candidate without execution. |
| `explain ID --report PATH` | Explain the historical report entry without requiring its source checkout. |
| `explain` with registry name/family/preset/alias | Preserve existing registry explanation without Cargo discovery. |

An ID is not a `--mutators` selector. Use complete IDs, never ordinal numbers or
implicitly accepted prefixes.

The report is a selection/evidence input, not a source of fresh verdicts and not
an executable configuration file. Current effective package, feature, profile,
source exclusion, test-oracle, and suppression settings still apply.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\commands\cli.rs`, `dispatch.rs`, `config.rs` | Context, option validation, registry-only explanation |
| `cargo-gamma-lib\src\commands\run.rs`, `list.rs`, `explain.rs` | Shared resolution followed by command-specific behavior |
| `cargo-gamma-lib\src\discover\survey.rs`, `plan.rs` | Existing `retain_only`, full discovery before strict filtering, source generation |
| `cargo-gamma-engine\src\model\identity.rs` | Existing ID scheme and normalization, unchanged by this feature |
| `cargo-gamma-lib\src\commands\suppress.rs` | Prior art for corroborated unchanged/moved source-site matching |
| `cargo-gamma-lib\src\merge\read.rs`, `elements\report.rs` | Bounded report reading and layer 1 provenance |

## Resolution contract

Every requested candidate must resolve before any build or test is launched.
Missing, ambiguous, incompatible, or conflicting requests fail together with
actionable diagnostics. A request containing a valid ID and an invalid ID must
not execute the valid portion and appear successful.

Deduplicate repeated input IDs deterministically and disclose the resulting
selection. Reject an empty report-derived selection rather than treating it as
evidence that old findings were resolved.

Strictness also applies to a bare `--from-report`: the report explicitly names
the requested population. If only part survives source edits, report the
unresolved entries and execute nothing. The caller can rediscover or supply an
explicit valid subset; this interface does not silently become best-effort
iteration.

When no mutator selection was explicitly provided by CLI or configuration,
resolve explicit IDs against all registered mutators, not just `@default`.
An explicit mutator restriction remains a constraint; do not silently widen it.
The same rule applies to package/file restrictions and current suppression.

Reject exact replay combined with `--only-survivors-from`, a new diff, or
sharding in the initial interface. These can silently shrink a named set and
are not necessary to satisfy exact replay. A report originally produced by a
shard is still a valid explicit candidate list when its identities/context can
be validated; do not reapply its shard filter.

Suppressed candidates remain suppressed and are identified before execution.
For a strict exact request, an excluded candidate is a conflict requiring the
caller to choose another set or deliberately change policy, not permission to
override suppression.

## Implementation sequence

### Extract a shared resolved-selection operation

Build a small internal result containing requested IDs, uniquely matched current
candidates, provenance when supplied, and all resolution diagnostics.

Reuse `Survey` discovery, but do not call `retain_only` so early that missing
requests become invisible. First collect the relevant current population, index
by `MutantId`, reject duplicate/ambiguous identities, validate every request, and
only then retain candidates and assign execution work.

Keep normal unrestricted campaigns on their existing staged-discovery path.
Exact requests can pay for complete discovery before building because their
all-or-nothing contract requires it.

Retain the source digests used during resolution and validate synchronization
against them. An edit between lookup and scratch preparation invalidates the
request; it must not execute a different source generation.

### Corroborate report-backed matches

A bare ID identifies a current candidate. It cannot independently prove that
the candidate is the same historical occurrence a user remembers.

For report-backed replay, verify producer, supported report shape, identity
scheme, file, mutator, replacement, and enclosing source context. Reuse the
embedded source and validated locations where possible; add minimal versioned
replay context to the free-form metadata only where reconstruction is
insufficient.

Use a conservative enclosing-function/item token fingerprint to corroborate
historical matches, with comments/formatting normalized consistently with the
engine. Function-local changes can invalidate a report-backed match even when
the smaller mutation-site ID remains unchanged. Test edits in separate test
items and unambiguous line movement need not invalidate it.

This specifically handles occurrence-index reuse: deleting the first of two
identical sites may give the remaining site the removed site's old ID. A hash
hit is not enough. A changed repeated-site group or changed enclosing item
requires rediscovery/new evidence rather than guessing correspondence.

Do not change the existing ID algorithm merely to implement corroboration.
Legacy reports remain explainable; absent identity metadata or insufficient
historical context is not automatically current/compatible for strict replay.
Give an instruction to rediscover or generate a new report.

Validate report paths/locations as report data. Do not use a report's absolute
`projectRoot` as authorization to read arbitrary local files. Historical
explanation can use embedded source; replay uses the explicitly selected current
workspace and its discovered files.

### Execute only the resolved set

Pass the resolved population through normal suppression, build viability,
baseline, confirmation, resource policy, and score handling. No previous
score-bearing result settles a new verdict.

Use layer 1's partial-report metadata and record the audited ID set and parent
report identity. Preserve the input bytes/identity before output publication so
reusing an input report's output path cannot change the selection mid-run.

Do not broaden `--only-survivors-from` to unresolved/new outcomes. It may reuse
safe helpers, but broader iterative policy remains deferred.

### Explain without executing

Keep recognized registry selectors on the existing cheap path.
Automatically recognize the emitted full-ID spelling; report context also
allows explicit lookup of historical IDs. Unknown full IDs get ID-specific
diagnostics, not a suggestion that they are mutator names.

For current-source explanation, resolve effective context through normal
configuration/discovery and show ID, package, item, file/location, mutator,
original/replacement, and current suppression/eligibility information.

For report-backed explanation, show source/replacement, recorded outcome and
available test evidence, report origin/time/identity version, and an explicit
historical label. Do not assert current-source correspondence unless it was
separately validated. Missing fields remain unavailable rather than invented.

Preserve broken-pipe handling and control-character encoding used by current
commands.

## Acceptance matrix

| Scenario | Required result |
| --- | --- |
| Several instances of one mutator in one file | One ID executes exactly its candidate. |
| Repeated IDs and multiple distinct IDs | One execution per distinct selected candidate; audited population matches. |
| Mixed valid/invalid IDs | Usage failure before any build/test side effects. |
| Report plus explicit subset | Every named ID is in the report and resolves currently. |
| Empty, corrupt, foreign, incompatible, or duplicate-ID report | Explicit failure, never successful empty replay. |
| Valid report with only some resolvable candidates | Whole-request failure before execution, not a silent subset. |
| Opt-in catalog mutator without explicit preset restriction | ID is discoverable without guessing its family selector. |
| Explicit conflicting mutator/file/package/suppression policy | Actionable conflict; no silent widening. |
| Test-only edits and unambiguous formatting/line movement | Corroborated match remains usable. |
| First of two identical sites deleted | Old occurrence IDs are not silently retargeted. |
| Source changes after resolution | Synchronization check prevents execution of the wrong generation. |
| Historical report with no current checkout | Explanation works from embedded evidence. |
| Unknown current ID | ID-specific diagnostic with context guidance. |
| Registry explanations | Existing names, families, aliases, and presets work without workspace discovery. |
| Partial report merged with earlier full report | Unselected findings are not retired. |

Extend `commands\explain.rs` unit tests, report decoder tests, `tests\cli.rs`,
and controlled real-Cargo `tests\session.rs` fixtures. Add bounded property tests
for duplicate sets, request ordering, and source-occurrence changes.

## Documentation and completion

Document current-ID versus historical-corroborated replay, context requirements,
selection conflicts, identity limitations, and fresh-verdict semantics.
Update generated command references and CLI examples.

Complete when the advertised explanation works and exact replay cannot silently
execute less, more, or a different historical occurrence than the request allows.
