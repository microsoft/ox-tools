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
    ├── anvil.just                 small import hub and aliases
    ├── checks.just                check, group, tier, impact, and developer recipes
    ├── setup.just                 pins and setup/validation recipes
    ├── container.just             container recipes and private helpers
    └── container/
        ├── Dockerfile
        ├── Dockerfile.dockerignore
        └── hooks.ps1              optional and repository-owned
```

The three generated recipe files are fully owned physical files. Source
templates remain split inside cargo-anvil for maintainability, and the catalog
exposes every top-level recipe as an independently addressable delimiter-free
section. A derived catalog can add, replace, or remove one recipe while the
engine composes sections into the appropriate physical file.

Repository-specific recipes belong outside the managed region in `Justfile` or
in repository-owned imports. Editing a generated `.just` file is supported by
the ordinary dirty-owned-file flow, but a later catalog change proposes that
complete physical file.

The two root settings are a one-time scaffold only when `Justfile` is absent.
Existing Justfiles keep their repository-owned settings; cargo-anvil adds only
the managed import region.

## 2. Recipe layers

### Checks

Every check has:

- `anvil-<check>` to run it;
- `anvil-<check>-setup installer=install|binstall` to provision prerequisites;
- `anvil-<check>-validate-prereqs` to validate without installation.

The common shape is a direct general-purpose tool invocation:

```just
anvil-clippy: anvil-clippy-validate-prereqs anvil-impact
    cargo each {{ anvil_affected_selection }} --once -- \
        cargo {{ anvil_stable_toolchain_arg }} clippy '{packages}' \
        --all-targets --all-features --locked -- -D warnings
