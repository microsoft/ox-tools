# Updates, Ownership, and TOML Adoption

`cargo anvil` updates generated infrastructure while preserving repository
configuration. Owned files and managed regions deliberately have different
ownership contracts: owned files support customization and proposals; the contents
of managed regions belong to the catalog and edits require reconciliation.

See [README.md](./README.md) for the CLI, [local.md](./local.md) for recipes,
[extensibility.md](./extensibility.md) for catalogs, and
[containers.md](./containers.md) for ordered container regions.

## 1. The manifest

`.anvil.lock` is committed TOML at the repository root. It records the checksum
of the last rendered content for each owned file and each `(host, id)` region:

```toml
version = 1
tool = "anvil"
tool_version = "0.4.1"
catalog_checksum = "sha256:..."

[[file]]
path = "justfiles/anvil/checks/fmt.just"
checksum = "sha256:..."

[[region]]
host = "deny.toml"
id = "anvil-deny-advisories"
checksum = "sha256:..."
```

Paths are repository-relative, slash-separated, and must stay inside the repository.
Entries are deterministic: files sorted by path, regions by `(host, id)`.
The lock is read at startup and refreshed after applying the plan; `--dry-run`
does not write it. Missing tracking means an item has not previously been rendered.
The schema version controls format compatibility; a newer schema is refused.

Checksums normalize CRLF to LF. Other whitespace differences are content changes.
Region checksums cover only the body, not the sentinels or user text outside them.
The recorded checksum is not the user's current checksum: keeping that distinction
is what permits clean template updates while detecting edits.

### The single-tool guard

A repository is managed by exactly one anvil-family tool. If `tool` names a
different subcommand, refuse before planning anything, including in dry-run mode.
Missing `tool` (including a legacy `rendered_by` lock) permits adoption.
`--force` only permits an explicit ownership switch; it does not override content
conflicts, edited regions, or safety checks. The new catalog follows the ordinary
update and retirement rules below and records its identity on save.

### Catalog checksum

`tool_version` is informational. `catalog_checksum` hashes the compiled catalog,
including artifact identities and bodies, to distinguish different builds with the
same version. Neither is a content-overwrite gate. Decisions compare per-item
checksums, not catalog versions. The current catalog checksum is also shown by
`--version`.

## 2. Owned files

Ownership is by path, not by a sentinel or generated-file warning. Generated
warnings explain where to regenerate files; supported extension wrappers may use
weaker provenance wording instead. Their update algorithm is the same.

Let `D` be the disk checksum, `L` the last-rendered checksum, and `T` the current
template checksum. The first matching row applies:

| State | Action |
| --- | --- |
| `D == T` | `InSync`; refresh tracking to `T`. |
| File absent | Write the template and record `T`. Deletion requests regeneration. |
| `L` absent, file present | Preserve the file, propose the template, record `T`. |
| `D == L` | Write the changed template and record `T`. |
| `D != L`, `T == L` | `LeaveAlone`; preserve customization silently. |
| `D != L`, `T != L` | Preserve the file, propose the template, record `T`. |

Empty owned files follow this algorithm unchanged: they can disable an owned
artifact. An unchanged template leaves the empty file alone; a changed template
can produce a proposal. A pre-created empty file is preserved on first adoption.

## 3. Managed regions

```toml
# >>> anvil-managed: anvil-deny-advisories
[advisories]
yanked = "deny"
unmaintained = "all"
# <<< anvil-managed: anvil-deny-advisories
# Repository-specific exceptions continue [advisories].
ignore = ["RUSTSEC-9999-0001"]
```

The region owns the header and its generated assignments. User-only settings
belong outside the sentinels, directly after the closing marker and before the
next table. They must not repeat the table header or a managed key.

### Strict ownership

| State | Action |
| --- | --- |
| Body matches the current template | `InSync`; refresh tracking. Required start placement may still cause a write. |
| Region absent | Adopt compatible unmanaged settings and write the region. |
| Body empty or whitespace-only | Repopulate from the current template, even if the template has not changed. |
| Body matches the last render | Update normally when the template changes. |
| Any other nonempty body, including an untracked block | Refuse immediately and preserve its body and recorded checksum. |

