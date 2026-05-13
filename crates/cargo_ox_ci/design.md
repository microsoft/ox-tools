# cargo-ox-ci — Design

> Status: **Draft**.
> Crate name: `cargo-ox-ci`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

## 1. Problem

Across the surveyed Rust repos (`oxidizer`, `oxidizer-github`, `ox-tools`, `ox-tools-gh`,
`assistants-oxide`, `ox-docs`) the build/test/CI infrastructure is conceptually similar but
implemented six different ways:

| Repo | CI | Justfile shape | Toolchain | Notable specifics |
|------|----|----------------|-----------|-------------------|
| `oxidizer`         | ADO 1ESPT (`SubstratePT`) | 500-line monolith + `just_mutants.just` | `ms-prod-1.93` | Stage flags (`enableStages`), `cargo-aprz`, stable-API checks |
| `oxidizer-github`  | GitHub Actions            | Modular `justfiles/{basic,coverage,format,setup,spelling}.just` + `constants.env` | `1.93` | `cargo-delta` impact-scoped builds, sticky semver comments, composite `setup` action |
| `ox-tools`         | ADO (CloudBuild + classic) | none in worktree                       | `ms-prod-1.92` | NuGet/MSBuild scaffolding, internal templates |
| `ox-tools-gh`      | GitHub Actions            | Same modular shape as `oxidizer-github` | `1.93` | Mirror surface to OSS oxidizer |
| `assistants-oxide` | ADO 1ESPT (custom `rust/`) | Monolith + `.just/tds.just`            | `ms-prod-1.93` | Symcrypt setup steps, NuGet publish stage |
| `ox-docs`          | ADO classic               | Monolith                                | `ms-prod-1.88` | Mixed C#/.NET + Rust, mdbook/docfx |

The same logical checks (clippy, fmt, deny, miri, mutants, coverage, hack feature-powerset, udeps,
semver, spellcheck, license headers, doc/doctest, careful, audit, ensure-no-cyclic-deps,
ensure-no-default-features, doc2readme, …) are spelled in subtly different ways in each repo, with
different argument sets, different tool versions, and different opinions about which tier (PR vs.
nightly) a check belongs to.

Maintaining six artisanal copies is expensive: improvements made in one repo (e.g. `cargo-delta`
impact scoping in `oxidizer-github`) take months to propagate, security/policy upgrades are
missed, and onboarding new Rust repos requires copying-and-praying.

## 2. Goals

1. **One opinionated build profile** for Rust repos, with sane defaults distilled from the
   strongest patterns observed across the existing repos.
2. **Two tiers**: `pr` (blocking on every pull request) and `nightly` (slow, scheduled).
3. **Both CI backends** — GitHub Actions and Azure DevOps Pipelines — generated from the same
   source of truth. The user picks one or both per repo via a CLI flag.
4. **Compliance preservation**: ADO pipelines that must `extends:` 1ESPT/SubstratePT continue to
   do so. The tool generates *templates* the repo composes, never root pipelines.
5. **Local/CI parity at every level**: every individual check, every group of checks, and the
   full tier are all reproducible locally with a single `just` invocation, using the exact same
   arguments CI uses. The three commands `just ox-ci-pr`, `just ox-ci-nightly`, and
   `just ox-ci-full` (= pr + nightly) are first-class local entry points.
6. **Plain-cargo fallback**: a developer with only `cargo` installed (no `just`, no
   `cargo-ox-ci`) can still build and run tests.
7. **Friendly updates**: the tool detects, per file and per managed region, whether the user has
   modified it, and updates only the unmodified bits.
8. **Open source**: the crate ships from `github.com/microsoft/ox-tools` and publishes to
   crates.io. The binary contains no Microsoft-internal dependencies; everything it can install
   on the user's behalf comes from crates.io.

## 3. Non-Goals

- Replacing 1ESPT, SubstratePT, CloudBuild, or any other compliance/release pipeline.
- Generating root pipeline YAML for ADO. We generate templates the user composes.
- Building a general-purpose CI compiler/IR. We share **check semantics**, not CI features.
- Owning `.cargo/config.toml`, `rust-toolchain.toml`, or workspace layout in `Cargo.toml`.
- Managing exact tool versions on the user's behalf — we enforce minimums only.
- Hosting a service. The tool is a CLI binary; updates ship via crates.io.
- Acting as a runtime: the tool emits `just` recipes and CI YAML, then exits. It is **not**
  invoked at build/test/CI time. `just` is the runtime.
- Destructive operations: `cargo ox-ci update` never deletes files. Removing a previously
  configured CI backend is a manual `rm -rf` by the user.

## 4. Guiding Principle

> **`cargo-ox-ci` writes files. `just` runs them. The repo composes everything.**

Corollaries that drive every section below:

- The tool's only job is to author and update files. It is not on the local-build hot path or in
  the CI graph at runtime.
- The local daily-driver is `just ox-ci` (and friends). Those recipes call `cargo …` directly. CI
  jobs invoke the same `just` recipes. Local and CI are bit-identical because they share one
  implementation in the imported `.just` files.
- Drift detection lives inside the files themselves (per-file checksums and per-managed-region
  checksums). There is no parallel metadata file. Updating a repo means parsing the current
  files, comparing checksums, and rewriting only the bits the user has not touched.
