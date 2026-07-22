# cargo-anvil container backend

This document defines the optional local container backend emitted by
`cargo-anvil`. The backend runs generated Anvil recipes in a content-addressed
Linux container while preserving native execution as the default.

The intended audience is `cargo-anvil` maintainers and downstream catalog
authors.

## Scope

- Local developer execution only. CI workflows continue to run Anvil recipes
  natively.
- Linux containers only. Drivers request `linux/amd64`; ARM hosts require
  Podman emulation support.
- Explicit and opt-in. Existing `just anvil-*` commands remain native unless a
  user or repository selects the container runner.
- Local image builds only. Remote image publication and registry integration
  are not part of this backend.
- No persisted engine setting. Container selection is runtime policy and does
  not change `.anvil.lock` semantics.

## Generated artifacts

The public catalog emits:

| Path | Purpose |
|---|---|
| `justfiles/anvil/container/container.just` | Public `anvil-container` recipe |
| `justfiles/anvil/container/Containerfile` | Generic Linux image definition |
| `justfiles/anvil/container/container.ignore` | Restricted build context |
| `justfiles/anvil/container/entrypoint.sh` | Non-root runtime initialization |
| `justfiles/anvil/container/image-id.ps1` | Windows image-ID helper |
| `justfiles/anvil/container/image-id.sh` | Unix image-ID helper |
| `justfiles/anvil/container/run-in-container.ps1` | Windows Podman driver |
| `justfiles/anvil/container/run-in-container.sh` | Linux, WSL, and macOS Podman driver |
| `justfiles/anvil/container/README.md` | User instructions and troubleshooting |

The catalog also emits `justfiles/anvil/runner.just` and an `anvil-runner`
region in the repository-root `Justfile` for optional tier routing.

## Execution

Run a specific recipe:

```text
just anvil-container anvil-clippy
```

Run a tier:

```text
just anvil-container anvil-pr
```

Open an interactive shell:

```text
just anvil-container
```

The public recipe selects the PowerShell driver on Windows and the Bash driver
on Unix hosts.

Native tier execution remains the default. Container tier execution can be
selected:

- for one command: `just anvil_runner=container anvil-pr`;
- for the current shell: set `ANVIL_RUNNER=container`;
- for the repository: change the default in the `anvil-runner` region of
  `<repository-root>/Justfile` and commit it.

`ANVIL_RUNNER=native` overrides a repository container default for the current
shell.

Tier recipes route through a tool-owned dispatch recipe. The image sets
`ANVIL_IN_CONTAINER=1`, which forces native execution inside the container and
prevents recursive container launches.

## Image construction and identity

The public `Containerfile`:

1. starts from a pinned public Linux base image;
2. installs `just`, `rustup`, and PowerShell;
3. copies the generated Anvil files and `rust-toolchain.toml`;
4. runs `just anvil-setup`.

The image therefore installs the same pinned tool catalog that the generated
recipes validate and invoke.

The image tag is a SHA-256 hash of build-relevant repository content:

- `rust-toolchain.toml`;
- generated `justfiles/anvil/**/*.just` files;
- the `Containerfile`, entrypoint, ignore file, and other build inputs under
  `justfiles/anvil/container/`;
- optional downstream authentication-hook source.

The public entry recipe, image-ID helpers, runtime drivers, and documentation
are excluded. CRLF and LF input is normalized, and both image-ID helpers use
the same ordinal, deduplicated input order.

A changed input selects a new immutable tag and triggers a local build. Images
for earlier hashes remain available for older branches. The runtime driver uses
`--pull=never` and never substitutes `latest` for the computed tag.

The backend requires a repository-owned `rust-toolchain.toml`; it does not fall
back to a floating toolchain channel.

## Runtime contract

- Podman runs the image with `--platform linux/amd64`.
- The repository is bind-mounted read/write at `/workspace`.
- Cargo registry data and Cargo Git data use shared named volumes.
- `target/` uses a repository- and image-specific named volume mounted over
  `/workspace/target`; container builds do not use the host `target/`.
- Both drivers pass `--userns keep-id` to preserve the invoking user identity.
- The entrypoint creates a writable per-user Cargo home and copies Cargo
  install metadata so `cargo install --list` can find image-installed tools.
- The entrypoint links the shared Cargo registry and Git caches into the
  per-user Cargo home.

## Authentication

### GitHub API

The public `anvil-aprz` recipe and public aggregate tiers that invoke it require
authenticated GitHub API access. The drivers recognize those public recipe
names and obtain a token from host `GITHUB_TOKEN` or an authenticated host `gh`
session. A downstream catalog that adds another token-requiring entry point
must extend that recognition.

For aggregate tiers, the token is mounted read-only for a separate
`anvil-aprz` container invocation. The remaining checks run without the token
mount. The temporary token file is restricted to the current user and removed
on exit.

Interactive runs can pause for `gh auth login` when authentication is missing.
Non-interactive runs fail with an actionable error before building the image.

### Downstream hooks

A downstream catalog may add `auth.sh` and `auth.ps1`. Drivers source the
platform-appropriate hook before build and runtime preparation.

Hooks may configure:

- build arguments and BuildKit secret mounts;
- a short-lived dependency-preparation command;
- runtime arguments;
- cleanup behavior;
- Windows Podman-machine builds when secret paths must be resolved inside the
  machine.

Hook source is included in the image hash because it can define non-secret
build behavior. Runtime token values and temporary secret-file contents are
never hashed or stored in image layers.

## Extensibility

The backend is a normal catalog artifact group. Downstream catalogs can:

- replace the `Containerfile` or entrypoint;
- add authentication hooks and supporting files;
- inherit the public recipe, drivers, image-ID helpers, cache layout, and
  runtime contract;
- remove the container artifact group when the backend is not supported.

See [extensibility.md](./extensibility.md) for the catalog builder API.

## Requirements and controls

Host requirements:

- Podman 4.3 or newer;
- `git` and `just`;
- Bash on Linux, WSL, and macOS;
- PowerShell Core (`pwsh`) on Windows;
- a running Podman machine on Windows and macOS;
- `linux/amd64` execution support.

Runtime controls:

| Variable | Effect |
|---|---|
| `ANVIL_CONTAINER_IMAGE` | Override the local image name; the content hash remains the tag |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail when the matching image is absent |
| `ANVIL_RUNNER` | Select `native` or `container` tier execution |
| `ANVIL_IN_CONTAINER` | Internal recursion guard set by the image |

The first image build installs the pinned tool catalog and can take several
minutes. Later runs with the same image ID reuse the image and target volume.
Cargo registry and Git cache volumes are reused across image IDs.

## Verification

The implementation is covered by:

- catalog tests for artifact registration, platform dispatch, image identity,
  non-root initialization, authentication isolation, and downstream hooks;
- snapshot tests for local, GitHub, and Azure DevOps catalog output;
- schema and Just parsing tests for generated files;
- generator dry-run checks for repository convergence;
- local Windows and WSL Podman smoke tests.

## References

- [Overall cargo-anvil design](./design.md)
- [Local recipe design](./local.md)
- [Catalog extensibility](./extensibility.md)
