# GitHub Actions integration

`cargo anvil --backend github` generates PR and scheduled validation workflows
that run the same [check groups](./checks.md) as the [local recipes](./local.md).
Generated files follow the [owned-file update contract](./updates.md):
repository edits are preserved; conflicting updates produce `.anvil-proposed`
siblings.

## Workflow configuration

The root workflows, `.github/workflows/anvil-pr.yml` and
`anvil-scheduled.yml`, control triggers, permissions, secrets, and runner
overrides. They call `anvil-pr-impl.yml` and `anvil-scheduled-impl.yml`,
which arrange the jobs and invoke shared actions under `.github/actions/anvil-*`.
Customize the roots for repository policy; changing the job graph requires
editing the implementation workflows.

The PR root handles `pull_request` and `merge_group`. A newer run cancels an
older run for the same branch. The scheduled root runs daily at 07:00 UTC and
supports manual dispatch.

Both reusable workflows accept string runner labels:

- `linux_runner`: `ubuntu-latest`
- `windows_runner`: `windows-latest`
- `linux_arm_runner`: `ubuntu-24.04-arm`
- `windows_arm_runner`: `windows-11-arm`

For example, add these overrides to the existing reusable-workflow call:

```yaml
with:
  linux_runner: self-hosted-rust
  windows_runner: self-hosted-rust-win
  linux_arm_runner: self-hosted-rust-arm
  windows_arm_runner: self-hosted-rust-win-arm
  base_ref: ${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha }}
  publish_commit_statuses: ${{ github.event_name == 'pull_request' }}
```

The last two inputs are PR-only: `base_ref` is required;
`publish_commit_statuses` defaults to `false` and the generated root enables it
for pull requests. Runner inputs change labels, not matrix membership.
Additional preparation on self-hosted runners belongs in the reusable
implementation; a caller cannot prepend steps to a reusable-workflow job.

Most groups run on Linux and Windows, x64 and ARM64. PR fuzzing runs only on
Linux x64 and ARM64; scheduled exhaustive checks run only on Linux and Windows
x64. Changing these axes requires customizing the implementation workflows.
Excluding a check from the catalog does not remove its workflow job.

The GitHub backend also emits a code-review skill and linked test-review rules
under `.github/skills/code-review/`. These are agent guidance, not CI jobs.
The adoption skill and repository-wide Anvil instructions under `.github/`
are emitted for all backends.

## Impact scoping

Two preparation jobs compute impact on Linux and Windows and upload
`anvil-impact-Linux` and `anvil-impact-Windows`. PR groups download their OS
family's artifact into `target/anvil/impact/`; ARM legs reuse the corresponding
x64 result. **Impact is per OS family, not per architecture:** ARM-only
dependency differences do not receive a separate impact computation.

Most PR groups depend on both preparation jobs; fuzzing depends only on Linux
impact. A failed preparation blocks its dependent groups. Successful impact
does not skip whole jobs: scoped recipes can no-op while unscoped checks still
run, including on documentation-only PRs.

PR groups consume the downloaded cache without recomputing it. Scheduled groups
disable impact and check the full workspace. Both local and cloud impact load
`.delta.toml`, including repository-owned exclusions, parsers, and comparison
settings. Existing top-level `trip_wire_patterns` are preserved rather than
duplicated in the managed region. See [local impact scoping](./local.md).

Configure branch protection or rulesets to require **`PR Job / Required Anvil
checks`**. This aggregate runs after both impact jobs and all six PR groups and
fails if any dependency fails, is cancelled, or is skipped. It has the same
name for pull requests and merge groups, so matrix changes do not require
enumerating new required checks.

`pr-fast` retrieves the current PR title from GitHub immediately before running
each matrix leg. Reruns therefore validate title edits, not the original event
payload. Lookup failures fail the leg; merge-group runs skip title validation.

## Toolchains, setup, and caches

The generated configuration requires Rust/Cargo 1.95 or newer and Just 1.46.0
or newer. Individual tools can impose higher requirements. Stable-toolchain
selection follows the [local contract](./local.md): caller
`RUSTUP_TOOLCHAIN`, then native rustup toolchain-file selection, then the root
manifest's MSRV. There is no runner-default fallback. The dedicated MSRV group
uses the root MSRV and skips when none is declared.

Setup expects rustup proxies on `PATH`, bootstraps cargo-binstall and Just, and
installs the selected group's tools and public toolchains. Custom/path
toolchains remain the runner owner's responsibility. GitHub-hosted Linux setup
installs `libclang-dev` for spellcheck groups; self-hosted runners must provision
system dependencies themselves.

## Caching

Cargo binaries, install ledgers, and registry downloads are cached by OS,
architecture, Cargo configuration, repository toolchain files, catalog
versions, and job. Jobs can restore other jobs' installs within the same cache
generation. `Cargo.lock` changes do not invalidate installed tools. Build
outputs and impact artifacts are not part of this cache.

The test and MSRV jobs enable disk cleanup on GitHub-hosted runners, removing
unused preinstalled non-Rust toolchains. Cleanup is disabled by default in the
shared action and never runs on self-hosted runners.

## Permissions and publication

Reusable workflows default to `contents: read`. Their callers grant the upper
permission bound; individual jobs restore only their publication scopes:

- PR groups receive `statuses: write`; only `pr-fast` also receives
  `pull-requests: write`.
- Mutation jobs receive `actions: read` and `security-events: write` for SARIF.
- The scheduled failure publisher receives only `issues: write`, without
  repository-content access.

Status and advisory-comment writes are restricted to same-repository
`pull_request` events, even if an administrator enables write tokens for
forks. Merge-group jobs still receive their declared write scopes but do not
perform those writes. Code-scanning uploads are separate and also run for
merge groups. Keep scheduled-only credentials out of PR workflows; the
generated roots use `secrets: inherit`.