- The tool inserts a managed section into the user's `Justfile` and into a small set of shared
  config files (`deny.toml`, `[workspace.lints]` in the workspace `Cargo.toml`, and `[lints]` in
  each crate's `Cargo.toml`). Outside those sections, the user's content is preserved verbatim.
  Everything else is in tool-owned files under `justfiles/ox-ci/` and the backend-specific CI
  directories.

## 5. User Experience

### 5.1 Installation (maintainer)

```sh
cargo install --locked cargo-ox-ci
```

Only the repo maintainer who runs updates needs the binary installed. Everyone else uses
`just` (or plain `cargo`).

### 5.2 The single command

```text
cargo ox-ci update [--backend github|ado|both|none] [--dry-run]
```

That is the entire CLI surface. There is intentionally no `init`, `migrate`, `check`, `run`,
`doctor`, `diff`, `explain`, `disable`, `enable`, or `versions` subcommand.

The algorithm is uniform — there is no distinction between "first run" and "subsequent run." For
every file or managed region the tool would emit, the decision table in §7.4 applies. The
high-level shape:

- **disabled** (an empty stub is in place — see §7.5) → never fill the stub, but still write a
  `.ox-ci-proposed` sibling so the user can see what they're opting out of and detect upstream
  changes to the template.
- **clean** (checksum matches) → overwrite with the new render (a no-op if content is unchanged).
- **dirty** (checksum missing or mismatched) → leave alone, write a `.ox-ci-proposed` sibling,
  report.
- **absent** (file or region not present and no disable stub) → create at the deterministic
  anchor.

`--dry-run` performs the same analysis but writes nothing. Exit code 0 means "everything is in
sync with the binary's current templates and all managed content matched, ignoring disabled
items"; exit code 1 means "something is out of date or user-modified." Disabled items never
flip the exit code — they are a stable user choice — but the summary lists them separately so
the available proposals are visible.

#### Backend selection

`--backend` controls which CI backend(s) get emitted: `github`, `ado`, `both`, or `none`.

If the flag is omitted, the tool auto-detects from the git remote URL of `origin`:

- `github.com` → `github`
- `dev.azure.com` or `*.visualstudio.com` → `ado`
- anything else, or if `origin` is missing: the tool errors out asking for `--backend`.

`--backend none` is valid and useful for repos that want only the local `just` setup with no
CI files (e.g. a repo where CI is configured elsewhere). The flag overrides autodetection in
all cases.

`update` never deletes files. To stop using a backend, the user removes the corresponding
directory (`.github/actions/ox-ci-*/` or `.pipelines/ox-ci/`) by hand. Subsequent runs respect
the flag — they will *create* missing backend files if requested but never destroy.

### 5.3 Daily driver

The local UX is plain `just`:

```text
$ just ox-ci
[just] running ox-ci-tools-check
[just] running ox-ci-pr-lint
[just] running ox-ci-pr-test
[just] running ox-ci-pr-mutants
ox-ci OK
```

`ox-ci` is an alias for `ox-ci-pr`. Both are plain `just` recipes (not wrappers around
`cargo ox-ci`). The PR tier is made up of a small set of *check groups* — each group is a `just`
recipe that runs the individual checks belonging to it. Groups are the level at which CI
parallelizes. See §8 for the group → check mapping.

Other tier entry points:

- `just ox-ci-pr` — fast checks suitable for every PR.
- `just ox-ci-nightly` — slow checks: miri, full mutants, feature-powerset, bench, etc.
- `just ox-ci-full` — both tiers, run sequentially.

A user with only `just` installed (no `cargo-ox-ci`) can run any check, any group, or any tier
without ever invoking the tool. `cargo-ox-ci` is only required by the maintainer who wants to
update the recipes or CI YAML.

### 5.4 No-tooling fallback

A user with only `cargo` (no `just`, no `cargo-ox-ci`) can still run the basics:

```sh
cargo test   --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The same commands appear as the body of the corresponding `just` recipes in
`justfiles/ox-ci/checks.just`, so they are discoverable by reading that file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective tools.

## 6. Repo Layout

The tool produces a small set of files. They fall into two categories:

The tool produces a small set of files. They fall into three categories:

- **owned** — the tool fully writes the file. The first line is an `ox-ci-checksum` comment.
- **managed-region** — a user-composed file with one or more tool-managed sections bracketed by
  sentinel comments. Each section carries its own checksum. Outside the sentinels, the user's
  content is preserved byte-for-byte.
- **user-authored** — files the user owns; the tool only reads them. `rust-toolchain.toml` and
  `.cargo/config.toml` fall in this category. The tool has no separate config file of its own.

Opt-out is expressed inline in the affected file itself (an empty managed-region stub or a
single-line `# ox-ci-disabled` marker for owned files); see §7.5.

```text
repo/
├── Justfile                                       managed-region: ox-ci-imports
│   # >>> ox-ci-managed: ox-ci-imports
│   # checksum: sha256:…  rendered-by: cargo-ox-ci 0.4.1
│   import 'justfiles/ox-ci/checks.just'
│   import 'justfiles/ox-ci/groups.just'
│   import 'justfiles/ox-ci/tiers.just'
│   import 'justfiles/ox-ci/tools.just'
│   alias ox-ci := ox-ci-pr
│   # <<< ox-ci-managed: ox-ci-imports
│   …user content…
│
├── justfiles/
│   └── ox-ci/                                     owned (one checksum per file)
│       ├── checks.just          per-check recipes (ox-ci-fmt, ox-ci-clippy, ox-ci-test, …)
│       ├── groups.just          group recipes (ox-ci-pr-lint, ox-ci-pr-test, ox-ci-nightly-test, …)
│       ├── tiers.just           tier aggregators (ox-ci-pr, ox-ci-nightly, ox-ci-full)
│       └── tools.just           ox-ci-tools-check + ox-ci-tools-install + helpers
│
├── Cargo.toml                                     managed-region: ox-ci-workspace-lints
│                                                  (or ox-ci-lints in a single-crate repo)
├── crates/<member>/Cargo.toml                     managed-region: ox-ci-lints (one per workspace member)
├── deny.toml                                      managed-region: ox-ci-deny
├── rustfmt.toml                                   managed-region: ox-ci-rustfmt (default on; opt out with empty stub — see §7.5)
├── rust-toolchain.toml                            user-authored (read only)
├── .cargo/config.toml                             user-authored (read only)
│
├── .github/                                       only if --backend github|both
│   └── actions/                                   owned (composite actions only)
│       ├── ox-ci-setup/action.yml
│       ├── ox-ci-pr-lint/action.yml
│       ├── ox-ci-pr-test/action.yml
│       ├── ox-ci-pr-mutants/action.yml
│       ├── ox-ci-nightly-test/action.yml
│       ├── ox-ci-nightly-advisories/action.yml
│       ├── ox-ci-nightly-runtime/action.yml
│       └── ox-ci-nightly-exhaustive/action.yml
│
└── .pipelines/                                    only if --backend ado|both
    └── ox-ci/
        └── steps/                                 owned (step templates only)
            ├── setup.yml
            ├── pr-lint.yml
            ├── pr-test.yml
            ├── pr-mutants.yml
            ├── nightly-test.yml
            ├── nightly-advisories.yml
            ├── nightly-runtime.yml
            └── nightly-exhaustive.yml
```

ox-ci does not emit workflow files (`.github/workflows/*.yml`) or pipeline files
(top-level `*.yml` extending 1ESPT/SubstratePT). Those are user-owned: ox-ci ships only
composable building blocks. See §10 for the rationale and copy-paste starter snippets.

There is no `clippy.toml` managed region. `clippy.toml` controls *lint parameters*
(thresholds, allow-lists for individual lints, `disallowed-methods`, etc.) — not lint *levels*.
The opinionated portion of the design ships in `[workspace.lints.clippy]` (lint levels) instead.
Users who need a `clippy.toml` for project-specific parameters write one themselves; the tool
ignores it.

Notes on each managed-region host:

- **`Justfile`** — single region containing the `import` lines for the four
  `justfiles/ox-ci/*.just` files and the `alias ox-ci := ox-ci-pr` line. If the file does not
  exist the tool creates it. If the region's sentinels are missing the tool inserts them after
  any leading `set …` / shebang lines and before the first user recipe.
- **Workspace `Cargo.toml`** — region named `ox-ci-workspace-lints` containing
  `[workspace.lints.rust]`, `[workspace.lints.clippy]`, and `[workspace.lints.rustdoc]`. The
  emitter uses `toml-edit` for round-trip-safe manipulation so user formatting elsewhere in
  `Cargo.toml` is preserved. In a single-crate repo (no `[workspace]` table), the region
  becomes `ox-ci-lints` and contains `[lints.*]` directly.
- **Per-crate `Cargo.toml`** (every workspace member) — region named `ox-ci-lints` containing
  exactly `[lints]\nworkspace = true`. Without this opt-in, Cargo does not apply
  `[workspace.lints.*]` to the member crate, so `[workspace.lints]` would be decorative. The
  tool enumerates members via `cargo metadata` and writes the region into each member's
  manifest. Region content is stable across binary versions (the line basically never changes),
  so updates are quiet. Users who want to deviate in a specific crate can add `[lints.clippy]`
  keys outside the managed region; closer-scope keys override workspace defaults.
- **`deny.toml`** — region at the end of the file, with the tool's baseline license/advisory
  rules. Users add their own keys outside the region. Created if absent.
- **`rustfmt.toml`** — created with the opinionated baseline if absent; managed region at the
  end of the file. This is the most contested opinion in the catalog; users who want to keep
  their own formatting opt the file out per §7.5. Once disabled, the tool will neither create
  nor update it.
- **`rust-toolchain.toml`** and **`.cargo/config.toml`** — never touched. Read-only inputs
  used by `_ox-ci-require` to validate the user's `rustc` version against the catalog
  minimum. The CI building blocks (§10) do not install Rust; that is the user's pipeline's
  job (msrustup in 1ESPT, rustup on GH runners).

The tool has no separate config file. All state — including opt-outs — lives in the affected
file itself; see §7.5.

## 7. Drift Detection and Update Algorithm

This is the single mechanism that makes "rerun `update` and your repo is up to date" work
without destroying user customization. There is no per-template version tracking and no
intermediate metadata. The questions the algorithm needs to answer are:

- "Did the user opt out of this file or region?" — answered by checking for an empty stub or
  `# ox-ci-disabled` marker in place (§7.5).
- "Did the user modify this file or region?" — answered by recomputing a checksum and comparing.
- "What should it look like?" — answered by rendering the current template embedded in the
  binary.

That is the whole design.

### 7.1 Owned-file checksum format

Every owned file starts with a comment whose syntax suits the file type:

```just
# ox-ci-checksum: sha256:8f3a…  rendered-by: cargo-ox-ci 0.4.1
# DO NOT EDIT. Regenerated by `cargo ox-ci update`.
```

```yaml
# ox-ci-checksum: sha256:8f3a…  rendered-by: cargo-ox-ci 0.4.1
# DO NOT EDIT. Regenerated by `cargo ox-ci update`.
```

The checksum covers the file content **with the `ox-ci-checksum` line removed** and with line
endings normalized to `\n`. A user who customized a file and now wants to take the new upstream
defaults deletes the entire file and reruns `cargo ox-ci update`; the tool re-creates it from
the current template.

### 7.2 Managed-region format

Files the user co-owns get one or more regions with paired sentinels carrying an `id`:

```just
# >>> ox-ci-managed: ox-ci-imports
# checksum: sha256:8f3a…  rendered-by: cargo-ox-ci 0.4.1
import 'justfiles/ox-ci/checks.just'
import 'justfiles/ox-ci/groups.just'
import 'justfiles/ox-ci/tiers.just'
import 'justfiles/ox-ci/tools.just'
alias ox-ci := ox-ci-pr
# <<< ox-ci-managed: ox-ci-imports
```

For TOML hosts the sentinels are TOML line comments around the affected table:

```toml
# >>> ox-ci-managed: ox-ci-workspace-lints
# checksum: sha256:8f3a…  rendered-by: cargo-ox-ci 0.4.1
[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
…
[workspace.lints.clippy]
…
# <<< ox-ci-managed: ox-ci-workspace-lints
```

The checksum covers everything between the sentinel lines, with the `# checksum:` line itself
excluded.

### 7.3 Per-host insertion anchors

If a host file exists but does not contain the expected region's sentinels (and the region is
not disabled per §7.5), the tool inserts the region at a deterministic per-host anchor:

