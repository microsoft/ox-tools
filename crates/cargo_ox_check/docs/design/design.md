# cargo-ox-check — Design

> Status: **Draft**.
> Crate name: `cargo-ox-check`.
> Home: `github.com/microsoft/ox-tools`, published to crates.io.

This is the top-level design document. It captures the why, the principles, and the
user-visible shape of the tool. Detail lives in companion documents:

- [checks.md](./checks.md) — the opinionated check catalog, the group/tier structure
- [local.md](./local.md) — the `justfiles/ox-check/` layout, recipe surface, and customization.
- [updates.md](./updates.md) — the drift-detection and update algorithm; opt-out semantics.
- [github.md](./github.md) — GitHub Actions emission, example workflows, impact wiring.
- [ado.md](./ado.md) — Azure DevOps Pipelines emission, 1ESPT/msrustup composition.
- [../verification.md](../verification.md) — continuous-validation strategy: dogfooding,
  fixture tests, schema validation.

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
   do so. The tool's emitted templates contain no references to those harnesses; the user's
   root pipeline does the wrapping. See [ado.md](./ado.md).
5. **Local/CI parity at every level**: every individual check, every group of checks, and the
   full tier are all reproducible locally with a single `just` invocation, using the exact same
   arguments CI uses. The three commands `just ox-check-pr`, `just ox-check-nightly`, and
   `just ox-check-full` (= pr + nightly) are first-class local entry points.
6. **Plain-cargo fallback**: a developer with only `cargo` installed (no `just`, no
   `cargo-ox-check`) can still build and run tests.
7. **Friendly updates**: the tool detects, per file and per managed region, whether the user has
   modified it, and updates only the unmodified bits.
8. **Open source**: the crate ships from `github.com/microsoft/ox-tools` and publishes to
   crates.io. The binary contains no Microsoft-internal dependencies; everything it can install
   on the user's behalf comes from crates.io.

## 3. Non-Goals

- Replacing 1ESPT, SubstratePT, CloudBuild, or any other compliance/release pipeline. ox-check's
  emitted templates contain no references to those harnesses; users wrap ox-check's stages
  template in their compliance-extending pipeline themselves. See [ado.md](./ado.md).
- Building a general-purpose CI compiler/IR. We share **check semantics**, not CI features.
- Owning `.cargo/config.toml`, `rust-toolchain.toml`, or workspace layout in `Cargo.toml`.
- Installing the Rust toolchain. msrustup owns it on 1ESPT; the runner image owns it on
  GitHub-hosted runners; the user owns it locally. The tool validates `rustc` version at
  recipe time and produces a clean failure when it doesn't meet the catalog minimum.
