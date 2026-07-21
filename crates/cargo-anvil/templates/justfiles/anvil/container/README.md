# cargo-anvil local container

Run any generated Anvil recipe in a pinned local Linux environment:

```text
just anvil-container anvil-clippy
just anvil-container anvil-pr
just anvil-container
```

The final command opens an interactive shell.

## Prerequisites

- Podman 4.3 or newer.
- `pwsh`, `git`, and `just` on the host.
- A repository-owned `rust-toolchain.toml`.
- On Windows, initialize and start a Podman machine through Podman Desktop or
  `podman machine init` / `podman machine start`.

## Default tier execution

Native execution is the default. Use the container for the current shell:

```powershell
$env:ANVIL_RUNNER = 'container'
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

To make containers the project default, edit `<repository-root>/Justfile` and
change the default value in the `anvil-runner` region from `"native"` to
`"container"`. Commit `<repository-root>/Justfile` with that policy change.

## Behavior

- The image tag is derived from `rust-toolchain.toml`, the generated Anvil
  recipe tree, and the Containerfile.
- A changed input produces a new image and triggers a local rebuild.
- Cargo registry, Cargo Git, and target output use named Podman volumes.
- The repository is mounted read/write at `/workspace`.
- The existing `anvil-*` recipe runs unchanged inside the container.

## Controls

- `ANVIL_CONTAINER_IMAGE`: change the local image repository/name. The content
  hash remains the tag; the public driver never pulls it remotely.
- `ANVIL_CONTAINER_NO_REBUILD=1`: fail if the matching image is unavailable.

For GitHub API checks, the driver automatically uses an existing host
`GITHUB_TOKEN` or the token from an authenticated host `gh` CLI session. It
mounts the token read-only for the command and removes the temporary file
afterward. If `gh` is installed but not authenticated, an interactive run
pauses before building the image, explains the unauthenticated API limit, and
continues after the user completes `gh auth login` and presses Enter.

## Troubleshooting

- The first run builds the matching image and may take several minutes.
- `podman images anvil-dev` lists cached images.
- Use `ANVIL_CONTAINER_NO_REBUILD=1` to fail on a cache miss.
- Non-interactive runs cannot pause for login. Authenticate `gh` or set host
  `GITHUB_TOKEN` before starting them.
- Rerun `cargo anvil` to update generated files; do not edit this directory.

Downstream catalogs can provide `auth.sh` and/or `auth.ps1` beside these files.
They may populate `ANVIL_CONTAINER_BUILD_ARGS` / `AnvilContainerBuildArgs`,
`ANVIL_CONTAINER_PREPARE_ARGS` / `AnvilContainerPrepareArgs`,
`ANVIL_CONTAINER_PREPARE_COMMAND` / `AnvilContainerPrepareCommand`,
`ANVIL_CONTAINER_RUN_ARGS` / `AnvilContainerRunArgs`, and a cleanup callback.
On Windows, an auth hook can set `AnvilContainerBuildInMachine = $true` when
Podman build secrets must be resolved through the WSL-backed Podman machine.