| Host                                  | Region                                  | Anchor for insertion                                          |
|---------------------------------------|-----------------------------------------|---------------------------------------------------------------|
| `Justfile`                            | `ox-ci-imports`                         | After leading `set …` / shebang lines, before the first user recipe. |
| Workspace `Cargo.toml` (workspace)    | `ox-ci-workspace-lints`                 | At end of the file (after the last existing table).           |
| Workspace `Cargo.toml` (single-crate) | `ox-ci-lints`                           | At end of the file.                                           |
| Per-crate `Cargo.toml`                | `ox-ci-lints`                           | At end of the file.                                           |
| `deny.toml`                           | `ox-ci-deny`                            | End of file.                                                  |
| `rustfmt.toml`                        | `ox-ci-rustfmt`                         | End of file.                                                  |

If the host file is missing entirely (and not disabled), the tool creates it containing only
the managed region.

### 7.4 Per-item decision table

For every file or region the tool would emit, exactly one of these rows fires. There is no
distinction between "first run" and "subsequent run."

| State                                                                             | Action                                                                                  |
|-----------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| Owned file present and contains only `# ox-ci-disabled` (§7.5)                    | leave the stub alone; write `<path>.ox-ci-proposed` with the rendered file              |
| Owned file absent                                                                 | render and write                                                                        |
| Owned file present, checksum matches                                              | overwrite (no-op if content unchanged)                                                  |
| Owned file present, checksum line missing or mismatched                           | leave alone, write `<path>.ox-ci-proposed`, report                                      |
| Managed-region host present, region present with empty body (§7.5)                | leave the stub alone; write `<host>.<region-id>.proposed` with the rendered region body |
| Managed-region host absent                                                        | create host containing only the region (with its rendered body)                         |
| Host present, region present, checksum matches                                    | rewrite region                                                                          |
| Host present, region present, checksum line missing or mismatched                 | leave region alone, write proposal side file, report                                    |
| Host present, region absent                                                       | insert region at the anchor in §7.3                                                     |

Side files for proposed regions are named `<host>.<region-id>.proposed` (e.g.
`Justfile.ox-ci-imports.proposed`, `Cargo.toml.ox-ci-workspace-lints.proposed`). Side files for
proposed owned files are `<path>.ox-ci-proposed`. The same file name is used in both the
"dirty" and "disabled" cases — the contents are always the freshly rendered template, so the
user's mental model is just "what would the tool put here right now."

