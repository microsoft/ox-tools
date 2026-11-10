# Azure DevOps Pipelines integration

`cargo anvil --backend ado` generates PR and scheduled pipelines that run the
same [check groups](./checks.md) as the [local recipes](./local.md).
Generated files follow the [owned-file update contract](./updates.md):
repository edits survive regeneration; conflicting updates produce
`.anvil-proposed` siblings.

## Pipeline configuration

The roots `.pipelines/anvil-pr.yml` and `anvil-scheduled.yml` select triggers,
pools, secrets, and any compliance-template composition. They include
`.pipelines/anvil/pr.yml` and `scheduled.yml`, which arrange stages and invoke
per-group templates under `anvil/steps/`.

For Azure Repos, configure PR build validation through branch policies;
YAML `pr:` triggers do not enable it. The generated PR root disables push
triggers. The scheduled root runs daily at 06:00 UTC on existing `main` and
`master` branches, including when nothing changed.

Both stages templates accept `linuxPool` and `windowsPool` objects, defaulting
to `ubuntu-latest` and `windows-latest`. For example:

```yaml
stages:
  - template: anvil/pr.yml
    parameters:
      linuxPool: { name: my-linux-pool }
      windowsPool: { name: my-windows-pool }
```

The default jobs target Linux and Windows x64, except PR fuzzing, which is
Linux-only. **No ARM jobs are generated.** Self-hosted ARM coverage or other
matrix changes require customizing the stages templates; an empty pool object
does not remove a job. Excluding a check does not remove its stage or setup.

## Repository extension points

### Custom stages

Each root also includes `anvil/custom-pr-stages.yml` or
`anvil/custom-scheduled-stages.yml`. These empty stage templates accept the
same pool parameters and provide space for repository-specific validation,
deployment, or downstream triggers without editing the generated roots.
Repository additions are preserved by the owned-file flow.

ADO stages depend on the preceding stage by default. Give custom stages an
explicit `dependsOn` when they need to wait for several Anvil stages or run
independently.

### Compliance templates and job wrapper

Anvil emits plain Azure Pipelines YAML. A repository requiring 1ESPT,
SubstratePT, or another compliance harness composes it in its root pipeline,
for example:

```yaml
resources:
  repositories:
    - repository: 1ESPipelineTemplates
      type: git
      name: 1ESPipelineTemplates/1ESPipelineTemplates
      ref: refs/tags/release

extends:
  template: v1/1ES.Unofficial.PipelineTemplate.yml@1ESPipelineTemplates
  parameters:
    pool: { name: my-default-pool }
    stages:
      - template: /.pipelines/anvil/pr.yml@self
        parameters:
          linuxPool: { name: my-linux-pool }
          windowsPool: { name: my-windows-pool }
```

The harness's own permissions and template requirements still apply. Include
the custom-stages template too if the repository uses that extension point.

Every generated job passes through `.pipelines/anvil/steps/job.yml`. Customize
this wrapper for `templateContext`, compliance metadata, checkout, or artifact
handling while retaining its parameter contract:

- `name`: job name.
- `stage`: enclosing stage identifier, for compile-time customization; the
  default wrapper ignores it.
- `pool`: pool object.
- `steps`: supplied step list.
- `inputArtifacts`: optional `{ name, path }` entries to download before checks.
- `artifacts`: optional `{ name, path }` entries to publish after checks.

The default wrapper uses pipeline-artifact tasks. A compliance wrapper can
translate these lists into its harness's input/output declarations.
Customized wrapper bodies are preserved, but a stages update that passes a
new parameter must be adopted together with the matching proposed wrapper
declaration: ADO rejects undeclared parameters before running jobs.

### Before/after check hooks

Repository prerequisites and cleanup belong in
`.pipelines/anvil/hooks/before-checks.yml` and `after-checks.yml`. Both start
as empty step templates and preserve repository edits. For example:

```yaml
# .pipelines/anvil/hooks/before-checks.yml
steps:
  - template: /ci/native-prerequisites.yml
```

The hooks run once per job, including impact jobs, in both tiers. With default
checkout behavior, the before hook follows checkout and input-artifact
downloads and precedes setup. The after hook follows check steps and precedes
artifact publication. It is not a pipeline-completion callback: preserve files
needed for publication and agent post-job processing.

