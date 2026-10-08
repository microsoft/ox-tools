# cargo-anvil design

`cargo-anvil` generates and maintains a Rust repository's local checks and cloud
workflow wiring. Generated files are committed with the repository; running
checks does not require the generator. The design separates **what policy is
generated**, **how updates preserve ownership**, and **where checks execute**.

This is the architecture map, not an installation guide or a second template
catalog. For commands, see the [crate documentation](../../README.md).

| Design | Owns |
|---|---|
| [Updates](./updates.md) | Ownership, adoption, checksums, refusals and retirement |
| [Extensibility](./extensibility.md) | Catalog composition and downstream distributions |
| [Checks](./checks.md) | Group boundaries, check semantics and impact selection |
| [Local execution](./local.md) | Just recipes, tool setup and impact-cache protocol |
| [GitHub](./github.md) | Actions topology, permissions and reporting |
| [Azure DevOps](./ado.md) | Pipeline topology and enterprise extension boundary |
| [Containers](./containers.md) | Explicit container execution and image identity |

## Purpose and non-goals

The default catalog provides a coherent engineering baseline: formatting,
linting, documentation, dependency policy, tests, coverage, MSRV validation,
runtime analysis and mutation testing. Repositories should share that policy
without maintaining parallel local and cloud implementations.

The generator is not a build runner, an installer invoked by every build, a
general configuration merger, or a compliance system. It does not infer whether
arbitrary repository-specific code is safe. Generated setup and check recipes
install tools and execute repository code; container hooks execute on the host.
Those are execution-time trust boundaries, not generator guarantees.

## Architecture and update flow

The implementation has two layers:

- The engine in [run.rs](../../src/run.rs), [catalog](../../src/catalog),
  [emit](../../src/emit), [plan.rs](../../src/plan.rs) and
  [manifest.rs](../../src/manifest.rs) discovers the workspace, plans ownership
  changes, applies them and records provenance.
- The built-in policy in [anvil](../../src/anvil) assembles artifacts from
  [templates](../../templates). An organization's distribution can compose a
  different catalog without implementing another updater.

A run:

1. Finds the Cargo workspace root and loads `.anvil.lock`.
2. Enforces the single-tool guard before parsing workspace members.
3. Loads the workspace and resolves the selected cloud backends.
4. Expands catalog host selectors and plans artifacts in catalog order.
   Regions sharing a host use accumulated in-memory text, not independent
   copies of the original file.
5. Plans retirement of previously tracked artifacts no longer selected.
6. Projects the next manifest, including tool identity, tool version and catalog
   checksum. Dry-run reports this plan without writing; a normal run applies
   files and then saves the manifest.

This is not a repository-wide transaction. Unsafe region operations are normally
refused individually while safe operations remain eligible to apply. Fatal
discovery, catalog or I/O failures still abort. The [update protocol](./updates.md)
defines the exact outcomes and exit behavior.

## CLI and backend selection

There is one update operation: `cargo anvil`, not an `update` subcommand.
`--dry-run` checks drift; `--force` permits switching the tool recorded in the
lock and does **not** bypass content protection.

Explicit repeated `--backend github` / `--backend ado` flags select cloud output.
`--no-backends` selects local-only output. Otherwise
[backend resolution](../../src/backend.rs) inspects `origin`; a missing or
unrecognized remote is an error with explicit selection as the escape hatch.
Local recipes and shared configuration are independent of backend selection.
Deselected cloud files follow retirement rules, rather than being left behind
unconditionally.

## Ownership model

There are two artifact kinds, not a single “generated file” policy:

- **Owned file:** the whole file is compared with the previous and current
  template checksums. Pristine files update automatically. Customized files are
  preserved; a changed template is offered as a `.anvil-proposed` sibling.
- **Managed region:** only the sentinel-delimited body belongs to the catalog.
  Nonempty edits inside it refuse reconciliation; an empty body requests
  regeneration. Repository settings belong outside the sentinels.

`.anvil.lock` is the checksum/provenance store. Generated banners are explanatory,
not in-file checksum records. Exact template matches can be adopted even without
previous tracking; conflicting existing content is not blindly replaced.

