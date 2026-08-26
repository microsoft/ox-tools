# cargo-anvil Implementation Guide

This guide describes internal implementation constraints that keep the emitted behavior aligned
with the user-visible contract in [design](./design/README.md).

## Pull request title validation

The `pr-title.just` template owns both title validation and its failure diagnostic. The ordered
human-readable pattern list is the source of truth: the PowerShell recipe expands its placeholders
into regular-expression fragments and uses the same list verbatim in the error message. Allowed
types are likewise defined once and used for both case-insensitive matching and diagnostics.

Changes to accepted title syntax must therefore modify the pattern or type data rather than adding
a separate regular expression. Pattern expansion escapes the display syntax before injecting the
trusted regular-expression fragments; the reverse order would escape the fragments themselves and
match them literally.

The skip path is shared by an unset and an explicitly empty `PR_TITLE`, because cloud backends
publish an empty value outside a pull request context. Snapshot tests pin the emitted template,
while the focused integration test executes the generated recipe and verifies accepted titles,
rejected titles, both skip cases, and corrective output.

## GitHub group execution and status reporting

The generated `anvil-run-group` composite action owns the capture-before-failure
protocol. Its inline Bash step invokes Just through `tee`, temporarily disables
immediate exit, and reads `PIPESTATUS[0]` so the saved result belongs to Just
rather than `tee`. It selects the final standard Just failed-recipe diagnostic,
including the optional line-number form, and falls back to the group recipe
when a tool exits without that diagnostic. The step writes the recipe and exit
code as outputs without failing so the reporter can consume them. After
best-effort reporting, a final guarded step propagates the captured failure.

The status reporter is an inline `actions/github-script` body. It validates the
pull-request head SHA, reads same-commit status history newest-first, and keeps
only the newest value for each context. An encoded group marker in the workflow
run URL identifies statuses owned by the current group. The visible context
also contains the group so identical recipe failures reached from different
groups remain independent. A new failure is published before old contexts are
superseded, preventing a temporary failure-free rollup. Clean runs only
supersede active failures; they do not add a fresh supplemental status.

The reporter's API errors are ignored by the composite action because the
native workflow job is authoritative. The generated root workflow grants the
required status permission only to the same-repository pull-request caller.
Merge-group and fork execution retain annotations and the named failure step
without a write-capable status token.

Tests extract and execute the exact YAML-embedded Bash and JavaScript bodies.
The Bash harness covers success, both Just diagnostic forms, and a failure
without a diagnostic. The JavaScript harness mocks status-history pagination
and publication to cover setup and recipe failures, cleanup, ownership,
deduplication, ordering, truncation, and missing event data. Snapshot tests pin
the complete emitted artifacts.
