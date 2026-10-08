# Update and ownership protocol

Updates must distinguish generated policy from repository customization without
guessing which edits are safe to overwrite. Owned files use a three-checksum
decision; managed regions use strict body ownership. Planning is separate from
application so dry-run reports the same projected state as a real update.

Implementation: [driver](../../src/run.rs),
[decisions](../../src/decision.rs), [owned-file emitter](../../src/emit/owned_file.rs),
[managed-region emitter](../../src/emit/managed_region.rs),
[region operations](../../src/region.rs), [plan](../../src/plan.rs).

## 1. The manifest

The repository-root `.anvil.lock` records checksums of tracked owned files and
managed-region bodies, plus `tool`, `tool_version` and `catalog_checksum`
provenance. Commit it with generated output. It is not a dependency lock or a
configuration file.

The [manifest implementation](../../src/manifest.rs) owns its serialized schema.
Owned-file entries are keyed by relative path; region entries by host and id.
Checksum input normalizes line endings, so a CRLF checkout is not interpreted as
a policy edit. Region checksums exclude sentinels and repository-owned text.
Catalog checksum describes catalog content, not the repository state.

### The single-tool guard

After finding the root and loading the lock, the driver compares its recorded
tool with the catalog's CLI subcommand. A different tool refuses before
workspace-member loading, including during dry-run. A missing tool field
(including a legacy lock) does not block adoption.

`--force` authorizes switching this identity only. It does not authorize
overwriting edited regions/files or bypassing TOML, marker or filesystem safety
checks.

## 2. Owned files

An owned file can intentionally diverge. Let `L` be its recorded template
checksum, `D` its current disk checksum, and `T` the current template checksum.

| State, in decision order | Result |
|---|---|
| File absent | Write `T` |
| `D = T` | In sync; adopt/refresh tracking |
| `D = L` | Pristine old output; write `T` |
| `D ≠ L` and `L = T` | Leave customized file alone |
| Both disk and template diverged from `L` | Preserve file, write `.anvil-proposed` sibling |
| File exists, no `L`, and `D ≠ T` | Same proposal behavior |

A proposal records `T` in the manifest. Repeating the same run therefore stays
quiet until the template changes again; it does not propose on every invocation.
Applying a proposal is a repository decision, not an automatic merge.

Deleting a selected owned file requests recreation. Emptying it preserves an
empty opt-out stub under the same decision table; it can still receive proposals
when the template changes. This is deliberately different from an empty region.

## 3. Managed regions

### Strict ownership

A region has a host, stable id, comment syntax and generated body. Hash syntax
uses `# >>> anvil-managed: <id>` and `# <<< anvil-managed: <id>` on separate lines.
Only the body is owned; surrounding text belongs to the repository.

| Body state | Result |
|---|---|
| Region absent | Introduce it if safe placement/adoption is possible |
| Body equals current template | In sync; adopt/refresh tracking |
| Body equals last recorded render | Update to current template |
| Body empty | Regenerate, even when the template is unchanged |
| Other nonempty body | Refuse and preserve body/tracking |

There are no `.anvil-proposed` region bodies. Keep user settings outside the
sentinels, restore the last generated content, or empty the body to request
regeneration. An id found without a lock entry is not permission to replace its
nonmatching body.

### Marker recovery

Ordinary-region planning can repair redundant markers where a complete boundary
is recoverable. Extra complete pairs lose their markers but retain their bodies
as unmanaged content. Marker-only repairs are independent plan items; a later
body refusal does not undo a safe repair.

**Unpaired markers are never stripped to “start over.”** Without both boundaries
the engine cannot know which existing text it generated. It refuses and retains
tracking so a later run can recover after the repository restores the boundary.
Array-entry regions use stricter whole-host marker validation instead of this
ordinary repair path.

### Adopting a hand-written table

Appending a second TOML table header is invalid even if both bodies look
reasonable. On ordinary TOML introductions and updates, the emitter first
reconciles hand-written copies of tables the generated body declares:

