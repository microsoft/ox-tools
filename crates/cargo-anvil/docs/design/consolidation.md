# Generated-file and recipe consolidation

> **Status:** Proposed. This document defines the target contract for the
> consolidation work. The other design documents continue to describe the
> implemented layout until this proposal lands.

This design reduces the visible Anvil footprint, moves command policy out of
shell programs, and preserves catalog extensibility when the generated recipe
tree becomes one file. It does not make `cargo-anvil` a runtime dependency and
does not introduce an Anvil-specific runner.

## 1. Goals

1. Put every Anvil-owned file under `.anvil/` unless a consumer requires a
   conventional location.
2. Replace the generated Just tree with one imported `.anvil/anvil.just`.
3. Keep recipe bodies portable: the shell may launch a command, but control
   flow, JSON processing, workspace traversal, and policy do not live in Bash
   or PowerShell.
4. Make the common check recipe one command, allowing a small number of
   sequential commands when the underlying tool has separate phases.
5. Preserve the current setup policy: install the exact catalog version when
   an install is needed, accept an already-installed equal or newer version,
   and retain the caller's `install` versus `binstall` choice.
6. Let a derived catalog add, replace, or remove one recipe without replacing
   the complete generated Justfile and without writing managed-region markers
   between recipes.

## 2. Target repository layout

```text
repo/
├── .anvil/
│   ├── manifest.toml
│   ├── anvil.just
│   ├── config/
│   │   ├── delta.toml
│   │   └── spellcheck.toml
│   ├── container/
│   │   ├── Dockerfile
│   │   ├── Dockerfile.dockerignore
│   │   └── hooks.ps1                    optional repository-owned hook
│   ├── github/
│   │   └── actions/
│   │       ├── setup/
│   │       ├── impact/
│   │       ├── run-group/
│   │       └── report-status/
│   └── ado/
│       ├── pr.yml
│       ├── scheduled.yml
│       ├── custom/
│       └── steps/
├── .github/
│   ├── workflows/                       fixed by GitHub workflow discovery
│   │   ├── anvil-pr.yml
│   │   ├── anvil-pr-impl.yml
│   │   ├── anvil-scheduled.yml
│   │   └── anvil-scheduled-impl.yml
│   ├── instructions/                    fixed by Copilot discovery
│   └── skills/                          fixed by Copilot discovery
├── .pipelines/
│   ├── anvil-pr.yml                     stable ADO registration/trigger stub
│   └── anvil-scheduled.yml
├── Justfile                             managed import region
├── Cargo.toml                           managed lint region
├── <member>/Cargo.toml                  managed lint region
├── deny.toml
├── rustfmt.toml
├── clippy.toml
└── .gitattributes
```

The root `Justfile` imports only `.anvil/anvil.just`. The root file remains
user-composed and keeps the existing managed-region ownership contract.

### 2.1 Files that can move

- The complete `justfiles/anvil/` tree becomes `.anvil/anvil.just`.
- `.anvil.lock` becomes `.anvil/manifest.toml`.
- GitHub local actions move from `.github/actions/anvil-*` to
  `.anvil/github/actions/*`. A local action may live anywhere in a checked-out
  repository.
- ADO implementation, customization, and step templates move from
  `.pipelines/anvil/` to `.anvil/ado/`.
- `.delta.toml` becomes `.anvil/config/delta.toml`; every invocation supplies
  the path explicitly.
- `spellcheck.toml` becomes `.anvil/config/spellcheck.toml`; every invocation
  supplies `--cfg`, and dictionary paths are adjusted relative to the new
  location.

### 2.2 Files that stay outside `.anvil/`

- GitHub only discovers workflow files that are direct children of
  `.github/workflows`. Both event workflows and same-repository reusable
  workflows remain there. Symlinks and nested workflow directories are not a
  supported indirection.
- Copilot instructions and skills remain in their `.github` discovery paths.
- Cargo lint tables remain in root and member manifests; Cargo has no include
  mechanism for them.
