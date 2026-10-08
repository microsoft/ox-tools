# Catalog extensibility

A downstream distribution changes **policy**, not the ownership engine. It
composes one catalog, chooses its CLI identity and invokes the shared runner.
This lets an organization layer policy over Anvil without forking discovery,
checksums, proposals, retirement or safety checks.

The implementation surface is [catalog](../../src/catalog),
[runner](../../src/run.rs) and the built-in
[artifact registry](../../src/anvil/artifacts/mod.rs). The exact public signatures
belong there; this document describes their composition contract.

## Distribution boundary

The engine accepts rendered strings and artifact specifications. It does not
discover plugins, execute extension callbacks while reconciling files, or read a
repository-specific catalog configuration language. A distribution is a Rust
program/library composing the catalog before calling the engine.

A layered distribution should consume its parent library's catalog, apply its
changes, and publish its own catalog for further consumers. Only the final
distribution runs against a repository. Running several generators in sequence
is not composition: `.anvil.lock` records one tool owner and the engine rejects
a different subcommand unless explicitly switched with `--force`.

The identity contains CLI name/help/version information, but is not merely
branding. Its subcommand is the persistent lock owner. Tool version and catalog
checksum are provenance; artifact checksums still decide whether content may be
changed.

## Artifact identity

| Artifact | Identity | Additional contract |
|---|---|---|
| `OwnedFileSpec` | Repository-relative path | Whole-file body; optional backend gate |
| `RegionSpec` | Host selector and region id | Body plus comment syntax |
| `TomlArrayRegionSpec` | The underlying region's host selector and id | Literal TOML key components selecting an array |

A backend gate is available for owned files. It uses the closed `Backend` enum:
extensions can add files for GitHub or ADO, not invent another backend name.
Regions are not backend-gated.

`HostSelector` expands at plan time:

- `Path` selects one repository-relative host.
- `EachMemberManifest` selects discovered workspace members, and no hosts in a
  non-workspace single crate.
- `WorkspaceCargoToml` selects the root only when it has a `[workspace]` table.
- `SingleCrateCargoToml` selects the root only when it does not.

Region ids may be distribution-specific; they are not a closed list of built-in
names. Existing ids and paths are compatibility keys, however. Renaming one is
an addition plus retirement, not an in-place override.

Runtime path resolution follows existing case-insensitive matches component by
component and rejects ambiguity rather than selecting arbitrarily. Keep
canonical repository-relative paths in catalogs. The runtime also enforces
filesystem containment when applying changes.

## Building a catalog

Start from `Catalog::anvil()` and convert it with `into_builder()`. Builder
operations express different intentions:

| Operation | Meaning |
|---|---|
| `with_artifact` | Add a new identity; an existing identity is an error |
| `replace_artifact` | Replace an existing identity at its current emission position; a missing identity is an error |
| `without_artifact` | Remove an existing identity; a missing identity is an error |
| `with_toml_array_region` | Add a region with array-selector metadata |
| `build` | Return a validated catalog or accumulated configuration errors |

Prefer registry constructors and `with_body` to stringly typed identity
reconstruction:

```rust,ignore
use cargo_anvil::{Catalog, artifacts};

let catalog = Catalog::anvil()
    .into_builder()
    .replace_artifact(
        artifacts::region::rustfmt().with_body("edition = \"2024\"\n"),
    )
    .build()?;
```

`with_body` preserves path, gate, host, id and syntax. The builder matches on
identity; it does **not** independently prohibit a manually reconstructed
replacement from changing its gate. Use the preserving API when the intention is
only to replace content.

Errors accumulate so a distribution author can correct multiple invalid changes
at once. Among the enforced constraints, owned files anywhere below `justfiles/`
must have a `.just` extension. Other executable helpers belong elsewhere;
generated imports must remain valid when artifacts are removed.

The builder is not a general semantic validator for every generated language.
Catalog authors must ensure that referenced imports exist, group/setup
dependencies agree, YAML callers match callee parameters, and bodies compose
valid configuration. Built-in registry and rendering tests cover the default
catalog, not arbitrary downstream combinations.

## Managed TOML array entries

Use `TomlArrayRegionSpec` when the catalog owns entries within a shared array
rather than the array assignment. For example, a selector with components
`["plugins", "default"]` addresses the `default` array under `plugins`; a component
containing a dot is one literal key, not a dotted-path expression.

Registration stores an ordinary `Artifact::Region` plus selector metadata. The
array selector is **not** part of identity: an ordinary region and an array
region cannot both claim the same host/id.

`build()` requires:

- a nonempty vector of path components (an empty literal key component is valid);
- hash-comment syntax;
- a body containing only valid TOML array entries and comments;
- no actual full-line managed-region sentinel comments in the body: they would
  introduce nested or malformed ownership when the engine wraps the entries;
- a trailing comma after the final value when the body has values.

Marker-looking string values, multiline string data, and quoted keys remain
valid entries. Invalid generated ownership comments fail catalog construction
and direct planning as invalid generated TOML, not malformed repository markers.

`into_builder` preserves selectors, same-identity replacement retains them, and
removal drops them. Replacement bodies are revalidated against retained array
metadata. Replacing a body is not a conversion back to ordinary table ownership.

At runtime the engine locates the array with parser spans, optionally creates
missing repository-owned scaffolding, adopts matching unmanaged entries, and
applies normal strict region reconciliation. Neither semantic matching nor a
valid TOML parse grants ownership of surrounding comments, delimiters or another
region. [Updates](./updates.md#toml-array-entries) specifies the refusal cases and
multiset adoption behavior.

## Ordering and composition

Catalog iteration order is significant to planning: later regions see earlier
accepted splices. Replacement preserves that order. Catalog checksum order is a
different concern: its canonical sorted records deliberately ignore insertion
order so the fingerprint describes policy content rather than construction
history. It includes artifact bodies, gates, syntax and array selectors, but not
CLI identity or repository content.

Ordinary region hosts generally append absent regions, with built-in placement
rules for root-level TOML and migrations. A composed host instead needs an
explicit semantic region order and one-time scaffold. Currently the engine
consults the built-in composed-host registry for the container Dockerfile.
`ComposedHost` is not a general downstream builder registration API.

A downstream region targeting that Dockerfile must belong to the registered
order; an unknown id there errors instead of being appended after the final
instructions. Customize an existing container region's body or use the
[repository-owned gaps](./containers.md#8-customization) rather than assuming
arbitrary new ids can extend its sequence.

Whole-table TOML regions must not claim the same table independently. Parser
validation refuses incompatible composition; it does not merge two catalogs'
competing policies. Split table ownership and preserve residue binding as
described in [updates](./updates.md#adopting-a-hand-written-table).

## Removal and customization responsibilities

Removing an artifact changes the next run's live set. Pristine tracked owned
files can be deleted; edited owned files are retained and untracked. Regions have
stricter retirement rules: edited bodies retain tracking and refuse removal.
Removing a backend also retires its unselected files.

Dependencies between generated files remain the distribution author's
responsibility. Removing a check file without updating its imports/groups
breaks Just parsing or execution. The container import is optional specifically
so removing its recipe artifact does not break unrelated recipes.

Choose the layer that owns the policy:

- Repository-specific nonconflicting settings belong outside sentinels.
- A one-off owned-file customization uses the ordinary proposal workflow.
- Shared organization policy belongs in a catalog replacement.
- A new engine behavior or backend requires an engine change, not a catalog
  trick that sidesteps its safety rules.

Extensions inherit the same lock format, sentinel vocabulary, proposals,
workspace discovery and CLI update semantics. They should retain those
contracts rather than creating a second ownership scheme around the engine.
