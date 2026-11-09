# Verification boundaries

The repository uses its own generated GitHub workflows and local recipes.
That exercises the catalog in one real workspace; it does not establish that
every adopter, runner, or compliance configuration works.

## What each layer establishes

- `tests/snapshots.rs` records the emitted local-only, GitHub, and ADO file
  trees. Snapshots detect textual changes, not runtime correctness.
- Engine and fixture tests cover ownership, adoption, preservation, refusals,
  retirement, and repeated updates. `tests/memory_engine.rs` and
  `tests/extensibility.rs` also exercise caller-owned repository state and
  downstream catalogs.
- `tests/recipe_contracts.rs`, `tests/impact.rs`, and `tests/pr_title.rs`
  cover selected recipe contracts. Structural assertions and controlled
  executable harnesses are not substitutes for runs with real tools.
- `tests/schemas.rs` invokes external validators for emitted TOML, Just,
  and GitHub workflow syntax. Missing validator executables cause skips;
  a successful test run alone does not prove those validators ran.
- `.github/workflows/regenerate-check.yml` builds the branch's generator and
  runs `cargo anvil --dry-run`. It checks generated content and lock drift.
  It does not rewrite workflows or make the other workflow runs wait for it.
- The generated PR and scheduled workflows execute the checks against this
  repository. Their verdict applies to the actual commit and runner used.

## Limits

ADO snapshots do not validate live Azure DevOps execution or composition with
1ESPT, SubstratePT, or CloudBuild. Those integrations need evidence from the
target environment.

The default GitHub matrix does not cover macOS, arbitrary self-hosted pools,
private toolchains, or every container engine. Fixture coverage is not a
promise that unknown workspace layouts or every older generated state can be
adopted automatically.

Regeneration proves synchronization, not correctness of a new check or its
external dependencies. A check can render consistently and still fail at
runtime.

Contribution and validation instructions live in [AGENTS.md](../AGENTS.md).