An edited region is refused whether or not its template changed. There is no
managed-region `.anvil-proposed` file, silent acceptance, ownership transfer, or
empty-body opt-out. Refusals repeat until reconciled. Restore generated content,
apply the current template explicitly, or empty the body to regenerate it.
Move compatible user-only settings outside the block before regenerating.

TOML cannot override a managed key or extend a managed array by repeating an
assignment outside the region. Conflicting values must be reconciled; there is
no array merge or separate override configuration. The catalog's lint severity
is generally `warn`; the Clippy recipe promotes warnings to errors at invocation.
Editing a managed lint value is an ownership violation, not a customization API.

### Adopting a hand-written table

Given this existing `deny.toml`:

```toml
[advisories]
# Waiting for upstream.
ignore = ["RUSTSEC-9999-0001"]
```

The output is the single-header region above, with the original `ignore`
assignment and its comment immediately below the closing sentinel. The shipped
advisory template does **not** own `ignore`.

Adoption compares parsed TOML values, not key order, whitespace, or comments:

| Existing setting | Template setting | Result |
| --- | --- | --- |
| `[lints] workspace = true # our policy` | `[lints] workspace = true` | Emit once from the template; the matching assignment's comment may be dropped. |
| `[advisories] ignore = ["RUSTSEC-9999-0001"]` | Only `yanked` and `unmaintained` | Preserve `ignore` outside the region in `[advisories]`. |
| `[advisories] yanked = "warn"` | `yanked = "deny"` | Refuse this region; keep `warn`. Other deny sections may update. |
| `ignore = ["ours"]` | `ignore = ["users"]` (synthetic catalog) | Refuse; never merge arrays or choose a side. |
| `value = "a#b"` | `value = "a#c"` (synthetic catalog) | Refuse; `#` inside a string is data. |

User-only assignments move as source slices, preserving formatting and comments.
Boundary blank lines and trailing boundary whitespace may be tidied. Matching
assignments are emitted by the template once; their old formatting and comments
need not survive.

The TOML parser locates headers and values. Multiline strings, bracketed text
inside strings, quoted keys, and array-element lines need no ad hoc rules.
`["a.b"]` and `[a.b]` are different tables. Explicit child tables such as
`[workspace.package]` remain intact when adopting `[workspace]`. User-authored
arrays of tables (`[[bin]]`) are preserved and bound the preceding table.
Different header/dotted-key layouts need not be reconciled automatically, even
when a reader considers them equivalent.

### Catalog composition and multi-table regions

Independently configurable tables have separate catalog regions:

* `deny.toml`: advisories, licenses, bans, sources.
* `spellcheck.toml`: root settings, `[Hunspell]`, `[Hunspell.quirks]`.

For example, a repository's `[Hunspell] transform_regex = ["^[0-9]+$"]`
stays under `[Hunspell]` below `anvil-spellcheck-hunspell`, not under quirks.
The old combined `anvil-spellcheck` block retires in the same run that introduces
the three replacement regions. An untouched old block is removed; the next run
is a no-op. An edited old block is preserved and remains tracked, with a refusal.
When the old body ends in `[Hunspell.quirks]`, the headed replacements are
inserted before it using the shared `At` splice placement, based on the parsed
table context rather than the region id. Removing that block leaves user settings after its
closing sentinel immediately below the new quirks region: for example,
`allow_dashes = true` remains `Hunspell.quirks.allow_dashes`, never a root key.
Empty or whitespace-only old bodies establish no table context: the replacements
append instead. A conflicting user root `dev_comments = true` stays at root
while adoption of the generated root defaults refuses, including on repeat runs.

Catalog composition tests parse the actual per-host bodies for every workspace
shape. They catch duplicate table claims and dotted-key/header collisions.
Managed templates must not introduce arrays of tables; this is a catalog
invariant, not a runtime deduplication or identity system.

Custom multi-table regions remain possible, but adoption conservatively refuses
user-only residue from any table other than the body's last table: placing it
after the closing marker would change its membership. The engine does not
automatically split arbitrary templates or repair unrelated invalid TOML.

Every actual TOML write has a generic parse-before-write backstop. Only regions
that will really retire are masked for that check; an edited retired region
remains visible. Existing managed content is masked when locating unmanaged
adoption candidates. This may incidentally repair duplicate headers, but is not
a general invalid-input repair contract.

### Marker recovery

Before ownership checks, remove redundant or unmatched marker **lines only**:

