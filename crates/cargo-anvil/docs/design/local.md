# Local execution

Generated Just recipes are the common execution layer for developers and cloud
jobs. They remain ordinary committed text: no cargo-anvil process participates
in running a check. This document defines recipe composition, tool resolution
and the impact-cache boundary; [checks](./checks.md) owns individual check
semantics.

## Recipe architecture

The root `Justfile` has a managed import of `justfiles/anvil/mod.just`.
Repository recipes remain outside that region. The `anvil` alias is defined in
[`mod.just`](../../templates/justfiles/anvil/mod.just), not repeated in the root.

| File/tree | Responsibility |
|---|---|
| `mod.just` | Imports and `anvil := anvil-pr` alias |
| `versions.just` | Tool/nightly pins and compiler-selection expressions |
| `tools.just` | Installation, version checks and compiler resolution |
| `helpers.just` | Shared base-ref, unscoped wrapper and platform helpers |
| `impact.just` | Snapshotting, projection, caching and scope consumption |
| `checks/*.just` | One check plus its setup/prerequisite recipes |
| `groups/*.just`, `tiers.just` | Dependency composition |
| `dev/build.just` | Developer build command |
| `container.just` | Optional explicit container driver |

All owned files under `justfiles/` must be `.just` files. The container import is
optional so a catalog can remove that driver without invalidating every other
recipe. Other imports and dependency references must be kept coherent by a
downstream catalog author.

Multi-statement recipes use `[script("pwsh", "-NoProfile")]`, avoiding Bash's
Windows path/shebang incompatibilities. PowerShell 7 (`pwsh`), not Windows
PowerShell 5.1, is the interpreter prerequisite. Native command exit codes are
explicitly propagated. A command pipeline must not replace the tool's result
with the result of its logging command.

Just runs the dependency graph locally on one host; cloud parallelism is across
group jobs, not an alternate local task scheduler. No transparent container
routing exists: use the [container entry point](./containers.md) explicitly.

## Public entry points

`anvil-pr` runs the complete PR tier; `anvil-pr-fast` is a narrower check group.
`anvil-scheduled` and `anvil-full` run unscoped backstops. Each public scheduled
group independently disables impact before entering its dependencies.

Developer commands share implementation with validation where appropriate:

- `anvil-build` supports package-scoped compilation.
- `anvil-check-all-targets` checks affected packages' targets independently with
  default features and with defaults disabled; see its
  [compile-check contract](./checks.md#tests-coverage-and-msrv).
- `anvil-fmt --fix` and `anvil-readme --fix` deliberately modify files.
- `anvil-doc-build --open` opens generated documentation.
- `anvil-examples --run` explicitly runs rather than only compiles examples.
- Miri accepts package/test/example selectors with the constraints in
  [checks](./checks.md#runtime-analysis).

Use `just --list` and `just --usage <recipe>` for the current argument interface.
These optional developer modes do not silently narrow the default CI calls.

## 3. Tool versions and installation

[`tools.just`](../../templates/justfiles/anvil/tools.just) separates two graphs:

- `anvil-<check/group/tier>-setup` installs what that entry point needs.
  `anvil-setup` covers the full catalog plus impact tooling.
- `anvil-<check/group/tier>-validate-prereqs` checks availability/version and
  reports corrective commands without installing. Checks depend on validation.

Setup is idempotent with respect to accepted installed versions. Catalog Cargo
tools are installed at the exact catalog version with `--locked`; an already
installed version at or above that minimum is accepted instead of downgraded.
That is a compatibility policy, not bit-for-bit environment reproducibility.
Dedicated schema-coupled tools such as external-types can still reject an
incompatible newer binary during execution.

Setup accepts `installer=install|binstall`. The default source path uses
`cargo install`. The binstall path prefers prebuilt binaries and can fall back to
source. Tools with source-only prerequisites check those prerequisites
immediately before compilation, including fallback; downloading a working binary
must not require unrelated build dependencies. Cloud defaults are binstall on
GitHub and source installation on ADO.

### Compiler and MSRV resolution

The generated resolver, not an arbitrary machine default, determines
stable-category commands:

- An explicit `RUSTUP_TOOLCHAIN` selects a provisioned compiler; otherwise a
  selecting root `rust-toolchain`/`rust-toolchain.toml` is honored.
- Otherwise the root `[workspace.package]` or `[package]` `rust-version`
  supplies the declared compiler. Missing both selection and root MSRV is an
  error, not an implicit floating stable choice.
- Workspace MSRV validation checks that members resolve a Rust version and do
  not require a newer compiler than that root declaration when this fallback is
  used.
- MSRV tests separately use the declared root MSRV; an explicit toolchain
  selection does not manufacture one. No root MSRV means that check skips.

Internal environments can map the declared MSRV to a provisioned compiler with
`ANVIL_MSRV_TOOLCHAIN`. The mapped compiler must already be available; setup does
not install a public version under that name. Setting this mapping without
`RUSTUP_TOOLCHAIN` or a root toolchain file is rejected because stable checks would
otherwise select an unrelated compiler. Container identity hashes the declared
root MSRV, not this host-specific mapping.

Setup owns compiler/component provisioning. Prerequisite checks avoid rustup
auto-install side effects. Nightly-dependent checks use the pins in
[`versions.just`](../../templates/justfiles/anvil/versions.just), including the
separate rustdoc-schema-compatible external-types pin.

### Caching

Tool cache policy belongs to the [GitHub](./github.md#8-caching) and
[ADO](./ado.md#7-caching) backends. Locally the recipes simply reuse installed
tools and their ordinary Cargo caches; they do not need a cloud cache service.
The impact cache below is distinct from installed-tool caches and build output.

## 4. Impact scoping via the `anvil-impact` recipe

[`impact.just`](../../templates/justfiles/anvil/impact.just) is the only place
that computes and projects cargo-delta impact. Checks depend on `anvil-impact`
and request `modified`, `affected` or `required` through
`_anvil-impact-include`. Cloud producers run the same recipe and transport its
output to consumers.

### Modes and failure behavior

| `ANVIL_IMPACT` | Behavior |
|---|---|
| Unset/empty | Compute or reuse a validated local projection |
| `off` | Do not snapshot or invoke cargo-delta; return full-domain scope |
| `consume` | Read an authoritative existing cache; do not recompute or resolve a base |
| Any other nonempty value | Caller-configuration error, exit 2 |

Consume mode requires all `include_<tier>.txt` files. Missing files fail; there
is **no** automatic full-workspace fallback for a broken artifact download.
`ANVIL_IMPACT_INPUT_DIR` can select another read-only cache in consume mode; it
does not redirect computed output.

The cache's optional `doctest_packages.txt` supplies a capability projection.
Doc-test uses it when present and falls back to one locked Cargo metadata query
for older caches/full-workspace paths that lack it. This fallback does not excuse
missing mandatory include files.

### Producer flow

1. Check mode and working-tree state. Tracked changes and nonignored untracked
   files outside `target/` widen every tier to full workspace. This bypasses
   unnecessary snapshot/base prerequisites during local edits.
2. Resolve the effective `.delta.toml` and base ref. The explicit config path is
   supplied to both snapshots and impact computation. Missing `.delta.toml`
   warns and uses cargo-delta defaults; it does not disable the impact job.
3. Reuse or create the base snapshot in a temporary Git worktree, and snapshot
   the current clean tree. Baseline checkout avoids LFS payload hydration.
4. Compute impact, map package identities through Cargo metadata, and write
   deterministic projection files under `target/anvil/impact/`.

The base is chosen by `_anvil-base-ref` in
[`helpers.just`](../../templates/justfiles/anvil/helpers.just): `BASE_REF`,
then ADO's `SYSTEM_PULLREQUEST_TARGETBRANCH`, then `GITHUB_BASE_REF`, then
`origin/main` or `origin/master`. ADO's `refs/heads/` prefix is stripped before
forming a remote-tracking ref. GitHub supplies the event's base SHA.

The recipe does not fetch missing history. An unavailable base or unsuitable
shallow history fails with recovery guidance. A valid baseline from before the
repository had its workspace instead widens conservatively to full workspace.
These are different states; operational failure must not resemble “nothing
changed.”

### Cache identity

Snapshots under `target/anvil/impact/snapshots/` have separate keys:

- Baseline: base commit SHA plus effective delta-config identity, including
  absence of the config.
- Current: HEAD SHA. The dirty-tree guard ensures a computed current snapshot
  corresponds to that commit.

The projection is reused only with matching inputs. Config changes invalidate
the baseline even if the base SHA is unchanged; otherwise two snapshots could
describe different parsing/exclusion rules. Consumers deliberately trust the
transferred projection rather than re-running producer checks.

Cloud producers compute separate Linux and Windows projections. This reflects
OS-conditional dependency graphs; ARM64 jobs reuse the corresponding family
cache. It is not a per-architecture proof of dependency equivalence.

### Projection contract

`affected` and `required` yield deterministic version-qualified
`--package name@version` pairs or `--skip`. Unknown/ambiguous metadata mappings
fail. `modified` is only a skip gate: when nonempty, formatting and similar tools
retain their native whole-domain command.

Every consumer captures the helper's exit status before inspecting its text.
Failed computation must not become an empty argument list and accidentally run
or skip a different scope. Per-check capability filtering, such as doctest or
library-only selection, happens after this projection.

The [mapping table](./checks.md#5-impact-scoping-check--include-mapping) records
which checks consume which tier. Repository-wide README/spelling and dependency
policy checks remain unscoped where package projection would omit shared inputs.

## Configuration and ownership

Generated recipes use the consuming tools' normal config files and Cargo
metadata rather than a parallel Anvil settings schema. Relevant boundaries
include:

- Lint namespaces and member inheritance in Cargo manifests.
- Root rustfmt, Clippy, deny, spellcheck and delta configuration, with
  repository additions outside managed regions.
- Coverage-gate package metadata for thresholds; an explicit zero threshold
  changes measurement routing for scoped tests, not whether tests run.
- Miri's package opt-out metadata and `ANVIL_MIRI_JOBS`.
- Target declarations for Loom; merely naming a file `loom.rs` is not enough.

TOML placement matters: settings below a closing marker still belong to the last
table until another header changes the context. Do not duplicate managed keys or
table headers to “override” them; use a downstream catalog replacement when the
generated policy itself must change.

## 9. Platform-specific mutation configuration

Both diff and full mutation recipes select `.cargo/mutants.<os>.toml` when it
exists, using Just's native `os()` value (for example `linux` or `windows`).
Otherwise they let cargo-mutants use its default configuration.

The platform file is a complete replacement, not a merged overlay. It must carry
all settings needed on that OS. An unreadable or invalid selected file fails;
falling back would silently run a different mutation policy. `just` is the
source of OS identity, so container execution naturally selects Linux policy
rather than the host's Windows policy.

## Extension and failure boundaries

Keep repository recipes outside the managed import and custom `.just` files
outside the generated tree where practical. Customize an owned recipe only when
necessary; it then uses the [proposal update protocol](./updates.md#2-owned-files).
A shared organization policy should instead use a composed catalog.

Direct Cargo commands can provide basic fallback diagnostics when Just or its
tools are unavailable, but they are not equivalent validation: plain stable
`cargo fmt --check`, for example, does not implement Anvil's pinned-nightly
workspace-member formatting contract. Fix missing prerequisites rather than
treating a smaller fallback suite as the full PR result.
