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

On Windows, validate the host without changing it:

```powershell
pwsh -NoProfile -File .anvil/container/setup-docker-in-wsl.ps1 -Doctor
```

If validation reports missing prerequisites, run the idempotent bootstrap:

```powershell
pwsh -NoProfile -File .anvil/container/setup-docker-in-wsl.ps1
```

The direct commands work immediately after `cargo anvil` generates this
directory, before the `just anvil-container*` recipes are used. The equivalent
convenience recipes are `just anvil-container-doctor` and
`just anvil-container-bootstrap`.

## Prerequisites

- [Docker Engine](https://docs.docker.com/engine/install/) 23.0 or newer,
  installed directly in Linux or WSL and usable by the current user.
- `git` and `just` on the host.
- Bash on Linux and WSL; PowerShell Core (`pwsh`) and WSL 2 on Windows.
- `[script]` support enabled in the root `Justfile`. Add `set unstable` when
  required by the installed `just` version.
- A `rust-toolchain.toml` in the repository root.
- A Linux or WSL environment capable of running `linux/amd64` images, either
  natively on x86-64 or through Docker emulation on ARM64.

The supported automatic Windows path is:

- Windows 11 build 22000 or newer;
- Microsoft Store WSL 2.1 or newer;
- an Ubuntu 22.04 or 24.04 WSL 2 distribution using systemd;
- Docker Engine 23.0 or newer. New installations use Docker's official Ubuntu
  repository; an existing working native Engine is preserved.

The driver invokes Docker in the selected default WSL distribution through
`wsl.exe`; it does not install or call Windows `docker.exe`, expose the daemon
over TCP, or configure SSH. This direct command forwarding is the supported
Windows-to-WSL connection:

```text
wsl -e env -u DOCKER_CONTEXT -u DOCKER_TLS_VERIFY -u DOCKER_CERT_PATH DOCKER_HOST=unix:///var/run/docker.sock docker version
```

The generated driver clears inherited Docker context and TLS variables and
forces the local `/var/run/docker.sock`; remote TCP and SSH contexts do not
satisfy the Anvil host contract.

Pass `-Distro <name>` to the setup script when Anvil should use a registered
distribution other than the Windows default, then make that distribution the
default with `wsl --set-default <name>` before running Anvil.

The bootstrap enables systemd, installs Docker Engine and its CLI, Buildx, and
Compose packages, enables `docker.service`, and adds the current WSL user to
the `docker` group. It terminates only the selected distribution when a restart
is needed. Repeated runs detect completed work and converge without duplicating
configuration. When installation or upgrade is required, the confirmation plan
lists conflicting Ubuntu Docker packages before replacing them; Docker data
under `/var/lib/docker` is preserved.

For unattended bootstrap after reviewing the planned changes, invoke the
script directly with `-Yes`. The `just anvil-container-bootstrap` recipe is
intentionally interactive.

Docker Desktop is not required. The bootstrap reports Docker Desktop
coexistence and refuses to overwrite a Docker CLI injected from Docker Desktop.
It never uninstalls Docker Desktop, unregisters a WSL distribution, or removes
containers, images, or volumes. Resolve such conflicts manually after reviewing
their data-loss impact.

On ARM64 hosts, Docker emulates the required `linux/amd64` environment. Image
builds and checks can therefore be substantially slower than on x86-64 hosts.

## Security boundary

> [!WARNING]
> `customize.sh` and `customize.ps1` execute on the host with the developer's
> permissions before container isolation begins. Reviewing and trusting these
> files is equivalent to reviewing and trusting any other host-executed script
> in the checked-out branch.

Membership in the `docker` group grants root-equivalent control of the WSL
distribution. The bootstrap requires root only inside WSL through
`wsl.exe --user root`; it does not require an elevated Windows terminal, and
ordinary Anvil runs require neither Windows elevation nor `sudo`. Review the
generated setup script before running it from an untrusted branch.

## Common workflows

Run one check:

```text
just anvil-container anvil-clippy
```

Run the complete pull-request tier:

```text
just anvil-container anvil-pr
```

Every argument is treated as a recipe name and must match `anvil-*` or
`_anvil-*`. Recipe parameters are not supported by this command surface.

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

Tier routing starts a nested `just` invocation. Output and exit status are
preserved, but outer `--dry-run`, dependency introspection, global options, and
CLI variable assignments are not propagated to the selected private tier.
Values other than `native` and `container` are rejected.

## Images and caches

The image name includes a content-based tag derived from the repository's Rust
toolchain, generated Anvil recipes, and container build configuration. A
relevant change selects a new image automatically; older branches can continue
using their matching images.

The following data is reused between runs:

- the matching container image;
- repository-scoped Cargo registry and Cargo Git caches;
- compilation output in a repository- and image-specific `target` volume.

The repository is mounted read/write at `/workspace`. Build output remains in a
named volume instead of the host `target/`, avoiding incompatible artifacts and
slow host-to-virtual-machine I/O.

## GitHub authentication

`anvil-aprz` and aggregate tiers that include it require GitHub API
authentication. The driver uses either:

- the host `GITHUB_TOKEN`; or
- the token from an authenticated host `gh` session.

Trusted customization can provision a short-lived token by setting
`GITHUB_TOKEN`; the driver reads it after loading and validating customization.

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
| `ANVIL_CONTAINER_BASE_IMAGE` | Selects a digest-pinned compatible Linux base image and changes the content-based tag |
| `ANVIL_CONTAINER_IMAGE` | Changes the local image name; the content-based tag is retained |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fails instead of building when the matching image is absent |

The public driver builds images locally and does not pull
`ANVIL_CONTAINER_IMAGE` from a registry.

The default base is digest-pinned Debian Bookworm. Set
`ANVIL_CONTAINER_BASE_IMAGE` to another image compatible with the generated
Debian-based `Containerfile` when a lower glibc baseline is required. A
different package ecosystem such as Azure Linux requires a derived
`Containerfile`. The value must use `image@sha256:<digest>` form so the
selected base remains part of the content-addressed image identity.

Two simultaneous cold invocations can both build the same missing image. This
is accepted for local development: the content-addressed tag converges on the
same inputs, at the cost of duplicate work.

## Troubleshooting

| Problem | Resolution |
|---|---|
| Docker is not found on Linux | Install Docker Engine 23.0 or newer inside that environment |
| Docker is unavailable from Windows | Run `just anvil-container-doctor`, then `just anvil-container-bootstrap` |
| Docker requires elevated access in WSL | Rerun the bootstrap; it adds the current user to the `docker` group and refreshes the selected distribution |
| Docker resolves under `/mnt/wsl/docker-desktop` | Restore a native Docker CLI inside the selected distribution; the bootstrap intentionally does not remove Docker Desktop or its data |
| The automatic bootstrap does not support the distribution | Follow Docker's manual Engine installation guide, enable systemd and `docker.service`, add the user to the `docker` group, and verify `wsl -d <name> -- docker version` |
| A partial installation remains after an error | Rerun the bootstrap; each operation detects current state and safely resumes |
| ARM64 execution is slow | The current image is `linux/amd64` and runs through Docker emulation |
| `linux/amd64` cannot run | Configure Docker to run `linux/amd64` images |
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
.anvil/container/customize.sh
.anvil/container/customize.ps1
```

The driver sources the matching file as trusted host code before authentication,
image construction, and recipe execution. The documented customization
contract provides inputs and validated outputs for APRZ classification, build
secrets, dependency preparation, runtime arguments, and cleanup.

Customization source is excluded from image identity and the build context.
Non-secret image behavior must be represented by hashed static files such as
the `Containerfile`, entrypoint, or supporting build scripts.

See the [container customization contract](https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/containers.md#8-container-customization)
for the complete interface and security requirements.
