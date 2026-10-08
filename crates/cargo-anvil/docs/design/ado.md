# Azure DevOps backend

The ADO backend wraps the [shared Just checks](./checks.md) in stage/job
orchestration. It separates Anvil's evolving check graph from repository-owned
enterprise pipeline policy. It does not certify compliance or automatically
infer the requirements of 1ES PT, SubstratePT or other organization templates.

Implementation: [artifact registry](../../src/anvil/artifacts/ado.rs) and
[ADO templates](../../templates/ado).

## Generated layers

| Location under `.pipelines/` | Responsibility |
|---|---|
| `anvil-pr.yml`, `anvil-scheduled.yml` | Root composition, pool parameters and triggers/schedule |
| `anvil/pr.yml`, `anvil/scheduled.yml` | Owned implementation stages and dependencies |
| `anvil/custom-pr-stages.yml`, `anvil/custom-scheduled-stages.yml` | Initially empty repository extension stages |
| `anvil/steps/job.yml` | Customizable job-shape boundary |
| `anvil/hooks/before-checks.yml`, `after-checks.yml` | Per-job step insertion points |
| `anvil/steps/setup.yml`, `impact.yml`, group steps | Bootstrap, scope computation and Just invocation |

All catalog-emitted files initially follow
[owned-file rules](./updates.md#2-owned-files), including customization stubs.
Editing a stub/wrapper is intentional divergence: the engine keeps it and offers
a `.anvil-proposed` sibling only when the template changes. There is no separate
create-once lifecycle or `.proposed` extension.

## Root composition

The default PR root sets `trigger: none`; the Azure Repos build-validation policy
must select the pipeline for PR validation. The root includes the Anvil stages
and the custom-stage stub, passing `linuxPool` and `windowsPool` to both.
The scheduled root similarly composes scheduled and custom stages.

Pool inputs are complete ADO pool objects, not a matrix-definition language.
Changing runner pools does not remove jobs; in particular `{}` is not a
documented “disable Windows” switch. Different OS topology requires customizing
the stages.

Enterprise roots can wrap this composition in their required template. Compliance
tasks, service connections, provenance attributes and policy-specific conditions
remain the adopter's responsibility.

## 4. Owned stages templates

The PR graph has one `impact` stage containing two parallel jobs,
`compute_linux` and `compute_windows`. They publish OS-family projections as
`anvil-impact-linux` and `anvil-impact-windows`.

Five independent stages depend on successful completion of `impact`:
`pr_fast`, `pr_test`, `pr_msrv`, `pr_runtime_analysis`, `pr_mutants`.
Each has Linux and Windows x86_64 jobs. They download the matching projection and
run a group with `ANVIL_IMPACT=consume`.

Both impact jobs must succeed. A failed impact stage blocks downstream stages
through the normal success condition rather than letting them appear green with
untrusted scope. An empty successful tier does not skip its stage: recipes
interpret the selection and complete normally.

Scheduled stages run the four scheduled groups independently on Linux/Windows
x86_64, without impact downloads and with `ANVIL_IMPACT=off`. ADO's default
templates do not emit ARM jobs.

## Job-wrapper contract

[`steps/job.yml`](../../templates/ado/steps/job.yml) is the small boundary between
Anvil topology and organization-specific job shape:

| Parameter | Meaning |
|---|---|
| `name` | Job name, which repeats across stages |
| `stage` | Exact compile-time stage identity; default empty for callers that omit it |
| `pool` | Pool object |
| `steps` | Anvil's job body as a step list |
| `inputArtifacts` | `{name, path}` entries to make available before the body |
| `artifacts` | `{name, path}` entries to publish after the body |

The default wrapper downloads inputs, includes `before-checks`, inserts the
body, includes `after-checks`, then publishes outputs. It relies on normal ADO
checkout behavior. Publication tasks use `succeededOrFailed()`; hooks do not
automatically inherit that condition, so hook authors must set conditions when
they need to run after failure.

A customized wrapper may translate artifact declarations into enterprise
`templateContext` inputs/outputs instead of the default pipeline-artifact tasks.
It must preserve their ordering and paths. `stage` lets compile-time policy
distinguish, for example, `pr_mutants/linux` from `pr_fast/linux`; runtime
`System.StageName` is not a replacement during template expansion.

The wrapper's body can diverge while stages continue updating. Its **parameter
contract** cannot be ignored: if a stages update passes a new parameter, the
customized wrapper must accept it at the same time. Review the wrapper proposal
alongside such an upgrade, because ADO rejects undeclared parameters before
executing checks.

## Setup and execution

[`steps/setup.yml`](../../templates/ado/steps/setup.yml) assumes the agent already
has Cargo/rustup and PowerShell, bootstraps Just, restores tool caches and calls
the shared setup graph. Its `group` parameter is empty for the full catalog,
`none` for bootstrap/cache only, or a validated concrete group name.

ADO uses `cargo install --locked`, not binstall, for catalog tools. Installed
versions are validated by the same recipes as local/GitHub execution; compiler
and component provisioning remain in the generated setup graph.

Emitted group steps call setup and then `just anvil-<group>`. PR groups consume
impact and scheduled groups force it off. `CARGO_INCREMENTAL=0` matches the
absence of cross-run build-output caching. PR-title acquisition and environment
mapping are injected where needed, not copied into every recipe.

## Impact scoping

[`steps/impact.yml`](../../templates/ado/steps/impact.yml) invokes the same
producer as local execution. The wrapper publishes its cache and downloads it
into `target/anvil/impact/` for group jobs. There is no second set of ADO
stage-output package-list variables to keep synchronized.

The [local protocol](./local.md#4-impact-scoping-via-the-anvil-impact-recipe)
defines base-ref resolution, config identity, dirty-tree widening and projection.
OS-family producers account for different target-conditional dependency graphs.
Missing `.delta.toml` warns and runs with defaults; it does not omit the stage.

A group step invoked directly still sets consume mode. Without its expected
artifact it fails—there is no standalone invocation fallback to full workspace.
Use the producer/download contract, or explicitly invoke a native recipe with
the intended mode instead of assuming the step template computes its own scope.

## 7. Caching

Setup caches Cargo binaries, registry data and installation ledgers, excluding
`target/`. Its key includes OS, architecture, the toolchain fingerprint, Cargo
configuration and generated versions. These caches accelerate source installs;
they are neither build-result caches nor substitutes for version validation.

Impact outputs are pipeline artifacts for the current run, separate from the
installation cache. A customized wrapper must preserve that distinction and
transfer them to the exact path the recipes consume.

## Coverage publication

PR/scheduled test jobs publish `target/coverage/lcov-*.info` from both OS legs
with `PublishCodeCoverageResults@2` and `succeededOrFailed()`. Empty output is
allowed because an empty impact selection can legitimately skip measurement.

This task makes reports visible in ADO; it is not the only enforcing layer.
The shared `llvm-cov` recipe runs `cargo-coverage-gate`, whose failure still fails
the check. Optional publication cannot convert a failed threshold into success.

## Advisory PR comments

The canonical Linux `pr_fast` job uses
[`steps/advisory-comments.yml`](../../templates/ado/steps/advisory-comments.yml)
to inspect explicitly enumerated advisory files. Today that is `semver`.

For a PR build, it finds a nonclosed thread whose first comment carries the
stable `<!-- anvil-semver -->` marker. File presence updates that comment or
creates an active thread. File absence closes the thread; it does not delete it.
The publication step runs after success or failure.

`System.AccessToken` is explicitly mapped into the step environment. The build
identity needs repository permission to contribute to PRs; persisting Git
checkout credentials does not grant that REST permission. Missing token or a
401/403 while listing threads results in a logged skip. Other REST failures,
including later write failures, propagate rather than being universally treated
as advisory.

The recipe's advisory result and the publisher's API result are separate.
Adding another advisory requires updating both the recipe convention and the
publisher's explicit check list.

## Extension choices

Use custom stages for additional repository work, hooks for before/after tasks,
and the job wrapper for enterprise job attributes/artifact translation. Change
root pool parameters for agent selection. Only take ownership of implementation
stages when the topology itself must differ.

These layers reduce unnecessary forks; they do not eliminate integration work.
Template-expansion contracts, OAuth permissions, checkout history, artifact
paths and task conditions must still be tested in the adopter's pipeline.
