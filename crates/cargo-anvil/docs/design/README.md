# cargo-anvil design

`cargo-anvil` generates a shared Rust check catalog for local development,
GitHub Actions, and Azure DevOps. Repositories commit the generated files;
developers and CI execute them with `just`, without running the generator.
This keeps check behavior shared while each repository controls its triggers,
credentials, runner pools, and additional checks.

## Requirements and limitations

- **Adopter toolchains must use Rust and Cargo 1.95 or newer**, including the
  declared MSRV. Older versions are outside the supported contract.
- **Building this checkout of cargo-anvil requires Rust 1.97 or newer**, as
  declared in the workspace `Cargo.toml`. This is separate from the minimum
  supported toolchain in an adopting repository.
- Generated recipes require **Just 1.46.0 or newer**, PowerShell Core (`pwsh`),
  Git, rustup, and the prerequisites of the selected checks. `just anvil-setup`
  installs catalog tools and public toolchains; it does not provision private
  toolchains, credentials, compliance infrastructure, or arbitrary native
  libraries.
- The generated recipes target one Cargo workspace or single-crate package at
  the repository root. They do not orchestrate multiple independent workspaces.
- Supported CI backends are GitHub Actions and Azure DevOps Pipelines.
  macOS is not part of the default CI matrix. See the backend documents for
  runner and group restrictions.
- Containers provide a Linux environment, not isolation from untrusted
  repository code. Docker is supported; Podman is best-effort.
- One anvil-family tool owns a repository at a time. Catalogs are compiled
  into that tool; there are no runtime plugins.
- Updates preserve repository edits but are not repository-wide transactions.
  A refusal can coexist with independent safe updates. Review the summary and
  working-tree diff before committing.

## Adoption and updates

Install the generator and run it from the repository root:

```sh
cargo install --locked cargo-anvil
cargo anvil
```

By default, Anvil selects the CI backend from the `origin` remote. Explicit
selection supports one backend, both, or local recipes only:

```sh
cargo anvil --backend github
cargo anvil --backend ado
cargo anvil --backend github --backend ado
cargo anvil --no-backends
```

The same command performs initial adoption and subsequent updates; there is no
`init` or `update` subcommand. Commit the generated files and `.anvil.lock`.

`--dry-run` plans without writing. Exit 0 means no pending changes or refusals;
exit 1 means drift, a refusal, or an error. `--version` reports the binary
version and catalog checksum. `--force` permits a deliberate switch from
another anvil-family tool; it does **not** override content conflicts.

## Running checks

```sh
just anvil-pr          # complete pull-request tier; also available as `just anvil`
just anvil-pr-fast     # partial validation, not a substitute for the complete tier
just anvil-scheduled  # full-workspace backstops and expensive checks
just anvil-full       # both tiers
just --list           # individual checks and groups
```

Local and cloud runs use the same check recipes. CI adds matrices, artifacts,
permissions, and reporting; it does not reproduce the checks in workflow code.
The [check catalog](checks.md) defines each group's scope and verdict.

Ordinary checks select the caller's `RUSTUP_TOOLCHAIN`, then either root
toolchain-file spelling, then the root manifest's `rust-version`. Without any
selection they fail rather than use an ambient compiler. Nightly checks use
catalog pins; the MSRV group separately tests the declared minimum.

PR checks use committed-change impact analysis. A dirty local tree widens to
the whole workspace; scheduled checks run unscoped. CI computes impact per OS
family, with ARM jobs sharing their OS family's result rather than computing
an architecture-specific dependency graph.

Plain `cargo build`, `cargo test`, and other Cargo commands remain available.
They do not reproduce the full Anvil policy.

## Ownership and customization

`.anvil.lock` records generated content checksums. There are two update models:

- **Owned files:** clean files update automatically. Edited files are preserved;
  changed catalog content is offered in a `.anvil-proposed` sibling.
- **Managed regions:** marked blocks belong to the catalog. Nonempty edits are
  refused until reconciled; emptying a body requests regeneration, not opt-out.
  Repository settings belong outside the markers.

Deleting a live generated artifact requests regeneration. Removing an artifact
from the catalog or deselecting a backend retires it: clean content is removed,
while customized content is protected. See [updates](updates.md) before
reconciling conflicts or removing a backend.

For repository-specific policy, use the underlying tool's configuration or
compose additional recipes and workflows. Add lint keys immediately after the
matching namespace's closing marker, without repeating its table header or
managed keys. Anvil does not own `.cargo/config.toml` or root toolchain files.

Generated files live under `justfiles/anvil/`, `.anvil/container/`, and the
selected backend directories. Shared files such as `Justfile`, `Cargo.toml`,
`deny.toml`, and formatting/spelling configuration contain managed regions.
Agent guidance and the adoption skill are emitted for every backend selection.

## Contract reference

- [Checks](checks.md): groups, feature configurations, scope, and check results.
- [Local execution](local.md): setup, toolchains, recipe options, and impact analysis.
- [Updates](updates.md): adoption, preservation, refusals, proposals, and retirement.
- [GitHub Actions](github.md): workflow inputs, matrices, permissions, and reporting.
- [Azure DevOps](ado.md): pipeline composition, pools, hooks, and compliance integration.
- [Containers](containers.md): image inputs, credentials, execution, and restrictions.
- [Extensibility](extensibility.md): downstream catalogs and in-process updates.
- [Shared arrays](array-regions.md): required TOML and JSON string entries.
- [Verification boundaries](../verification.md): what the repository's checks establish.

Contributor instructions live in the crate's [AGENTS.md](../../AGENTS.md).
