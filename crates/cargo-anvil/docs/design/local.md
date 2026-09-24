# Local Recipe Surface

This document describes the generated Just interface. Local and cloud runs
invoke the same recipes; cloud backends only provide checkout, caching,
artifacts, and service-specific reporting.

See [checks.md](./checks.md) for the check catalog,
[updates.md](./updates.md) for ownership, and
[consolidation.md](./consolidation.md) for the layout rationale.

## 1. Layout and ownership

```text
repo/
├── Justfile
│   set unstable
│   set windows-shell := ["pwsh.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]
│   # >>> anvil-managed: anvil-imports
│   import '.anvil/anvil.just'
│   # <<< anvil-managed: anvil-imports
│   …repository recipes and imports…
└── .anvil/
    ├── manifest.toml
    ├── anvil.just
    └── container/
        ├── Dockerfile
        ├── Dockerfile.dockerignore
        └── hooks.ps1                  optional and repository-owned
```

`.anvil/anvil.just` is one fully owned file. Its source templates remain split
inside cargo-anvil, and the catalog exposes every top-level recipe as an
independently addressable delimiter-free section. A derived catalog can add,
replace, or remove one recipe. On disk, drift and proposals still apply to the
complete file.

Repository-specific recipes belong outside the managed region in `Justfile` or
in repository-owned imports. Editing `.anvil/anvil.just` is supported by the
ordinary dirty-owned-file flow, but a later catalog change proposes the whole
file.

The two settings are a one-time scaffold only when the root Justfile is absent.
Existing Justfiles keep their repository-owned shell and unstable-feature
settings; cargo-anvil adds only the managed import region.

The container Dockerfile is different: it is a repository-composed host with
ordered managed regions. See [containers.md](./containers.md).

## 2. Recipe layers

### Checks

Every check has:

- `anvil-<check>` to run it;
- `anvil-<check>-setup installer=install|binstall` to provision prerequisites;
- `anvil-<check>-validate-prereqs` to validate without installation.

The common recipe shape is a single general-purpose tool invocation:

```just
anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    cargo each {{ anvil_affected_selection }} --once -- \
        cargo {{ anvil_stable_toolchain_arg }} clippy {packages} \
        --all-targets --all-features --locked -- -D warnings
```

`cargo-each` owns package/target metadata, filtering, placeholders,
concurrency, and timeouts. `cargo-coverage-gate run` owns collection and
evaluation. `cargo-delta` owns Git analysis and impact artifacts. `cargo-aprz`
owns GitHub credential discovery.

Scripts remain only where the domain tool has no equivalent interface: PR-title
policy, advisory comment aggregation, README comparison, spell dictionary
generation, Miri artifact execution, cargo-careful cache repair, mutation-diff
file preparation, Bolero target discovery, and container orchestration. They
are not shared runners and do not parse package selection.

### Groups

Groups are the cloud parallelism boundary:

```text
anvil-pr-fast
anvil-pr-test
anvil-pr-msrv
anvil-pr-runtime-analysis
anvil-pr-mutants
anvil-scheduled-test
anvil-scheduled-advisories
anvil-scheduled-runtime-analysis
anvil-scheduled-exhaustive
```

`anvil-pr-slow` is a local convenience umbrella for the four non-fast PR
groups. Setup and validation recipes exist at group and tier levels and fan out
through dependencies.

### Tiers

```text
anvil-pr
anvil-scheduled
anvil-full
```

`anvil` aliases `anvil-pr`. Scheduled groups execute through the private
`_anvil-unscoped` wrapper so `ANVIL_IMPACT=off` is inherited while their
dependency graph is evaluated.

## 3. Setup and toolchain policy

Generated recipes require Just 1.47 or newer. `set lazy` ensures
`cargo install --list` runs only when setup or validation uses the shared tool
inventory.

Cargo tools follow this contract:

| State | Result |
| --- | --- |
| Missing or below the catalog minimum | Install exactly the catalog version. |
| Equal or newer | Accept without reinstalling or downgrading. |

`installer=install` builds from source with `cargo install --locked`.
`installer=binstall` is an explicit binary-only policy and uses
`--disable-strategies compile`; it never falls back to source.

`cargo-each` is the bootstrap Cargo tool. It installs with the Cargo already
available to the caller. It then resolves `{workspace-rust-version}` and
provisions the stable fallback. `RUSTUP_TOOLCHAIN` or a root
`rust-toolchain[.toml]` continues to take precedence. Pinned nightly
toolchains and components are direct `rustup` invocations.

## 4. Impact scoping

`anvil-impact` invokes cargo-delta managed mode three times, once per tier:

```text
target/anvil/impact/modified.packages
target/anvil/impact/affected.packages
target/anvil/impact/required.packages
```

Each file contains one canonical `name@version` package spec per line. An empty
file is an explicit empty selection, which cargo-each treats as a successful
no-op.

cargo-delta owns merge-base resolution, snapshots, cache invalidation, and
committed/staged/unstaged/deleted/non-ignored-untracked change detection. The
base-ref precedence is `BASE_REF`, ADO target branch, GitHub base branch, then
`origin/main`. A repository whose default is not `main` sets `BASE_REF`.

Scoped checks pass the corresponding package file to cargo-each. No recipe
parses JSON, strips package versions, or synthesizes Cargo argument strings.

`ANVIL_IMPACT` is strict:

- unset: compute under `target/anvil/impact`;
- `consume`: trust package files already produced by another job;
- `off`: use `--workspace` and do not invoke cargo-delta.

In consume mode, `ANVIL_IMPACT_INPUT_DIR` selects an alternate package-file
directory. Missing files fail through cargo-each rather than widening scope.

PR backends compute and upload the directory once per OS, then group jobs use
`ANVIL_IMPACT=consume`. Scheduled jobs use `off` as the full-workspace
backstop.

## 5. Daily use

```text
just anvil-pr-fast
just anvil-pr
just anvil-scheduled
just anvil-full
just anvil-<check>
just anvil-<check>-setup installer=install
```

Developer options such as `anvil-fmt --fix`, `anvil-readme --fix`,
`anvil-doc-build --open`, and `anvil-examples --run` are declared with Just
arguments and validated before execution.

The no-tooling fallback remains ordinary Cargo:

```sh
cargo test --workspace --tests --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --check
```
