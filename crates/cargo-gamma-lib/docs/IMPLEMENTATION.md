# cargo-gamma-lib — Implementation guide

This guide records coordinator mechanics behind the user-visible contracts in
[`DESIGN.md`](DESIGN.md).

## Internal facade

Production modules remain private. The `internals` feature mirrors them only
for this crate's integration tests, and the self dev-dependency is the only
workspace consumer that enables it. Helpers used solely within production
modules keep crate or module visibility.

## Cache representation

The default cache name is a truncated BLAKE3 digest of the resolved physical
workspace root. Canonicalizing the existing root makes case and link aliases
converge before lock selection; the ownership marker remains the collision
defense. Explicit cache paths retain their caller-selected identity.

Completed campaign state is serialized once and the exact merged in-memory
record is returned to finalization; it is not immediately read back. After the
record is durable, the external cache's `campaign-location` pointer is
published and resolved through the same path used by postprocessing commands.
Exact and generalized learning from a successful sweep is staged into that
same record, so outcomes and scheduling knowledge publish atomically. Only an
incomplete incremental sweep writes a separate
`incomplete-gamma-learning.json` best-effort cache, preserving what it learned
without altering the completed outcome ledger consumed by postprocessing
commands; `--incremental no` never reads or writes that learning. A completed
campaign clears the incomplete cache before publishing its atomic record.
That resolver walks the selected directory's ancestors for persisted locators,
accepts a located cache only when its owner marker names the selected
workspace, and uses Cargo metadata only as a final fallback, retaining both its
resolved workspace root and target directory. Completed-record construction
also verifies that the pre-execution input snapshot contains the normalized
source generations discovery used, so an edit between snapshot capture and
scanning cannot acquire completed evidence. The same pre-execution survey
indexes test declarations; completed killed outcomes persist the unambiguous
workspace-relative file that declared their killer instead of trying to recover
that identity after execution.

State-only commands validate an explicit cache against Cargo's resolved
workspace root and reject foreign or unowned caches rather than adopting their
records. Hint promotion holds the stable workspace lock from campaign-record
locator re-resolution through record load, conditional YAML publication, and
verification, so a completing campaign cannot be overtaken by a stale
promotion. Promotion filters
generalized knowledge through the completed record's persisted file, item, and
site identities before merging or replacing the checked-in artifact.

Suppression claims the stable workspace lock before loading the selected
campaign record and retains it through conditional source publication,
verification, and any rollback. Campaign completion and suppression therefore
cannot observe different ledger generations within one source-edit
transaction. Intent tracking unions the mutator selectors of separately tagged
directives at the same file and line. Verification diagnostics translate
internal mutant identities back to source location, mutation description, and
prior verdict before a rejected transaction is rolled back. Source paths in
explicitly consumed records must be relative, normal workspace paths; roots and
parent traversal are rejected before edit planning.

Package-by-package discovery indexes plan file paths once before stage
convergence. Each scan moves retained source text, including any leading UTF-8
byte-order mark removed for parsing, directly into its indexed slot. This keeps
source retention linear in the number of files rather than rescanning the full
workspace file list for every dependency stage. Source generation validation
uses normalized text and is limited to the freshly scanned plan before it is
absorbed and instrumented; accumulated plans can still contain guards written
by an earlier stage.

## Hint artifacts

Checked-in hints use grouped YAML schema version 3 and independently versioned
generalized schema version 2. Generations validate forbidden YAML references
before deserializing the strict typed schema. Incremental exact and generalized
replacement uses Fx-keyed tables;
canonical sorting and reach-set interning restore deterministic output before
publication. Identical generalized generations merge without duplicating reach
clusters, and no-op promotion preserves the original generation metadata.

Completed ledgers persist the latest campaign population separately from their
accumulated outcome files. Narrow campaigns can therefore retain unchanged
outcomes for incremental execution without allowing state-only hint promotion
to treat those carried entries as newly selected evidence.

Version-1 generalized records migrate candidate identities as seed evidence
and reset their conflated transfer counters and measured cost. Unsupported
generalized versions remain fail-open for automatic scheduling and fail-closed
for incremental promotion, which cannot safely round-trip unknown fields.
Each generalized tier atomically reserves from a campaign-wide eight-attempt
budget immediately before launching a hinted subprocess. A first hit makes the
tier productive and removes that bound; concurrent zero-hit workers cannot
over-admit before their attempt counters are published.

## Process supervision

Platform and process errors remain typed through launch, retry, containment,
and output-reader setup. They become verdict or event text only where the
coordinator constructs user-visible output. Reader threads publish through a
channel so subtree cleanup can precede bounded output draining.

## Deterministic concurrency checks

Loom models cover reader-accounting races with the smallest actor sets that
create contention. Test-only command pauses synchronize on channels rather
than elapsed time. Last-resort watchdogs are disabled under cargo-gamma so
mutation campaign supervision, rather than a test deadline, classifies hangs.
