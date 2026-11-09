# Required strings in shared arrays

The catalog's TOML and JSON array-region APIs require string entries without
taking ownership of the enclosing array or table. For example, a catalog can
require a plugin in `plugins.default` while leaving other plugins untouched.

## Supported input

- TOML hosts require a `.toml` suffix; JSON hosts require `.json`, case
  insensitive. TOML supports literal paths and manifest selectors; JSON
  requires a literal host path.
- Array selectors are nonempty lists of literal key/property components, not
  dotted expressions. TOML parents must be normal tables, not inline tables
  or arrays of tables. JSON parents must be objects.
- Generated bodies contain one or more strings and end with a comma.
  Empty strings are allowed; zero-entry templates are not. JSON generated
  strings must be unique.
- JSON hosts use JSONC: comments and trailing commas are accepted, but other
  JSON5 extensions are not.
- The host must parse or be absent. Missing parents and arrays are created
  only when this preserves existing content and does not enter another region.
- TOML array regions use hash comments; JSON array regions use slash-slash
  comments. Duplicate resolved host/ID identities and ambiguous case aliases
  are refused.

## Ownership and adoption

Only the marked body is owned and checksummed. Headers, properties, array
delimiters, unrelated entries, comments, and surrounding formatting remain
repository-owned. Scaffolding created during adoption remains after retirement.

Matching unmanaged strings are adopted by decoded value, not spelling.
Comments remain outside ownership. Strings inside another region are not
adopted. TOML adopts one match per required entry and leaves extra copies;
JSON adopts all unmanaged matches, leaving each required string once.
Generated order follows the catalog.

Matching bodies are in sync; empty bodies regenerate. Nonempty edits that
match neither the recorded render nor current template are refused.
Retirement removes clean or empty bodies and markers while preserving
repository entries and adjacent blank lines. Edited regions remain tracked
until reconciled.

The lock retains JSON selector provenance so ownership can be checked after
a catalog declaration disappears. Moving those markers outside the recorded
array is refused. TOML placement is inferred from the current structure; an
empty block moved outside its array no longer has structural-array retirement
semantics.

## Refusals and limits

Array scaffolding cannot depend on another region's headers or delimiters.
Independent regions may coexist, but retirement and subsequent writes must
leave the host valid. A structural composition failure returns no applicable
plan rather than allowing part of an invalid composition to be written.

Refusals include overlapping ownership, misplaced or malformed markers,
split values, non-array selectors, duplicate JSON properties on the ownership
path, whole-file ownership of the same host, and edits requiring relocation
or rebinding of the selected array.

JSON-aware scanning ignores marker-looking strings and block comments.
Corruption wholly inside a tracked body can be regenerated when intact
markers and a valid template establish its boundary. Errors outside managed
bodies and malformed boundaries remain refusals.

The API does not support extensionless hosts, generated compound entries,
inline-table scaffolding, broad JSON5 syntax, ownership transfer, or
automatic conversion of whole-table/object regions to array regions.
Resolve unsupported ownership layouts in a separate valid repository change
before adopting the API.
