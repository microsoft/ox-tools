# Containers

Any generated recipe can run inside a pinned Linux image:

```bash
just anvil-container anvil-clippy   # one check
just anvil-container anvil-pr       # the whole PR tier
just anvil-container                # interactive shell
```

See also [design.md](./README.md) for the overall principles, [local.md](./local.md) for the recipe surface this
wraps, and [extensibility.md](./extensibility.md) for the catalog seam a downstream fork uses.

- [1. Problem](#1-problem)
- [2. What this is](#2-what-this-is)
- [3. The two artifacts](#3-the-two-artifacts)
- [4. Image identity](#4-image-identity)
- [5. Credentials — the hook](#5-credentials--the-hook)
- [6. Host setup](#6-host-setup)
  - [6.1 Docker](#61-docker)
  - [6.2 Podman](#62-podman)
- [7. Customizing the image](#7-customizing-the-image)
- [8. Limits](#8-limits)

## 1. Problem

The local layer assumes a usable host toolchain ([design.md §3][design]: "the user owns it locally"). Two situations
break that assumption:

1. **Linux-on-Windows parity.** A developer on Windows cannot reproduce a Linux-only failure — a `cfg(unix)` path, a
   Linux-specific lint, an `mmap`-shaped test — without a Linux box.
2. **Toolchain drift.** Even on Linux, the host toolset can differ from the one the checks expect, so a green local
   run is not predictive.

Both are solved the same way: run the recipe in an image whose toolchain and tools are the ones this repository pins.

## 2. What this is

- **Explicit.** `just anvil-pr` runs natively, exactly as before. The container is reached through
  `just anvil-container` or not at all. There is no PATH shim, no routing toggle, and no recipe that behaves
  differently depending on where it runs.
- **Unconfigured.** There is no `anvil.toml`. Whether the artifacts are emitted is a catalog decision; the only
  host-specific value is an environment variable read at run time.
- **Local.** Cloud workflows continue to run the recipes natively on their own pools. The image is pinned to be
  *like* CI, not to *be* CI.
- **Additive.** The recipes it runs are byte-identical to the ones a native run uses.

## 3. The two artifacts

```text
repo/
├── justfiles/anvil/
│   ├── container.just                     the anvil-container recipe and its helpers
│   └── …                                  checks, groups, tiers (unchanged; run natively *inside* the image)
└── .anvil/container/
    ├── Dockerfile                         what the image contains
    ├── Dockerfile.dockerignore            what the build context admits
    └── hooks.ps1                          optional; credentials, not emitted by default
```

`container.just` is generated and should not be edited. The `Dockerfile` pair is generated but **deliberately
editable**: anvil's drift handling preserves a repository's changes.

The image installs its tools by running `just anvil-setup` — the same recipe the checks use, from the same generated
pins. There is no second tool list to keep in step, so "the image has the right tools" is true by construction. It is
also why a tool-pin bump changes the image identity: `versions.just` is both what the image installs and part of what
names it.

### Recipes

| Recipe | Purpose |
| --- | --- |
| `just anvil-container <recipe>` | Run any anvil recipe in the image. No argument opens an interactive shell. |
| `just anvil-container-status` | Report the engine, the image reference, and whether it is present. |
| `just anvil-container-rebuild` | Rebuild ignoring every cached layer. |
| `just anvil-container-down` | Remove this repository's cache volumes. The image is left in place. |

`ANVIL_IN_CONTAINER=1` is set inside the image, so a nested invocation runs natively and the work happens exactly
once — one container per top-level command, not one per check.

## 4. Image identity

The tag **is** the hash of the inputs that define it:

```text
anvil-<repo>:<16 hex characters>
```

Hashed: the `Dockerfile`, its ignore file, `rust-toolchain.toml`, `hooks.ps1` when present, and every `*.just` under
`justfiles/anvil/` except `container.just` itself — hashing the driver would make the tag depend on the tag.

Change any of them and the tag names an image that cannot already exist, so a build follows. Change nothing and the
tag resolves instantly. There is no staleness check because there is nothing to check: an image that is present is,
by construction, built from the current inputs.

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_ENGINE` | `docker` (default) or `podman`. |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is missing. Distinguishes a cache miss from a build failure. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild even when the tag resolves. What `anvil-container-rebuild` sets. |

The hook's *output* is deliberately not hashed: a credential must never influence a tag.

## 5. Credentials — the hook

crates.io needs none, so nothing is emitted by default. A repository or a downstream catalog that needs credentials
adds `.anvil/container/hooks.ps1`, which the recipe loads whenever it is present, regardless of who put it there:

```powershell
function Anvil-PreBuild {
    @{ Secrets = @{ feed_token = (az account get-access-token --resource … --query accessToken -o tsv) } }
}

function Anvil-PreRun {
    @{ Env = @{ CARGO_REGISTRIES_INTERNAL_TOKEN = "Bearer …" } }
}
```

Both functions are optional. `Secrets` become `--secret id=…,env=…` mounts at build time; `Env` becomes `-e NAME`
at run time. In both cases the value is handed over **by environment variable name**, so it never appears in the
host's process command line — where endpoint telemetry records and retains it far longer than a short-lived token is
meant to live — and never touches disk. The engine keeps build secrets out of every image layer. When the engine is
reached through WSL (§6.1), the names are exported with `WSLENV` so the value crosses the boundary without ever
becoming an argument.

The corresponding `RUN` should declare the mount as required, which closes the same hole from the Dockerfile's side:

```dockerfile
RUN --mount=type=secret,id=feed_token,required=true \
    TOKEN="$(cat /run/secrets/feed_token)" …
```

Anything the build *writes* with a secret is ordinary content: anvil's own Dockerfile deletes `credentials.toml` and
`.netrc` in the same layer as the install, and a replacement must do the same or the credential is baked into a layer.

**An empty value is a hard error.** BuildKit is not: `--secret id=t,env=UNSET` exits 0 having mounted an empty secret,
so the build would install a reduced tool set and be tagged with the *same* content hash a credentialed build
produces — and every later run would reuse the broken image.

**Trust.** The hook runs on the host, with the developer's permissions, before any container isolation. Only run one
from a repository or catalog you trust. Everything inside the container then runs as one user in one mount namespace,
so a forwarded credential is reachable by anything the checks execute, including dependency build scripts and proc
macros. Keep the set narrow and the token short-lived.

## 6. Host setup

anvil installs nothing. It calls the engine you selected and lets that engine's own diagnostics
surface when something is wrong — the one exception is a missing binary, which is reported with
the variable to set and a pointer here.

The engine must be **callable from the shell that runs `just`**. On Windows there is one
exception, and it is automatic: if the engine is not on `PATH`, anvil retries it inside the
default WSL distribution and translates the repository path with `wslpath`. That exists because
Docker installed in WSL leaves no Windows CLI behind, which would otherwise make the setup this
page recommends unusable.

| | Docker | Podman |
| --- | --- | --- |
| Status | **supported** — what the e2e validates and what CI uses | works, with one exception below |
| Selected by | default | `ANVIL_CONTAINER_ENGINE=podman` |
| Builder | BuildKit | buildah |

Podman has been run through the same end-to-end test as docker: it builds the image, computes and
reuses the content-addressed tag, and runs recipes. Rootless uid mapping differs (`--userns
keep-id` rather than `--user $(id -u)`), which anvil does not currently set for podman.

**Build secrets do not work on podman for Windows.** Podman composes its own temp path from the
build context after translating it into its machine's view, and joins it with a Windows
separator, so any `--secret` fails before the build starts:

```text
Error: creating temp file: open /mnt/c/Users/…/repo\podman-build-secret-4085781963
```

This is not something anvil can work around — `src=` and `env=` fail identically, and a
four-line Dockerfile reproduces it with no anvil involved. It affects only a repository that
supplies `Anvil-PreBuild` (§5); the public catalog ships no hook, so ordinary use is unaffected.
Use docker if you need build-time credentials on Windows.

**Podman also ignores the build-context ignore file.** Anvil emits
`.anvil/container/Dockerfile.dockerignore`, which BuildKit reads in preference to a root
`.dockerignore`. Podman and buildah only honour `.containerignore` or `.dockerignore` at the
*context root*, so on podman the whole worktree — `target/` included — is streamed to the
daemon on every build, and a consumer repository that owns a root `.dockerignore` has that one
obeyed instead. Neither breaks the build unless the repository's own ignore file excludes
`justfiles/` or `rust-toolchain.toml`; the cost is transfer time. Passing `--ignorefile` would
fix it, but only podman accepts that flag, so the recipe does not.

Docker also carries more mileage: it is what CI uses and what the e2e runs by default. Prefer it
if you have no reason not to.

### 6.1 Docker

**Linux.** Install Docker Engine from your distribution or `get.docker.com`, add yourself to the
`docker` group, and you are done.

**Windows — Docker Desktop.** Nothing to configure. `docker` is on `PATH`, so anvil calls it
directly.

**Windows — Docker Engine in WSL** (no Docker Desktop, no licence question):

```powershell
wsl --install -d Ubuntu-24.04
wsl -d Ubuntu-24.04 -- sh -c 'printf "[boot]\nsystemd=true\n" | sudo tee /etc/wsl.conf'
wsl -d Ubuntu-24.04 -- sh -c 'curl -fsSL https://get.docker.com | sh'
wsl -d Ubuntu-24.04 -- sudo usermod -aG docker "$USER"
wsl --shutdown
wsl -d Ubuntu-24.04 -- docker version     # verify
```

That is the whole setup: no Windows `docker` CLI is needed, because anvil reaches the engine
through `wsl.exe` when it finds none on `PATH`. `just` and `pwsh` stay on Windows — the
distribution needs only Docker.

If you *do* install a Windows `docker` CLI and point `DOCKER_HOST` at the WSL socket, anvil uses
it directly and the WSL fallback never engages. In that arrangement the daemon is Linux-side, so
the repository must be bind-mountable at a path that daemon understands.

### 6.2 Podman

**Linux.** Install podman and set `ANVIL_CONTAINER_ENGINE=podman`.

**Windows.** `podman machine init` provisions and manages its own WSL2 virtual machine, and
`podman.exe` is on `PATH`, so anvil calls it directly and the WSL fallback never engages.

```powershell
winget install RedHat.Podman-Desktop   # or the podman CLI alone
podman machine init
podman machine start
$env:ANVIL_CONTAINER_ENGINE = 'podman'
```

## 7. Customizing the image

Two audiences, three levers. A **repository** owns its own copy of the emitted files; a
**downstream catalog** (an anvil fork — see [extensibility.md](./extensibility.md)) changes what
every repository it manages receives.

| You want | Do this | Who |
| --- | --- | --- |
| Extra packages, one repository | Edit `.anvil/container/Dockerfile` in place; the drift flow preserves it | repository |
| A different base OS or toolchain source, everywhere | `replace_artifact(artifacts::container::dockerfile().with_body(…))` | catalog |
| Credentials | Write `.anvil/container/hooks.ps1`, or ship one with `with_artifact(artifacts::container::hooks(…))` | either |
| No container support at all | `without_artifact` each of the three artifacts | catalog |

Editing the Dockerfile in a single repository is supported but noisy: anvil keeps proposing its
own version against a file it can see has diverged. A fork that wants the change everywhere
should replace the artifact instead.

The hook is loaded by path, not by provenance: `container.just` sources
`.anvil/container/hooks.ps1` whenever it exists, so a hand-written file and one shipped by a
catalog behave identically. That is deliberate — it lets a repository try a credential flow
before anyone commits to forking the catalog for it.

**Coupled artifacts.** The Dockerfile and its ignore file move together. A replacement that
`COPY`s more of the tree must also replace `artifacts::container::dockerignore()`, or the extra
files are excluded from the build context and the build fails on a missing path. The recipe
tree needs no such care: the image identity hashes every `*.just` under `justfiles/anvil/`
recursively, so a catalog that adds a recipe directory gets it hashed automatically — but the
same widening rule applies before it can be copied.

**What a fork does *not* touch.** The recipe, the identity hash, the cache volumes, the mounts
and the uid mapping are inherited unchanged. Substrate is the worked example: a different base
OS and a different toolchain source, expressed as one Dockerfile replacement plus one hook, and
nothing else.

Keep `ARG BASE_IMAGE` digest-pinned. A floating tag can change underneath a tag that claims to name fixed content,
which would make every cached image a potential lie.

## 8. Limits

- Linux-only, `linux/amd64`. On ARM64 hosts the image is emulated and is substantially slower.
- Local-only: no registry integration, no push, no promotion. The image is built and consumed locally, which is why
  it needs no published artifact to exist.
- A repository-owned `rust-toolchain.toml` is required — it is both what the image installs and part of what names it.
- The first build takes several minutes: it installs a toolchain and the whole pinned tool catalog. Later runs reuse
  it until an input changes.
- `target/` stays on the bind mount; the cargo and rustup homes live in named volumes so the hot write path does not
  cross the host boundary.

[design]: ./README.md
