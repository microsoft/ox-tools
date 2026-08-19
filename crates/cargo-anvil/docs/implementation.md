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
