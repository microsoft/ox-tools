# Layer 3: fail on unresolved flaky outcomes by default

[Stack root](README.md) | [Previous layer](02-relational-default.md) |
[Next layer](04-library-targets.md)

## Outcome

`run` and `merge` return gate failure when effective results contain a flaky
outcome, even when a percentage threshold passes or is absent. The explicit
`--no-fail-on-flaky` option disables only this gate. Flaky outcomes remain
inconclusive and excluded from the mutation score.

Do not turn this into a generic retry feature, change the meaning of a survivor,
or claim that confirmation proves a suite deterministic.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\commands\cli.rs` | `RunArgs`, `MergeArgs`, CLI documentation |
| `cargo-gamma-lib\src\config.rs` | Optional `no_fail_on_flaky`, effective-option validation |
| `cargo-gamma-lib\src\commands\run.rs` | `run_session` exit gates and report execution policy |
| `cargo-gamma-lib\src\commands\merge.rs` | Gate on final merged evidence, not original input counts |
| `cargo-gamma-lib\src\model\summary.rs` | Existing flaky tally; score formula is unchanged |
| `cargo-gamma-lib\src\elements\digest.rs` and `elements\report.rs` | Lossless Gamma outcome interpretation and reason prefixes |
| `cargo-gamma-lib\src\merge\verdict.rs`, `merged.rs`, `union.rs` | Preserve final-verdict confirmation provenance and flaky counts |
| `cargo-gamma-lib\src\exec\verdict.rs`, `exec\sweep.rs` | Existing confirmation and bounded failure evidence |

## CLI, configuration, and compatibility

Add `no_fail_on_flaky` to run and merge arguments, with the negative CLI spelling.
Add `no-fail-on-flaky` as an optional run configuration key. Default false means
the gate is active. Apply the same precedence pattern as existing negative
boolean settings; `--no-config` remains the way to disregard a configured opt-out.

Validate effective options after configuration merging. `--no-confirm` is
incompatible with an active flaky gate and requires explicit opt-out. This is a
usage error before any build, not an automatic policy change.

Do not disable the flaky gate for `--no-baseline`: later unmutated confirmation
is distinct from the initial baseline.

Record effective confirmation and gate policy in new Gamma reports. A merge
does not inherit an input report's gate opt-out. Preserve confirmation provenance
for the winning verdict through staged merges.

For legacy reports, missing confirmation metadata means unknown. It is not
evidence of a flaky outcome and must not silently become `confirmed = true`.
Retain legacy readability and display the evidentiary limitation. A known
unconfirmed detection in a new Gamma report cannot bypass strict merging:
require explicit opt-out and identify the affected finding as unconfirmed,
not flaky. Superseded or retired observations do not trigger this check.

## Implementation sequence

### Share a lossless outcome decoder

Factor a Gamma-specific decoder for `(status, statusReason)` from the existing
prefix definitions. It must distinguish:

- Genuine `Survived` from timeout and out-of-memory.
- Deliberate `Ignored` from flaky and not-built.
- Pending, killed, compiler-unviable, and uncovered outcomes.

Do not reuse `settled_verdict` as the decoder: that function answers an
admissibility question and intentionally maps several outcomes to no reusable
knowledge. Decoding a historical observation must not grant reuse permission.

Keep standard foreign-report scoring separate. A foreign producer's free-form
reason text is not automatically Gamma's outcome protocol. Preserve enough
per-verdict provenance through merge to interpret Gamma-specific reasons after
remerging mixed input documents.

### Add run gating without disturbing score semantics

Evaluate flaky failure after obtaining the executed plan and honoring
infrastructure/build-failure precedence, but before an empty scored population
can return success. The gate must catch an all-flaky campaign without requiring
`--min-score`.

Reuse the existing gate-failure exit code. Name the affected IDs and tests in
diagnostics and explain that evidence was inconclusive, not that mutations
survived. Publish reports before returning the gate failure as the normal
reporting pipeline does.

Preserve expectations, pending/minimum-score checks, ungraded-score protection,
and full-precision threshold comparison. If several gates fail, diagnostics may
name each, but the opt-out cannot suppress the others.

### Gate the merged winners

Tally flaky observations from the final compatible, presented verdict set.
Do not sum flaky counts from input files. A later trustworthy result can replace
an earlier flake; a later flaky observation must not be hidden behind an old
detection.

Verify the existing `retain` ranking with these cases. Keep pending listings
from displacing real observations, while retaining their source/presentation
role.

Extend the merged summary and existing notes to identify inconclusive findings.
Add only the policy/provenance fields needed to preserve this behavior through
merge/remerge.

Freshness is not an opt-out from this gate. A retained old flaky observation
still fails when it remains part of the explicitly supplied merged population.
For incompatible scopes, explain why retirement was not established and point
to selecting a coherent current campaign's inputs; do not silently age out
unreliable evidence or disable the gate for all historical findings.

### Preserve useful evidence

Audit the path from the mutated failure and unmutated confirmation to the
published finding. Retain bounded, appropriately redacted evidence or diagnostic
references so users can identify the failing test and both observations.

Do not copy unrestricted subprocess output into every report. Reuse the existing
failure-evidence size limits and control-character handling. If evidence
publication fails, surface the failure; do not replace it with a success-shaped
placeholder.

## Acceptance matrix

| Scenario | Required result |
| --- | --- |
| 99 killed and one flaky, no flag | Exit `2`, score still 100%, flaky finding identified. |
| Same population with `--min-score 100` | Still exit `2`; percentage does not override flaky policy. |
| All flaky, no percentage gate | Exit `2`. |
| Fully killed | Exit `0` if no other failure applies. |
| Flaky with explicit opt-out | No flaky-only gate failure; score is unchanged. |
| Opt-out plus score, expectation, pending, or build failure | Existing failure remains. |
| `--no-confirm` without opt-out, including split CLI/config sources | Usage error before building. |
| Legacy confirmation provenance absent | Readable with unknown provenance, never fabricated confirmation or flakiness. |
| Known unconfirmed effective detection in a new report | Strict merge fails actionably; explicit opt-out is required. |
| Historical flake replaced by newer confirmed result | Gate evaluates the newer result. |
| Newer flake after older detection | Flake remains visible and fails by default. |
| Suppression and not-built encoded as `Ignored` | Neither is misclassified as flaky. |
| Direct merge and staged merge | Same effective gate outcome and provenance. |

Use `tests\gate.rs` for report-driven dispatch tests and fixed timestamps.
Use injected observation sequences or fake-host results for run/confirmation
tests; do not create a test that randomly fails to manufacture flakiness.

## Documentation and completion

Update design scoring/exit semantics, CLI/configuration references, example
configuration, and the executable's flakiness documentation. Explain the separate
roles of baseline, confirmation, percentage threshold, and flaky gate.

Complete when strict behavior is the default on both surfaces, opt-out is narrow,
and console/JSON/HTML/merge continue to agree on score and outcome meaning.