`.ox-ci-proposed` files accumulate on disk until the user resolves them — by merging changes by
hand, by deleting the host file to take the new render, or by deleting the proposal. The tool
prints a one-line summary on every run listing outstanding proposals, partitioned by source
(*dirty* vs *disabled*), so they don't get forgotten. *Dirty* proposals demand action and flip
the exit code to 1; *disabled* proposals are informational, do not affect the exit code, but
still appear in `git status` so an upstream template change is visible at the next commit. They
are intentionally **not** added to `.gitignore`: showing up in `git status` and diffs is the
point — a proposal you can't see is a proposal you'll forget about.

### 7.5 Opting out: in-file stubs

Opt-out is expressed in the affected file itself. There is no separate config file listing
disabled IDs. This keeps the answer to "is this region/file managed?" entirely local to the
file you are looking at.

**Disabling a managed region.** Leave the sentinel pair in place but empty the body:

```just
# >>> ox-ci-managed: ox-ci-imports
# <<< ox-ci-managed: ox-ci-imports
```

The tool sees the region, treats it as user-disabled, and never refills it. To re-enable, delete
both sentinel lines (so the region is "absent"); the next `update` will re-insert it at the
anchor (§7.3) with fresh content.

A region with a `# checksum:` line is considered live and managed. A region with no body other
than (optionally) a single `#` comment line for human notes is considered disabled. The
checksum line cannot be present in a disabled stub.

**Disabling an owned file.** Replace its contents with a single line, in the comment syntax
that fits the file type:

```text
# ox-ci-disabled
```

```yaml
# ox-ci-disabled
```

The tool sees the file, treats it as user-disabled, and neither rewrites nor proposes anything
for it. To re-enable, delete the file; the next `update` will recreate it.

**Disabling a region or file that the tool would otherwise create from scratch.** Pre-create
the empty stub or marker file by hand before running `update`. This is the only case that
requires "creating before the tool would have." `cargo ox-ci update --dry-run` lists every file
and region the tool intends to manage so users know what stubs are available to create.

**Disabled items still get a proposal.** When a stub is in place, the tool does not touch the
host file or owned-file content, but it does write a `.ox-ci-proposed` sibling carrying what
the live template would render to (see §7.4). This makes upstream template changes visible
without forcing the user to re-enable the item — once the proposal is reviewed, it can be
deleted, merged into the user's own version, or the stub can be removed to take the upstream
version. Disabled-item proposals never flip the `--dry-run` exit code.

**No special cases.** Disabling `ox-ci-imports` will break the `just ox-ci-*` recipes; that is
the user's choice. The tool does not refuse to honor a disable stub for any ID.

**Bulk handling.** There is no bulk-disable command. The four-line stub is short enough to
copy-paste, and disabling more than two or three regions is a strong signal that the tool is
not the right fit for the repo (in which case the user should remove it entirely).

### 7.6 Why no metadata file

- One source of truth per managed item — the file it describes. The presence, contents, and
  disable state of every managed item is visible by reading the file itself.
- A separate config file (`unify.lock`, `.ox-ci-disabled`, etc.) would either need to be
  updated by the tool (creating spurious merge conflicts) or would have to be hand-maintained
  in lockstep with the affected files (creating an opportunity for drift between the two).
- The complete set of "files/regions the tool owns" is discoverable by grepping for
  `ox-ci-checksum`, `ox-ci-managed:`, and `ox-ci-disabled` in the working tree.


## 8. Default Profile

The check catalog is hardcoded in the binary. Each check belongs to one or more *groups*, and
each group belongs to exactly one *tier*. Groups are the unit of CI parallelization (one CI
job per group) and the unit of local invocation through `just` (one `just` recipe per group).
A user (or CI) never has to enumerate individual checks — they operate at the group level.

The **single-tier-per-group** rule is deliberate: if you see `just ox-ci-pr-lint` in CI logs,
you know it is a PR-tier check; if you see `just ox-ci-nightly-runtime`, you know it is
nightly-only. This
makes "what gets executed" trivially answerable from the group name.

A consequence is that some checks must appear in two groups — one PR group and one nightly
group — when the check should run in both tiers. The two invocations may differ (e.g. `mutants`
runs diff-scoped in PR and full-workspace in nightly) or be identical (e.g. `tests` runs the
same way in both, but the nightly run catches flakes/environmental drift on `main`).

### 8.1 Groups

Group recipes follow the pattern `ox-ci-<tier>-<group>` (e.g. `ox-ci-pr-lint`,
`ox-ci-nightly-runtime`). The tier prefix removes the need to pick distinct names for groups
in different tiers and makes the tier of any failing job obvious from its name alone.

#### PR tier (3 groups)

| Group              | Purpose                                                                                                                              |
|--------------------|--------------------------------------------------------------------------------------------------------------------------------------|
| `pr-lint`          | All static analysis: nothing here compiles user tests or examples through to execution. Fast feedback, fail-fast.                    |
| `pr-test`          | Code execution: tests (instrumented for coverage), doctests, examples. Coverage reporting is folded in via `cargo llvm-cov nextest`. |
| `pr-mutants`       | Diff-scoped mutation testing on the change in this PR.                                                                               |

#### Nightly tier (4 groups)

| Group              | Purpose                                                                                                                                |
|--------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| `nightly-test`     | Re-runs the test suite on `main` (with coverage instrumentation) to catch flakes/environment-dependent failures and to publish a full coverage snapshot of the current `main`. |
| `nightly-advisories` | Re-runs the supply-chain checks that can newly fail without any code change — `deny`, `audit`, `aprz` — picking up newly published advisories. |
| `nightly-runtime`  | Tests under stricter runtimes that catch UB and timing/threading bugs: `miri`, `careful`.                                              |
| `nightly-exhaustive` | The expensive whole-workspace permutations that don't fit the PR budget: full `cargo mutants`, `cargo-hack --feature-powerset`, and `cargo bench --no-run` plus a single-iteration smoke run per bench target. |

The `nightly-exhaustive` group's checks are independent and could in principle live in three
parallel jobs; they're folded into one group because each individually is just one check, and
nightly tolerates the longer wall-clock that serial execution within one job implies. Repos
that want to parallelize them can split the recipe into three group recipes locally — see
§9 and §12.

### 8.2 Checks by group

The cell format is `cargo invocation (short rationale)`. "Source" cites the surveyed repo that
provided the strongest version of the check.

#### `pr-lint`

