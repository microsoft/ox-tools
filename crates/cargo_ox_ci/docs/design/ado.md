# Azure DevOps Pipelines Integration

This document describes what `cargo ox-ci update --backend ado|both` emits for Azure DevOps
Pipelines, and how a repo wires those building blocks into pipelines (typically
1ESPT/SubstratePT-extending ones). ox-ci emits **only step templates** — never job
templates, stage templates, or root pipelines. Compliance harnesses own the stage tree;
ox-ci contributes steps inside.

See also:

- [design.md](./design.md) for the overall principles and the "building blocks only" stance.
- [checks.md](./checks.md) for what each group runs.
- [local.md](./local.md) for the `just` recipes the templates invoke.
- [github.md](./github.md) for the GitHub Actions counterpart.

## 1. Why only step templates

- **1ESPT/SubstratePT extension model.** Compliance pipelines `extends:` a vendored
  template that defines the stage tree. The repo's role is to plug stage-flag-gated steps
  into specific extension points. Emitting our own jobs/stages would either fight the
  template (it would refuse) or duplicate compliance work the template must do anyway.
- **msrustup ownership of the Rust toolchain.** 1ESPT-compliant Rust install goes through
  msrustup (Microsoft-internal, SDL-compliant). The standard `RustInstaller` Azure DevOps
  task is not used. ox-ci must not emit a conflicting install step. The user's pipeline
  installs Rust before the ox-ci step templates run.
- **Compliance parity with GitHub.** The GH side also ships only building blocks
  ([github.md](./github.md)). Same mental model both places: "ox-ci ships steps; you wire
  them in."

## 2. Emitted artifacts

All under `.pipelines/ox-ci/steps/`:

```text
.pipelines/ox-ci/steps/
├── setup.yml                 # installs just + catalog tools
├── impact.yml                # cargo-delta scoping; omitted if .delta.toml disabled
├── pr-fast.yml               # one step template per group
├── pr-test.yml
├── pr-mutants.yml
├── nightly-test.yml
├── nightly-advisories.yml
├── nightly-runtime.yml
└── nightly-exhaustive.yml
```

What ox-ci does **not** emit:

