# Layer 4: library-only compilation and verdict targets

[Stack root](README.md) | [Previous layer](03-flaky-gate.md) |
[Next layer](05-external-artifacts.md)

## Outcome

`--lib` constrains both harness compilation and every test execution to eligible
library unit-test targets. A broken excluded integration target cannot prevent
the campaign. A library and integration target with the same name remain
distinct. Default selection retains its existing broader behavior.

Mutation-source selection and the test oracle remain separate. A production
dependency can be compiled and mutated without supplying its own test harness.

## Entry points

| Location | Responsibility |
| --- | --- |
| `cargo-gamma-lib\src\commands\cli.rs`, `config.rs` | First-class target policy and configuration |
| `cargo-gamma-lib\src\discover\survey.rs` | Cargo target inventory, eligibility, package reachability |
| `cargo-gamma-lib\src\exec\cargo_options.rs`, `config.rs` | Effective build/test-target policy |
| `cargo-gamma-lib\src\exec\build.rs` | Preflight, stage, convergence, target narrowing, widening/retreat |
| `cargo-gamma-lib\src\exec\test_binary.rs` | Artifact decoding, target identity, oracle filtering |
| `cargo-gamma-lib\src\exec\measure.rs` | Preflight oracle, baseline, calibration, runner preparation |
| `cargo-gamma-lib\src\exec\workspace.rs`, `nextest.rs` | Nextest inventory and command construction |
| `cargo-gamma-lib\src\discover\record.rs`, `hints.rs` | Context and persisted binary/test identities |

## Target identity and eligibility

Represent a target using Cargo package identity, target name, target kind, and
source root when needed to distinguish artifacts. Keep physical executable
paths separate: rebuilding or moving the scratch tree changes paths, not the
logical target.

Use Cargo's library classification rather than only the string `lib`; verify
supported library crate kinds against metadata and actual Cargo behavior.
Eligibility also requires `test = true`. Preserve custom-harness information
rather than assuming every eligible artifact supports libtest enumeration.

Under `--workspace --lib`, a binary-only member or `test = false` library supplies
no judging harness. It must not force a workspace-wide error while other eligible
libraries exist. Its production library may still be required as a dependency.
If the requested oracle as a whole contains no eligible library harness, fail
before expensive mutation work with an actionable diagnostic.

Do not remove already-discovered candidates to make the score look better.
Candidates whose source was never compiled retain the existing not-built
classification; compiled candidates with no eligible judging test use ordinary
uncovered handling.

## Implementation sequence

### Resolve the policy before command construction

Add `--lib` and `lib` configuration. Carry the effective value into the shared
discovery/build context used by run and list, without conflating it with
`--test-package`, `--test-workspace`, or name-glob filtering.

Reject contradictory target selectors in pass-through Cargo arguments before
launch. Recognize separate and equals spellings and argument-taking forms
correctly; do not inspect unrelated string substrings. Do not attempt to
reinterpret all Cargo arguments as a new parser.

Existing include/exclude test-name globs apply inside the eligible target set.
Validate an include request against that set so a name existing only as an
excluded integration target does not produce a misleading successful selection.

### Centralize invocation policy

Build a small mode-specific invocation description shared by all Cargo paths.
Do not repair the behavior by appending `--lib` to an existing `--tests` vector.

| Stage | Ordinary mode | Library-only mode |
| --- | --- | --- |
| Pristine preflight | Existing `check --tests --keep-going` behavior | `test --no-run --lib` over eligible harness roots |
| Production-only stage | Existing build/check | Library-scoped build/check; harness preflight still required |
| Final harness compilation | Existing `build --tests` or safe narrowing | `test --no-run --lib` |
| Convergence fallback | Existing permitted broadening | May widen package roots only within the library-harness policy |
| Nextest binary inventory | Existing selection | Same eligible library target/package policy; never an implicit broad build |