- Managing exact tool versions on the user's behalf — we enforce minimums only. See
  [local.md §3](./local.md#3-tool-versions-and-installation).
- Hosting a service. The tool is a CLI binary; updates ship via crates.io.
- Acting as a runtime: the tool emits `just` recipes and CI building blocks, then exits.
  It is **not** invoked at build/test/CI time. `just` is the runtime. (A narrow exception
  may be made in the future for runtime subcommands tightly coupled to CI execution —
  e.g. coverage gating — but the generator stance remains the default.)
- Destructive operations: `cargo ox-check update` never deletes files. Removing a previously
  configured CI backend is a manual `rm -rf` by the user.

## 4. Guiding Principle

> **`cargo-ox-check` writes files. `just` runs them. The repo composes everything.**

Corollaries that drive every section below:

- The tool's only job is to author and update files. It is not on the local-build hot path or in
  the CI graph at runtime.
- The local daily-driver is `just ox-check` (and friends). Those recipes call `cargo …` directly. CI
  jobs invoke the same `just` recipes. Local and CI are bit-identical because they share one
  implementation in the imported `.just` files.
- Drift detection lives inside the files themselves (per-file checksums and per-managed-region
  checksums). There is no parallel metadata file. See [updates.md](./updates.md).
- The tool inserts managed sections into the user's `Justfile` and into a small set of shared
  config files (`deny.toml`, `[workspace.lints]` in the workspace `Cargo.toml`, and `[lints]`
  in each crate's `Cargo.toml`, plus `.delta.toml` and `rustfmt.toml`). Outside those sections,
  the user's content is preserved verbatim. Everything else is in tool-owned files under
  `justfiles/ox-check/` and the backend-specific CI directories.

## 5. User Experience

### 5.1 Installation (maintainer)

```sh
cargo install --locked cargo-ox-check
```

Only the repo maintainer who runs updates needs the binary installed. Everyone else uses
`just` (or plain `cargo`).

### 5.2 The single command

```text
cargo ox-check update [--backend <name>]... [--no-backends] [--dry-run]
```

That is the entire CLI surface. There is intentionally no `init`, `migrate`, `check`, `run`,
`doctor`, `diff`, `explain`, `disable`, `enable`, or `versions` subcommand.

The algorithm is uniform — there is no distinction between "first run" and "subsequent run."
The full per-item decision table lives in [updates.md](./updates.md).

`--dry-run` performs the same analysis but writes nothing. Exit code 0 means "everything is in
sync with the binary's current templates and all managed content matched, ignoring disabled
items"; exit code 1 means "something is out of date or user-modified."

`--backend <name>` is a repeatable flag controlling which CI backend(s) get emitted. Valid
backend names today are `github` and `ado`; the flag is repeatable (`--backend github
--backend ado`) so that adding a third backend in the future doesn't require new CLI
syntax. If `--backend` is omitted, the tool autodetects from the `origin` git remote URL
(`github.com` → `github`; `dev.azure.com` / `*.visualstudio.com` → `ado`). `--no-backends`
is valid and useful for repos that want only the local `just` setup with no CI files.
`update` never deletes files; to stop using a backend the user removes its directory by
hand and reruns without that backend.

### 5.3 Daily driver

The local UX is plain `just`:

```text
$ just ox-check
[just] running ox-check-tools-check
[just] running ox-check-pr-fast
[just] running ox-check-pr-test
[just] running ox-check-pr-mutants
ox-check OK
```

`ox-check` is an alias for `ox-check-pr`. Both are plain `just` recipes (not wrappers around
`cargo ox-check`). The PR tier is made up of a small set of *check groups* — each group is a
`just` recipe that runs the individual checks belonging to it. Groups are the level at which
CI parallelizes. See [checks.md](./checks.md) for the group → check mapping and
[local.md](./local.md) for the recipe tree.

Other tier entry points:

- `just ox-check-pr` — fast checks suitable for every PR.
- `just ox-check-nightly` — slow checks: miri, full mutants, feature-powerset, bench, etc.
- `just ox-check-full` — both tiers, run sequentially.

A user with only `just` installed (no `cargo-ox-check`) can run any check, any group, or any tier
without ever invoking the tool. `cargo-ox-check` is only required by the maintainer who wants to
update the recipes or CI building blocks.

### 5.4 No-tooling fallback

A user with only `cargo` (no `just`, no `cargo-ox-check`) can still run the basics:

```sh
cargo test   --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```

The same commands appear as the body of the corresponding `just` recipes in
`justfiles/ox-check/checks.just`, so they are discoverable by reading that file. The fallback
covers core hygiene only — coverage, miri, mutants, etc. still require their respective tools.

## 6. Repo Layout

The tool produces a small set of files. They fall into three categories:

- **owned** — the tool fully writes the file. There is no in-file checksum line; ox-check
  tracks ownership and last-rendered content in a sidecar manifest at the repo root
  (`.ox-check.lock`). An advisory one-line `# Managed by cargo-ox-check` comment may appear
  at the top of each owned file, but it carries no metadata. Updates apply
  automatically when the user hasn't touched the file. If the user edits the file, the
  next `update` writes a `.ox-check-proposed` sibling **only if the template has changed
  since the last render** — claiming a file with no upstream churn produces zero
  noise.
- **managed-region** — a user-composed file with one or more tool-managed sections
  bracketed by sentinel comments. The sentinel pair (`# >>> ox-check-managed: <id>` …
  `# <<< ox-check-managed: <id>`) delimits the region body and identifies it by stable ID;
  the manifest tracks the last-rendered checksum per `(host, id)`. Outside the
  sentinels, the user's content is preserved byte-for-byte.
- **user-authored** — files the user owns; the tool only reads them.
  `rust-toolchain.toml` and `.cargo/config.toml` fall in this category.

Opt-out is expressed inline by **emptiness**: an empty managed-region body (just the
sentinels, no content between them) disables a region; an empty owned file disables
that owned item. See [updates.md §6](./updates.md#6-opting-out-in-file-stubs).

```text
repo/
├── .ox-check.lock                                    sidecar manifest tracking last-rendered checksums (see updates.md)
├── Justfile                                       managed-region: ox-check-imports
├── justfiles/ox-check/                               owned (see local.md)
├── Cargo.toml                                     managed-region: ox-check-workspace-lints (or ox-check-lints in single-crate)
├── crates/<member>/Cargo.toml                     managed-region: ox-check-lints (one per workspace member)
├── deny.toml                                      managed-region: ox-check-deny
├── rustfmt.toml                                   managed-region: ox-check-rustfmt (opt out with empty stub)
├── .delta.toml                                    managed-region: ox-check-delta (opt out disables impact scoping)
├── rust-toolchain.toml                            user-authored (read only)
├── .cargo/config.toml                             user-authored (read only)
│
├── .github/                                       only if --backend github (or autodetected) — see github.md
│   ├── actions/ox-check-*/                             owned   (per-group composite actions)
│   ├── workflows/ox-check-pr-impl.yml                  owned   (reusable workflow doing the wiring)
│   ├── workflows/ox-check-nightly-impl.yml             owned
│   ├── workflows/ox-check-pr.yml                       owned   (root workflow: triggers/permissions/runner)
│   └── workflows/ox-check-nightly.yml                  owned
│
└── .pipelines/                                    only if --backend ado (or autodetected) — see ado.md
    ├── ox-check/pr.yml                                 owned   (stages template doing the wiring)
    ├── ox-check/nightly.yml                            owned
    ├── ox-check/steps/*.yml                            owned   (per-group step templates)
    ├── ox-check-pr.yml                                 owned   (root pipeline: triggers/pool/optional extends:)
    └── ox-check-nightly.yml                            owned
```

Detail on each host:

- **`Justfile` and `justfiles/ox-check/*.just`** — see [local.md](./local.md).
- **`Cargo.toml` lints regions** — workspace `Cargo.toml` carries the
  `ox-check-workspace-lints` region containing a single `[workspace.lints]` table whose
  rust/clippy/rustdoc entries are written in dotted-key form
  (`rust.unsafe_op_in_unsafe_fn = "warn"`, `clippy.unwrap_used = "warn"`, etc.). This
  form is chosen because TOML forbids re-declaring a table header — if ox-check wrote
  `[workspace.lints.clippy]` inside the region, users couldn't add another
  `[workspace.lints.clippy]` block elsewhere in the file. With dotted keys, users
  append new lints in the same scope right after the closing sentinel; see §7. Each
  member `Cargo.toml` carries an `ox-check-lints` region with exactly
  `[lints]\nworkspace = true`. The emitter uses `toml-edit` for round-trip-safe
  manipulation. In a single-crate repo (no `[workspace]` table), the workspace region
  becomes `ox-check-lints` and contains a single `[lints]` table with the same
  dotted-key layout.
- **`deny.toml`** — managed region at the end of the file with the tool's baseline
  license/advisory rules. Users add their own keys outside the region. Created if absent.
  Content detailed in [checks.md](./checks.md).
- **`rustfmt.toml`** — created with the opinionated baseline if absent; managed region at the
  end of the file. The most contested opinion in the catalog; users who want to keep their own
  formatting opt the file out via the empty-stub mechanism in [updates.md](./updates.md).
- **`.delta.toml`** — cargo-delta configuration that drives impact-scoped CI runs. Created if
  absent. Region at the end of the file. Disabling the region opts the repo out of impact
  scoping entirely. See [checks.md](./checks.md#impact-scoping) and the per-backend wiring in
  [github.md](./github.md) / [ado.md](./ado.md).
- **`rust-toolchain.toml`** and **`.cargo/config.toml`** — never touched. Read-only inputs
  used by `_ox-check-require` to validate the user's `rustc` version against the catalog
  minimum. The CI building blocks do not install Rust; that is the user's pipeline's job
  (msrustup in 1ESPT, rustup on GH runners).

The tool's persistent state lives in `.ox-check.lock` at the repo root — the sidecar
manifest tracking last-rendered checksums per owned file and per managed region. See
[updates.md §1](./updates.md#1-the-manifest). All other state — including opt-outs —
lives in the affected file itself; see [updates.md](./updates.md).

## 7. Customization

Four escape valves, in increasing severity:

1. **Compose around the tool**: add your own `.just` files and import them from your `Justfile`
   alongside the `ox-check/*` imports. Add your own `.github/workflows/*.yml` files (anything not
   prefixed `ox-check-` is left alone). Add your own `.pipelines/` templates and root pipelines.
   The path of least resistance and the recommended approach for project-specific checks.
2. **Edit a managed-region host file outside the sentinels**: extra recipes in your
   `Justfile`, extra rules in `deny.toml` outside the managed region, extra clippy
   lints written in dotted-key form after the closing sentinel (e.g. `clippy.pedantic = "warn"`
   in the `[workspace.lints]` scope). The tool preserves everything outside the
   sentinels verbatim. Note that TOML forbids redeclaring a table header (`[workspace.lints.clippy]`
   etc.), so user extensions must use dotted-key form or sit in a different parent
   table; overriding an individual key already set inside the region requires editing
   inside it, which triggers the dirty-file flow (see [updates.md §5](./updates.md#5-the-decision-algorithm)).
3. **Opt out by emptying.** Empty a managed region (leave only the sentinels) or empty
   an owned file. The tool will skip the item on every future `update` and only emit a
   `.ox-check-proposed` sibling when the template actually changes. See
   [updates.md §6](./updates.md#6-opting-out-in-file-stubs).
4. **Take ownership of an owned file or managed region by editing it.** The next
   `update` detects the dirt (via checksum comparison against the manifest), leaves your
   file alone, and writes a `.ox-check-proposed` sibling only if the template changed since
   the last render. Re-bless by deleting your file (or region) and rerunning `update`.
   Suitable for one-off divergence; for permanent divergence prefer the opt-out stub.

What the tool deliberately does **not** do:

- Modify `Cargo.toml` outside the `ox-check-workspace-lints` / `ox-check-lints` managed regions.
- Modify `.cargo/config.toml` or `rust-toolchain.toml`.
- Replace existing workflows, root pipelines, or any file it didn't create.

The intentional consequence: there is exactly one place to look for "what does this repo do
differently from the default?" — the working tree itself, plus the `--dry-run` summary listing
outstanding proposed updates.

## 8. Cross-Cutting Concerns

### 8.1 Security

- Generated GH composite actions and ADO step templates do nothing privileged on their own;
  they just invoke `just` recipes. The user's workflow / pipeline file controls permissions
  and secrets.
- All cargo-tool installs done by the setup building blocks use `--locked`. No
  `cargo-binstall`.
- The tool never sources or executes content from any user-edited file at runtime;
  everything executable in the repo is plain `just` recipes the user can read.
- Recommended user-workflow shape: `permissions: contents: read` on PR workflows; grant
  `pull-requests: read` only on the pr-fast job (for PR title). Nightly secrets, if any, live
  on the nightly workflow only — never on the PR workflow. See the snippets in
  [github.md](./github.md) and [ado.md](./ado.md).

### 8.2 Monorepo / multi-workspace

Out of scope for v1. `ox-check-*` recipes always operate on `--workspace` from the repo root.
Repos with multiple workspaces (uncommon in the surveyed set) compose by having a separate
ox-check tree per workspace root, each with its own `cargo ox-check update`. Revisit after first
adopters report friction.

### 8.3 Cross-OS test matrices

CI fans out the catalog across operating systems and architectures. The default matrix
differs by backend:

**GitHub backend (default: four legs).** GH ships Microsoft-hosted ARM runners
(`ubuntu-24.04-arm`, `windows-11-arm`), so the default matrix covers Linux/Windows ×
x86_64/aarch64 for every group except groups with no cfg-sensitivity (none currently).
Matches `oxidizer-github`'s `extended-analysis` matrix.

**ADO backend (default: two legs).** ADO has no Microsoft-hosted ARM agents. The default
matrix is x86_64 Linux + x86_64 Windows. Matches the platforms list in `oxidizer`'s root
pipelines. Adopters with self-hosted ARM pools extend the stages template in their root
pipeline.

| Group                                                       | OS / arch scope (default)              | Rationale                                                                                                                                          |
|-------------------------------------------------------------|----------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|
| `pr-fast`, `nightly-advisories`                             | All legs above                         | Contain compile-sensitive checks (clippy, doc-build, udeps, semver-check, external-types) that only see the host's compiled crate graph — cfg-gated code is invisible to a single-leg run. Text/metadata checks running redundantly is cheaper than splitting jobs. |
| `pr-test`, `nightly-test`                                   | All legs above                         | Where compile-time and runtime OS / arch bugs actually surface.                                                                                    |
| `pr-mutants`, `nightly-exhaustive`                          | All legs above                         | Match `oxidizer`'s policy: mutation testing and full-workspace feature/bench compile checks on cfg-gated code matter. Adopters who can't afford `mutants-full` on every leg (hours-long) override the matrix in their root workflow / pipeline. |
| `nightly-runtime`                                           | All legs above                         | Both surveyed repos run `miri` and `careful` cross-OS. Both tools work on every Tier 1 Rust target; the earlier "Linux-primary" framing was incorrect.                                  |

macOS is not in the default matrix — adopters who need it add it via the root workflow's
`test_os` input (GH) or root pipeline's `testPools` parameter (ADO). Both knobs are
documented in [github.md §4](./github.md#4-owned-reusable-workflows) for `test_os` and
[ado.md §4](./ado.md#4-owned-stages-templates) for `linuxPool`/`windowsPool`.

Locally there is no matrix — `just ox-check-pr-test` runs against whatever OS the developer
is on. CI fan-out lives entirely in the owned wiring layer (the reusable workflow / stages
template), so users don't write per-OS jobs.

cargo-delta impact runs once on Linux and feeds the same excludes to every matrix leg —
the source tree is OS-invariant. Caching keys already include OS (see
[github.md §8](./github.md#8-caching) and [ado.md §7](./ado.md#7-caching)).

**Helper scripts use PowerShell Core (`pwsh`) on every platform.** Almost every check
recipe is a single-line `cargo …` invocation that works unmodified on Windows —
including `license-headers` (which calls `cargo heather`), `ensure-no-cyclic-deps`
(`cargo ensure-no-cyclic-deps`), and `ensure-no-default-features`
(`cargo ensure-no-default-features`), all of which are plain cargo subcommands from the
ox-tools family. The one current exception is `pr-title`, which does a regex match
against `$PR_TITLE` (no equivalent cargo subcommand and `just` itself has no
boolean-regex primitive). That check is written as a `[script("pwsh")]` block. `pwsh`
is preinstalled on GH-hosted runners (`ubuntu-latest` included) and Microsoft-hosted
ADO Linux agents; Linux/macOS developers install it from
<https://github.com/PowerShell/PowerShell> as a one-time prerequisite. The
`_ox-check-require pwsh` recipe enforces this with a clean failure message and a per-OS
install hint. The dependency is kept (rather than dropped to remove the one script)
so future additions that don't fit cleanly as cargo subcommands have an established
escape hatch.

### 8.4 Internal vs OSS

The crate ships from `github.com/microsoft/ox-tools` (alongside the existing tools published
from that repo) and from crates.io. The binary contains:

- The full check catalog (see [checks.md](./checks.md)), including `cargo aprz`, which is
  itself published to crates.io.
- All emitters (GH, ADO).

There is no overlay system, no internal-only check, and no proprietary content. ADO
templates are plain ADO templates — they happen to be shaped to compose cleanly with
SubstratePT/1ESPT, but they are freely usable in any ADO environment.

