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
- A Linux or WSL environment capable of running `linux/amd64` images, either
  natively on x86-64 or through Docker emulation on ARM64.

On Windows, the driver invokes Docker from the default WSL distribution rather
than calling Windows `docker.exe`. Regardless of how Docker is installed, this
command must succeed from PowerShell:

```text
wsl -e docker version
```

Start the Docker service inside WSL when it is stopped and add the WSL user to
the `docker` group when non-root access is not already configured. Docker
Desktop is not required.

On ARM64 hosts, Docker emulates the required `linux/amd64` environment. Image
builds and checks can therefore be substantially slower than on x86-64 hosts.

## Security boundary

> [!WARNING]
> `customize.sh` and `customize.ps1` execute on the host with the developer's
> permissions before container isolation begins. Reviewing and trusting these
> files is equivalent to reviewing and trusting any other host-executed script
> in the checked-out branch.

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

## Git worktrees

Ordinary repositories and [linked
worktrees](https://git-scm.com/docs/git-worktree) both work, including
worktrees created on Windows and run through WSL.

In a linked worktree the shared Git data lives outside the worktree, so the
driver additionally mounts that common Git directory **read-only** and points
Git at it. Checks that read history — impact scoping, `anvil-aprz`, and
`semver-check` — therefore behave the same as in an ordinary clone, while
containerized commands cannot modify shared Git or Git LFS state. Nothing else
about the run changes: the same image, caches, and target volume are used.

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
| Docker is not found on Linux or WSL | Install Docker Engine 23.0 or newer inside that environment |
| Docker is unavailable from Windows | Run `wsl -e docker version`; install or start Docker Engine in the default WSL distribution |
| Docker requires elevated access | Add the Linux/WSL user to the `docker` group, then start a new shell |
| ARM64 execution is slow | The current image is `linux/amd64` and runs through Docker emulation |
| `linux/amd64` cannot run | Configure Docker to run `linux/amd64` images |
| `[script]` recipes are unavailable | Enable `[script]` support; older `just` versions require `set unstable` |
| `rust-toolchain.toml` is missing | Add the repository-owned toolchain file at the repository root |
| GitHub authentication is unavailable | Run `gh auth login --hostname github.com` or set host `GITHUB_TOKEN` |
| A matching image is missing with `ANVIL_CONTAINER_NO_REBUILD=1` | Unset the variable to allow the local image build |
| The first run is slow | The initial image build installs the pinned tool catalog; later runs reuse it |
| `.anvil/container/ is out of date` | Run `cargo anvil` to regenerate after editing `.anvil/config.toml`, or after locally modifying a generated container file |
| A declared mount source does not exist | Create the path, or correct `source` in `.anvil/config.toml` |
| A Windows-created worktree fails in WSL | Run the checks from Windows PowerShell, or recreate the worktree from Linux so its `.git` pointer records a path WSL Git can resolve |
| Git metadata cannot be resolved | Run `git worktree repair` in the worktree; the `.git` pointer must name existing metadata |

Use `docker images anvil-dev` inside Linux or WSL to list locally cached
default Anvil images.

## Configuration

A repository can extend its container environment through a repository-owned
file:

```text
.anvil/config.toml
```

It declares four things, all optional:

| Section | Purpose | Rebuilds the image? |
|---|---|---|
| `[container.image]` | Packages, files, environment, and build steps added to the image | **Yes** |
| `[[container.cache]]` | Persistent named cache volumes | No |
| `[[container.mount]]` | Explicit host mounts | No |
| `[[container.command]]` | Repository commands runnable in the container | No |
Run `cargo anvil` after editing it. `cargo-anvil` validates the file and
compiles it into the generated `Containerfile` and `runtime.conf`; the drivers
read the generated files, never the TOML. If the two fall out of step,
`anvil-container` refuses to run and tells you to regenerate.

```toml
[container.image]
packages = ["protobuf-compiler"]

[[container.cache]]
name = "pip"
target = "/tmp/anvil-user/.cache/pip"
scope = "worktree"     # or "image", or "global"

[[container.mount]]
name = "shared-protos"
source = { sibling = "shared-protos" }   # or { repository = ... } / { host = ... }
target = "/shared-protos"
mode = "read-only"                       # the default
```

Cache scope decides what a volume is shared with. `worktree` is named for what
it keys on — a hash of the worktree path — so two linked worktrees of the same
repository do **not** share one; use `global` when the content is genuinely
interchangeable.

### Registered commands

A repository can make its own `just` recipes runnable in the container:

```toml
[[container.command]]
name = "build-image"
recipe = "build-service-image"
workdir = "services/gateway"

[[container.command.arg]]
name = "tag"
type = "token"        # or "integer", "path", "enum"
```

```text
just anvil-container anvil-clippy anvil-fmt   # every argument is an anvil recipe
just anvil-container build-image v1.2.3       # one registered command with arguments
```

A command name can never start with `anvil-`, so the first argument selects
between the two modes with no flag, and existing `anvil-*` invocations are
unchanged. Arguments are validated against their declared type before anything
starts, and the recipe is invoked as `just <recipe> -- <args>` so a value can
never be read as a `just` option. Argument values may not contain whitespace.

> [!WARNING]
> `.anvil/config.toml` is trusted repository content, in the same class as
> `customize.sh` / `customize.ps1`. A host mount grants containerized code
> access to a host path, a build step runs as root with network access during
> image construction, and a registered command runs repository code with the
> worktree mounted read/write. Reviewing a branch that changes this file is
> reviewing code that will run. Anvil validates declarations to prevent
> mistakes and accidental privilege; it is not a sandbox.

The image tag is a hash of the declared build *instructions and inputs*, not of
the resulting filesystem. A `packages` entry resolving against a moving package
repository, or a `step` that downloads from the network, can still produce
different images under the same tag — the same property the public
`Containerfile` already has. Pin versions and verify checksums inside your own
`step` when that matters.

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