| On disk for one id | Interpretation |
| --- | --- |
| Opening marker with no later close | No region. Remove the opener; treat all remaining content as unmanaged. |
| Close before open, no complete pair | Remove both unmatched markers; preserve all other text and onboard normally. |
| Two openers followed by a close | Keep the first opener and close; remove the duplicate opener, preserving intervening content. |
| Opener followed by two closers | Keep the first complete pair; remove the extra close. Content after the first close stays unmanaged. |

The same rules apply to `#` and `//` sentinels. Removed marker noise does not
participate in body checksums. Cleanup is persisted even for otherwise in-sync
regions and even if remaining content conflicts. Cleanup never blesses a changed
non-marker body. Nested cross-id recovery and general broken-input repair are
outside this contract.
Retired markers in hosts receiving new regions are repaired before adoption
and validation, using the same accumulated text through retirement. If two
complete pairs share a retired id, only the first body is owned; the second
body becomes unmanaged text and must participate in conflict checks. Marker
cleanup alone does not make the retired catalog entry live again.

### Line endings

Generated bodies, markers, and separators use the original host's first newline
(CRLF or LF), captured before adoption. New, empty, or no-newline hosts default
to LF. Mixed hosts choose the first newline without normalizing user content.
Moved residue retains its own endings; a missing terminator uses the original
host style. In-sync content is not rewritten solely for line-ending differences.

## 4. Per-host insertion anchors

Regions normally append in catalog order; existing regions update in place.
`Start` retains leading header comments and their whitespace, then places root
keys before user settings and every TOML table. Managed sentinels end the header;
the insertion must not enter an existing block. Root-setting adoption also
preserves these leading comments instead of dropping a copyright header attached
to a matching assignment. The same insertion anchor is used to recognize an
already correctly placed region, making the next run a no-op. `.delta.toml`'s
`anvil-delta` and `spellcheck.toml`'s `anvil-spellcheck-root` use this placement.
A clean legacy delta block below `[git]` moves to the beginning even when its
template is unchanged, so `trip_wire_patterns` is a root key rather than a
`git` setting. Existing repository-owned delta trip wires retain the established
preservation behavior and visible note.

`At` inserts a missing region at a specific boundary. Ordered composed hosts
(notably the container Dockerfile) retain their scaffold, region ordering,
classification, and insertion anchors. Non-TOML host text is not TOML-adopted.

All actual writes and removals compose against one accumulating host text,
so later operations preserve earlier changes. Refusing one region does not
prevent independent regions or owned files from updating.

## 5. Retirement

An item tracked by the lock but absent from the current catalog retires. Backend
deselection and removed workspace members can also cause retirement.

| Disk state | Owned file | Managed region |
| --- | --- | --- |
| Absent | Drop tracking. | Drop tracking. |
| Matches last render | Delete and drop tracking. | Remove markers and body; drop tracking. |
| Empty/whitespace-only | Ordinary owned-file comparison. | Remove the empty block and drop tracking. |
| Nonempty edits | Preserve; drop tracking (`OrphanedKept`). | Refuse; preserve content **and tracking**. |

An edited retired region is not transferred to the user. Repeated runs still
report it; restoring the old generated body, emptying it, or removing it resolves
retirement. Other artifacts continue to update.

## 6. Owned-file proposals

Only owned files generate `<path>.anvil-proposed`, containing the full current
template. The live file is untouched. Recording `L = T` after proposing means
the same proposal is not reported repeatedly unless the template changes again.
Users may diff and merge it, replace the original with it, or delete it to
dismiss it. Proposal files are intentionally not ignored by Git.

Managed regions have no proposal composition pass and no proposal race-recovery
behavior. Their outcomes are writes, in-sync observations, or scoped refusals.

## 7. Dry-run and summary

`--dry-run` does not modify files or the lock. Exit 0 means no changes (including
manifest changes) and no refusals. Exit 1 means pending changes or at least one
refusal. The same categorized plan is printed during an ordinary update.
Content refusals are scoped diagnostics, not a global fatal error: ordinary
updates still apply safe items and preserve tracking for refused regions.

## 8. Backend selection

`--backend github` and `--backend ado` are repeatable. Without explicit selection,
anvil detects the backend from the origin remote; `--no-backends` emits local
artifacts only. Previously tracked files for deselected backends follow the
owned-file retirement rules. Backend selection does not change region ownership.