| Check                          | Invocation                                                | Source |
|--------------------------------|-----------------------------------------------------------|--------|
| `fmt`                          | `cargo fmt --all --check`                                 | all |
| `clippy`                       | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | all |
| `cargo-sort`                   | `cargo sort --workspace --check`                          | oxidizer-github |
| `license-headers`              | `cargo deny check sources` (license enforcement via deny) ＋ a small embedded `cargo-ox-ci`-shipped helper recipe that greps for the required SPDX header in `*.rs` | oxidizer (`heather`), oxidizer-github |
| `ensure-no-cyclic-deps`        | `cargo tree --workspace --duplicates` post-processed by a small embedded script | oxidizer-github |
| `ensure-no-default-features`   | `cargo metadata --format-version 1` parsed by an embedded helper that fails on `default = [...]` in any workspace member | oxidizer-github |
| `doc-build`                    | `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps` | oxidizer-github |
| `readme-check`                 | `cargo doc2readme --check` for each crate that opts in (presence of a `[package.metadata.doc2readme]` table) | oxidizer-github |
| `spellcheck`                   | `cargo spellcheck check --code 1`                         | oxidizer-github |
| `pr-title`                     | Conventional-Commits regex applied to the title in the `PR_TITLE` env var, with a fallback to `git log -1 --pretty=%s HEAD` when unset. The check itself is a single `just` recipe that defers to a tool-shipped script (`scripts/ox-ci/check-pr-title.sh`). The CI emitter sets `PR_TITLE` on the lint job: GitHub Actions reads `${{ github.event.pull_request.title }}`; ADO reads `$(System.PullRequest.Title)` (populated by ADO on PR-triggered build-validation runs). Local `just ox-ci-pr-lint` works without setup via the git fallback. | oxidizer-github |
| `deny`                         | `cargo deny check`                                        | all |
| `audit`                        | `cargo audit`                                             | oxidizer |
| `udeps`                        | `cargo +nightly udeps --workspace --all-targets --all-features` | oxidizer, oxidizer-github |
| `semver-check`                 | `cargo semver-checks --workspace`                         | oxidizer-github |
| `external-types`               | `cargo check-external-types --workspace`                  | oxidizer-github |
| `aprz`                         | `cargo aprz check` — third-party risk analysis published on crates.io | oxidizer |

#### `pr-test`

| Check        | Invocation                                                                  | Source |
|--------------|-----------------------------------------------------------------------------|--------|
| `test`       | `cargo llvm-cov nextest --workspace --all-features --locked --lcov --output-path target/coverage/lcov.info` ＋ HTML report ＋ enforced minimum threshold. The instrumented `nextest` run produces both the test pass/fail signal and the coverage artifacts in a single pass. | oxidizer, oxidizer-github |
| `doc-test`   | `cargo test --doc --workspace --all-features --locked` (nextest does not run doctests, so this is a separate cargo-test invocation) | oxidizer, oxidizer-github |
| `examples`   | `cargo run --example <name>` for each example target                        | oxidizer, oxidizer-github |

#### `pr-mutants`

| Check     | Invocation                                                                 | Source |
|-----------|----------------------------------------------------------------------------|--------|
| `mutants` | `cargo mutants --in-diff <base>…HEAD --no-shuffle --jobs 0` (diff-scoped)  | oxidizer-github |

The PR mode requires a base ref. Locally, the recipe defaults to `origin/main` (or `master`)
and can be overridden via a `BASE_REF` env var; in GitHub Actions the workflow passes
`${{ github.event.pull_request.base.sha }}`; in ADO the template parameter `prBaseRef` is wired
to `System.PullRequest.TargetBranch`.

#### `nightly-test`

| Check        | Invocation                                                                  | Source |
|--------------|-----------------------------------------------------------------------------|--------|
| `test`       | `cargo llvm-cov nextest --workspace --all-features --locked --lcov --output-path target/llvm-cov/nightly.lcov` | oxidizer, oxidizer-github |
| `doc-test`   | `cargo test --doc --workspace --all-features --locked`                      | oxidizer, oxidizer-github |
| `examples`   | `cargo run --example <name>` for each example target                        | oxidizer, oxidizer-github |

The same checks as `pr-test`, run on `main`. Two purposes: catch flakes/environmental
sensitivities that didn't trip in PR, and publish a full coverage snapshot for the current
state of `main` (the PR `test` upload only reflects diffed code; this one reflects the whole
codebase). The CI emitter wires the lcov artifact upload step in the nightly workflow only.

#### `nightly-advisories`

| Check    | Invocation              | Source |
|----------|-------------------------|--------|
| `deny`   | `cargo deny check`      | all |
| `audit`  | `cargo audit`           | oxidizer |
| `aprz`   | `cargo aprz check`      | oxidizer |

These three checks consult external databases (RustSec advisory DB, license registries, Azure
risk indices). They can fail without any code change in the repo. Re-running them nightly turns
"new advisory landed yesterday" into a tracked failure rather than an invisible regression
discovered next time someone opens an unrelated PR.

#### `nightly-runtime`

| Check     | Invocation                                                                          | Source |
|-----------|-------------------------------------------------------------------------------------|--------|
| `miri`    | `cargo +nightly miri nextest run --workspace`                                       | oxidizer, oxidizer-github |
| `careful` | `cargo +nightly careful test --workspace --all-features --locked`                   | oxidizer-github |

#### `nightly-exhaustive`

| Check                | Invocation                                                                                                   | Source |
|----------------------|--------------------------------------------------------------------------------------------------------------|--------|
| `mutants-full`       | `cargo mutants --workspace --no-shuffle --jobs 0`                                                            | oxidizer-github |
| `cargo-hack` powerset| `cargo hack --workspace --feature-powerset --depth 2 check`                                                  | oxidizer, oxidizer-github |
| `bench`              | `cargo bench --workspace --all-features --no-run` ＋ a single-iteration smoke benchmark for each bench target | oxidizer |

### 8.3 Per-check vs grouped CI execution

Each *group* is one CI job. Within a job, the checks belonging to the group run sequentially as
the `just` recipe defines them. A failure in any check fails the group; the per-check log lines
are visible in the job log but the CI surface (the green/red pill in the PR view) is per-group.

This is the deliberate middle ground between "one giant CI step running `just ox-ci-pr`" (loses
all per-check structure, one red X for any failure) and "twenty-five individual CI steps"
(unmaintainable YAML, fragile, and the tool would have to re-emit the workflow file every time
the catalog changes). Groups are stable units of meaning the user can talk about; checks are
implementation details that can churn.

### 8.4 What nightly does not re-run

Nightly does **not** re-execute the PR-only groups (`pr-lint`, the diff-scoped `pr-mutants`).
The single-tier-per-group rule means each group has one home tier. If a clippy lint becomes
more strict in a new toolchain, that won't be detected until a PR happens to exercise it; the
trade-off is that nightly's purpose is sharply scoped to "things that need nightly cadence,"
not "everything, again." Repos that want a belt-and-suspenders cron run of `just ox-ci-pr` on
`main` can wire one up in their own workflow/pipeline file alongside the ox-ci composite
actions / step templates.