TOML regions need semantic handling as well as byte boundaries. Ordinary regions
can adopt compatible hand-written tables without discarding repository-only
settings. Array-entry regions own selected entries, not the array assignment or
its brackets. Both use parser validation and explicit refusals when ownership
cannot be separated safely. See [TOML array entries](./updates.md#toml-array-entries).

## 6. Repo layout

| Generated location | Ownership and purpose |
|---|---|
| `.anvil.lock` | Engine state; commit alongside generated artifacts |
| `justfiles/anvil/` | Owned `.just` files: entry point, helpers, tools, checks, groups and tiers |
| Root `Justfile` | Managed import region; repository recipes remain outside it |
| Root/member `Cargo.toml` | Managed lint regions selected for workspace or single-crate shape |
| `deny.toml`, `rustfmt.toml`, `clippy.toml`, `spellcheck.toml`, `.delta.toml`, `.gitattributes` | Shared hosts with managed regions |
| `.github/workflows/`, `.github/actions/` | GitHub-gated owned workflow/action files |
| `.pipelines/` | ADO-gated roots, implementation templates and customization stubs |
| `.anvil/container/Dockerfile` | Ordered managed regions with repository-owned gaps |
| `.anvil/container/Dockerfile.dockerignore` | Owned build-context filter |

The [artifact registry](../../src/anvil/artifacts/mod.rs) is the complete inventory,
including generated agent instructions and adoption/review skills. The table
above describes ownership boundaries rather than mirroring every registry entry.

Paths normally follow existing on-disk casing. Ambiguous case-insensitive matches
refuse resolution. The composed Dockerfile is an exception: its exact spelling is
required to pair with `Dockerfile.dockerignore`.

## 7. Customization

Prefer the narrowest boundary that expresses the change:

1. Put repository recipes and nonconflicting configuration outside managed
   regions.
2. Change documented workflow inputs or customization stubs, such as runner
   labels, ADO pools/hooks or the ADO job wrapper.
3. Take ownership of an owned file when its behavior really must diverge.
   Reconcile future `.anvil-proposed` files intentionally.
4. Compose a downstream catalog when policy should be shared across repositories.
   Replace artifacts by identity, add artifacts, or remove them using the
   [catalog API](./extensibility.md).

Editing a managed region is not another customization tier: the next run refuses
that region. Removing an owned file recreates it while selected; emptying it
preserves a local opt-out under owned-file rules. Emptying a region instead asks
the generator to restore it.

## Execution boundaries

Checks live in generated Just recipes. Cloud backends arrange checkout, setup,
impact transfer, matrices, credentials and reporting around those recipes. Local
and cloud runs share implementations, but are not bit-identical executions:
local runs use one host, cloud runs fan out, scheduled runs disable impact, and
developer recipes expose explicit narrowing/fix options.

PR validation is impact-scoped; scheduled validation is a full-workspace
backstop, also covering expensive or externally changing checks. A broken impact
computation fails rather than silently skipping the dependent groups. An
intentionally empty tier skips at the recipe boundary, so cloud jobs still have
terminal results.

### Tool policy

Tool versions and nightly pins live in generated
[`versions.just`](../../templates/justfiles/anvil/versions.just). Installation
uses catalog pins, while version checks generally accept an already installed
compatible newer version. Setup and validation are separate: running a check does
not silently install its missing tools. GitHub can use `cargo-binstall`; ADO uses
source installation. [Local execution](./local.md) defines this contract.

### 8.3 Cross-OS test matrices

GitHub's default PR groups cover Linux and Windows on x86_64 and ARM64. Scheduled
exhaustive checks use x86_64 Linux/Windows; the other scheduled groups also cover
ARM64. ADO uses x86_64 Linux/Windows. These are backend topology decisions, not
per-recipe duplicate implementations. Unsupported checks can explicitly skip
within a host job; for example, Bolero is Linux-only.

Runner labels/pools are configurable; changing the OS-axis shape requires
customizing implementation workflows/stages. macOS is not a default CI leg.
Impact is computed once per OS family, with ARM64 jobs reusing their OS-family
projection. Architecture-specific dependency differences are consequently a
limitation of the default impact topology.

### Caches and reproducibility

Caches accelerate installation and impact analysis; they do not establish
ownership or substitute for validation. Cloud tool caches exclude `target/`,
and scheduled checks explicitly ignore impact caches. Container image tags
identify their declared source inputs, not a hermetic build or a cryptographic
attestation. Tool downloads, registries and executing repository code remain
part of the trust model.

## Maintaining the design

Keep durable contracts here and executable details in source/templates. Changes
to identity, ownership, impact modes, required-check names, wrapper parameters or
secret handling need corresponding design updates. A template version bump does
not require reproducing the new pin in these documents.
