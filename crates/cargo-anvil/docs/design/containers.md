# Containerized execution

Any generated recipe can be executed inside a Linux image built from the toolchain and tool versions the repository
pins:

```bash
just anvil-container anvil-clippy   # one check
just anvil-container anvil-pr       # the whole PR tier
just anvil-container                # interactive shell
```

The feature consists of two generated artifacts and one optional hook. There is no configuration file, and no
invocation is routed into a container implicitly.

See [README.md](./README.md) for the overall design principles, [local.md](./local.md) for the recipe surface this
wraps, and [extensibility.md](./extensibility.md) for the catalog seam a downstream fork uses.

- [1. Purpose](#1-purpose)
- [2. Command surface](#2-command-surface)
- [3. Execution model](#3-execution-model)
  - [3.1 Engine resolution](#31-engine-resolution)
  - [3.2 Path translation](#32-path-translation)
  - [3.3 Mounts and working directory](#33-mounts-and-working-directory)
  - [3.4 Process identity](#34-process-identity)
  - [3.5 Re-entry](#35-re-entry)
- [4. Emitted artifacts](#4-emitted-artifacts)
- [5. Image identity](#5-image-identity)
  - [5.1 Hashed inputs](#51-hashed-inputs)
  - [5.2 Digest computation](#52-digest-computation)
  - [5.3 Guarantees](#53-guarantees)
- [6. Environment variables](#6-environment-variables)
- [7. The hook](#7-the-hook)
  - [7.1 Anvil-PreBuild](#71-anvil-prebuild)
  - [7.2 Anvil-PreRun](#72-anvil-prerun)
  - [7.3 Anvil-ResolveImage](#73-anvil-resolveimage)
  - [7.4 Trust boundary](#74-trust-boundary)
- [8. Host requirements](#8-host-requirements)
  - [8.1 Docker](#81-docker)
  - [8.2 Podman](#82-podman)
- [9. Customization](#9-customization)
- [10. Limitations](#10-limitations)

## 1. Purpose

The generated recipes assume a usable host toolchain; anvil does not install one ([README.md §3][design]: "the user
owns it locally"). Two conditions invalidate that assumption:

1. **Platform-specific failures.** A `cfg(unix)` code path, a Linux-only lint, or a test that depends on Linux memory
   semantics cannot be reproduced on a Windows or macOS host.
2. **Toolchain divergence.** The installed toolset can differ from the one the checks expect, so a passing local run
   stops predicting a cloud result.

Both are addressed by executing the recipe unchanged inside an image constructed from the repository's own pins.

## 2. Command surface

`just anvil-pr` and every other recipe continue to execute natively. A container is entered only through
`anvil-container`, which accepts a recipe name and its arguments:

```bash
just anvil-container anvil-setup binstall
```

| Recipe | Behaviour |
| --- | --- |
| `just anvil-container <recipe> [args…]` | Execute a recipe in the image. With no argument, opens an interactive shell. |
| `just anvil-container-tag` | Print the image reference for the current inputs. Builds nothing. |
| `just anvil-container-status` | Print the engine, working directory, image reference, and whether it is present. Never builds or pulls. |
| `just anvil-container-rebuild` | Rebuild the image with every layer cache disabled. |
| `just anvil-container-down` | Remove this repository's cache volumes. The image is retained. |

All five are annotated `[group("anvil-container")]` and appear as one cluster in `just --groups`.

Recipe bodies are identical in both execution modes. No wrapper shadows `just` on `PATH`, and no recipe behaves
differently according to where it runs. Cloud workflows are unaffected: they execute the same recipes natively on
their own agents. The image is pinned to resemble that environment, not to reproduce it.

## 3. Execution model

One container is created per `anvil-container` invocation, not one per check. The container is removed on exit
(`--rm`).

### 3.1 Engine resolution

`ANVIL_CONTAINER_ENGINE` selects the engine and defaults to `docker`. Any value other than `docker` or `podman` is
rejected before the engine is invoked. Because the engine is a property of the host rather than of the repository, it
is read at run time and is never committed; a single invocation can override it with
`just anvil_container_engine=podman anvil-container anvil-pr`.

Resolution proceeds in a fixed order:

1. If the named binary is on `PATH`, it is invoked directly.
2. Otherwise, on Windows, the binary is probed inside the default WSL distribution (`wsl.exe -- <engine> --version`).
   If the probe succeeds, every subsequent engine call is prefixed with `wsl.exe --`.
3. Otherwise the invocation fails with a message naming the variable and linking to this document.

anvil does not probe for an engine other than the one requested. Presence is not reachability; `podman-docker` aliases
`docker` onto podman; and silently selecting between two installed engines yields two image stores and an unexplained
rebuild. Every failure other than a missing binary surfaces the engine's own diagnostic unmodified.

Step 2 exists because installing Docker Engine inside WSL without Docker Desktop leaves no Windows CLI on `PATH`, and
that setup is the one this repository's own development guide describes. Docker Desktop and Podman both install a
Windows CLI, are found in step 1, and never reach step 2.

### 3.2 Path translation

When the engine is reached through WSL it does not share the Windows filesystem view, so host paths are translated
with `wslpath -a -u` before they are passed as a bind-mount source, a build context, or a `--file` argument. An
untranslated Windows path is not rejected by the daemon: it is bind-mounted as an empty directory, and the failure
surfaces much later as a missing file inside the container.

Paths are converted to forward slashes before translation, because arguments crossing into WSL pass through a shell
that would otherwise consume the backslashes. `wslpath` accepts either separator.

### 3.3 Mounts and working directory

| Mount | Target | Purpose |
| --- | --- | --- |
| repository root (bind) | `/workspace` | The worktree under test, including `target/`. |
| `anvil-<repo>-cargo` (volume) | `/usr/local/cargo` | `CARGO_HOME`: registry cache and installed binaries. |
| `anvil-<repo>-rustup` (volume) | `/usr/local/rustup` | `RUSTUP_HOME`: installed toolchains. |

The cargo and rustup homes are named volumes rather than bind mounts, so the write-heavy paths never cross the host
boundary and the host's own toolchain is untouched. `target/` remains on the bind mount, so build output stays visible
from the host and is shared between native and containerized runs.

The caller's working directory is mapped to its in-container equivalent, so relative paths continue to resolve when
`anvil-container` is invoked from a subdirectory.

Volume names derive from the repository directory name, lowercased with every character outside `[a-z0-9._-]`
replaced by `-`. Two checkouts with the same directory name therefore share cache volumes. This is harmless in normal
use, because cargo's caches are content-addressed, but `anvil-container-down` removes volumes that the other checkout
is also using.

### 3.4 Process identity

On a Linux host the run passes `--user <uid>:<gid>`, matching the invoking user. Without it, everything written under
the bind mount — `target/`, generated files — is owned by root on the host, and the next native `cargo build` or
`git clean` fails with `EACCES` far from the cause. The flag is omitted when the invoking user is root.

Docker Desktop on Windows and macOS maps ownership itself, and `id` is not available to query, so the flag is not
passed on those hosts.

### 3.5 Re-entry

`ANVIL_IN_CONTAINER=1` is set in the image and passed again on each run. `anvil-container` checks it first: inside the
image, the requested recipe is executed directly instead of launching another container. A recipe that reaches
`anvil-container` transitively therefore performs its work exactly once.

## 4. Emitted artifacts

```text
repo/
├── justfiles/anvil/
│   ├── container.just                     the anvil-container recipes
│   └── …                                  checks, groups, tiers — executed natively *inside* the image
└── .anvil/container/
    ├── Dockerfile                         what the image contains
    ├── Dockerfile.dockerignore            what the build context admits
    └── hooks.ps1                          optional; not emitted by default (§7)
```

`container.just` is generated and reconciled on every run; local edits to it are replaced. The `Dockerfile` and its
ignore file are generated but intended to be edited: anvil's drift handling preserves a repository's changes to them
(§9).

The image installs its tools by running `just anvil-setup`, the same recipe the checks use, reading the same generated
pins. There is no second tool list to keep synchronized, which is also why a tool-pin change renames the image:
`versions.just` is both what the image installs and part of what names it (§5).

The build context is scoped by `Dockerfile.dockerignore`, a deny-all list that re-admits `justfiles/` and
`rust-toolchain.toml` and nothing else. BuildKit reads `<dockerfile>.dockerignore` in preference to a root
`.dockerignore`, so the repository does not need to own a root ignore file and cannot have one silently overridden.

## 5. Image identity

The image reference is `anvil-<repo>:<16 hex characters>`, where the tag is a SHA-256 digest over the inputs that
define the image. The name is derived from the repository directory as described in §3.3.

### 5.1 Hashed inputs

| Input | Hashed |
| --- | --- |
| `.anvil/container/Dockerfile` | always |
| `.anvil/container/Dockerfile.dockerignore` | always |
| `rust-toolchain.toml` | always |
| `.anvil/container/hooks.ps1` | when the file exists |
| `justfiles/anvil/**/*.just` | always, recursively, except `container.just` |

The recipe tree is included because the image installs its tools by running `just anvil-setup`, whose dependency chain
reaches the tier, group, check, and tool recipes. `container.just` is excluded because hashing the driver would make
the tag depend on the tag.

The hook file's **content** is an input, because it determines what the build installs. Its **output** is deliberately
excluded: a credential must never influence a tag.

A declared input that does not exist is a hard error rather than an omission from the digest.

### 5.2 Digest computation

Inputs are sorted by relative path using an ordinal comparison, then serialized into a single stream. Each entry
contributes a literal `file`, its relative path, and its content, each terminated by a newline. Tagging each entry
this way ensures no rearrangement of names and contents can produce a collision. Line endings are normalized to LF, so
a CRLF checkout and an LF checkout compute the same tag. The ordinal sort matters because a case-insensitive one would
silently drop one of two inputs differing only in case on the case-sensitive filesystem where the image is built.

The tag is the first eight bytes of the digest, hex-encoded — 64 bits, far beyond any practical collision risk for a
local image set, and short enough to keep `docker images` readable.

`anvil-container-tag` is the only place this computation exists; every other recipe calls it. A publisher and a
consumer therefore derive the same reference independently, with no `latest` tag and no digest maintained by hand.

### 5.3 Guarantees

Changing any input names a tag that cannot already exist, so a build follows. Changing nothing resolves the existing
tag immediately. There is no staleness check because there is no staleness to detect: a locally built image that is
present was built from the inputs that name it.

That guarantee is exact only for a locally built image. An image obtained through `Anvil-ResolveImage` (§7.3) merely
*claims* those inputs — the digest is computed over source files and cannot be re-derived from layers — so the claim
is only as strong as the registry it came from. Publish to a registry with immutable tags, and restrict push to the
identity that builds them.

Two inputs sit outside the digest and must be pinned by other means. The base image is not resolved during hashing, so
`ARG BASE_IMAGE` must remain digest-pinned or a floating tag can change beneath a tag that claims to name fixed
content. The platform is pinned to `linux/amd64` on both build and run, so hosts of differing architecture cannot
compute one tag for two different images.

## 6. Environment variables

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_ENGINE` | `docker` (default) or `podman`. Read at run time. |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is absent, distinguishing a cache miss from a build failure. |
| `ANVIL_CONTAINER_NO_RESOLVE=1` | Skip the resolve hook, so a query never pulls. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild with `--no-cache` even when the tag already resolves. Also skips the resolve hook. |
| `ANVIL_IN_CONTAINER=1` | Set inside the image. Makes a nested invocation execute natively (§3.5). |

`ANVIL_CONTAINER_NO_CACHE` skips the hook because "ignore what is cached" must include the remote cache; otherwise a
rebuild would be undone by the next resolve. `ANVIL_CONTAINER_NO_REBUILD` is evaluated independently of it, so the two
compose: `anvil-container-status` sets both `NO_REBUILD` and `NO_RESOLVE`, and answers from local state alone. When
`NO_REBUILD` stops a build, the reference is still printed, because a caller that asked not to build is usually asking
*which* image is missing.

## 7. The hook

`.anvil/container/hooks.ps1` supplies the two things anvil cannot derive: credentials, and where a prebuilt image
might be obtained. The file is optional and is not emitted by default — crates.io requires no credentials, and an
empty script would be one more generated file to review.

It is loaded by path rather than by provenance: the recipe dot-sources it whenever the file exists, whether a
repository wrote it or a catalog shipped it. A repository can therefore adopt a credential flow without forking the
catalog.

| Function | Invoked | Returns |
| --- | --- | --- |
| `Anvil-PreBuild` | before a build | `@{ Secrets = @{ <id> = <value> } }` |
| `Anvil-PreRun` | before a run | `@{ Env = @{ <NAME> = <value> } }` |
| `Anvil-ResolveImage $tag` | before a build, when no local image matches | an image reference, or nothing |

All three are optional, and each is called only if defined.

Both value-returning functions **fail closed on an empty value**. This is not the engine's behaviour: BuildKit accepts
`--secret id=t,env=UNSET`, mounts an empty secret, and exits 0. The build would install a reduced tool set, be tagged
with the same content hash a credentialed build produces, and be reused by every later run.

### 7.1 Anvil-PreBuild

Each returned entry becomes a BuildKit `--secret id=<id>,env=ANVIL_SECRET_<id>` mount. The value is placed in a
process environment variable and passed **by name**, so it never appears in a command line, where endpoint telemetry
records and retains it far longer than a short-lived token is intended to live. The variables are removed once the
build completes. BuildKit keeps a mounted secret out of every image layer.

```powershell
function Anvil-PreBuild {
    @{ Secrets = @{ feed_token = (az account get-access-token --resource … --query accessToken -o tsv) } }
}
```

Minting the value inside a function is the point: a short-lived token must be acquired at the moment it is used, not
read from a committed file or a declared variable.

Declare the mount as required in the Dockerfile, which closes the same gap from the build's side:

```dockerfile
RUN --mount=type=secret,id=feed_token,required=true \
    TOKEN="$(cat /run/secrets/feed_token)" …
```

Anything the build *writes* using a secret is ordinary layer content. The default Dockerfile removes
`credentials.toml` and `.netrc` in the same `RUN` layer as the install; a replacement must do the same, or the
credential is baked into a layer that a later deletion cannot remove.

When the engine is reached through WSL, the secret variable names are exported through `WSLENV` so the values cross
that boundary.

### 7.2 Anvil-PreRun

Each returned entry is forwarded into the container with `-e <NAME>`, again by name rather than as `NAME=VALUE`, for
the reason given above. Inside the image the value is an ordinary environment variable. The forwarded names — never
their values — are echoed to stderr, because everything executing inside the container can read them.

```powershell
function Anvil-PreRun {
    @{ Env = @{ CARGO_REGISTRIES_INTERNAL_TOKEN = (mint-a-token) } }
}
```

### 7.3 Anvil-ResolveImage

When no local image matches the computed tag, the reference is offered to `Anvil-ResolveImage` before a build starts.
A catalog that publishes images implements it; without one, the build proceeds.

```powershell
function Anvil-ResolveImage($tag) {
    $remote = "myregistry.azurecr.io/anvil:$($tag.Split(':')[-1])"
    az acr login --name myregistry | Out-Null
    docker pull $remote | Out-Null
    if ($LASTEXITCODE -eq 0) { $remote }
}
```

Three properties are load-bearing:

- **The returned reference is used as-is, never re-tagged to the local name.** A local tag asserts "built here from
  these inputs"; a fetched image only claims it (§5.3). Retaining the registry reference keeps the run honest about
  the image's origin.
- **The reference is verified before use.** Runs pass `--pull=never`, so a hook that reported an image it had not
  actually fetched would otherwise fail later and further from the cause.
- **Every failure falls through to a local build**, with the reason printed — a missing image, an expired credential,
  a hook that threw. A publisher that has not caught up with a change must not block the developer who made it.

Resolution is attempted before the `ANVIL_CONTAINER_NO_REBUILD` guard, because fetching a published image is not
building one.

### 7.4 Trust boundary

The hook executes on the host, with the invoking user's permissions, before any container isolation exists. Only use
one from a repository or catalog you trust.

Inside the container everything executes as a single user in a single mount namespace, so a forwarded credential is
readable by anything the checks execute, including dependency build scripts and procedural macros. Keep the forwarded
set narrow and the tokens short-lived.

## 8. Host requirements

anvil installs nothing and manages no virtual machine. It invokes the engine you selected and lets that engine's own
diagnostics surface. The only failure message it owns is for a missing binary, which names the variable to set and
links here.

The engine must be callable from the shell that runs `just`, with the single Windows exception described in §3.1. The
host also needs `just` and PowerShell Core (`pwsh`), which every generated recipe requires, and the repository must
own a `rust-toolchain.toml`.

| | Docker | Podman |
| --- | --- | --- |
| Selected by | default | `ANVIL_CONTAINER_ENGINE=podman` |
| Builder | BuildKit | buildah |
| Status | supported | best-effort; see §8.2 |

### 8.1 Docker

**Linux.** Install Docker Engine from your distribution or `get.docker.com`, and add your user to the `docker` group.

**Windows, with Docker Desktop.** No configuration required: `docker` is on `PATH`.

**Windows, Docker Engine in WSL.** No Docker Desktop and no Windows CLI:

```powershell
wsl --install -d Ubuntu-24.04
wsl -d Ubuntu-24.04 -- sh -c 'printf "[boot]\nsystemd=true\n" | sudo tee /etc/wsl.conf'
wsl -d Ubuntu-24.04 -- sh -c 'curl -fsSL https://get.docker.com | sh'
wsl -d Ubuntu-24.04 -- sudo usermod -aG docker "$USER"
wsl --shutdown
wsl -d Ubuntu-24.04 -- docker version     # verify
```

`just` and `pwsh` remain on Windows; the distribution needs only Docker. anvil detects this configuration
automatically (§3.1) and translates paths accordingly (§3.2).

Installing a Windows `docker` CLI and pointing `DOCKER_HOST` at the WSL socket also works and takes precedence, since
the WSL path is used only when no CLI is found on `PATH`. The daemon is then Linux-side, so the repository must be
bind-mountable at a path it can resolve.

### 8.2 Podman

**Linux.** Install podman and set `ANVIL_CONTAINER_ENGINE=podman`.

**Windows.** `podman machine init` provisions and manages its own WSL2 virtual machine, and `podman.exe` is placed on
`PATH`, so anvil invokes it directly.

```powershell
winget install RedHat.Podman-Desktop   # or the podman CLI alone
podman machine init
podman machine start
$env:ANVIL_CONTAINER_ENGINE = 'podman'
```

Three differences from Docker are known and unresolved:

- **Build secrets are unavailable on podman for Windows.** Podman derives a temporary path from the build context
  after translating it into the machine's view, then joins it using a Windows separator, so any `--secret` fails
  before the build begins:

  ```text
  Error: creating temp file: open /mnt/c/Users/…/repo\podman-build-secret-4085781963
  ```

  The defect is in podman, and no form of the flag avoids it. It affects only a repository whose hook defines
  `Anvil-PreBuild` (§7.1); building, running, and tag reuse are unaffected. Use Docker if you need build-time
  credentials on Windows.

- **The ignore file is passed explicitly.** buildah honours only an ignore file at the context root, so anvil passes
  `--ignorefile` on podman. Without it the entire worktree, including `target/`, is streamed to the daemon on every
  build.

- **Rootless user-namespace mapping is not applied.** The run passes `--user` (§3.4) but not `--userns keep-id`, which
  rootless podman requires for bind-mount ownership to map back to the invoking user.

## 9. Customization

A **repository** changes what its own image contains. A **downstream catalog** — an anvil fork, see
[extensibility.md](./extensibility.md) — changes what every repository it manages receives. Containerized execution is
an ordinary artifact group and uses the same levers as any other.

| Goal | Mechanism | Owner |
| --- | --- | --- |
| Extra packages in one repository | Edit `.anvil/container/Dockerfile` in place | repository |
| A different base OS or toolchain source, everywhere | `replace_artifact(artifacts::container::dockerfile().with_body(…))` | catalog |
| Credentials, or a published image | Add `.anvil/container/hooks.ps1`, or ship `artifacts::container::hooks(…)` | either |
| No containerized execution at all | `without_artifact` for each of the three artifacts | catalog |

Editing the Dockerfile in a single repository is supported and the drift flow preserves the edit, but anvil continues
to offer its own version against a file it can see has diverged. A change that belongs everywhere is better made in a
catalog.

**The Dockerfile and its ignore file must be replaced together.** The ignore file is a deny-all list re-admitting only
`justfiles/` and `rust-toolchain.toml` (§4). A replacement Dockerfile that `COPY`s anything else must also replace
`artifacts::container::dockerignore()`, or the additional paths never reach the build context and the build fails on a
missing file. Recipes need no such care: `justfiles/` is re-admitted as a directory and hashed recursively, so a new
recipe subdirectory is both copied and part of the identity automatically.

`justfiles/anvil/` must contain `.just` recipes and nothing else. `CatalogBuilder::build` enforces this, because a
non-recipe file placed there would be copied into the image without being part of its identity: editing it would
change what the image contains without renaming the tag. Non-recipe assets belong in a tool-owned directory such as
`.anvil/`.

A fork inherits everything else unchanged: the recipes, the identity scheme, the cache volumes, the mounts, and the
re-entry guard. A different base OS with a different toolchain source is one Dockerfile replacement plus one hook.

## 10. Limitations

- Linux images only, pinned to `linux/amd64`. On ARM64 hosts the image is emulated and is substantially slower.
- The first build takes several minutes: it installs a toolchain and the entire pinned tool catalog. Subsequent runs
  reuse it until an input changes.
- Any edit under `justfiles/` invalidates the install layer, including files the image's synthetic Justfile never
  imports.
- anvil never pushes and never promotes an image. It builds one, and it will use one a hook fetched (§7.3);
  publishing belongs to whoever owns the registry.
- A repository-owned `rust-toolchain.toml` is required. It is both what the image installs and part of what names it.
- Podman on Windows cannot mount build secrets (§8.2).

[design]: ./README.md