- Compatible overlapping values can be adopted by semantic equality, rather
  than requiring identical quoting/formatting.
- Repository-only assignments and their source decoration remain outside
  ownership. They are preserved as source slices, not serialized from a parsed
  replacement document.
- Residue must remain in the same TOML table. Moving text below a closing
  sentinel is safe only when the final generated table supplies that binding.
  Incompatible values or residue that cannot be placed safely refuse.
- Arrays of tables are not a supported generated table-ownership shape.

The whole projected TOML host must parse, not merely the new body. Validation
distinguishes an already-invalid host, invalid generated TOML, collisions with
hand-written settings and collisions between managed regions. Diagnostics name
the ownership boundary to repair rather than suggesting a destructive rewrite.

Root-level `.delta.toml` and spellcheck regions are placed before table context.
If `.delta.toml` already defines repository-owned `trip_wire_patterns`, the
managed delta region is left empty instead of duplicating that key. Removing the
repository key permits adoption of the managed list.

### TOML array entries

An array region owns entries **inside** a selected TOML array. The assignment,
array brackets, containing tables and unrelated entries remain repository-owned.
The selector is a vector of literal key components, not dotted-path text.
Registration constraints are in [extensibility](./extensibility.md#managed-toml-array-entries);
the [array emitter](../../src/emit/toml_array_region.rs) implements this protocol.

For example, the ownership boundary is:

```toml
plugins = [
  # >>> anvil-managed: company-plugins
  "required-plugin",
  # <<< anvil-managed: company-plugins
  "repository-plugin",
]
```

On first introduction:

1. Parse the existing host and validate all hash sentinels. Empty ids, nested or
   duplicate opening markers, and unmatched/mismatched markers refuse.
2. Locate the selected array. Missing tables/array can be scaffolded outside
   ownership; an incompatible existing value cannot be converted to an array.
   Scaffolding must not invade another region's ownership.
3. For each generated entry, remove at most one semantically equal unmanaged
   entry and its following separator. This is multiset adoption: repeated
   generated values adopt corresponding occurrences, not every duplicate.
   Values covered by another region are never adopted.
4. Insert the generated block immediately inside the opening bracket. Other
   entries and surrounding comments keep their source text.
5. Parse and validate the result before accepting the splice.

Array parsing and scaffolding use the same pending-retirement projection as
ordinary-region validation. A temporary duplicate table from an accepted
migration does not refuse a later array introduction, update or in-sync check.
The projection preserves byte offsets; splices, marker checks and ownership
checks still use the original text, including the retiring region until removal.
Safe retirements are rediscovered on every run, including before array planning
and when an ordinary replacement is already in sync. If an interrupted apply
wrote that replacement but not the retirement, the remaining duplicate table
does not strand recovery. The retirement candidate participates in its own
validation projection only after its markers and recorded body ownership pass.
When the raw host is invalid, validation also reconstructs the pre-write view
by masking only complete live ordinary regions matching their current templates.
This preserves checks on array delimiters and source bindings supplied by the
retiring region; masking the candidate alone cannot prove those safe to remove.
Edited or untracked orphans, malformed markers, and unrelated invalid repository
content are not bypassed by this recovery.

Semantic matching does not authorize deleting comments inside a matching
compound value. Such a candidate refuses, as does a separator owned by another
region. Comments outside adopted value spans are retained. Parser spans keep
quoted strings, commas and `#` characters in data from being mistaken for
structural punctuation.

For an existing same-id region, both sentinels must lie within the selected array
and the boundaries must not split a parsed value. Neither array delimiter may
belong to another region. The body then uses the same checksum/empty-body/edited
body rules as other regions. Updating an established region does not re-adopt
new matching values elsewhere in the array.

Generated entry/comment lines receive two spaces of indentation and the host's
newline style. Continuation lines inside parsed values are not reindented, so
multiline-string data is not changed by presentation formatting. Scaffold edits
are mapped back to original source rather than normalizing the whole host.

Neighboring ordinary-region writes and retirements also protect live array
selectors. Keeping TOML parseable is insufficient: a change must neither remove
their delimiters nor rebind the selector to a different source array. This can
refuse an otherwise pristine neighboring region's update or retirement.

## 4. Composing one host

Every accepted write or removal updates an in-memory host accumulator. Later
regions splice against that text. Planning each region against the initial disk
file would make the last write erase earlier regions.

Only regions independently safe to retire are masked during projected TOML
validation. Sibling regions that remain live still participate in collision
checks. A pending retirement is not blanket permission to ignore all managed
text.

Placement is ordered, not a general dependency solver. Moving a table between
regions can require another run when the old owner has not yet been updated;
cyclic swaps are not automatically solved.

Composed hosts additionally declare a semantic order and one-time scaffold.
The container Dockerfile's regions must appear in that order. Missing known
regions can be inserted at their declared position while preserving gaps;
reordering existing regions is refused. A pristine legacy whole-file render can
transition to regions. An edited legacy render, unknown existing file, or a
previously composed file with all regions lost requires explicit recovery.
See [containers](./containers.md#8-customization).

## 5. The decision algorithm

The driver separates classification from effects:

1. Resolve current artifact paths and host selectors; collect live identities
   and array dependencies.
2. Perform safe marker repairs where applicable.
3. Plan owned files with the decision table and regions with strict ownership,
   adoption and host validation.
4. Accumulate accepted host changes; record scoped refusals as no-ops retaining
   provenance.
5. Retire previous identities not covered by the selected catalog.
6. Project the next manifest and compare its serialization with the existing
   lock, including provenance-only changes.

The lock does not override observed content. Neither an old checksum nor a
matching catalog fingerprint allows an edited region to be overwritten.

## Retirement and migrations

Retirement applies to removed artifacts, deselected backend files and hosts no
longer selected by the current workspace shape.

| Retired state | Outcome |
|---|---|
| Owned file matches its recorded checksum | Delete file and tracking |
| Owned file edited | Keep file, drop tracking, transfer ownership |
| Owned file already missing | Drop tracking |
| Region pristine or empty | Remove block and tracking, preserving the host |
| Region edited or has unpaired markers | Refuse; retain content and tracking |
| Region/host already missing | Drop its tracking |

Array-dependency and composed-host safety checks still constrain retirement.
Case-only renames are resolved before comparing the live set: they must not make
a just-updated file or region look obsolete.

A whole-file entry whose host is now managed by live regions is not deleted.
Its obsolete file tracking is dropped after a safe transition; refused composed
hosts retain the old provenance needed to recognize and recover them.

Built-in lint and spellcheck migrations preserve these rules. Replacement lint
regions require a tracked, pristine or empty legacy block before retiring it.
Split spellcheck tables are positioned so trailing repository settings keep
their table binding. Edited legacy content is not silently redistributed across
new ownership boundaries.

## Application, dry-run and recovery

`--dry-run` writes nothing and exits 1 if files/proposals/retirements or the
manifest would change, **or** if any artifact could not be safely inspected.
Otherwise it exits 0. A customized owned file with an unchanged template can be
a valid steady state.

A normal run applies safe items, reports scoped refusals and returns 0 unless a
fatal error occurs. A refusal alone therefore does not make a normal update a
failing process; use dry-run for a drift/safety gate.

File writes precede the lock save. Application is not all-or-nothing across the
repository: an I/O failure can leave earlier writes applied. Re-run after fixing
the failure and inspect version-control changes rather than assuming rollback.
Application checks containment instead of trusting arbitrary lock/catalog paths.

Keep `.anvil.lock` rather than deleting it to suppress a refusal. Losing
provenance turns previously generated, nonmatching content into unknown content
and cannot establish permission to overwrite it.
