# Containerized execution

Any generated recipe can be executed inside a Linux image built from the toolchain and tool versions the repository
pins:

```bash
just anvil-container anvil-clippy   # one check
just anvil-container anvil-pr       # the whole PR tier
just anvil-container                # interactive shell
```

Execution is opt-in per invocation: recipes run natively unless a container is requested by name. The feature is three
generated artifacts and one optional hook script, with no configuration file.

See [README.md](./README.md) for the overall design principles, [local.md](./local.md) for the recipe surface this
wraps, and [extensibility.md](./extensibility.md) for the catalog seam a downstream fork uses.

- [1. Purpose](#1-purpose)
- [2. Command surface](#2-command-surface)
- [3. Emitted artifacts](#3-emitted-artifacts)
- [4. Image identity](#4-image-identity)
  - [4.1 Inputs](#41-inputs)
  - [4.2 Digest](#42-digest)
  - [4.3 What the tag guarantees](#43-what-the-tag-guarantees)
- [5. Execution model](#5-execution-model)
  - [5.1 Mounts and working directory](#51-mounts-and-working-directory)
  - [5.2 Process identity](#52-process-identity)
  - [5.3 Re-entry](#53-re-entry)
- [6. Engines and host setup](#6-engines-and-host-setup)
  - [6.1 Engine resolution](#61-engine-resolution)
  - [6.2 Docker](#62-docker)
  - [6.3 Podman](#63-podman)
- [7. The hook](#7-the-hook)
  - [7.1 Anvil-PreBuild](#71-anvil-prebuild)
  - [7.2 Anvil-PreRun](#72-anvil-prerun)
  - [7.3 Anvil-ResolveImage](#73-anvil-resolveimage)
  - [7.4 Trust boundary](#74-trust-boundary)
- [8. Customization](#8-customization)
- [9. Limitations](#9-limitations)

## 1. Purpose

The generated recipes assume a usable host toolchain; anvil does not install one ([README.md §3][design]: "the user
owns it locally"). Two conditions invalidate that assumption:

1. **Platform-specific failures.** A `cfg(unix)` code path, a Linux-only lint, or a test that depends on Linux memory
   semantics cannot be reproduced on a Windows or macOS host.
2. **Toolchain divergence.** The installed toolset can differ from the one the checks expect, so a passing local run
   stops predicting a cloud result.

Both are addressed by executing the recipe unchanged inside an image built from the repository's own pins. Recipe
bodies are identical in either mode, and cloud workflows are unaffected: they run the same recipes natively on their
own agents. The image is pinned to resemble that environment, not to reproduce it.

## 2. Command surface

`anvil-container` accepts a recipe name and its arguments; every other recipe continues to execute natively.

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

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_ENGINE` | `docker` (default) or `podman`. Read at run time (§6.1). |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is absent, separating a cache miss from a build failure. |
| `ANVIL_CONTAINER_NO_RESOLVE=1` | Skip the resolve hook (§7.3), so a query never pulls. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild with `--no-cache` even when the tag resolves. Skips the resolve hook too (§7.3). |
| `ANVIL_IN_CONTAINER=1` | Set inside the image. Makes a nested invocation execute natively (§5.3). |

`NO_REBUILD` is evaluated independently of `NO_CACHE`, so the two compose: `anvil-container-status` sets `NO_REBUILD`
and `NO_RESOLVE` together and answers from local state alone. When `NO_REBUILD` stops a build the reference is still
printed, since a caller that asked not to build is usually asking which image is missing.

## 3. Emitted artifacts

```text
repo/
├── justfiles/anvil/
│   ├── container.just                     the anvil-container recipes
│   └── …                                  checks, groups, tiers, executed natively *inside* the image
└── .anvil/container/
    ├── Dockerfile                         what the image contains
    ├── Dockerfile.dockerignore            what the build context admits
    └── hooks.ps1                          optional; not emitted by default (§7)
```

`container.just` is reconciled on every run, so local edits to it are replaced. The `Dockerfile` and its ignore file
are generated but intended to be edited; anvil's drift handling preserves a repository's changes to them (§8).

The image installs its tools by running `just anvil-setup`, the same recipe the checks use, reading the same
generated pins. There is no second tool list to keep synchronized, and consequently a tool-pin change renames the
image (§4.1).

`Dockerfile.dockerignore` scopes the build context to `justfiles/` and `rust-toolchain.toml`, denying everything else.
BuildKit reads `<dockerfile>.dockerignore` in preference to a root `.dockerignore`, so the repository neither needs to
own a root ignore file nor can have one silently override this.

## 4. Image identity

The image reference is `anvil-<repo>:<16 hex characters>`, where the tag is a SHA-256 digest over the inputs that
define the image. The name derives from the repository directory (§5.1).

### 4.1 Inputs

| Input | Hashed |
| --- | --- |
| `.anvil/container/Dockerfile` | always |
| `.anvil/container/Dockerfile.dockerignore` | always |
| `rust-toolchain.toml` | always |
| `.anvil/container/hooks.ps1` | when the file exists |
| `justfiles/anvil/**/*.just` | always, recursively, except `container.just` |

The recipe tree is an input because `just anvil-setup` decides what the image installs (§3), and its dependency chain
reaches the tier, group, check, and tool recipes. `container.just` is excluded because hashing the driver would make
the tag depend on the tag. A declared input that does not exist is a hard error, not an omission from the digest.

The hook file's **content** is an input, since it determines what the build installs. Its **output** is deliberately
excluded: a credential must never influence a tag.

### 4.2 Digest

Inputs are sorted by relative path with an ordinal comparison, then serialized into one stream in which each entry
contributes a literal `file`, its relative path, and its content, each newline-terminated. Tagging entries this way
prevents a rearrangement of names and contents from colliding. Line endings are normalized to LF, so CRLF and LF
checkouts agree on the tag. The sort is ordinal because a case-insensitive one would drop one of two inputs differing
only in case on the case-sensitive filesystem where the image is built.

The tag is the first eight bytes of the digest, hex-encoded: 64 bits, far beyond practical collision risk for a local
image set, and short enough to keep `docker images` readable.

`anvil-container-tag` is the only place the digest is computed; every other recipe calls it. A publisher and a
consumer therefore derive the same reference independently, with no `latest` tag and no digest maintained by hand.

### 4.3 What the tag guarantees

Changing an input names a tag that cannot already exist, so a build follows; changing nothing resolves the existing
tag immediately. There is no staleness check because a locally built image that is present was built from the inputs
that name it.

That holds only for a locally built image. One obtained through `Anvil-ResolveImage` (§7.3) merely *claims* those
inputs, since the digest covers source files and cannot be re-derived from layers, so the claim is only as strong as
its registry. Publish to a registry with immutable tags and restricted push.

Two properties sit outside the digest. The base image is not resolved during hashing, so `ARG BASE_IMAGE` must remain
digest-pinned; a floating tag could otherwise change beneath a tag that claims to name fixed content. The platform is
pinned to `linux/amd64` on build and run, so hosts of differing architecture cannot compute one tag for two images.

## 5. Execution model

One container is created per `anvil-container` invocation, however many checks the requested recipe runs, and is
removed on exit (`--rm`).

### 5.1 Mounts and working directory

| Mount | Target | Purpose |
| --- | --- | --- |
| repository root (bind) | `/workspace` | The worktree under test, including `target/`. |
| `anvil-<repo>-cargo-registry` (volume) | `/usr/local/cargo/registry` | Downloaded crate sources. |
| `anvil-<repo>-cargo-git` (volume) | `/usr/local/cargo/git` | Git checkouts of git dependencies. |

Only cargo's content-addressed download caches are volumes, so the write-heavy download path never crosses the host
boundary and the host's own toolchain is untouched. `$CARGO_HOME` and `$RUSTUP_HOME` themselves are **not** mounted:
they carry the installed tools and toolchains, and an engine populates a named volume from the image only when that
volume is first created. Mounting them would pin the first image's binaries over every later one, so a tool bump
would change the tag, build a new image, and still run the old tools — defeating the identity guarantee in §4.
Tools and toolchains therefore always come from the image layer the tag names.

`target/` stays on the bind mount, remaining visible from the host and shared between native and containerized runs.
The caller's working directory is mapped to its in-container equivalent, so relative paths resolve when
`anvil-container` is invoked from a subdirectory.

Image and volume names derive from the repository directory name, lowercased with every character outside
`[a-z0-9._-]` replaced by `-`. Two checkouts with the same directory name therefore share cache volumes. That is
harmless, since both volumes hold only content-addressed downloads, but `anvil-container-down` then removes volumes
the other checkout is also using.

### 5.2 Process identity

On a Linux host the run passes `--user <uid>:<gid>`, matching the invoking user, unless that user is root. Without it,
everything written under the bind mount is owned by root on the host, and the next native `cargo build` or `git clean`
fails with `EACCES` far from the cause. Docker Desktop on Windows and macOS maps ownership itself, and `id` is not
available to query, so the flag is not passed there.

### 5.3 Re-entry

`ANVIL_IN_CONTAINER=1` is set in the image and passed on each run. `anvil-container` checks it first and, inside the
image, executes the requested recipe directly instead of launching another container, so a recipe that reaches
`anvil-container` transitively still performs its work once.

## 6. Engines and host setup

anvil installs nothing and manages no virtual machine. Beyond the engine, the host needs `just` and PowerShell Core
(`pwsh`), which every generated recipe requires, and the repository must own a `rust-toolchain.toml`.

| | Docker | Podman |
| --- | --- | --- |
| Selected by | default | `ANVIL_CONTAINER_ENGINE=podman` |
| Builder | BuildKit | buildah |
| Status | supported | best-effort; see §6.3 |

### 6.1 Engine resolution

`ANVIL_CONTAINER_ENGINE` defaults to `docker`, and any value other than `docker` or `podman` is rejected before the
engine is invoked. Being a host property rather than a repository one, it is read at run time and never committed.
It is the only control: the recipes resolve the engine through nested `just` invocations, which a
`just anvil_container_engine=...` override would not reach.

Resolution proceeds in a fixed order:

1. If the named binary is on `PATH`, it is invoked directly.
2. Otherwise, on Windows, the binary is probed inside the default WSL distribution
   (`wsl.exe --exec <engine> --version`). If the probe succeeds, every subsequent engine call is prefixed with
   `wsl.exe --exec`. This accommodates Docker Engine installed inside WSL without Docker Desktop, which leaves no
   Windows CLI behind; Docker Desktop and Podman both install one and resolve at step 1.
3. Otherwise the invocation fails with a message naming the variable and linking to this document.

anvil never falls back to the other engine: if the selected one is unusable, the invocation fails rather than
substituting. Automatic detection is avoided deliberately, because a binary on `PATH` does not prove a reachable
daemon, and silently choosing between two installed engines would split the image cache across two stores and produce
rebuilds with no visible cause. A missing binary is the only failure anvil reports itself; every other engine
diagnostic is shown unchanged.

When the engine is reached through WSL it does not share the Windows filesystem view, so anvil translates every path
it hands over (bind-mount source, build context, `--file`) with `wslpath -a -u`. An untranslated path is not
rejected by the engine; it silently resolves to an empty directory. The `--exec` form is required rather than
cosmetic: plain `wsl.exe --` hands the command line to the distribution's login shell, which would expand `$NAME`
and split on `;` in repository paths and forwarded recipe arguments alike. A path holding `$` would be truncated by
that expansion and `wslpath -a` would still exit 0, bind-mounting the wrong directory.

### 6.2 Docker

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

`just` and `pwsh` remain on Windows; the distribution needs only Docker. Installing a Windows `docker` CLI and
pointing `DOCKER_HOST` at the WSL socket also works and takes precedence, since the WSL path is used only when no CLI
is found. The daemon is Linux-side either way, so the repository must be bind-mountable at a path it can resolve.

### 6.3 Podman

**Linux.** Install podman and set `ANVIL_CONTAINER_ENGINE=podman`.

**Windows.** `podman machine init` provisions and manages its own WSL2 virtual machine, and `podman.exe` is placed on
`PATH`, so anvil invokes it directly.

```powershell
winget install RedHat.Podman-Desktop   # or the podman CLI alone
podman machine init
podman machine start
$env:ANVIL_CONTAINER_ENGINE = 'podman'
```

Podman differs from Docker in three respects:

- **Build secrets are not supported on Windows.** A build that mounts one fails before it starts, with an error
  naming a temporary file:

  ```text
  Error: creating temp file: open /mnt/c/Users/…/repo\podman-build-secret-4085781963
  ```

  Only a repository whose hook defines `Anvil-PreBuild` (§7.1) is affected; building, running, and tag reuse are not.
  Use Docker if you need build-time credentials on Windows.

- **The ignore file is passed explicitly.** buildah honours only an ignore file at the context root, so anvil passes
  `--ignorefile`. Without it the entire worktree, `target/` included, is streamed to the daemon on every build.

- **Rootless user-namespace mapping is not applied.** The run passes `--user` (§5.2) but not `--userns keep-id`, which
  rootless podman requires for bind-mount ownership to map back to the invoking user.

## 7. The hook

`.anvil/container/hooks.ps1` is a single optional PowerShell script supplying the two things anvil cannot derive:
credentials, and where a published image might be obtained. It is not emitted by default, since crates.io requires no
credentials and an empty script would be one more generated file to review.

It is loaded by path rather than by provenance: the recipe dot-sources it whenever the file exists, whether a
repository wrote it or a catalog shipped it, so a repository can adopt a credential flow without forking the catalog.

The script may define up to three independent functions. All are optional, and each is called only if defined.

| Function | Invoked | Returns |
| --- | --- | --- |
| `Anvil-PreBuild` | before a build | `@{ Secrets = @{ <id> = <value> } }` |
| `Anvil-PreRun` | before a run | `@{ Env = @{ <NAME> = <value> } }` |
| `Anvil-ResolveImage $tag` | before a build, when no local image matches | an image reference, or nothing |

Both value-returning functions **fail closed on an empty value**, which the engine does not: BuildKit accepts
`--secret id=t,env=UNSET`, mounts an empty secret and exits 0, so the build would install a reduced tool set, be
tagged with the digest a credentialed build produces, and be reused by every later run.

Credentials are passed to the engine **by name** in both phases, as a `--secret … env=` reference at build time and
`-e <NAME>` at run time, so a value never appears in a process command line, where endpoint telemetry records and
retains it far longer than a short-lived token is intended to live. The variables are removed once the engine call
returns, and when the engine is reached through WSL the names are exported through `WSLENV` so the values cross that
boundary.

### 7.1 Anvil-PreBuild

Each entry becomes a BuildKit `--secret id=<id>,env=ANVIL_SECRET_<id>` mount, which BuildKit keeps out of every image
layer.

```powershell
function Anvil-PreBuild {
    @{ Secrets = @{ feed_token = (az account get-access-token --resource … --query accessToken -o tsv) } }
}
```

Minting the value inside the function is the point: a short-lived token must be acquired when it is used, not read
from a committed file or a declared variable.

Declare the mount as required in the Dockerfile, closing the same gap from the build's side:

```dockerfile
RUN --mount=type=secret,id=feed_token,required=true \
    TOKEN="$(cat /run/secrets/feed_token)" …
```

Anything the build *writes* using a secret is ordinary layer content. The default Dockerfile removes
`credentials.toml` and `.netrc` in the same `RUN` layer as the install; a replacement must do the same, or the
credential is baked into a layer that a later deletion cannot remove.

### 7.2 Anvil-PreRun

Each entry is forwarded with `-e <NAME>` and is an ordinary environment variable inside the image. The forwarded
names, never their values, are echoed to stderr, because everything executing in the container can read them.

```powershell
function Anvil-PreRun {
    @{ Env = @{ CARGO_REGISTRIES_INTERNAL_TOKEN = (mint-a-token) } }
}
```

### 7.3 Anvil-ResolveImage

When no local image matches the computed tag, the reference is offered to `Anvil-ResolveImage` before a build starts,
ahead of the `NO_REBUILD` guard, since fetching a published image is not building one. A catalog that publishes images
implements it; without one, the build proceeds.

```powershell
function Anvil-ResolveImage($tag) {
    $remote = "myregistry.azurecr.io/anvil:$($tag.Split(':')[-1])"
    az acr login --name myregistry | Out-Null
    docker pull $remote | Out-Null
    if ($LASTEXITCODE -eq 0) { $remote }
}
```

Three properties are load-bearing:

- **The returned reference is used as-is, never re-tagged locally.** A local tag asserts "built here from these
  inputs"; a fetched image only claims it (§4.3). Keeping the registry reference keeps the run honest about origin.
- **The reference is verified before use.** Runs pass `--pull=never`, so a hook reporting an image it had not actually
  fetched would otherwise fail later and further from the cause.
- **Every failure falls through to a local build**, with the reason printed. A publisher that has not caught up with a
  change must not block the developer who made it.

### 7.4 Trust boundary

The hook executes on the host, with the invoking user's permissions, before any container isolation exists. Only use
one from a repository or catalog you trust.

Inside the container everything executes as one user in one mount namespace, so a forwarded credential is readable by
anything the checks execute, including dependency build scripts and procedural macros. Keep the forwarded set narrow
and the tokens short-lived.

## 8. Customization

A **repository** changes what its own image contains; a **downstream catalog** (an anvil fork, see
[extensibility.md](./extensibility.md)) changes what every repository it manages receives. Containerized execution is
an ordinary artifact group and uses the same levers as any other.

| Goal | Mechanism | Owner |
| --- | --- | --- |
| Extra packages in one repository | Edit `.anvil/container/Dockerfile` in place | repository |
| A different base OS or toolchain source, everywhere | `replace_artifact(artifacts::container::dockerfile().with_body(…))` | catalog |
| Credentials, or a published image | Add `.anvil/container/hooks.ps1`, or ship `artifacts::container::hooks(…)` | either |
| No containerized execution at all | `without_artifact` for each of the three artifacts | catalog |

Editing the Dockerfile in one repository is supported and the drift flow preserves the edit, but anvil keeps offering
its own version against a file it can see has diverged. A change that belongs everywhere is better made in a catalog.

**The Dockerfile and its ignore file must be replaced together.** A replacement that `COPY`s anything beyond
`justfiles/` and `rust-toolchain.toml` must also replace `artifacts::container::dockerignore()` (§3), or the added
paths never reach the build context and the build fails on a missing file. Recipes need no such care: `justfiles/` is
admitted as a directory and hashed recursively, so a new recipe subdirectory is copied and covered automatically.

`justfiles/anvil/` must contain `.just` recipes and nothing else, which `CatalogBuilder::build` enforces: a non-recipe
file there would be copied into the image without being part of its identity, so editing it would change the image's
contents without renaming the tag. Non-recipe assets belong in a tool-owned directory such as `.anvil/`.

A fork inherits everything else: the recipes, the identity scheme, the cache volumes, the mounts, and the re-entry
guard. A different base OS with a different toolchain source is one Dockerfile replacement plus one hook.

## 9. Limitations

- On ARM64 hosts the `linux/amd64` image is emulated and is substantially slower.
- The first build takes several minutes, installing a toolchain and the entire pinned tool catalog. Later runs reuse
  it until an input changes.
- Any edit under `justfiles/` invalidates the install layer, including files the image's synthetic Justfile never
  imports.
- anvil never pushes or promotes an image. It builds one, and will use one a hook fetched (§7.3); publishing belongs
  to whoever owns the registry.

## 10. Verification

The behaviour above needs a live daemon, so it cannot join `anvil-pr`. `scripts/test-anvil-container.ps1` covers it
end to end against a real engine, driving only the public surface: first build then reuse, a tag that changes with
an input and reverts, an edit surviving regeneration, a build secret that never reaches a layer, empty hook values
failing closed, resolve-then-verify, and the re-entry guard.

```powershell
./scripts/test-anvil-container.ps1                  # docker
./scripts/test-anvil-container.ps1 -Engine podman   # podman
```

What runs unattended is narrower: the unit tests in `artifacts::container` assert the driver's invariants against
the template text, and the snapshots pin the emitted files.

[design]: ./README.md
