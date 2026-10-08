# GitHub Actions backend

The GitHub backend orchestrates the [shared Just checks](./checks.md): checkout,
tool setup, impact artifacts, matrices, permissions and publication. It does not
carry a second implementation of the checks. Generated workflows/actions are
local to the adopter's repository, so a run uses that commit's policy rather than
downloading cargo-anvil.

The [registry](../../src/anvil/artifacts/github.rs) selects and renders the
[templates](../../templates/github). All emitted files use the
[owned-file update protocol](./updates.md#2-owned-files), including root
workflows. Editing a root is supported but moves it onto proposal-based updates.

## Topology and ownership

| Layer | Generated role |
|---|---|
| `anvil-pr.yml`, `anvil-scheduled.yml` | Triggers, concurrency, caller permissions, inputs and secret forwarding |
| `anvil-pr-impl.yml`, `anvil-scheduled-impl.yml` | Group jobs, matrices, impact dependencies and result publication |
| `anvil-setup` action | Bootstrap, tool cache and group-scoped installation |
| `anvil-impact` action | Compute and upload the shared impact projection |
| `anvil-run-group` action | Setup, invoke Just, capture outcome and optionally report a supplemental status |
| `anvil-report-status` action | Best-effort commit-status history management |

Root workflows are the narrow customization point for triggers and runner
labels. Changing matrix shape or orchestration requires customizing an
implementation workflow. Changing check behavior belongs in Just or the catalog,
not in a second YAML command list.

## Root workflow contract

The [PR root](../../templates/github/pr-root-workflow.yml) handles both
`pull_request` and `merge_group` through one reusable-workflow caller, displayed
as `PR Job`. It supplies the event's base commit SHA, cancels superseded runs
through concurrency, and inherits secrets. Using one caller preserves a single
check hierarchy across PR and merge-queue events.

Its default workflow permissions are read-only contents; the reusable call
allows `statuses: write` and `pull-requests: write` as well. Called workflows
cannot elevate beyond their caller. The implementation has no permission
override narrowing those scopes by group, so they are an inherited ceiling for
its jobs—not a guarantee that only the publisher process can access credentials.

Actual advisory/status publication is guarded to same-repository
`pull_request` events. Fork PRs and merge-group executions cannot reach those
write steps; merge groups nevertheless share the caller's permission ceiling.
Generated checks execute repository code, so adopters must evaluate that trust
boundary before changing event or token policy.

The [scheduled root](../../templates/github/scheduled-root-workflow.yml) supplies
schedule/manual triggers and permits issue publication by the implementation.
Repositories that customize the root must preserve required callee inputs and
permission ceilings when adopting newer implementation templates.

## 4. Owned reusable workflows

The PR implementation runs:

1. Linux and Windows impact jobs in parallel.
2. Five independent group matrices after **both** impact jobs succeed:
   `pr-fast`, `pr-test`, `pr-msrv`, `pr-runtime-analysis`, `pr-mutants`.
3. An always-evaluated required-check aggregate.

The default PR matrix is `[linux, windows, linux-arm, windows-arm]`.
`linux_runner`, `windows_runner`, `linux_arm_runner` and `windows_arm_runner`
change labels, not that axis. `base_ref` is required; supplemental status
publication is separately controlled by `publish_commit_statuses`.

The `pr-fast` job retrieves the current title through GitHub's pull-request REST
API and passes it as `PR_TITLE`. Workflow reruns retain their original event
payload, so the event supplies the stable PR number, not the title. Lookup
failures fail loudly. Fork PRs can read their current title without reaching
publication steps; merge groups skip lookup and leave `PR_TITLE` empty.

Scheduled groups run independently with impact off. Test, advisory and runtime
analysis groups use the four-leg matrix; exhaustive checks use Linux/Windows
x86_64. Scheduled validation is a full-workspace backstop even if a runner has
an old impact cache.

### Required-check identity

Require the aggregate **`PR Job / Required Anvil checks`** in repository rules,
not recipe-specific supplemental statuses. The aggregate runs with `always()`
and requires both impact jobs and all five group results to be `success`.
Failure, cancellation or skipped dependencies cannot make the aggregate green.

Job display names contribute to GitHub check contexts. Renaming the caller or
aggregate is a compatibility change for branch protection/rulesets. Per-group
matrix jobs remain useful diagnostics, but the aggregate provides one stable
required contract for PR and merge-group events.

## Impact scoping

[`anvil-impact`](../../templates/github/impact-action.yml) bootstraps the shared
setup with `group: none`, installs cargo-delta, runs `just anvil-impact`, and
uploads `target/anvil/impact/` as `anvil-impact-<runner.os>`.

Impact jobs check out full history without LFS hydration: they need paths and
dependency metadata, not large asset payloads. Group jobs hydrate LFS where the
checks need it; base-dependent checks also need local history. Each group
downloads its OS family's projection and invokes the runner with
`impact_mode: consume`. ARM jobs reuse their OS-family result.

Impact failures block the group matrices. Successful empty projections do not
skip whole jobs: individual recipes interpret `--skip`, while other unscoped
checks can still run and required jobs reach a terminal result. Missing mandatory
consume files fail rather than falling back to workspace scope.

The producer's cache keys, dirty-tree widening and missing-config warning are
defined in [local execution](./local.md#4-impact-scoping-via-the-anvil-impact-recipe),
not reimplemented in Actions expressions.

## Setup and group execution

[`setup-action.yml`](../../templates/github/setup-action.yml) accepts a group:
empty installs the full catalog, `none` only bootstraps/cache-prepares, and a
validated group name installs `anvil-<group>-setup binstall`. Group names are
validated before interpolation into command names. Tool versions come from the
generated recipe tree.

Hosted disk cleanup is explicit and restricted to GitHub-hosted runners. The
test/MSRV and scheduled-test callers opt in; self-hosted runners are not cleaned.
This setup can remove preinstalled runner components, so “the action only runs
Just” is not its entire side-effect contract.

[`run-group-action.yml`](../../templates/github/run-group-action.yml) invokes
`just anvil-<group>` after successful setup. It streams output through `tee`,
captures Just's status via `PIPESTATUS[0]`, extracts the last terminal failed
recipe diagnostic, and then propagates the status. Missing recipe diagnostics
fall back to the group name. Log capture never turns a failed check into a
successful action.

The action exports impact mode and `GITHUB_TOKEN`; workflows provide contextual
values such as `BASE_REF` and `PR_TITLE`. CI disables incremental compilation.
Setup failure, absent group output and recipe failure remain distinct reporting
states.

## Supplemental commit statuses

The optional [status reporter](../../templates/github/report-status-action.yml)
runs after success or failure with `continue-on-error`. It writes to the PR
**head SHA**, only for same-repository pull requests. Its failure does not alter
the authoritative workflow result.

Failure contexts identify the concrete failed recipe (or setup/no-result state)
and runner. The reporter recognizes prior contexts for this group through its
context namespace and target-URL marker. It publishes the new failure first,
then supersedes stale failure contexts with success. A clean run only supersedes
prior failures; it creates no fresh success row. GitHub statuses are appended,
not deleted.

These changing contexts improve the PR rollup's diagnostic text; they must not
be required checks. Required-check identity belongs to the stable aggregate.

## 8. Caching

The setup action caches Cargo binaries, registry data and installation ledgers,
not `target/`. Keys include OS, architecture, configuration/toolchain/version
fingerprints and job identity, with restore prefixes for reusable warm state.
Installed tool versions are still checked after restoration.

No `target/` cache means less cross-job archive traffic and no promise of
incremental artifacts across different toolchains/checks. This is separate from
impact artifacts, which are explicitly produced for the current run and
downloaded as inputs.

External Actions are pinned in templates to commit SHAs or immutable release
tags. Update those executable references rather than maintaining a parallel
version list in this document.

## Coverage publication

PR and scheduled test jobs upload both LCOV configurations to Codecov whenever
both files exist, including after an unrelated step failure. Every supported leg
uploads except Windows ARM64, whose recipe uses plain nextest. OS flags distinguish
slices; scheduled uploads also carry a scheduled flag.

`CODECOV_TOKEN` is an optional reusable-workflow secret and the default roots use
`secrets: inherit`. Repository/Codecov authentication still needs to be configured
appropriately. Uploads use `fail_ci_if_error: false`; an upload outage is not the
coverage verdict. `cargo-coverage-gate` in the recipe is the enforcing layer,
independent of any additional Codecov status the adopter chooses to require.

## Scheduled failure issues

The scheduled publisher inspects every scheduled group with `always()` and
publishes only when at least one result is `failure`. Success, cancellation and
skips alone do not open an incident.

`<!-- anvil scheduled failure -->` identifies the incident; the default title is
`[Anvil] Scheduled checks failed`. One bounded Search API request looks for open
issues, followed by exact client-side marker verification. The publisher creates
an issue if none matches, otherwise comments with failed groups and the run URL.
It does not attach logs or environment contents.

This is best-effort deduplication, not a singleton guarantee: search is eventually
consistent, the result set is bounded, and simultaneous failures can race.
Successful runs do not close incidents; a maintainer closes one after resolving
the failure, and a later failure creates a new incident.

Only the scheduled publishing job requests `issues: write`; scheduled check jobs
remain read-only. Missing permission or disabled Issues fails the publisher
rather than silently losing the notification. Set repository variable
`ANVIL_PUBLISH_FAILURE_ISSUE=false` to disable publication without editing owned
YAML. The root call's permission ceiling remains present.

## Advisory PR comments

The canonical x86_64 Linux `pr-fast` leg publishes
[advisory files](./checks.md#6-advisory-pr-comments) with explicit sticky-comment
steps. File presence upserts the `anvil-semver` comment; absence deletes it.
`always()` allows cleanup after unrelated failures. Event and repository guards
exclude fork PRs and merge groups.

Adding another advisory requires a corresponding recipe and explicit publish/
clear pair. Arbitrarily discovering files would make retirement of old comments
ambiguous. PR comment publication and optional commit-status reporting are
different mechanisms with different failure handling.