- `rustfmt.toml` and `clippy.toml` remain at the repository root so editors,
  `cargo fmt`, and `cargo clippy` discover repository policy without an
  Anvil-specific wrapper.
- `deny.toml` could move by passing `cargo deny --config`, but stays at the root
  so direct invocations continue to discover security policy.
- `.gitattributes` stays where Git discovers it.
- `<path>.anvil-proposed` remains beside the conflicted host or owned file.
- The two ADO root files remain as stable registration and trigger stubs.
  Azure Pipelines can instead be reconfigured to point directly into
  `.anvil/ado/`, but Anvil cannot safely assume that external registration has
  moved.

## 3. Portable recipe contract

Just remains the runtime. Ordinary recipe lines may be launched by the
configured shell, but generated recipes do not depend on shell-specific
variables, arrays, pipelines, condition syntax, JSON parsers, or process
orchestration.

The preferred shapes are:

```just
anvil-audit: anvil-audit-validate-prereqs
    cargo audit

anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    cargo each --package-file target/anvil/impact/affected.packages --once -- \
        cargo clippy {packages} --all-targets --all-features --locked -- -D warnings
```

Static ordering and fan-out stay in Just dependencies. Dynamic package and
target iteration belongs to `cargo-each`; Git and impact analysis belongs to
`cargo-delta`; coverage collection belongs to `cargo-coverage-gate`; and a
domain tool owns authentication or output peculiar to its own service.

The local `anvil-impact` invocation always passes cargo-delta
`--dirty workspace`. This preserves the current safety contract: tracked or
non-ignored untracked work widens all three package files to the complete
workspace instead of failing or silently under-scoping. `ANVIL_IMPACT=consume`
reads an already-produced artifact and does not invoke cargo-delta;
`ANVIL_IMPACT=off` bypasses it and gives checks `--workspace` directly.

No umbrella `cargo-anvil-runner` is introduced. Such a binary would duplicate
the responsibilities of the domain tools, create another bootstrap dependency,
and make generated recipes depend on an Anvil runtime even though
`cargo-anvil` is intentionally needed only when regenerating files.

The container driver is the exception that proves the boundary. Docker/Podman
selection, WSL path translation, linked-worktree mounts, image hashing, and
credential hooks are not workspace-package iteration. They remain contained
under `.anvil/container/` as directly invoked scripts unless the container
contract is simplified enough to express as a few Docker commands. They do not
justify broadening `cargo-each`.

## 4. Setup and prerequisite validation

### 4.1 Installer policy

`installer="install"|"binstall"` remains a caller-selected parameter at every
setup layer:

- `install` is the default and builds exclusively from source with
  `cargo install --locked`. Internal environments use this path because
  downloading community-produced executable artifacts is outside their supply
  chain policy.
- `binstall` is an explicit opt-in for environments that permit prebuilt
  binaries. It passes `--disable-strategies compile` and does not fall back to
  source. If no permitted binary strategy succeeds, setup fails and tells the
  caller to retry with `installer=install`.

Installer choice is not a per-tool constant and is never inferred from the
machine. Group and tier setup recipes forward the selected mode unchanged.

### 4.2 Exact install, minimum acceptance

The installed-version check can use stable, portable Just expressions instead
of a PowerShell implementation. Anvil raises its minimum Just version from
1.46 to 1.47 so the inventory query is lazy:

```just
set lazy

installed_cargo_tools := `cargo install --list`

[private]
[arg("installer", pattern="^(install|binstall)$")]
_install-tool name minimum installer="install" \
    pattern=('(?m)^' + name + ' v([^:\r\n]+):\r?$') \
    found=(if installed_cargo_tools =~ pattern { "true" } else { "false" }) \
    installed=(if found == "true" { replace_regex(installed_cargo_tools, ('(?ms)\A.*^' + name + ' v([^:\r\n]+):\r?$.*\z'), '$1') } else { "" }) \
    satisfied=(if found == "true" { semver_matches(installed, ">=" + minimum) } else { "false" }) \
    source_command=("cargo install --locked --version =" + minimum + " " + name) \
    binary_command=("cargo binstall --no-confirm --locked --disable-strategies compile --version =" + minimum + " " + name):
    @{{ if satisfied == "true" { "" } else if installer == "install" { source_command } else { binary_command } }}
```

