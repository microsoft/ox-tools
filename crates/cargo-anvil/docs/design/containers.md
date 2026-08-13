# Containers

Any generated recipe can run inside a Linux image that carries exactly the toolchain and tools this repository pins:

```bash
just anvil-container anvil-clippy   # one check
just anvil-container anvil-pr       # the whole PR tier
just anvil-container                # interactive shell
```

See also [design.md](./README.md) for the overall principles, [local.md](./local.md) for the recipe surface this
wraps, and [extensibility.md](./extensibility.md) for the catalog seam a downstream fork uses.

- [1. Why](#1-why)
- [2. How it runs](#2-how-it-runs)
- [3. What it adds to a repository](#3-what-it-adds-to-a-repository)
- [4. Image identity](#4-image-identity)
- [5. The hook](#5-the-hook)
- [6. Host setup](#6-host-setup)
  - [6.1 Docker](#61-docker)
  - [6.2 Podman](#62-podman)
- [7. Customizing the image](#7-customizing-the-image)
- [8. Limits](#8-limits)

## 1. Why

The local recipes assume a usable host toolchain — anvil does not install one ([design.md §3][design]: "the user owns
it locally"). Two situations break that assumption:

1. **Linux-only failures on a Windows machine.** A `cfg(unix)` path, a Linux-specific lint, an `mmap`-shaped test:
   none of them can be reproduced without a Linux environment.
2. **Toolchain drift.** Even on Linux, the installed toolset can differ from the one the checks expect, so a green
   local run stops being predictive of a cloud one.

Both are answered the same way: run the recipe in an image built from the repository's own pins.

## 2. How it runs

`just anvil-pr` and every other recipe continue to run natively. The container is entered only through
`just anvil-container`, which takes any recipe name and its arguments:

```bash
just anvil-container anvil-setup binstall
```

Inside the image, `ANVIL_IN_CONTAINER=1` is set, so a recipe that reaches `anvil-container` again runs natively
instead of nesting. The work happens once — one container per command, not one per check.

The recipes themselves are identical in both cases. Nothing behaves differently depending on where it runs, and no
wrapper shadows `just` on `PATH`.

| Recipe | Purpose |
| --- | --- |
| `just anvil-container <recipe> [args…]` | Run a recipe in the image. No argument opens an interactive shell. |
| `just anvil-container-tag` | Print the image reference for the current inputs, without building it. |
| `just anvil-container-status` | Report the engine, the image reference, and whether it is present. |
| `just anvil-container-rebuild` | Rebuild from scratch, ignoring every cached layer. |
| `just anvil-container-down` | Remove this repository's cache volumes. The image is left in place. |

The repository is mounted at `/workspace`, and the working directory is mapped to its in-container equivalent so
relative paths keep working from a subdirectory. The cargo and rustup homes live in named volumes, so the hot write
path never crosses the host boundary and the host's own toolchain is untouched.

Cloud workflows are unaffected: they run the recipes natively on their own agents. The image is pinned to resemble
that environment, not to be it.

## 3. What it adds to a repository

```text
repo/
├── justfiles/anvil/
│   ├── container.just                     the anvil-container recipes
│   └── …                                  checks, groups, tiers — run natively *inside* the image
└── .anvil/container/
    ├── Dockerfile                         what the image contains
    ├── Dockerfile.dockerignore            what the build context admits
    └── hooks.ps1                          optional; supplied by you or a catalog (§5)
```

`container.just` is generated and reconciled on every run; edits to it are replaced. The `Dockerfile` and its ignore
file are generated too, but they are meant to be edited — anvil's drift handling preserves a repository's changes to
them (§7).

The image installs its tools by running `just anvil-setup`: the same recipe the checks use, reading the same
generated pins. There is no second list of tools to keep in step, which is also why bumping a tool pin changes the
image — `versions.just` is both what the image installs and part of what names it (§4).

## 4. Image identity

The tag **is** the hash of the inputs that define the image:

```text
anvil-<repo>:<16 hex characters>
```

Hashed: the `Dockerfile`, its ignore file, `rust-toolchain.toml`, `hooks.ps1` when present, and every `*.just` under
`justfiles/anvil/` — except `container.just` itself, since hashing the driver would make the tag depend on the tag.

Change any input and the tag names an image that cannot already exist, so a build follows. Change nothing and the tag
resolves immediately. There is no staleness check because there is no staleness: an image that is present was built
from the inputs that name it.

That guarantee is exact for an image built locally. An image fetched by the resolve hook (§5.2) only *claims* those
inputs — the hash is over source files and cannot be recomputed from layers — so the claim is only as good as the
registry it came from. Publish to one with immutable tags, and restrict push to the identity that builds them.

`just anvil-container-tag` prints the reference without building anything. Every other recipe asks it, so a publisher
and a consumer compute the same reference independently: no `latest`, no digest to maintain by hand.

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_ENGINE` | `docker` (default) or `podman`. |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is missing, which distinguishes a cache miss from a build failure. |
| `ANVIL_CONTAINER_NO_RESOLVE=1` | Skip the resolve hook, so a query never pulls. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild even when the tag resolves, ignoring the hook. |

Values a hook returns never enter the hash: a credential must not be able to influence a tag.

## 5. The hook

`.anvil/container/hooks.ps1` supplies the two things anvil cannot know — credentials for a private feed, and where a
prebuilt image might come from. It is optional, and it is loaded by path rather than by provenance: the recipe sources
it whenever the file exists, whether a repository wrote it or a catalog shipped it. A repository can therefore try a
credential flow without forking anything.

| Function | Called | Returns |
| --- | --- | --- |
| `Anvil-PreBuild` | before a build | `@{ Secrets = @{ id = value } }` |
| `Anvil-PreRun` | before a run | `@{ Env = @{ NAME = value } }` |
| `Anvil-ResolveImage $tag` | before a build, when nothing local matches | an image reference, or nothing |

All three are optional.

### 5.1 Credentials

crates.io needs none, so the default image ships no credential plumbing. An internal feed does:

```powershell
function Anvil-PreBuild {
    @{ Secrets = @{ feed_token = (az account get-access-token --resource … --query accessToken -o tsv) } }
}

function Anvil-PreRun {
    @{ Env = @{ CARGO_REGISTRIES_INTERNAL_TOKEN = "Bearer $token" } }
}
```

Minting the value in a function, rather than reading it from a committed file or a declared variable, is the point: a
short-lived token has to be acquired at the moment it is used.

`Secrets` become `--secret id=…,env=…` mounts at build time; `Env` becomes `-e NAME` at run time. In both cases the
value is handed to the engine **by variable name**, so it never appears in a command line — where endpoint telemetry
records and retains it far longer than the token is meant to live — and it never touches disk. Build secrets stay out
of every image layer. When the engine is reached through WSL (§6.1), the names are exported with `WSLENV` so the value
crosses that boundary the same way.

Declare the mount as required, which closes the same hole from the Dockerfile's side:

```dockerfile
RUN --mount=type=secret,id=feed_token,required=true \
    TOKEN="$(cat /run/secrets/feed_token)" …
```

Anything the build *writes* with a secret is ordinary content. Anvil's own Dockerfile deletes `credentials.toml` and
`.netrc` in the same layer as the install; a replacement must do the same, or the credential is baked into a layer.

**An empty value is a hard error.** BuildKit itself is not: `--secret id=t,env=UNSET` mounts an empty secret and
exits 0, so the build would install a reduced tool set and then be tagged with the same content hash a credentialed
build produces — and every later run would reuse that broken image.

**Trust.** The hook runs on the host, with your permissions, before any container isolation. Only run one from a
repository or catalog you trust. Inside the container everything runs as one user in one mount namespace, so a
forwarded credential is readable by anything the checks execute, including dependency build scripts and proc macros.
Keep the set narrow and the token short-lived.

### 5.2 Prebuilt images

When no local image matches the tag, `Anvil-ResolveImage` is offered that reference before a build starts. A catalog
that publishes images implements it; without one, the build proceeds as usual.

```powershell
function Anvil-ResolveImage($tag) {
    $remote = "myregistry.azurecr.io/anvil:$($tag.Split(':')[-1])"
    az acr login --name myregistry | Out-Null
    docker pull $remote | Out-Null
    if ($LASTEXITCODE -eq 0) { $remote }
}
```

Three properties are worth knowing:

- **The returned reference is used as-is, not re-tagged to the local name.** A local tag asserts "built here from
  these inputs"; a fetched image only claims it (§4). Keeping the registry reference keeps the run honest about where
  the image came from.
- **The reference is verified before use.** Runs are `--pull=never`, so a hook that reported an image it had not
  actually fetched would otherwise fail later and further from the cause.
- **Every failure falls through to a local build**, with the reason printed — a missing image, an expired credential,
  a broken hook. A publisher that has not caught up with your change must never block you.

`ANVIL_CONTAINER_NO_RESOLVE=1` skips this step.

## 6. Host setup

anvil installs nothing and manages no virtual machine. It calls the engine you selected and lets that engine's own
diagnostics surface; the one message it owns is for a missing binary, which names the variable to set and points
here.

The engine must be **callable from the shell that runs `just`**. On Windows there is one automatic exception: if the
engine is not on `PATH`, anvil retries it inside the default WSL distribution and translates the repository path with
`wslpath`, which is what makes a WSL-only Docker installation work with no Windows CLI.

| | Docker | Podman |
| --- | --- | --- |
| Selected by | default | `ANVIL_CONTAINER_ENGINE=podman` |
| Builder | BuildKit | buildah |
| Support | full | full, except build secrets on Windows (§6.2) |

### 6.1 Docker

**Linux.** Install Docker Engine from your distribution or `get.docker.com` and add yourself to the `docker` group.

**Windows, with Docker Desktop.** Nothing to configure: `docker` is on `PATH`.

**Windows, Docker Engine in WSL** — no Docker Desktop, and no Windows CLI required:

```powershell
wsl --install -d Ubuntu-24.04
wsl -d Ubuntu-24.04 -- sh -c 'printf "[boot]\nsystemd=true\n" | sudo tee /etc/wsl.conf'
wsl -d Ubuntu-24.04 -- sh -c 'curl -fsSL https://get.docker.com | sh'
wsl -d Ubuntu-24.04 -- sudo usermod -aG docker "$USER"
wsl --shutdown
wsl -d Ubuntu-24.04 -- docker version     # verify
```

That is the whole setup. `just` and `pwsh` stay on Windows; the distribution needs only Docker.

Installing a Windows `docker` CLI and pointing `DOCKER_HOST` at the WSL socket also works, and takes precedence — the
WSL path is used only when no CLI is found. The daemon is then Linux-side, so the repository must be bind-mountable
at a path it understands.

### 6.2 Podman

**Linux.** Install podman and set `ANVIL_CONTAINER_ENGINE=podman`.

**Windows.** `podman machine init` provisions and manages its own WSL2 virtual machine, and `podman.exe` is on
`PATH`, so anvil calls it directly.

```powershell
winget install RedHat.Podman-Desktop   # or the podman CLI alone
podman machine init
podman machine start
$env:ANVIL_CONTAINER_ENGINE = 'podman'
```

**Build secrets are unavailable on podman for Windows.** Podman builds its own temp path from the build context after
translating it into the machine's view and joins it with a Windows separator, so any `--secret` fails before the
build starts:

```text
Error: creating temp file: open /mnt/c/Users/…/repo\podman-build-secret-4085781963
```

The failure is in podman rather than in anvil, and no form of the flag avoids it. It affects only a repository whose
hook supplies `Anvil-PreBuild` (§5.1); everything else — building, running, tag reuse — works. Use docker if you need
build-time credentials on Windows.

Two smaller differences: anvil passes `--ignorefile` explicitly on podman, because buildah honours only an ignore file
at the context root, and rootless uid mapping (`--userns keep-id`) is not currently applied.

## 7. Customizing the image

A **repository** changes what its own image contains; a **downstream catalog** (an anvil fork — see
[extensibility.md](./extensibility.md)) changes what every repository it manages receives.

| You want | Do this | Who |
| --- | --- | --- |
| Extra packages, one repository | Edit `.anvil/container/Dockerfile` in place | repository |
| A different base OS or toolchain source, everywhere | `replace_artifact(artifacts::container::dockerfile().with_body(…))` | catalog |
| Credentials, or a prebuilt image | Write `.anvil/container/hooks.ps1`, or ship one with `with_artifact(artifacts::container::hooks(…))` | either |
| No container support at all | `without_artifact` each of the three artifacts | catalog |

Editing the Dockerfile in one repository is supported, and the drift flow keeps the edit — but anvil will keep
offering its own version against a file it can see has diverged. A change that belongs everywhere is better made in a
catalog.

**The Dockerfile and its ignore file move together.** A replacement that `COPY`s more of the tree must also replace
`artifacts::container::dockerignore()`, or the extra files never reach the build context and the build fails on a
missing path. Recipes need no such care: the identity hash covers `justfiles/anvil/` recursively, so a new recipe
directory is hashed automatically — though the same widening rule applies before it can be copied.

A fork inherits the rest unchanged: the recipes, the identity hash, the cache volumes, the mounts and the uid
mapping. A different base OS with a different toolchain source is one Dockerfile replacement plus one hook.

Keep `ARG BASE_IMAGE` digest-pinned. The identity hash covers this file's text, but it does not resolve the base, so
a floating tag can change underneath a tag that claims to name fixed content.

## 8. Limits

- Linux-only, `linux/amd64`. On ARM64 hosts the image is emulated and is substantially slower.
- Anvil never pushes and never promotes an image. It builds one, and it will use one a hook fetched (§5.2);
  publishing belongs to whoever owns the registry.
- A repository-owned `rust-toolchain.toml` is required — it is both what the image installs and part of what names it.
- The first build takes several minutes: it installs a toolchain and the whole pinned tool catalog. Later runs reuse
  it until an input changes.
- `target/` stays on the bind mount, so build output is visible from the host.

[design]: ./README.md