`cargo test` on the inspected toolchain has no `--keep-going`. Omit it in
library-only harness builds. Consume available structured diagnostics, withdraw
only actually attributed mutants, and retry to uncover later errors. Preserve
bounded rounds, progress checks, and explicit not-built failure when convergence
cannot complete.

In this mode a convergence round is one aggregate eligible-root Cargo invocation,
and attribution is limited to the diagnostics it actually returned. Retain the
existing configurable rollback-round bound over those invocations; do not assume
each invocation observed every package or reset the bound for hidden retries.
A successful production-stage check may eliminate candidates earlier, but it
does not authorize blaming unreported failures. If the bound is reached before
all libraries can be judged, report not-built and fail the campaign rather than
publishing a complete score. Do not split the harness build into independent
package invocations merely to collect more errors: that changes feature
unification and requires its own proof.

Library-mode pristine preflight is intentionally a full harness compilation:
`cargo check --lib` alone cannot establish that `cfg(test)` code compiles. Update
failure diagnostics to name the invocation that actually reproduces that mode.

Resolve package roots so passing `--lib` never overrides `test = false`
inadvertently. Building a disabled library as a production dependency is allowed;
requesting its unit-test harness is not.

### Preserve feature and fallback correctness

Keep the existing feature-unification checks, but make the explicit target-kind
boundary non-negotiable. Preflight retry, package retreat, final-build widening,
and `linked_target_args` fallback all consume the same policy.

A failed narrowed build cannot recover by silently compiling integration tests.
If the requested library-only graph cannot build, report the actual limitation.
Do not change baseline scope after calibration without recalibrating it.

### Carry identity through execution and knowledge

Decode target kind from compiler-artifact messages and match against the
metadata inventory. Apply the same target identity in baseline, census,
confirmation, sweep, diagnostics, and nextest mapping.

Add the policy to population provenance and any cache term that depends on
compiled units or oracle selection. Extend binary/killer hint identity where
name-only keys are ambiguous. Old name-only hints may be used only when they
resolve uniquely; otherwise ignore them as performance hints, never attach them
to an arbitrary same-named binary.

Do not turn library-only selection into test-verdict reuse. No cached detection
is authoritative.

## Acceptance matrix

| Fixture | Required observation |
| --- | --- |
| Library and same-named integration target | Artifact identity selects the library; integration is never launched. |
| Integration source contains an intentional compile error | Library-only campaign succeeds; ordinary mode observes the error. |
| Integration target writes an execution marker | Marker remains absent throughout baseline, census, sweep, and confirmation. |
| Library `test = false` plus another eligible library | Disabled harness is not explicitly compiled; eligible one can judge linked code. |
| Only binary/disabled targets in the requested oracle | Early diagnostic, not a successful empty campaign. |
| Default package-local and explicit cross-package judging | Existing package bounds remain effective. |
| Feature-unification fallback required | Package widening does not widen target kinds. |
| Unviable mutants across multiple eligible packages | Later rounds reveal sibling errors; only reported mutants are blamed. |
| Multi-package convergence reaches its round bound | Explicit not-built/incomplete failure; no unreported candidate is called unviable. |
| Name glob matches only an integration target | Clear invalid/empty eligible-selection diagnostic. |
| Libtest and nextest | Both inventory and execution respect the same target policy. |
| Legacy ambiguous hint | No wrong-binary binding or omitted work. |
| Ordinary invocation without `--lib` | Existing eligible integration tests remain available. |

Use recording command-construction tests plus real temporary Cargo workspaces
in `exec\build\tests.rs`, `exec\test_binary.rs`, and `tests\session.rs`.
Assertions must inspect actual artifacts and execution markers, not merely
command-line text or final counts.

## Documentation and completion

Document `--lib`, `test = false`, package/oracle separation, Cargo pass-through
conflicts, and the full-build library preflight. Update cache/hint compatibility
documentation where identity fields change.

Complete when every build and execution path honors the policy and defaults
still support the existing broader oracle.
