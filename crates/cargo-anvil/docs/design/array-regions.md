# Required strings in shared TOML arrays

`CatalogBuilder::with_toml_array_region(TomlArrayRegionSpec)` adds required
string entries without taking ownership of the enclosing array or table.
For example, a catalog can require `"example:required"` in `plugins.default`
while leaving the repository's other plugins alone.

## Supported input

- Hosts have a `.toml` suffix, case insensitive. Workspace/member manifest
  selectors remain available. After expansion, duplicate `(host, region id)`
  identities are errors, including aliases between different selectors.
  Unresolved case aliases for the same array host also refuse.
- Selectors are nonempty lists of literal key components, not dotted
  expressions. Quoted keys work. Parents must be normal TOML tables, not
  inline tables or arrays of tables.
- Generated bodies contain TOML **strings**, whitespace, and hash comments.
  Every nonempty body ends its last entry with a comma. Actual full-line
  ownership sentinels in a template are rejected; marker-looking multiline
  string contents are data, not boundaries.
- The host must already parse, or be absent. A missing table or array is
  created only when scaffolding can be inserted without rewriting existing
  bytes or entering another region.

## Ownership and lifecycle

Only the sentinel-delimited body is owned and checksummed. Table headers,
keys, array delimiters, unrelated values (including compound values),
comments, and surrounding formatting remain repository-owned. Scaffolding
created on first use is left behind on retirement.

On introduction, one matching **unmanaged string** is adopted per generated
entry, comparing decoded string values. Only its source token and separator
are removed: comments stay outside ownership. Matches in another region are
not adopted. Extra copies remain. Generated order is the catalog's order;
this is not a general set-union operation.

Updates replace only the recorded body. Matching content is in sync; an
empty body is regenerated; an edited body that matches neither the recorded
checksum nor the current template refuses. Retirement removes a clean or
empty body and its markers, preserves repository entries and scaffolding,
and drops its lock entry. Adjacent repository blank lines are preserved;
unlike ordinary regions, array retirement removes exactly the sentinel
lines and body, without consuming a separator blank line.
Edited retirements retain their ownership record
until reconciled. Removing the final catalog declaration still uses the
TOML-aware scanner, derived from the host suffix.

The lock remains **schema 1**: no scanner provenance or array selectors are
persisted. Selectors are static catalog metadata and participate in the
catalog checksum, but not the region's identity.

## Composition and deterministic refusal

Array scaffolding cannot depend on another region's headers or delimiters.
Existing independent regions may coexist. The planner checks whether each
neighbor can be removed without changing the array's source identity or
making the host invalid. It also validates each planned host write in
application order, including retirement, before returning the plan. A
structural array-host refusal returns no applicable plan, so unrelated
pending writes cannot expose a partially composed invalid host.

Overlapping/malformed ownership, misplaced markers, split values,
non-array selectors, owned-file/array-host overlap, structural dependencies,
and insertions requiring relocation refuse. Invalid TOML retirement hosts
also refuse: schema 1 cannot prove that a historical region did not own
array entries when the document cannot be parsed. Existing live ordinary
regions otherwise retain their behavior. Dry runs run the same planning
checks without writing anything.

## Deliberate non-goals

No extensionless TOML, generated compound entries, inline-table scaffolding,
automatic ownership transfer, scaffold relocation, header rebinding, or
reconstruction of interrupted migrations. Resolve unsupported ownership
layouts explicitly in separate, valid repository changes before adopting
this API. In particular, converting a whole-table region to array entries
is not an automatic upgrade path.

Future review suggestions that require these features should be declined
for this API unless a separate design establishes a concrete need and an
equally clear preservation contract. Broad TOML editing is not the purpose
of required string entries.