## 9. Local Recipe Surface

`justfiles/ox-ci/` is structured to make all three levels (check, group, tier) addressable from
the command line.

`checks.just` defines one recipe per individual check, each named `ox-ci-<check>`. Recipes are
usually a single `cargo …` line; a handful (license-headers, ensure-no-cyclic-deps,
ensure-no-default-features, pr-title, the bench smoke loop) are short `[script]` blocks. Every
check recipe is prefixed with a quick version-gate dependency:

```just
ox-ci-clippy: (_ox-ci-require "cargo-clippy")
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

`_ox-ci-require` is a private recipe in `tools.just` that calls `cargo install --list` to
verify the tool meets the catalog's declared minimum version; missing or below-minimum tools
fail with a one-line `cargo install` hint. The cost is one cheap `cargo install --list`
invocation per check, well under a second on a warm cache.

`groups.just` defines one recipe per group, named `ox-ci-<tier>-<group>`. Where a check name
and a group's nested check name would collide (the `pr-test` group contains a check also
called `test`), the check recipe is suffixed `-only`: `ox-ci-test-only` is the single-check
recipe. The `pr-mutants` group runs the diff-scoped recipe; `nightly-exhaustive` runs the
full-workspace recipe:

```just
ox-ci-pr-lint: ox-ci-fmt ox-ci-clippy ox-ci-cargo-sort ox-ci-license-headers \
               ox-ci-ensure-no-cyclic-deps ox-ci-ensure-no-default-features \
               ox-ci-doc-build ox-ci-readme-check ox-ci-spellcheck ox-ci-pr-title \
               ox-ci-deny ox-ci-audit ox-ci-udeps ox-ci-semver-check \
               ox-ci-external-types ox-ci-aprz

ox-ci-pr-test: ox-ci-test-only ox-ci-doc-test ox-ci-examples
ox-ci-pr-mutants: ox-ci-mutants-diff

ox-ci-nightly-test: ox-ci-test-only ox-ci-doc-test ox-ci-examples
ox-ci-nightly-advisories: ox-ci-deny ox-ci-audit ox-ci-aprz
ox-ci-nightly-runtime: ox-ci-miri ox-ci-careful
ox-ci-nightly-exhaustive: ox-ci-mutants-full ox-ci-cargo-hack ox-ci-bench-only
```

`tiers.just` defines `ox-ci-pr`, `ox-ci-nightly`, `ox-ci-full`. Each tier is a recipe that
depends on the appropriate set of groups in a deterministic order:

```just
ox-ci-pr: ox-ci-tools-check ox-ci-pr-lint ox-ci-pr-test ox-ci-pr-mutants
ox-ci-nightly: ox-ci-tools-check ox-ci-nightly-test ox-ci-nightly-advisories \
               ox-ci-nightly-runtime ox-ci-nightly-exhaustive