Third-party actions are pinned by commit SHA in the source templates. The
shared actions are implementation details, not a public per-group action API.

The opt-in `.github/workflows/anvil-release-profile.yml` workflow runs
full-workspace release-profile validation on Linux and Windows. It supports
manual dispatch and `workflow_call`; repositories can call it from their own
event-driven workflow without adding the group to Anvil's default PR or
scheduled job graphs.

### Failure attribution and commit statuses

Just's failed-recipe diagnostic appears as an annotation, and the group step
returns Just's exit status with its complete output. Opt-in supplemental
commit statuses identify the failed recipe, for example:

```text
Anvil / license-headers [pr-fast] (linux-x64)
```

These statuses target the PR head SHA and link to the workflow run. Setup
failures report `error`; recipe failures report `failure`. Clean runs add no
new status, but supersede earlier failures for the same group and runner with
`success`. A rerun leaves the prior failure visible until its result is known.

GitHub cannot delete commit statuses: superseded rows and their context names
remain repository metadata. **Do not require supplemental status contexts.**
Publication and reconciliation are best-effort and non-atomic; cancellation
or concurrent reruns can leave stale presentation rows. The aggregate native
check remains authoritative.

### Coverage

PR and scheduled test jobs upload the same released-surface-filtered LCOV
files evaluated by the local gate. Both `lcov-all-features.info` and
`lcov-no-default.info` must exist; completed reports can upload after a group
failure. Windows ARM is excluded because its LLVM profiles are unusable.
Uploads carry per-OS flags, plus `scheduled` for scheduled runs.

Both reusable workflows accept optional `CODECOV_TOKEN`, forwarded by the
default roots. Configure it for repositories requiring authenticated uploads.
The generated workflow does not enable Codecov OIDC authentication. Codecov
upload errors are non-blocking; the local coverage gate determines the verdict.

### Test results

When a group writes JUnit files under `target/anvil/test-results/`, the shared
group action uploads them after success or failure. Release-profile validation
uses `test-results-release-profile-<os>-<arch>-<attempt>` and retains the
artifact for 30 days, subject to repository retention policy.

### Gamma reports and code scanning

After PR mutation or scheduled exhaustive checks, completed Gamma reports
upload even if the group fails. The artifact
`gamma-<group>-<runner.os>-<runner.arch>-<run_attempt>` retains JSON, HTML,
SARIF, diagnostics, performance advice, and promoted root `gamma-hints.yaml`
for 30 days, subject to repository retention policy. The hints file alone
does not trigger an upload; scratch and compiled outputs are excluded.

GitHub mutation recipes force reports into `target/cargo-gamma`, overriding
`gamma.toml`'s artifact directory. Local and ADO runs retain native directory
configuration.

Available SARIF is also uploaded to code scanning, categorized by group, OS,
and architecture. Repositories must enable code scanning; private/internal
repositories need GitHub Code Security availability. SARIF upload errors fail
the job. Downloadable-artifact and SARIF uploads are independently guarded so
one failure does not suppress the other.

### Scheduled failure issues

Any failed scheduled group triggers an issue or comment containing the failed
groups and run link, not logs or environment data. Skips and cancellations
alone do not publish. Set the Actions repository variable
`ANVIL_PUBLISH_FAILURE_ISSUE` to `false` to disable publication.

The publisher identifies an open incident by the exact body marker
`<!-- anvil scheduled failure -->`, not its title. It comments on a matching
issue or creates `[Anvil] Scheduled checks failed`. A single search considers
at most 100 results; eventual indexing and concurrent runs can create
duplicates. Successful runs do not close incidents. After a maintainer closes
one, a later failure creates a new issue.

Issues must be enabled and the caller must retain `issues: write`; publication
errors fail the publishing job rather than silently dropping the notification.

### Advisory PR comments

The canonical Linux `pr-fast` leg maintains the `anvil-semver` sticky comment
from `target/anvil/comments/semver.md`, even after another check fails.
Presence upserts the comment; absence deletes it. Fork and merge-group runs
do not publish. Advisory findings do not themselves fail the recipe; see
[checks](./checks.md) for the advisory convention.

## Dependabot ownership

With the GitHub backend selected, Anvil adds exact catalog-owned workflow and
action-manifest paths to existing `github-actions` Dependabot jobs'
`exclude-paths`. It escapes glob metacharacters and preserves scan directories,
so repository-owned workflows remain eligible even when they use the same
actions.

This is an additive patch, not file ownership. Existing exclusions, comments,
formatting, schedules, groups, cooldowns, other settings, and other ecosystems
are preserved. Missing configurations or update jobs are not created; rerun
Anvil after adding one. Existing exclusions are never removed.

Both `.github/dependabot.yml` and `.github/dependabot.yaml` are recognized.
If both exist, neither is patched. Whole-file retirements are excluded from
that check and are not recreated. Refused region retirement blocks patching;
successful region removal permits patching the remaining content.

Malformed or ambiguous YAML is left unchanged with an actionable refusal.
Read-only lookup supports aliases and merged defaults: explicit settings win,
and merge sequences use the first matching setting. Unrelated anchors and
already-complete shared exclusion lists remain unchanged. An insertion that
would edit a shared or merged container is refused; cyclic defaults are also
refused.

Catalog source repositories can retain generated-action update PRs with this
exact comment in the configuration's leading comment block:

```yaml
# anvil: keep-generated-action-updates
```

It disables new exclusions without removing existing ones. Dependabot
`exclude-paths` affects version updates, **not security updates**, which may
still need reconciliation with the source catalog.
