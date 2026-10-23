# Required strings in shared arrays

`CatalogBuilder::with_toml_array_region(TomlArrayRegionSpec)` adds required
string entries without taking ownership of the enclosing array or table.
For example, a catalog can require `"example:required"` in `plugins.default`
while leaving the repository's other plugins alone.

`CatalogBuilder::with_json_array_region(JsonArrayRegionSpec)` provides the
same ownership model for JSON object paths. The motivating use is a
`CloudBuild.json` catalog that requires `"build\\Extensions\\Anvil"` in
`EnlistmentRootPaths.SourceRootDirs` while preserving every other property,
entry, comment, delimiter, and formatting byte the repository owns.

## Supported input

- TOML hosts have a `.toml` suffix and JSON hosts have a `.json` suffix, case
  insensitive. Workspace/member manifest selectors are available only for
  TOML arrays; JSON arrays require a literal path selector. After expansion,
  duplicate `(host, region id)` identities are errors, including aliases
  between different selectors. All regions sharing a resolved TOML array host
  must use hash comments; JSON array hosts use slash-slash comments. Unresolved
  case aliases for the same array host also refuse.
- Selectors are nonempty lists of literal key or property components, not
  dotted expressions. Quoted TOML keys and arbitrary JSON property names work.
  TOML parents must be normal tables, not inline tables or arrays of tables;
  JSON parents must be objects. Repeated JSON property names on an ownership
  path are ambiguous and refuse.
- Generated bodies contain at least one format-valid **string**, allowing
  whitespace and the format's comments. Empty string values are valid;
  zero-entry templates (including whitespace-only or comment-only templates)
  are rejected on build, including replacements. The last entry ends with a
  comma. Actual full-line ownership sentinels in a template are rejected;
  marker-looking string contents are data, not boundaries. JSON generated
  entries must be unique.
- JSON hosts accept the JSONC dialect: line comments, block comments, and
  trailing commas are allowed; single-quoted strings, unquoted properties,
  missing commas, hexadecimal/non-finite numbers, and other JSON5 extensions
  are rejected.
- The host must already parse, or be absent. A missing table/object or array is
  created only when scaffolding can be inserted without rewriting existing
  bytes or entering another region.

## Ownership and lifecycle

Only the sentinel-delimited body is owned and checksummed. Table headers,
keys, array delimiters, unrelated values (including compound values),
comments, and surrounding formatting remain repository-owned. Scaffolding
created on first use is left behind on retirement.

On introduction, matching **unmanaged strings** are adopted by decoded value.
Only source tokens and separators are removed: comments stay outside
ownership. Matches in another region are not adopted. TOML adopts one match
per generated entry and leaves extra copies; JSON removes every unmanaged
match and rejects duplicate generated values, so each required JSON string is
present exactly once after introduction. Generated order is the catalog's
order; this is not a general set-union operation.

Updates replace only the recorded body. Matching content is in sync; an
empty body is regenerated; an edited body that matches neither the recorded
checksum nor the current template refuses. Retirement removes a clean or
empty body and its markers, preserves repository entries and scaffolding,
and drops its lock entry. Adjacent repository blank lines are preserved;
unlike ordinary regions, array retirement removes exactly the sentinel lines
and body, without consuming a separator blank line. Edited retirements retain
their ownership record until reconciled. Removing the final catalog declaration
still uses format-aware retirement for structural array regions. TOML array
ownership is rediscovered from the host structure and suffix. JSON array
ownership uses the selector provenance recorded when the structural region was
written; JSON marker-shaped text in strings and block comments is not ownership
evidence.

The lock uses **schema 2**. TOML selectors are static catalog metadata and
participate in the catalog checksum, but are not persisted or part of the
region's identity. Structural JSON regions additionally record an optional
`json_array_path` on their existing region entry. This additive provenance
selects JSON-aware retirement after the catalog declaration disappears.
Hosts containing ordinary regions alongside a structural JSON array are listed
in top-level `json_scanner_hosts`; the host remains listed while any catalog
region targets it, even if that region is untracked or refused. If the final
array region retires, later runs therefore keep ignoring marker-shaped text
inside block comments. Historical and standalone ordinary JSON hosts without
either provenance form retain the line-oriented retirement contract.
Schema 1 locks remain readable with no JSON provenance and are rewritten as
schema 2; older binaries refuse schema 2 rather than silently dropping fields
they do not understand.
Rejecting zero-entry templates does not change empty on-disk body regeneration
or retirement. After declaration removal, TOML placement is inferred from the
current structure; JSON placement is checked against the recorded selector.
A historical TOML body that the user empties and moves outside its array cannot
be identified as an array region, so ordinary retirement behavior, including
separator blank removal, applies there. JSON regions with selector provenance
instead refuse when their markers no longer belong to the recorded array.

## Composition and deterministic refusal

Array scaffolding cannot depend on another region's headers or delimiters.
Existing independent regions may coexist. The planner checks whether each
neighbor can be removed without changing the array's source identity or
making the host invalid. It also validates each planned host write in
application order, including retirement, before returning the plan. A
structural array-host refusal returns no applicable plan, so unrelated
pending writes cannot expose a partially composed invalid host.

Overlapping/malformed ownership, misplaced markers, split values,
non-array selectors, duplicate JSON properties, owned-file/array-host overlap,
structural dependencies, and insertions requiring relocation refuse. Invalid
TOML retirement hosts refuse while structural ownership is being inferred.
Invalid JSON retirement hosts refuse when the lock carries `json_array_path`;
records without that provenance remain ordinary line-oriented regions.
Existing live ordinary regions otherwise retain their behavior. Dry runs run
the same planning checks without writing anything. Every projected write is
rechecked against retained JSON selector provenance before a plan is returned,
so changing a structural declaration to an ordinary region cannot generate
markers outside the array that the lock still records.

Whole-file overlap checks resolve both catalog paths and recorded ownership
paths using the same path resolver as writes, including repeated path separators.

## Deliberate non-goals

No extensionless TOML/JSON, generated compound entries, inline-table
scaffolding, JSON5 beyond comments/trailing commas, automatic ownership
transfer, scaffold relocation, header/property rebinding, or reconstruction
of interrupted migrations. Resolve unsupported ownership layouts explicitly
in separate, valid repository changes before adopting this API. In particular,
converting a whole-table or whole-object region to array entries is not an
automatic upgrade path.

Future review suggestions that require these features should be declined for
this API unless a separate design establishes a concrete need and an equally
clear preservation contract. Broad TOML or JSON editing is not the purpose of
required string entries.