```

`cargo-each` owns package/target metadata, filtering, placeholders,
concurrency, and timeouts. `cargo-coverage-gate run` owns collection and
evaluation. `cargo-delta` owns Git analysis and impact artifacts. `cargo-aprz`
owns GitHub credential discovery.

Scripts remain only where the domain tool has no equivalent interface:
README comparison, spell dictionary generation, Miri profile environment,
mutation-diff preparation, and container orchestration. Cargo-careful uses an
identity-keyed target directory; Bolero discovery uses a JSONL file consumed by
cargo-each; PR-title policy is a Just regex; and the MSRV path uses cargo-each's
optional workspace-version contract. The remaining scripts are not shared Anvil
runners and do not own generic package iteration.

### Groups and tiers

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

`anvil-pr-slow` is a local convenience umbrella. `anvil`, `anvil-pr`,
`anvil-scheduled`, and `anvil-full` are the tier entry points. Scheduled groups
execute through `_anvil-unscoped`, which starts a child Just process with the
internal impact override set to `off` while its dependency graph is evaluated.

## 3. Setup and toolchain policy

Generated recipes require Just 1.47 or newer. `set lazy` ensures
`cargo install --list` runs only when setup or validation uses the shared tool
inventory.

| Installed state | Result |
| --- | --- |
| Missing or below catalog minimum | Install exactly the catalog version. |
| Equal or newer | Accept without reinstalling or downgrading. |

`installer=install` builds from source with `cargo install --locked`.
`installer=binstall` prefers a prebuilt artifact through `cargo binstall` and
allows cargo-binstall's compile strategy when no binary source has the pinned
release. This keeps newly published tools installable before binary indexes
catch up. The selected installer is forwarded unchanged through group, tier,
stable-toolchain, and cargo-each bootstrap setup.

`cargo-each` is the bootstrap Cargo tool. It installs with Cargo already
available to the caller, then resolves `{workspace-rust-version}` for the
stable fallback. `RUSTUP_TOOLCHAIN` or root `rust-toolchain[.toml]` takes
precedence. Pinned nightly toolchains and components are direct `rustup`
invocations. An absent root declaration is an empty optional value: dedicated
MSRV recipes are cargo-each `--none` no-ops, while ordinary stable commands use
Cargo's default toolchain selection.

## 4. Impact scoping

`anvil-impact` asks cargo-delta to write:

```text
target/anvil/impact/modified.packages
target/anvil/impact/affected.packages
target/anvil/impact/required.packages
```

Each file contains one canonical `name@version` package spec per line. An empty
file is an explicit successful no-op in cargo-each. cargo-delta owns merge-base
resolution, snapshots, cache invalidation, and committed/staged/unstaged/
deleted/non-ignored-untracked change detection.

Base-ref precedence is `BASE_REF`, ADO target branch, GitHub base branch, then
`origin/main`. Repositories whose default is not `main` set `BASE_REF`.

`ANVIL_IMPACT` is strict:

- unset: compute under `target/anvil/impact`;
- `consume`: trust package files produced by another job;
- `off`: select `--workspace` and do not invoke cargo-delta.

In consume mode, `ANVIL_IMPACT_INPUT_DIR` selects an alternate package-file
directory. Missing files fail closed through cargo-each. PR backends upload and
consume this directory; scheduled jobs use `off` as the full-workspace
backstop.

## 5. Platform-specific cargo-mutants configuration

Cargo-mutants does not evaluate platform cfgs while discovering mutants, so a
Linux run can otherwise report a Windows-only mutation as missed. Anvil selects
an optional native cargo-mutants configuration:

| Host | Preferred file |
| --- | --- |
| Linux | `.cargo/mutants.linux.toml` |
| Windows | `.cargo/mutants.windows.toml` |
| Other | `.cargo/mutants.<just-os-name>.toml` |

When present, both diff and full mutation recipes pass the file through
cargo-mutants' native `--config` option. Platform files replace cargo-mutants'
default configuration; they do not overlay it. Repositories duplicate shared
policy intentionally rather than introducing an Anvil-specific merger.

## 6. Remaining shell-backed check boundaries

### Bolero

For each affected package the recipe runs
`cargo bolero list --profile release --package <name>` and redirects the
combined stdout to `target/bolero-list-<just-pid>.jsonl`. The private
`_ensure-target-dir` recipe runs after `anvil-impact` and creates `target/` only
when it is still absent, so clean `ANVIL_IMPACT=off` checkouts work without
shell-specific directory flags. The Just process ID isolates concurrent local
runs. Pinned cargo-bolero 0.13.4 emits one
`{"package":"…","test":"…"}` record per target on stdout; Cargo build output and
diagnostics use stderr. A second direct invocation passes that file to
`cargo each --json-lines-file`, which expands `{json:package}` and `{json:test}`
into bounded `cargo bolero test --profile release --engine libfuzzer -T 60s`
commands. `--keep-going` preserves aggregate failure behavior, and an empty file
is cargo-each's successful no-op. A successful run removes the JSONL file.
Failed discovery or execution leaves the PID-scoped file in place for
diagnostics; the next Just process uses a different name.

### Spell dictionary preparation

The recipe converts the repository-owned `.spelling` word list into Hunspell
`.dic` format: sort lines, remove empty and numeric-only entries, prepend the
word count, create the output directory, and write deterministic text. This is
a good boundary for a small standalone tool with explicit input/output paths,
atomic output, and tests for encoding, ordering, filtering, duplicates, and
empty/missing inputs.

### cargo-careful artifact isolation

cargo-careful builds a custom standard library into a stable cache path. Cargo
fingerprints the `--sysroot` path but not the contents behind that path. When
the pinned nightly or cargo-careful executable changes, cargo-careful replaces
the sysroot in place while workspace artifacts can remain apparently fresh;
the next build may then fail with rustc metadata-version mismatches.

The recipe hashes the complete pinned-nightly `rustc -vV` output together with
the SHA-256 of the resolved cargo-careful executable, using Just's `which`,
`sha256_file`, and `sha256` functions. It forwards
`--target-dir target/anvil/careful/<identity>` through cargo-careful to its final
Cargo invocation. A compiler or tool change therefore gets a fresh artifact
directory instead of reusing binaries built against the previous contents of
the stable sysroot path. No marker file, conditional `cargo clean`, or
PowerShell state machine is required. Old identity directories remain ordinary
Cargo cache entries under `target/` and are removed by normal target cleanup.

## 7. Daily use

```text
just anvil-pr-fast
just anvil-pr
just anvil-scheduled
just anvil-full
just anvil-<check>
just anvil-<check>-setup install
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
