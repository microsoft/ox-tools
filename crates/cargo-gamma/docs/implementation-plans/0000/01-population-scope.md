# Layer 1: preserve population scope in reports and merges

[Stack root](README.md) | [Next layer](02-relational-default.md)

## Outcome

Reports distinguish a complete discovered population from a selected subset.
Merge uses absence as evidence of retirement only when a compatible, explicit
population snapshot supports it. Existing survivor-only reruns become safe merge
inputs without adding a broader iteration feature.

## Current entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\commands\run.rs` | `run_info`, `measured`, report publication; applies survivor-ID filtering |
| `cargo-gamma-lib\src\commands\list.rs` | `write_population`; currently records shard identity but not effective selection |
| `cargo-gamma-lib\src\discover\survey.rs` | Discovery context, file inventory, diff/ID/shard filtering, skipped analysis |
| `cargo-gamma-lib\src\discover\plan.rs` | Carries discovered population and source evidence |
| `cargo-gamma-lib\src\elements\report.rs` | `RunInfo`, merge provenance, `build_from`, standard-schema projection |
| `cargo-gamma-lib\src\merge\incoming.rs` | Reads optional producer metadata |
| `cargo-gamma-lib\src\merge\read.rs` | Bounded regular-file reading and schema/ID validation |
| `cargo-gamma-lib\src\merge\union.rs` | `populations`, source selection, retirement, verdict selection, report reconstruction |

The present `populations` rule treats an unsharded, non-merged report as complete
for each file it contains. That inference is invalid for survivor-only, diff, or
other candidate-filtered reports.

## Contract and data model

Add an optional versioned `population` object under the existing free-form
`RunInfo` configuration. Do not change the interchange schema's closed mutant
status vocabulary.

The object carries:

- Selection kind: ordinary discovery/run, survivor-only, diff, exact-ID, or merged.
  Exact-ID support is consumed by layer 6; do not expose its CLI in this layer.
- Effective population-shaping settings: resolved mutator names, selected
  package identities, file policy, feature/cfg context, and ID scheme.
- Digests for opaque compilation/discovery inputs that cannot be safely or
  usefully published as raw strings, including arbitrary flags and environment
  input. Never dump ambient environment values into report metadata.
- Explicit per-file completeness under a compatible discovery-context key.
  Record which analyzed files support that assertion.
- Known reductions such as sharding, diff restriction, survivor filtering, or
  unavailable analysis. Unknown legacy metadata is not completeness.

Keep population completeness independent of execution completion. A full
discovery listing may be complete while every verdict is pending; a fully
executed survivor-only run remains partial.

The context key is stable over source edits whose population changes need to be
compared. Do not hash the entire source tree into this compatibility key, which
would make detecting retired sites impossible. Source-generation hashes remain
separate evidence for presentation and replay.

Resolve preset names to their actual mutator membership before comparing scope.
Two versions of `@default` need not select the same candidates. Conservatively
withhold retirement when contexts cannot be compared; retaining an old finding
with a limitation is preferable to silently deleting it.

Serialize a canonical shaping record and derive its opaque compatibility digest
from every field in that record. Preserve unknown additive fields when reading
and checking the digest; do not deserialize them away and recompute a key from
only the settings an older reader knows. Layers 4 and 7 add target/const selection
to that record, so different settings remain incompatible for older readers too.
Change the metadata version when existing fields change meaning, not merely
because a new shaping input is recorded. Unknown future format versions remain
non-authoritative rather than being assumed compatible.

## Implementation sequence

### Build one resolved scope description

Derive the scope from effective arguments after configuration application, plus
Cargo/cfg discovery. Thread that value through `Survey`/`Plan` or the existing
execution result, so run and list writers do not independently reconstruct it.

Mark a file complete only after all required discovery for it succeeds. An
unreadable/unanalyzable declaration dependency must prevent an overconfident
completeness assertion for affected files. A build failure affects verdict
completion, not necessarily a successfully discovered file's population; keep
those assertions separate.

Keep explicit suppression represented by ordinary ignored candidates. Do not
confuse suppressed members of a known population with members omitted by
survivor, diff, or ID filtering.

### Write and read metadata consistently

Extend `RunInfo` and its construction in `run_info`, `write_population`, fixtures,
and merge output. A `list --json-report` must preserve the same population context
as the corresponding run.