The catalog emits the actual recipe with formatting suitable for the generated
file; the example shows the semantic contract. `cargo install --list` is the
only shell-launched probe and contains no shell-specific syntax.

`set lazy` means the inventory is captured only when a setup or validation
recipe references it. Within one Just process all tool checks share that
capture. The decision remains:

| Installed state | Action |
| --- | --- |
| Missing or below the catalog minimum | Install exactly the catalog version using the selected installer. |
| Equal to or newer than the catalog minimum | Do nothing; never downgrade or reinstall. |

The matching validation helper uses the same inventory and
`semver_matches(installed, ">=" + minimum)`, but reports an actionable error
instead of installing.

Installer arguments use `[arg(..., pattern=...)]`, so invalid values are
rejected by Just before a recipe starts. Source-build prerequisites remain
static dependencies of the source path. The binstall setup graph bootstraps
`cargo-binstall`; the source-only graph never touches it. The lack of automatic
fallback is deliberate: installer mode is an environment policy, not an
ordered performance preference, and setup never changes that policy after a
failed strategy.

### 4.3 Toolchains

Pinned nightly toolchains and components remain direct `rustup` invocations.
The root stable fallback is exposed by cargo-each as
`{workspace-rust-version}`:

```just
_anvil-install-workspace-toolchain:
    cargo each --workspace --once -- rustup toolchain install {workspace-rust-version} --profile minimal
```

The placeholder resolves the root `[workspace.package].rust-version`, falling
back to root `[package].rust-version` for a single package. It validates that
every workspace member has a resolved `rust_version` no newer than the root
floor. It is one workspace value; no command-deduplication mode is needed.

This creates one intentional bootstrap step: `cargo-each` is installed first
with the already available Cargo toolchain. It then resolves and installs the
repository-selected stable toolchain, after which remaining Cargo tools are
installed with that selection. `cargo-each` must therefore retain an MSRV low
enough to build with the bootstrap toolchains Anvil supports. A caller-selected
`RUSTUP_TOOLCHAIN` or root toolchain file continues to take precedence and
avoids the root-MSRV fallback.

## 5. Tool responsibilities

### cargo-each

- Read package selections from a generic package file.
- Resolve Cargo package and target metadata.
- Filter packages and targets.
- Expand package, target, and workspace-Rust-version placeholders.
- Spawn commands once, per package, or per target.
- Optionally bound concurrency and enforce per-command timeouts.

It does not inspect Git, understand Anvil impact tiers, install tools, collect
coverage, manage containers, or call cloud APIs.

### cargo-delta

- Resolve an explicit base ref and compute the merge base.
- Snapshot the base and current workspace without mutating the caller's
  checkout.
- Own its snapshot cache and write outputs atomically.
- Emit canonical package identity and modified, affected, and required package
  files directly consumable by cargo-each.
- Detect a dirty working tree and apply the caller-selected conservative policy.

It does not execute checks.

### cargo-coverage-gate

Its orchestration mode owns the coverage-specific sequence: the supported
feature configurations, nextest/cargo-llvm-cov invocation, package opt-outs,
LCOV generation, and Windows response-file handling. Its existing evaluation
mode remains usable with externally produced LCOV files.

### cargo-aprz

When no explicit token or environment token is present, cargo-aprz may ask
`gh auth token` for the configured GitHub host. This removes credential
discovery from the Anvil recipe without making `gh` mandatory.

### Backend wiring

GitHub-specific status and comment behavior stays in GitHub actions. ADO
metadata and comment behavior stays in ADO steps, preferably as checked-in
JavaScript or another backend-native implementation rather than PowerShell
policy embedded in Just. Neither belongs in cargo-each.

## 6. Delimiter-free owned-file sections

The catalog gains a third artifact kind:

