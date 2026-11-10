# Updates, ownership, and adoption

`cargo anvil` updates generated infrastructure while preserving repository
configuration. Owned files support customization and proposals. Managed
region bodies belong to the catalog and edits require reconciliation.

## 1. The manifest

Commit `.anvil.lock` with the generated files. It records the managing tool,
tool version, catalog checksum, and last-rendered checksum of each owned file
and `(host, region ID)`. Do not edit checksums to accept an update.

The current schema is 2. A lock with a newer schema is refused: upgrade the
generator rather than deleting its ownership history. Paths are relative to
and contained within the repository. Structural JSON regions also retain
selector and scanner provenance for safe retirement.

Checksums normalize CRLF to LF but otherwise treat whitespace as content.
Region checksums cover the body, not markers or surrounding repository text.
The lock is refreshed after artifact application; dry runs never write it.

### The single-tool guard

If the recorded `tool` names a different subcommand, the generator refuses
before planning, including in dry-run mode. An absent tool identity permits
adoption. `--force` permits a deliberate tool switch, but does not override
content conflicts, edited regions, or safety checks.

### Catalog checksum

`--version` reports the binary version and catalog checksum. The checksum
distinguishes builds with different generated content; it is not an overwrite
gate. Per-item checksums determine whether repository content can be updated.

## 2. Owned files

| Existing file | Catalog state | Result |
| --- | --- | --- |
| Matches current template | Any | In sync; refresh tracking. |
| Absent | Live artifact | Write the template. |
| Present without tracking | Differs from template | Preserve and propose the template. |
| Matches last render | Template changed | Update automatically. |
| Edited | Template unchanged | Preserve silently. |
| Edited | Template changed | Preserve and propose the template. |

Empty owned files follow the same rules. They can disable an artifact without
being regenerated on every run; a later catalog change may produce a proposal.
Deleting a live owned file instead requests regeneration.

## 3. Managed regions

Repository-only settings belong after the closing marker and before the next
table. For example:

```toml
# >>> anvil-managed: anvil-deny-advisories
[advisories]
yanked = "deny"
unmaintained = "all"
# <<< anvil-managed: anvil-deny-advisories
ignore = ["RUSTSEC-9999-0001"]
```

Do not repeat the managed table header or its keys. TOML cannot override a
managed value or extend an array by repeating an assignment.

### Strict ownership

- A body matching the current template is in sync.
- A missing region is introduced, adopting compatible unmanaged settings.
- An empty or whitespace-only body is regenerated.
- A body matching the last render updates when the template changes.
- Any other nonempty body is preserved and refused, whether or not the
  template changed.

Managed regions have no `.anvil-proposed` sibling and no empty-body opt-out.
Refusals persist until reconciled. Restore generated content, apply the
current template explicitly, or empty the body to regenerate it. Move
repository-only settings outside the region first.

Markers must be complete comment lines in the host's syntax: `#`, `//`, or
XML comments. Changing the syntax of an existing region is refused while old
markers remain.

XML regions require an existing valid document. Introducing a new region also
requires a separate root closing tag line, before which the region is inserted.
Existing regions inside the root update in place even when its closing tag
shares a line with other content. Missing, self-closing, or malformed hosts
are refused; Anvil does not scaffold XML.
Marker-looking strings, CDATA, and other non-comment content are not markers.
XML region IDs cannot contain `--`, line breaks, or invalid XML characters.

### Adopting a hand-written table

Matching unmanaged values are adopted by parsed value rather than formatting.
Their old formatting and attached comments need not survive. Repository-only
assignments retain their source text and remain under the same table.
Conflicting values are refused rather than overwritten or merged.

Each independently extensible namespace has its own region: the deny
sections, spelling sections, and Rust/rustdoc/Clippy lint tables. Append
repository lint keys after the matching closing marker without opening the
table again.

Equivalent-looking dotted-key and header layouts are not always automatically
reconciled. Custom multi-table regions refuse adoption when repository-only
settings cannot be preserved under their original table. Anvil is not a
general TOML repair tool.

### Catalog composition and multi-table regions

Every written TOML host must parse. A refusal distinguishes invalid existing
TOML, an invalid generated template, overlapping managed tables, and a
managed table conflicting with repository text. Correct the indicated owner
rather than deleting unrelated settings.

Moving a table between live regions can require a second run if the gaining
region is processed first. Exchanging tables between two live regions is
unsupported: repeated runs cannot resolve the conflict. Such a catalog needs
a staged retirement and introduction rather than an automatic exchange.

### Marker recovery

Redundant markers around a complete pair can be removed without deleting
surrounding content. An opener without a later close, or a close before an
opener with no complete pair, is refused and preserved: restore the boundary
before retrying. Marker cleanup never accepts edits to the body.

Nested cross-region damage and arbitrary malformed input are not repaired.

### Line endings

Generated region text uses the host's first newline style. New or empty hosts
default to LF. Repository text is not normalized, and in-sync content is not
rewritten solely because it uses CRLF.

## 4. Per-host insertion anchors

Existing regions update in place. New regions normally append, except where
their configuration requires another position. Root TOML settings precede
tables while preserving leading comments. Ordered container regions retain
their declared order and repository-owned gaps.

Refusing one region does not prevent independent safe updates. Structural
array hosts have stricter composition checks; see [shared arrays](array-regions.md).

## 5. Retirement

Tracked artifacts absent from the selected catalog retire. Deselecting a
backend or removing a workspace member can also retire artifacts.

| Disk state | Owned file | Managed region |
| --- | --- | --- |
| Absent | Drop tracking. | Drop tracking. |
| Matches last render | Delete and drop tracking. | Remove markers/body and drop tracking. |
| Empty/whitespace-only | Ordinary owned-file comparison. | Remove the block and drop tracking. |
| Edited | Preserve and drop tracking. | Refuse; preserve content and tracking. |

An edited retired region remains managed until reconciled. Restore its
generated body, empty it, or deliberately remove it before retrying.

## 6. Owned-file proposals

`<path>.anvil-proposed` contains the current template, not a merge. The live
file is untouched. Review and merge it, replace the original, or delete the
proposal to dismiss it. The same template is not proposed again unless the
catalog changes. Proposal files are not ignored by Git.

## 7. Dry-run and summary

`--dry-run` performs the same planning without changing artifacts or the lock.
Exit 0 means no pending changes or refusals. Exit 1 means drift, refusal, or
error. Normal updates print the categorized plan and apply safe items while
retaining refusals.

Application is not a repository-wide transaction. Earlier artifact changes
can remain after a later I/O failure; the lock is published only after artifact
operations succeed.

## 8. Backend selection

Repeat `--backend github` and `--backend ado` to select both. Without explicit
selection, Anvil detects the backend from `origin`; `--no-backends` selects
local artifacts only. Previously tracked files of deselected backends follow
the retirement rules, not an unconditional preserve-or-delete policy.

## Shared array entries

Required string entries have a narrower ownership contract than whole tables
or objects. See [shared arrays](array-regions.md).