`build_from` currently omits files without grouped mutants. Include successfully
analyzed complete files with an empty mutant array when needed to prove that the
last candidate in an existing file disappeared. Do not include a skipped file
as an apparently complete empty population.

Keep absent metadata readable as unknown. For recognized Gamma metadata, reject
malformed supported-version fields rather than allowing
`Incoming`'s current failed-parse-to-`None` path to erase a claimed scope.
Unknown future scope versions remain non-authoritative for retirement and are
diagnosed explicitly. Foreign reports retain their standard-schema support.

### Make retirement context-aware

Index complete population evidence by file and compatible scope, not just file.
An old verdict can be retired only by a population assertion that:

1. Is explicitly complete for that file and context.
2. Is at least as recent under the existing deterministic ranking rules.
3. Does not contain the old identity.

A report from another mutator or feature selection cannot retire it. Neither
shards nor partial reports gain authority merely because their files include
the complete source text.

Keep the existing source/presentation checks: retaining an identity does not
justify drawing its old location over incompatible new source. Report
presentation incompatibility separately from retirement. Do not silently count
it as an explicitly withdrawn mutant.

This layer does not need a new repository-wide deleted-file protocol. If a file
is absent altogether and no explicit compatible inventory proves removal, retain
the existing conservative treatment rather than inferring deletion.

### Preserve provenance through staged merges

Keep original verdict timestamps and origins, and retain the population-scope
association required to interpret each historical verdict on a later merge.
Add optional provenance fields rather than replacing existing lineage rules.
Merged output is not itself a fresh complete source snapshot.

It must nevertheless preserve the complete population assertions it consumed:
file, context record/key, complete ID set, original origin/time, and lineage rank.
Retain the newest assertion for each file/context and its original authority.
On remerge, `populations` reads these inherited assertions rather than skipping
all merged inputs. Do not refresh their timestamp to the merge's publication
time. This prevents an older input from resurrecting an identity already retired
by an inherited snapshot.

An empty complete snapshot also needs to survive when the merged document has
no remaining mutant to carry its provenance. Bound these records through the
existing report input budgets and discard superseded same-context assertions
deterministically, not by forgetting all retirement evidence.

Expose partial/unknown scope through existing summary/report notes. Keep the
percentage computed from actual retained verdicts; do not invent pending or
killed entries for omitted candidates.

Different incompatible scopes may retain historical observations that cannot
be retired from each other's snapshots. Explain that limitation and retain
strict gating on the observations actually included; do not make a historical
flake pass solely because it is old. A fresh campaign should be evaluated from
its intended current report set, not an indiscriminate directory of incompatible
historical campaigns.

## Acceptance matrix

| Scenario | Required result |
| --- | --- |
| Full report followed by survivor-only report for the same file | Omitted IDs and original verdict times remain. |
| Full report followed by diff or exact-like subset fixture | Absence cannot retire unrelated IDs. |
| Different mutator membership, features, or cfg context | No cross-context retirement. |
| Compatible complete listing removes one candidate | Only that candidate is retired. |
| Existing file now has zero candidates | Explicit complete empty file can retire its old candidates. |
| File or declaration analysis is skipped | No complete-empty assertion is manufactured. |
| Legacy/foreign report without population metadata | Readable, but cannot authorize retirement. |
| Malformed known Gamma scope metadata | Actionable input failure, not silent metadata loss. |
| Direct merge versus staged/reversed merge | Equivalent identities, scope, retirement decisions, and original provenance. |
| Merge retires an ID, then is merged with an older report containing it | Inherited population evidence prevents resurrection. |
| Additional target/const shaping field not understood by an older reader | Opaque key differs; no false compatible retirement. |
| Dry-run/listing with pending verdicts | Can describe a population but cannot claim fresh execution. |

Extend tests in `merge\union.rs`, `merge\read.rs`, `elements\report.rs`,
`tests\schema_conformance.rs`, `tests\gate.rs`, and `tests\cli.rs`.
Use fixed timestamps. Add bounded permutation/round-trip property tests for
scope and merge ordering using existing test infrastructure.

## Documentation and completion

Update design sections on identity, sharding/merging, and reports; clarify
survivor-only scope in the command/README source. Document legacy compatibility
without promising that old inputs were complete.

Complete when partial inputs cannot erase unrelated findings, complete
compatible inputs can still retire known sites, and standard JSON/HTML score
semantics remain aligned. Subsequent layers consume this model rather than
reintroducing their own completeness flags.