```rust
pub struct OwnedFileSectionSpec {
    pub path: &'static str,
    pub id: &'static str,
    pub body: String,
    pub gate: Option<Backend>,
}

pub enum Artifact {
    OwnedFile(OwnedFileSpec),
    OwnedFileSection(OwnedFileSectionSpec),
    Region(RegionSpec),
}
```

An owned-file section is independently addressable in a catalog but does not
have an on-disk delimiter. Identity is `(path, id)`. Catalog order is rendering
order. At plan time, enabled sections with the same path are joined with one
canonical blank-line separator and passed to the existing owned-file planner as
one physical file.

The existing catalog verbs remain uniform:

```rust
.replace_artifact(artifacts::justfile::clippy().with_body(our_clippy))
.without_artifact(artifacts::justfile::bolero())
.with_artifact(Artifact::owned_file_section(
    ".anvil/anvil.just",
    "command:myorg-check",
    our_check,
))
```

The builder rejects:

- duplicate `(path, id)` identities;
- a path claimed by both an `OwnedFile` and any `OwnedFileSection`;
- a section path claimed as a managed-region host;
- inconsistent backend gates among sections composing one file.

No Just parser is required. The catalog already knows each section boundary
before composition, and disk drift remains a whole-file decision.

For `.just` output, each section declares exactly one logical top-level item and
uses a kind-qualified id from the namespace that Just enforces:
`command:<name>` for recipes and aliases, `assignment:<name>` for variables,
and `setting:<name>` for settings such as `set lazy`. Public recipes, private
helpers, assignments, aliases, and settings are separate sections rather than
hidden additional declarations inside another section. A duplicate Just symbol
is therefore a duplicate `(path, id)` and fails during catalog construction
instead of making the complete generated file unparsable later.

A body whose declaration does not match its id, or which declares additional
top-level items, is an invalid catalog, analogous to an owned file whose body is
not valid for its extension. Built-in catalog tests render and parse the
complete Justfile to enforce the convention.

Backend gates apply to the composed physical file, not independently to
arbitrary fragments. Every section for one path must carry the same gate,
including `None`; mixing an unconditional section with a GitHub-only section is
the inconsistent case rejected above. A catalog that needs differently gated
content emits separate physical files.

### 6.1 Ownership trade-off

Derived catalog authors retain per-recipe add/replace/remove operations.
Repository authors do not gain delimiter-free per-recipe merge behavior:
editing one recipe dirties `.anvil/anvil.just` as a whole, and a later template
change produces `.anvil/anvil.just.anvil-proposed` for the whole file.
Repository-specific recipes should remain in the root `Justfile` or another
repository-owned import.

The manifest stores one rendered checksum for `.anvil/anvil.just`; section
identities contribute independently to the catalog checksum but do not require
a manifest schema entry.

## 7. Migration

The first run that carries this design:

1. reads either legacy `.anvil.lock` or `.anvil/manifest.toml`, refusing when
   both contain different ownership state;
2. writes `.anvil/manifest.toml` and retires the legacy path;
3. composes `.anvil/anvil.just`;
4. retires untouched files under `justfiles/anvil/` and keeps edited files as
   ownership-transferred orphans;
5. moves untouched backend/config files and preserves edited old files using
   the normal owned-file rules;
6. leaves stable ADO stubs and unavoidable GitHub/discovery files in place.

Upgrade tests seed the previous manifest and cover untouched, edited, missing,
and already-migrated inputs. A second run must be a no-op. Backend snapshots
must assert that every referenced local file exists at its new path, and
container tests must derive their allowed build-context inputs from the
relocated catalog rather than from a hard-coded file list.

Repositories with edited files under `justfiles/anvil/` require an explicit
one-time reconciliation. The old files are retained as orphans so no
customization is lost, but they are no longer imported by the generated entry
point. Maintainers move still-needed recipes into a repository-owned import or
the root Justfile, preferably under repository-specific names. Moving them into
`.anvil/anvil.just` would immediately dirty the whole generated file and lose
the update isolation the old per-check files provided.