Hook steps use ADO's normal success condition unless overridden. Cleanup can
use `always()`, or `succeededOrFailed()` to exclude cancellation; agent
availability and cancellation timeouts still apply. Customized wrappers must
include these hook calls themselves—regeneration does not inject calls into
repository-owned bodies.

## Impact scoping

The `impact` stage computes Linux and Windows impact independently and
publishes `anvil-impact-linux` and `anvil-impact-windows`. Each PR group
depends on successful impact, downloads its OS artifact into
`target/anvil/impact/`, and consumes the cache without recomputing it.
Failure of either impact job blocks the dependent stages.

Successful impact does not skip whole stages: scoped recipes can no-op while
unscoped checks still run. Scheduled groups run independently over the full
workspace with impact disabled. Cloud impact loads `.delta.toml`, including
repository-owned parsers, exclusions, and comparison settings; existing
top-level `trip_wire_patterns` are preserved rather than duplicated.

Per-group step templates can also be included directly in another pipeline.
PR templates consume impact; arrange an artifact download to obtain scoping.
Without that cache, checks fall back to their full-workspace defaults.
Scheduled templates always disable impact.

Only `pr-fast` resolves the current PR title through the REST API using
`System.AccessToken`. A known PR whose metadata cannot be retrieved fails
validation; non-PR builds leave the title empty. The mutation recipe uses
ADO's predefined pull-request target branch.

## Toolchains, setup, and caches

The generated configuration requires Rust/Cargo 1.95 or newer and Just 1.46.0
or newer; individual tools can require newer versions. The runner or repository
bootstrap must provide Rust/rustup, Cargo, PowerShell, and native dependencies.
Generated command steps use `pwsh`, not Bash.

Stable-toolchain selection follows the [local contract](./local.md): inherit
`RUSTUP_TOOLCHAIN`, otherwise let rustup process a repository toolchain file,
otherwise select the root manifest MSRV. Missing all three is an error, not
permission to use the agent default. Setup provisions the selected public
toolchain; custom/path toolchains remain the repository's responsibility.
The dedicated MSRV group uses the declared root MSRV and skips if none exists.

`steps/setup.yml` restores Cargo downloads and installed tools before
bootstrapping Just. Its `group` parameter selects full setup when empty,
bootstrap-only setup for `none`, or that group's prerequisites otherwise.
ADO installs catalog tools from source with `--locked`; it does not use
cargo-binstall.

## Caching

Cache identity includes OS, architecture, Cargo configuration, repository
toolchain files, and catalog versions. Manifest and lockfile changes do not
invalidate installed tools. Build outputs, rustup toolchains, and impact
artifacts are not cached. Check steps disable incremental compilation.
The setup template exposes no cache-namespace override.

The root pipeline controls service connections, variable groups, and approval
gates. Keep scheduled-only credentials out of PR pipelines; compliance
composition does not grant permissions automatically.

## Coverage

Linux and Windows test jobs publish the released-surface-filtered
`target/coverage/lcov-*.info` reports through `PublishCodeCoverageResults@2`.
These are the same reports evaluated by the local coverage gate, not a second
coverage policy.

Publication runs after success or failure, but not cancellation. Missing
reports are allowed because unaffected coverage checks can no-op. Collection
or evaluation failures still fail the recipe. ADO's Code Coverage tab is an
informational view; the generated recipe determines the coverage verdict.

## Advisory PR comments

The canonical Linux `pr_fast` job maintains a semver advisory thread from
`target/anvil/comments/semver.md`, including after another check fails.
The publisher finds an open thread by `<!-- anvil-semver -->` in its first
comment, then updates that comment or creates a thread. If the file is absent,
it closes the matching thread rather than deleting it. A later finding after
closure creates a new thread.

The step explicitly maps `System.AccessToken`. The build identity needs
**Contribute to pull requests** on the repository; `persistCredentials` does
not grant that permission. Missing PR/token context or HTTP 401/403 while
listing threads skips publication. Other API errors, including errors during
writes, propagate rather than being silently ignored.

ADO retains group-stage and Just failure diagnostics in logs; it does not
publish GitHub-style supplemental commit statuses or scheduled failure issues.