- Pipeline files (`<repo>.PullRequest.yml`, top-level `azure-pipelines.yml`, etc.).
- Job templates, stage templates.
- Rust toolchain install steps (msrustup is the user's responsibility).

### 2.1 `setup.yml`

Step template that installs `just` (`cargo install just --locked --version >=<min>`) and
runs `just ox-ci-tools-install-missing`. Does not install Rust; expects `cargo` on PATH
(provided by the user's msrustup step in 1ESPT pipelines).

### 2.2 Per-group step templates

One step template per group (3 PR + 4 nightly = 7 templates). Each declares its parameters
and emits `template: setup.yml` followed by the `script: just ox-ci-<tier>-<group>` step
with env vars wired.

Example `pr-fast.yml`:

```yaml
parameters:
- name: prTitle
  type: string
  default: $(System.PullRequest.Title)
- name: excludeNotModified
  type: string
  default: ""
steps:
- template: setup.yml
- script: just ox-ci-pr-fast
  displayName: ox-ci pr-fast
  env:
    PR_TITLE: ${{ parameters.prTitle }}
    OX_CI_EXCLUDE_NOT_MODIFIED: ${{ parameters.excludeNotModified }}
```

Parameter surface per template:

| Template                  | Parameters                                                              |
|---------------------------|-------------------------------------------------------------------------|
| `pr-fast.yml`             | `prTitle` (default `$(System.PullRequest.Title)`), `excludeNotModified` |
| `pr-test.yml`             | `excludeNotAffected`, `excludeNotRequired`                              |
| `pr-mutants.yml`          | `prBaseRef` (default `$(System.PullRequest.TargetBranch)`), `excludeNotAffected` |
| `nightly-test.yml`        | (none)                                                                  |
| `nightly-advisories.yml`  | (none)                                                                  |
| `nightly-runtime.yml`     | (none)                                                                  |
| `nightly-exhaustive.yml`  | (none)                                                                  |

`$(System.PullRequest.Title)` and `$(System.PullRequest.TargetBranch)` are auto-populated
by ADO on PR-triggered build-validation runs. No manual web-UI wiring is needed.

## 3. Example user-owned pipeline

A typical `<repo>.PullRequest.yml` extending 1ESPT/SubstratePT, with impact scoping:

```yaml
extends:
  template: v1/SubstratePT.Unofficial.PipelineTemplate.yml@SubstratePipelineTemplate
  parameters:
    stages:
    - stage: Build
      jobs:
      - job: impact
        steps:
        - template: /.pipelines/ox-ci/steps/impact.yml@self
          parameters:
            baseRef: $(System.PullRequest.TargetBranch)
      - job: rust_pr
        dependsOn: impact
        condition: ne(dependencies.impact.outputs['delta.skip'], 'true')
        variables:
          excludeNotModified: $[ dependencies.impact.outputs['delta.exclude_not_modified'] ]
          excludeNotAffected: $[ dependencies.impact.outputs['delta.exclude_not_affected'] ]
          excludeNotRequired: $[ dependencies.impact.outputs['delta.exclude_not_required'] ]
        steps:
        - template: /.pipelines/steps/msrustup-install.yml@self   # user-owned
        - template: /.pipelines/ox-ci/steps/pr-fast.yml@self
          parameters: { excludeNotModified: $(excludeNotModified) }
        - template: /.pipelines/ox-ci/steps/pr-test.yml@self
          parameters:
            excludeNotAffected: $(excludeNotAffected)
            excludeNotRequired: $(excludeNotRequired)
        - template: /.pipelines/ox-ci/steps/pr-mutants.yml@self
          parameters: { excludeNotAffected: $(excludeNotAffected) }
```

Splitting the work into multiple jobs (one per group) for parallelism is fine and
recommended; the example above keeps a single `rust_pr` job for brevity. The README that
ox-ci writes on first run shows both shapes and an impact-free variant for repos that
disabled `.delta.toml`.

For nightly pipelines, omit the `impact` job entirely — nightly always runs full-workspace
on `main`.

## 4. Impact scoping

ox-ci emits `.pipelines/ox-ci/steps/impact.yml` as a step template with one parameter
`baseRef`. It runs the same logic as the GitHub composite action
([github.md §4](./github.md#4-impact-scoping)) but exports the four results as ADO output
variables via `##vso[task.setvariable variable=…;isOutput=true]`:

- `delta.exclude_not_modified`
- `delta.exclude_not_affected`
- `delta.exclude_not_required`
- `delta.skip`

Downstream jobs reference them via `dependencies.impact.outputs['delta.<name>']`. The
runtime macro `$[ … ]` is required (rather than the compile-time `${{ … }}` macro) because
output variables aren't resolved until the producing job has finished.

The check → tier mapping is in
[checks.md §5](./checks.md#5-impact-scoping-check--env-var-mapping). The recipe-side
mechanics are in [local.md §4](./local.md#4-impact-scoping-pass-through-env-vars).

If `.delta.toml`'s managed region is disabled
([updates.md §opt-out](./updates.md#opting-out-in-file-stubs)), ox-ci suppresses emission
of `impact.yml` entirely. The per-group step templates still accept the `excludeNot*`
parameters for compatibility; with no `impact` job feeding them they default to empty and
every group runs full-workspace.

## 5. Rust toolchain

ox-ci does not install Rust on ADO. The step templates assume `cargo` is on PATH. The
user's pipeline installs Rust before invoking any ox-ci template — typically by including
a repo-local msrustup step template:

```yaml
steps:
- template: /.pipelines/steps/msrustup-install.yml@self
- template: /.pipelines/ox-ci/steps/pr-fast.yml@self
```

Why ox-ci doesn't ship a Rust install step:

- **1ESPT compliance.** The compliance pipeline installs Rust via msrustup. ox-ci must not
  emit a conflicting `RustInstaller` task or a parallel rustup install.
- **Toolchain choice is a repo decision.** msrustup channels (`ms-prod-1.93`, etc.) are
  repo-policy questions ox-ci has no business making.

`_ox-ci-require` (invoked by every check recipe) validates the installed `rustc` against
the catalog minimum at recipe time; missing or below-minimum `rustc` produces a clean
failure message. For nightly-requiring checks (miri, careful, udeps), the failure message
suggests asking the team's pipeline owner to add `nightly` to msrustup.

## 6. Caching

`setup.yml` computes a cache key from: OS, rustc version (read from
`rust-toolchain.toml`), `Cargo.lock`, `.cargo/config.toml`, and the binary's embedded
catalog hash. Uses the ADO pipeline workspace cache (`Cache@2` task). `CARGO_HOME` is
pinned to a workspace-scratch location to keep cache scoping predictable.

The cache covers:

- The `cargo install`-ed tools from `ox-ci-tools-install-missing`.
- The `target/` directory (per ox-ci recipe; a per-recipe cache scope means a `pr-test`
  cache hit doesn't have to wait on a `pr-fast` cache miss).

Cache scoping inside 1ESPT-compliant pipelines is bounded by the template's allowed cache
namespaces; the emitted cache step uses the project-scoped namespace by default and the
user can override via a parameter on `setup.yml` if their compliance policy requires a
different one.

## 7. Security

The step templates do nothing privileged on their own — they just install tools and
invoke `just`. The user's pipeline controls service-connection scoping, secret variable
groups, and approval gates.

Recommended user-pipeline shape:

- PR pipelines (build validation) and nightly pipelines are separate files extending the
  same 1ESPT template with different parameters and triggers.
- Nightly variable groups (with any external-service credentials) are referenced only by
  the nightly pipeline.
- All cargo-tool installs done by `setup.yml` use `--locked`. No `cargo-binstall`.

## 8. Composing with existing 1ESPT pipelines

For repos that already have a working 1ESPT-extending pipeline, adopting ox-ci is
incremental:

1. Run `cargo ox-ci update --backend ado` to emit the step templates and managed regions.
2. Add a `template: /.pipelines/ox-ci/steps/<group>.yml@self` line for one ox-ci group at
   a time inside an existing job.
3. Verify the group runs green on a PR.
4. Iterate until all ox-ci groups are wired in.
5. Optionally split into one job per group for parallelism — `dependsOn` and the impact
   variables from §4 compose cleanly with the 1ESPT job/stage flag system
   (`enableStages` etc.).

The pre-existing repo-specific compliance steps (msrustup, NuGet pushes, signing, …) keep
running alongside the ox-ci steps. ox-ci does not own the pipeline's shape — it just
contributes steps.