ox-ci-full: ox-ci-pr ox-ci-nightly
```

`tools.just` defines:

- `ox-ci-tools-check` — print a status table of every tool's installed version vs. minimum.
- `ox-ci-tools-install` — install every catalog tool at the minimum version (or skip if already
  satisfied). Used as a one-shot in CI setup and locally on first use.
- `ox-ci-tools-install-missing` — install only the tools that are missing or below minimum.
- `_ox-ci-require <tool>` — internal helper called by each check.

## 10. CI Emission

The tool emits **building blocks only**, never runnable workflows or pipelines. On both
backends, the user owns the file that wires the building blocks to triggers and runners.
The pragmatic reasons:

- **Compliance harnesses own job/stage shape.** ADO/1ESPT pipelines extend a compliance
  template that defines the stage tree; the repo can only contribute steps inside the
  template's job. Emitting our own jobs/stages would either fight the template or duplicate
  work that compliance must do anyway. GitHub repos with their own workflow framework hit the
  same friction.
- **Toolchain install is the user's responsibility.** In 1ESPT-extending pipelines the Rust
  toolchain is installed via msrustup (Microsoft-internal, SDL-compliant) — the standard
  `RustInstaller` task is not used. On GitHub-hosted runners, `rustup` is pre-installed and
  `rust-toolchain.toml` triggers auto-install on first `cargo` invocation. In both cases an
  ox-ci-emitted Rust install step would be either wrong or redundant. ox-ci therefore treats
  `cargo`/`rustc` as a precondition.
- **Single source of truth across backends.** With only steps/composite-actions to emit, the
  per-backend surface is small and the user-facing model is the same: "ox-ci ships building
  blocks; you wire them in."

What ox-ci *does* emit:

- The `just` recipe tree (§9) — the actual logic of every check, group, and tier.
- A small per-backend setup wrapper that installs `just` and the catalog tools (§10.1, §10.2).
- A per-group composite action (GH) / step template (ADO) that invokes
  `just ox-ci-<tier>-<group>` with the correct env vars wired.

What ox-ci does **not** emit:

- Workflow files (`.github/workflows/*.yml`).
- Pipeline files (`<repo>.PullRequest.yml`, top-level `azure-pipelines.yml`).
- Job or stage templates.
- Rust toolchain install steps.

### 10.1 GitHub Actions

Emitted artifacts (all under `.github/actions/`):

- `ox-ci-setup/action.yml` — composite action that installs `just` and runs
  `just ox-ci-tools-install-missing`. Does not install Rust; expects `cargo` on PATH.
- `ox-ci-pr-lint/action.yml`, `ox-ci-pr-test/action.yml`, `ox-ci-pr-mutants/action.yml`,
  `ox-ci-nightly-test/action.yml`, `ox-ci-nightly-advisories/action.yml`,
  `ox-ci-nightly-runtime/action.yml`, `ox-ci-nightly-exhaustive/action.yml` — one composite
  action per group. Each composite action declares the inputs it needs (`pr_title`,
  `base_ref`) and invokes `just ox-ci-<tier>-<group>` with them wired to env vars.

Example `ox-ci-pr-lint/action.yml`:

```yaml
name: ox-ci-pr-lint
description: ox-ci PR lint group
inputs:
  pr_title:
    description: PR title for the pr-title check
    required: false
    default: ""
runs:
  using: composite
  steps:
    - uses: ./.github/actions/ox-ci-setup
    - shell: bash
      env:
        PR_TITLE: ${{ inputs.pr_title }}
      run: just ox-ci-pr-lint
```

The user's workflow file owns triggers, runners, permissions, concurrency, and any
non-ox-ci jobs. A typical hand-written `ox-ci-pr.yml` looks like:

```yaml
name: ox-ci-pr
on: { pull_request: {}, merge_group: {} }
permissions: { contents: read }
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-lint
        with:
          pr_title: ${{ github.event.pull_request.title }}
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-test
  mutants:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/actions/ox-ci-pr-mutants
        with:
          base_ref: origin/${{ github.event.pull_request.base.ref }}
```

The README ox-ci writes on first run includes this snippet as a copy-paste starting point.

### 10.2 Azure DevOps Pipelines

Emitted artifacts (all under `.pipelines/ox-ci/steps/`):

- `setup.yml` — installs `just` (`cargo install just --locked --version >=<min>`) and runs
  `just ox-ci-tools-install-missing`. Does not install Rust; expects `cargo` on PATH (provided
  by msrustup in 1ESPT pipelines).
- `pr-lint.yml`, `pr-test.yml`, `pr-mutants.yml`, `nightly-test.yml`,
  `nightly-advisories.yml`, `nightly-runtime.yml`, `nightly-exhaustive.yml` — one step
  template per group. Each declares its parameters and emits `template: setup.yml` followed
  by the `script: just ox-ci-<tier>-<group>` step with env vars wired.

Example `pr-lint.yml`:

```yaml
parameters:
- name: prTitle
  type: string
  default: $(System.PullRequest.Title)
steps:
- template: setup.yml
- script: just ox-ci-pr-lint
  displayName: ox-ci pr-lint
  env:
    PR_TITLE: ${{ parameters.prTitle }}
```

The user's pipeline file owns extension of 1ESPT/SubstratePT, msrustup-based Rust install,
stages, jobs, pool selection, and any non-ox-ci steps. A typical fragment for a
1ESPT-extending pipeline:

```yaml
extends:
  template: v1/SubstratePT.Unofficial.PipelineTemplate.yml@SubstratePipelineTemplate
  parameters:
    stages:
    - stage: Build
      jobs:
      - job: rust_pr
        steps:
        - template: /.pipelines/steps/msrustup-install.yml@self   # user-owned
        - template: /.pipelines/ox-ci/steps/pr-lint.yml@self
        - template: /.pipelines/ox-ci/steps/pr-test.yml@self
        - template: /.pipelines/ox-ci/steps/pr-mutants.yml@self
```

Splitting into multiple jobs (one per group) for parallelism is fine and recommended; ox-ci
doesn't prescribe a shape. The README's copy-paste snippet shows both single-job and
job-per-group layouts.

### 10.3 What about the Rust toolchain?

ox-ci does not install Rust. Justification:

- **ADO + 1ESPT:** the compliance pipeline installs Rust via msrustup. ox-ci must not emit a
  conflicting `RustInstaller` task or a parallel rustup install.
- **GH-hosted runners:** `rustup` is pre-installed; `rust-toolchain.toml` triggers auto-install
  on first `cargo` invocation; the cache hit on subsequent runs is good. For runners without
  rustup, the user adds whatever install step they prefer (`dtolnay/rust-toolchain`,
  `actions-rust-lang/setup-rust-toolchain`, msrustup, a pre-baked image, …) before invoking
  ox-ci composite actions.
- **Local:** `rustup` (or msrustup) is a one-time developer-setup concern documented in
  the README, not something ox-ci can or should re-do per recipe.

A check catalog entry (`rustc >= 1.93`, where `1.93` comes from `rust-toolchain.toml`) is
still validated at recipe time by `_ox-ci-require` — if the user's environment has no
`rustc` or one below the minimum, recipes fail with a clean message naming the version
mismatch.

## 11. Tool Versions and Installation

### 11.1 Policy

The tool **never pins exact versions** for the user. The catalog records, for each tool, a
*minimum required version* (e.g. `cargo-nextest >= 0.9.122`). Users are free to install newer
versions, use `mise`/`asdf`, install via package manager, etc.

### 11.2 Detecting installed versions

`_ox-ci-require <tool>` uses `cargo install --list` to enumerate currently-installed cargo
subcommands and their versions, then compares against the catalog minimum. This avoids the
problem of tools without a stable `--version` flag, is fast, and works uniformly for everything
the tool cares about (all the cargo-* checks). For the small number of non-cargo dependencies
(`just` itself), the recipe falls back to `tool --version` and a known parser.

### 11.3 Installing tools

`ox-ci-tools-install` and `ox-ci-tools-install-missing` are plain `just` recipes that loop over
the catalog and run `cargo install --locked <tool> --version >=<min>`. They are the *only*
mechanism the tool uses to install — there is no separate code path for CI. CI setup just calls
the recipes. Locally, the user runs the recipes once when `ox-ci-tools-check` complains.

Trade-off acknowledged: `cargo install --locked` is slow on a cold cache (several minutes for
the full catalog). It is also the most reliable mechanism in restricted networks. Caching (via
the GH cache action and the ADO pipeline workspace cache) is configured by the setup
action/template to key on `Cargo.lock`, the toolchain channel, and the binary's catalog hash.

### 11.4 Per-check warnings

Every check recipe depends on `_ox-ci-require <its-tool>` so even ad-hoc invocations like
`just ox-ci-miri` warn loudly if the installed tool predates the catalog minimum. The full
tier invocations additionally print a one-line tools summary at the top.

### 11.5 The Rust toolchain

`rust-toolchain.toml` is read but never written, and ox-ci never installs a Rust toolchain
itself (see §10 for the full rationale — short version: msrustup owns it on ADO/1ESPT, the
runner image owns it on GH). `_ox-ci-require` validates the installed `rustc` against the
catalog's minimum at recipe time; missing or below-minimum `rustc` produces a clean failure
message naming the version mismatch. Per-check toolchain requirements (e.g. miri, careful,
udeps need nightly) are also enforced by `_ox-ci-require`, which suggests the
user-environment-appropriate install command in the failure message (`rustup install nightly`
or "ask your team's pipeline owner to add `nightly` to msrustup").

## 12. Customization

Four escape valves, in increasing severity:

1. **Compose around the tool**: add your own `.just` files and import them from your `Justfile`
   alongside the `ox-ci/*` imports. Add your own `.github/workflows/*.yml` files (anything not
   prefixed `ox-ci-` is left alone). Add your own `.pipelines/` templates and root pipelines.
   The path of least resistance and the recommended approach for project-specific checks.
2. **Edit a managed-region host file outside the sentinels**: extra recipes in your `Justfile`,
   extra rules in `deny.toml` outside the managed region, extra clippy lints in
   `[workspace.lints.clippy]` after the closing sentinel. The tool preserves everything outside
   the sentinels verbatim.
3. **Opt out with an in-file stub** (§7.5). Empty out a managed region (leave only the
   sentinels) or replace an owned file's contents with `# ox-ci-disabled`. The tool will skip
   the item on every future `update`. Use this when you want the tool to *not exist* for a
   given file or region — e.g. you maintain `deny.toml` out of band, or you have a vendored
   crate where workspace lints shouldn't apply.
4. **Take ownership of an owned file or managed region by editing it inside the
   sentinels/checksum boundary.** The next `update` will detect the dirt, leave your file
   alone, and write a `.ox-ci-proposed` sibling. Re-bless by deleting your file (or region) and
   rerunning `update`. Suitable for one-off divergence; for permanent divergence prefer §12.3.

What the tool deliberately does **not** do:

- Modify `Cargo.toml` outside the `ox-ci-workspace-lints` / `ox-ci-lints` managed regions.
- Modify `.cargo/config.toml` or `rust-toolchain.toml`.
- Replace existing root pipelines or workflow files it didn't create.
- Carry a separate config file. Behavior tweaks (including opt-outs) are expressed inline in
  the affected file.

The intentional consequence: there is exactly one place to look for "what does this repo do
differently from the default?" — the working tree itself, plus the `--dry-run` summary listing
outstanding proposed updates.

## 13. Cross-Cutting Concerns

### 13.1 Backend selection

`--backend github|ado|both|none`. If omitted, autodetected from the `origin` git remote URL.
Autodetection runs every time; there is no "first run" special case. `update` never deletes
files.

### 13.2 Toolchain policy

`rust-toolchain.toml` is read but never written. ox-ci does not install Rust (see §10 and
§11.5); it assumes `cargo`/`rustc` are present and validates the version via
`_ox-ci-require` at recipe time. Nightly-requiring checks (miri, careful, udeps) similarly
just expect a working `cargo +nightly` and produce a clean failure if absent.

### 13.3 Security

- Generated GH workflows ship with `permissions: contents: read` and add narrower per-job
  grants only when a specific check needs them (PR title needs `pull-requests: read`).
- PR-tier and nightly-tier workflows are separate files. Nightly secrets are not granted to
  PR-tier runs.
- All cargo-tool installs done by the tool's CI scripts use `--locked`. No `cargo-binstall`.
- The tool never sources or executes content from any user-edited file at runtime; everything
  executable in the repo is plain `just` recipes the user can read.

### 13.4 Monorepo / multi-workspace

Out of scope for v1. `ox-ci-*` recipes always operate on `--workspace` from the repo root.
Repos with multiple workspaces (uncommon in the surveyed set) compose by having a separate
ox-ci tree per workspace root, each with its own `cargo ox-ci update`. Revisit after first
adopters report friction.

### 13.5 Impact-scoped (delta) builds

`oxidizer-github` uses `cargo-delta` to skip checks for unaffected workspace members on a PR.
Powerful but deferred to v2 for three concrete reasons: (1) it requires careful base-ref
selection (PR base SHA, not branch HEAD) with per-backend plumbing; (2) it can silently produce
empty exclude lists that skip everything when misused, undermining required-check policies;
(3) it changes the mental model — a green ox-ci-pr no longer means "every check ran" but
"every check that the impact analysis said matters ran." Worth doing, but worth doing
carefully; v1 runs the full tier on every PR.

### 13.6 Caching

The composite GH actions and the ADO step templates both compute a cache key from: OS, rustc
version (read from `rust-toolchain.toml`), `Cargo.lock`, `.cargo/config.toml`, and the binary's
embedded catalog hash. Each backend uses its native cache action/task. `CARGO_HOME` is pinned
to a workspace-scratch location to keep cache scoping predictable. `RUSTUP_HOME` has a sane
default and is not exposed as a template parameter — the defaults are universally fine.

### 13.7 Internal vs OSS

The crate ships from `github.com/microsoft/ox-tools` (alongside the existing tools published
from that repo) and from crates.io. The binary contains:

- The full check catalog (§8), including `cargo aprz`, which is itself published to crates.io.
- All emitters (GH, ADO).

There is no overlay system, no internal-only check, and no proprietary content. ADO templates
are plain ADO templates — they happen to be shaped to compose cleanly with SubstratePT/1ESPT,
but they are freely usable in any ADO environment.

## 14. Open Questions

1. **Crate name**: `cargo-ox-ci` is the working name. Confirm before crates.io publication. The
   `cargo-ox-*` prefix collides nicely with the existing `ox-tools` family but may read as
   MS-internal to OSS consumers. Rename is cheap pre-publication, expensive after.
2. **Where does the catalog live?** Hardcoded in the binary keeps the design simple and matches
   the single-CLI-command shape. Externalizing to a versioned schema is possible later but
   explicitly deferred.
3. **First-run preview UX**. The tool prints a summary of "files about to be created" before
   doing so, and asks for confirmation. Open question: skip flag (`--yes`) yes/no? Probably
   yes, for CI/scripting use.
4. **`Cargo.toml` round-tripping fidelity in practice**. We've committed to `toml-edit` for
   editing the `ox-ci-workspace-lints` and `ox-ci-lints` regions. The remaining unknown is
   whether constraining the managed region's layout (always one sub-table per category,
   alphabetized keys) is enough to keep diffs minimal across `toml-edit` versions. Will revisit
   if early adopters hit churn.
5. **`pr-title` regex format**. Hard-coded Conventional-Commits regex vs configurable. Current
   plan: hard-coded with an escape-valve disable. Revisit if a repo wants a different scheme
   (e.g. ticket-prefixed) and the disable-and-bring-your-own pattern proves awkward.

## 15. Phased Rollout

1. **MVP**. Binary in `microsoft/ox-tools` with `update` + `--dry-run` + `--backend github`.
   Full catalog (§8). Used in a fresh throwaway repo end-to-end.
2. **ADO emitter**. Add ADO templates (steps/jobs/stages). Validate against `oxidizer-github`
   first, then `assistants-oxide`, then `oxidizer` (the hardest case — full SubstratePT
   integration including `enableStages`).
3. **Drift hardening**. Polish the dirty-file/dirty-region UX. Iterate on the dry-run summary.
4. **Adoption**. Migrate `oxidizer-github`, `ox-tools-gh`, then `assistants-oxide`, then
   `ox-docs`. Migrate `oxidizer` last (highest blast radius).
5. **OSS visibility**. Publish to crates.io. Document in `ox-tools` README. Solicit feedback
   from external Rust maintainers.

## 16. Success Criteria

- A new Rust repo can run `cargo ox-ci update --backend github` and have a passing PR pipeline
  within 15 minutes (longer if ADO compliance pipelines need composing).
- All six surveyed repos can adopt the tool incrementally, retaining their compliance pipelines
  and custom recipes, with no regression in CI signal coverage.
- After a binary upgrade, `cargo ox-ci update --dry-run` on the median repo reports zero
  user-action-required items: every change either applies cleanly or doesn't apply at all.
- A non-Microsoft Rust developer can `cargo install cargo-ox-ci`, run
  `cargo ox-ci update --backend github` in their repo, and ship a working open-source CI setup
  without touching anything Microsoft-specific.
