# Run Anvil checks in a local container

Use `just anvil-container` to run generated Anvil checks in a reproducible
Linux environment without installing the complete Rust and Cargo tool catalog
on the host.

Native execution remains the default. The first container run builds an image
matching the repository's generated configuration. Later runs reuse that image,
dependency caches, and compilation output.

## Quick start

Ensure Docker Engine is running, then run:

```text
just anvil-container anvil-clippy
```

The first run builds the matching image and can take several minutes.

## Prerequisites

- [Docker Engine](https://docs.docker.com/engine/install/) 23.0 or newer,
  installed directly in Linux or WSL and usable by the current user.
- `git` and `just` on the host.
- Bash on Linux and WSL; PowerShell Core (`pwsh`) and WSL 2 on Windows.
- `[script]` support enabled in the root `Justfile`. Add `set unstable` when
  required by the installed `just` version.
- A `rust-toolchain.toml` in the repository root.
- An x86-64 Linux or WSL environment capable of running `linux/amd64` images.

On Windows, install Docker Engine inside the default WSL distribution rather
than relying on Docker Desktop. Verify the engine from PowerShell:

```text
wsl -e docker version
```

The Windows driver invokes that WSL Docker Engine directly. Start the Docker
service inside WSL when it is stopped and add the WSL user to the `docker`
group when non-root access is not already configured. Docker Desktop is not
required.

## Common workflows

Run one check:

```text
just anvil-container anvil-clippy
```

Run the complete pull-request tier:

```text
just anvil-container anvil-pr
```

Open an interactive Bash shell in the image:

```text
just anvil-container
```

### Use containers for tier commands

Native execution remains the default. To route tier commands such as
`just anvil-pr` through the container for the current shell:

```powershell
$env:ANVIL_RUNNER = "container"
just anvil-pr
```

On Unix:

```sh
ANVIL_RUNNER=container just anvil-pr
```

For one invocation:

```text
just anvil_runner=container anvil-pr
```

To make container execution the repository default, change the default value
in the `anvil-runner` region of the repository-root `Justfile` from `"native"`
to `"container"` and commit that policy. Set `ANVIL_RUNNER=native` to override
the repository default for the current shell.

## Images and caches

The image name includes a content-based tag derived from the repository's Rust
toolchain, generated Anvil recipes, and container build configuration. A
relevant change selects a new image automatically; older branches can continue
using their matching images.

The following data is reused between runs:

- the matching container image;
- Cargo registry and Cargo Git caches;
- compilation output in a repository- and image-specific `target` volume.

The repository is mounted read/write at `/workspace`. Build output remains in a
named volume instead of the host `target/`, avoiding incompatible artifacts and
slow host-to-virtual-machine I/O.

## GitHub authentication

`anvil-aprz` and aggregate tiers that include it require GitHub API
authentication. The driver uses either:

- the host `GITHUB_TOKEN`; or
- the token from an authenticated host `gh` session.

Authenticate the GitHub CLI with:

```text
gh auth login --hostname github.com
```

For an aggregate tier, the driver first runs `anvil-aprz` in a short-lived
container with the token mounted read-only. After it succeeds, the driver runs
the remaining checks in another container without the token. Temporary token
files are removed afterward.

An interactive invocation can pause while you authenticate. A non-interactive
invocation fails with instructions when authentication is unavailable.

## Configuration

| Variable | Effect |
|---|---|
| `ANVIL_RUNNER` | Selects `native` or `container` execution for tier commands |
| `ANVIL_CONTAINER_IMAGE` | Changes the local image name; the content-based tag is retained |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fails instead of building when the matching image is absent |

The public driver builds images locally and does not pull
`ANVIL_CONTAINER_IMAGE` from a registry.

## Troubleshooting

| Problem | Resolution |
|---|---|
| Docker is not found on Linux or WSL | Install Docker Engine 23.0 or newer inside that environment |
| Docker is unavailable from Windows | Run `wsl -e docker version`; install or start Docker Engine in the default WSL distribution |
| Docker requires elevated access | Add the Linux/WSL user to the `docker` group, then start a new shell |
| `linux/amd64` cannot run | Use an x86-64 Linux or WSL environment |
| `[script]` recipes are unavailable | Enable `[script]` support; older `just` versions require `set unstable` |
| `rust-toolchain.toml` is missing | Add the repository-owned toolchain file at the repository root |
| GitHub authentication is unavailable | Run `gh auth login --hostname github.com` or set host `GITHUB_TOKEN` |
| A matching image is missing with `ANVIL_CONTAINER_NO_REBUILD=1` | Unset the variable to allow the local image build |
| The first run is slow | The initial image build installs the pinned tool catalog; later runs reuse it |

Use `docker images anvil-dev` inside Linux or WSL to list locally cached
default Anvil images.

## Managed files

This directory is managed by `cargo-anvil`. Regenerate it with `cargo anvil`
instead of editing its files directly.

## Advanced repository customization

A repository or derived catalog can add one trusted customization file per
supported host:

```text
justfiles/anvil/container/customize.sh
justfiles/anvil/container/customize.ps1
```

The driver sources the matching file as host code before image construction and
recipe execution. Customization API version 1 provides documented inputs and
validated outputs for build secrets, dependency preparation, runtime arguments,
and cleanup.

Customization source is excluded from image identity and the build context.
Non-secret image behavior must be represented by hashed static files such as
the `Containerfile`, entrypoint, or supporting build scripts.

Only run customization files from a repository or catalog you trust. See the
[container customization contract](https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/containers.md#8-container-customization)
for the complete versioned interface and security requirements.
