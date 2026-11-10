# Catalog extensibility

A downstream Cargo subcommand can reuse the Anvil update engine with its own
catalog. Start from `Catalog::anvil()` for the default checks, or
`Catalog::builder` for an empty catalog. The
[API reference](https://docs.rs/cargo-anvil/latest/cargo_anvil/) documents
signatures; this page defines composition and ownership.

## One tool per repository

The downstream tool chooses its CLI name, description, and version. The
on-disk namespace remains `.anvil.lock`, `anvil-managed` markers,
`.anvil-proposed` files, and `anvil-` recipes under `justfiles/anvil/`.

The lock records the managing tool. A different tool refuses before planning;
`--force` permits an intentional ownership switch under the normal
[update rules](updates.md#the-single-tool-guard). It does not authorize
overwriting customizations.

Catalogs can extend other catalogs, but the result is one compiled tool.
Ancestor catalogs are libraries, not additional generators run on the same
repository. There is no runtime plugin loading or per-tool namespace.

## Composing a catalog

`into_builder()` derives from an existing catalog. Its operations are explicit:

- `with_artifact` adds a new identity and rejects an existing one.
- `replace_artifact` replaces an existing identity and rejects a missing one.
- `without_artifact` removes an existing identity and rejects a missing one.
- `build` validates the assembled catalog and returns errors rather than
  silently resolving conflicting declarations.

Use the public `artifacts` registry to select built-in artifacts. Deriving a
replacement with `with_body` preserves its path, backend gate, and region
identity. New recipes need imports and group membership where applicable;
adding a file alone does not make Just execute it.

Owned files are identified by repository-relative path. Managed regions are
identified by host and region ID. Region IDs must be unique within the
resolved host, including when multiple selectors resolve to the same file.
Changing an existing region's comment syntax is refused while its old markers
remain.

Host selectors support a literal path, every workspace member manifest, or
the root manifest conditional on workspace versus single-crate layout.
Canonical paths reuse existing case variants; ambiguous non-exact matches
are refused.

The structural array APIs add required strings while preserving the enclosing
configuration. Their narrower input and ownership rules are documented in
[shared arrays](array-regions.md).

## Backend and placement limits

The backend set is closed: GitHub and ADO. Catalogs can add, replace, or remove
files gated on either backend, but cannot introduce another backend name.
Ungated artifacts apply to every selection, including local-only generation.

`justfiles/anvil/` accepts only `.just` owned files. Put other assets in a
separate directory such as `.anvil/`. Container artifacts use the same catalog
operations; see [container customization](containers.md#8-customization).

Recipe contents are text, not a structured check-editing API. Changing a
check's behavior means replacing the relevant artifact. The update algorithm,
preservation rules, and on-disk format are shared by all catalogs.

## Running the catalog

`run_app` supplies the Cargo-subcommand CLI, diagnostics, and exit status.
The crate's [public example](../../src/lib.rs) shows a minimal downstream
binary and catalog.

For embedded use, `update` accepts explicit `UpdateOptions` and caller-owned
`UpdateIo`. Backend selection is explicit; an empty set means local-only.
The call needs no process arguments, environment changes, working-directory
changes, Git discovery, or global tracing setup. `dry_run` performs the same
planning without mutation.

An in-memory adapter can exercise real generation, preservation, and upgrade
behavior without mocking the engine's result. The
[`memory_catalog` example](../../examples/memory_catalog.rs) provides a complete
adapter. No optional feature is required, including with default features
disabled. `test-util` and `test_support` are not downstream APIs.

`UpdateResult` describes the inspected input, drift, refusals, diagnostics,
and plan summary. It is not a second scan after application. Independent safe
changes may apply despite content refusals. Application is not a repository-wide
transaction: earlier writes can remain after a later I/O failure; the lock is
published only after artifact operations succeed.

## Provenance

The fully composed catalog determines the catalog checksum, including
artifact identities and bodies. CLI identity determines `tool` and
`tool_version`. `--version` reports both the version and checksum so builds
with different catalogs can be distinguished.

The checksum describes generated inputs; it is not permission to overwrite
repository edits. Per-artifact checksums govern reconciliation.
