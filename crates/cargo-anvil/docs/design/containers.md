# Containerized execution

Any command can be executed inside a Linux image built from the toolchain and tool versions the repository
pins:

```bash
just anvil-container just anvil-pr    # a tier
just anvil-container cargo build      # any other command
just anvil-container                  # interactive shell
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
  - [5.3 Environment](#53-environment)
  - [5.4 Re-entry](#54-re-entry)
- [6. Engines and host setup](#6-engines-and-host-setup)
  - [6.1 Engine resolution](#61-engine-resolution)
  - [6.2 Docker](#62-docker)
  - [6.3 Podman](#63-podman)
- [7. The hook](#7-the-hook)
  - [7.1 Anvil-BuildSecrets](#71-anvil-buildsecrets)
  - [7.2 Anvil-RunEnv](#72-anvil-runenv)
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

`anvil-container` takes the argv to run inside the image; every recipe continues to execute natively unless a
container is requested by name. Anvil recipes are reached by naming `just`, like any other command:

```bash
just anvil-container just anvil-pr
just anvil-container cargo build
```

Arguments are whitespace-delimited tokens. `just` joins a variadic `*command` with spaces before the recipe body sees
it, so the original argv is unrecoverable and an argument containing a space does not round-trip. Pass such a value
through the environment instead.

| Recipe | Behaviour |
| --- | --- |
| `just anvil-container <command…>` | Execute a command in the image. With no argument, opens an interactive shell. |
| `just anvil-container-tag` | Print the image reference for the current inputs. Builds nothing. |
| `just anvil-container-status` | Print the engine, working directory, image reference, and whether it is present. Never builds or pulls. |
| `just anvil-container-down` | Remove this repository's cache volumes. The image is retained. |

All four are annotated `[group("anvil-container")]` and appear as one cluster in `just --groups`. Each repeats a
one-line summary immediately above its attributes, because `just --list` takes the last comment line before them as the
description and would otherwise print the tail of a rationale paragraph as a fragment.

There is deliberately no `anvil-container-rebuild`. Its whole body would be `ANVIL_CONTAINER_NO_CACHE=1` followed by
the ordinary resolve, and that variable is already public below — where it also composes with `NO_REBUILD` and
`NO_RESOLVE`, which a recipe form does not.

What a recipe form would supply is **scope**: it sets the variable in its own process and exits, so exactly one build
ignores the cache. An exported variable is sticky, and every container command reads it, so a forgotten
`ANVIL_CONTAINER_NO_CACHE` rebuilds from scratch on each later invocation with nothing to indicate why. Scope it to
the one run:

```powershell
$env:ANVIL_CONTAINER_NO_CACHE = '1'
try { just anvil-container just anvil-fmt } finally { Remove-Item Env:ANVIL_CONTAINER_NO_CACHE }
```

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_ENGINE` | `docker` (default) or `podman`. Read at run time (§6.1). |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is absent, separating a cache miss from a build failure. |
| `ANVIL_CONTAINER_NO_RESOLVE=1` | Skip the resolve hook (§7.3), so a query never pulls. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild with `--no-cache` even when the tag resolves. Skips the resolve hook too (§7.3). |
| `ANVIL_IN_CONTAINER=1` | Set inside the image. Makes a nested invocation execute natively (§5.4). |
| `GITHUB_TOKEN` | Forwarded into the run. Taken from the host environment, or derived from the gh CLI for a target that reads it (§5.3). |

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
    ├── Dockerfile                         composed: anvil's six regions, your content in the gaps
    ├── Dockerfile.dockerignore            what the build context admits
    └── hooks.ps1                          optional; not emitted by default (§7)
```

`container.just` and `Dockerfile.dockerignore` are owned files carrying the usual `DO NOT EDIT DIRECTLY` marker.

The Dockerfile is **composed**, not owned: anvil maintains six managed regions inside it, and the repository owns
everything between them. Regions are updated in place on every run. Gap content is preserved byte-for-byte and is
never read, rewritten or reordered.

| Region | What anvil puts there | What belongs in the gap after it |
| --- | --- | --- |
| `anvil-container-header` | An orientation comment naming the regions and their gaps. | — |
| `anvil-container-base-image` | `ARG BASE_IMAGE`, pinned to a digest. | A second `ARG BASE_IMAGE=…` to build on a different base. |
| `anvil-container-base` | `FROM`, the version pins for `pwsh`, `just`, `rustup` and `cargo-binstall`, and the `ENV` block. | Anything the first network access needs: a root CA, `http_proxy`, an internal package mirror. |
| `anvil-container-tools` | System packages and those four tools. | Libraries a catalog tool needs to compile, for tools `binstall` has no prebuilt binary for. |
| `anvil-container-setup` | `COPY` of the recipe tree, then `just anvil-setup`. | Anything the repository's own checks need at run time. |
| `anvil-container-entry` | `ANVIL_IN_CONTAINER`, `WORKDIR`, `CMD`. | — |

Each gap sits at the only point in the build where its kind of addition works: a certificate has to land before the
first download, a run-time tool after the toolchain exists. That is what makes the image extensible without forking
the catalog — a repository adds to a gap and keeps receiving base-image and tool-pin updates, where a fork or an edit
inside a region freezes them.

Line 1 is `# syntax=docker/dockerfile:1` and belongs to no region. Anvil writes it when it creates the file and never
touches it again; a repository that needs a different frontend edits that line and owns it from then on.

**Why not an owned file.** An edited owned file is preserved and anvil's version is written to `.anvil-proposed`
(`updates.md` §2). There is no three-way merge and no recorded common ancestor, so each upgrade leaves two files to
reconcile by hand. For this file the consequence is silent: it carries the base digest and four tool pins, so a
repository that edits it once builds on a frozen base and frozen versions indefinitely, while `anvil-container-tag`
still resolves, because the tag hashes the file as it stands. Identity stays correct and the image stays stale.

**Regions are not write protection.** The ownership rules in `updates.md` §2 apply to a region body exactly as they do
to a file: an edit inside a region is preserved and produces a proposal rather than being overwritten. The gaps exist
so that editing a region is never the right way to add something.

**Overriding the base image.** `ARG BASE_IMAGE` and the `FROM` that consumes it are separate regions, so the override
is a gap edit rather than a region edit: a second `ARG BASE_IMAGE=…` in the gap wins, because a later declaration
replaces the default, and every pin the repository did not touch keeps updating. A base with an older glibc breaks
`binstall`, and moving the catalog to source installs is not a repository-level lever.

**Three properties a Dockerfile host requires that an order-independent TOML or line-set host does not:**

- **The parser directive must be line 1.** BuildKit honours `# syntax=…` only when nothing precedes it, not even a
  comment, and a region's opening sentinel is a comment. The directive therefore cannot be managed, and is instead the
  whole of the scaffold anvil writes when the file is absent. The scaffold is one line because anvil never reconciles
  it: anything placed there is uncorrectable on a repository that has already generated the file.
- **Region order is semantic.** `ARG BASE_IMAGE` precedes the `FROM` that consumes it, `FROM` precedes everything, and
  the toolchain exists before `anvil-setup` runs. Anvil compares the on-disk sequence with the declared one and refuses
  the file, naming the region that is out of place, rather than writing a Dockerfile that cannot build.
- **A missing region is spliced at its declared position, not appended.** Appending suits every other host; here a
  region introduced in a later release would land after ones it must precede. Anvil inserts it after the nearest
  preceding region present in the file, or directly below the scaffold, leaving gap content untouched.

Anvil classifies an existing Dockerfile before writing to it. A file the lock records as an owned file and that
carries no regions is a render from a version that owned the whole path; it is replaced. A file carrying every region
is composed and is updated in place. Anything else — a Dockerfile the repository wrote itself, or one whose regions
have been removed — is refused, because there is no position for the regions that would not place existing content
above `FROM`. The refusal names the file and the recovery; nothing is written to it.

The image installs its tools by running `just anvil-setup`, the same recipe the checks use, reading the same
generated pins. There is no second tool list to keep synchronized, and consequently a tool-pin change renames the
image (§4.1).

`Dockerfile.dockerignore` scopes the build context to `justfiles/anvil/` and `rust-toolchain.toml`, denying everything
else. The whole recipe tree is copied because `just` has to parse it to run `anvil-setup`, and the whole tree is
hashed (§4). BuildKit reads `<dockerfile>.dockerignore` in preference to a root `.dockerignore`, so the
repository neither needs to own a root ignore file nor can have one silently override this.

## 4. Image identity

The image reference is `anvil-<repo>:<16 hex characters>`, where the tag is a SHA-256 digest over the inputs that
define the image. The name derives from the repository directory (§5.1).

### 4.1 Inputs

| Input | Hashed |
| --- | --- |
| every file under `.anvil/container/` | always |
| `rust-toolchain.toml` | always |
| every file under `justfiles/anvil/` | always |

`.anvil/container/` is hashed by walking it, not as a fixed list of three known files. The Dockerfile is composed, so a
repository can `COPY` something from one of its gaps — a root CA, an install script, a patch — and a downstream
catalog's replacement region can do the same. Naming only the files anvil happens to know about would let any of those
change the image under a reference that already resolves, which is the hole the digest exists to close. A missing
Dockerfile is still a hard error, checked by name: the walk alone would let it contribute nothing and yield a confident
tag for an image that cannot be built.

The recipe tree is hashed in full. `just anvil-setup` reaches the install recipes through the tier, group and check
recipes, so the routing decides *whether* a tool is installed just as surely as `tools.just` decides *how*: dropping an
`anvil-<check>-setup` dependency from a group changes the installed set while `tools.just` and `versions.just` stay
byte-identical. Hashing only the install definitions would leave that change unnamed, and the tag would claim contents
the image does not have.

`container.just` is hashed too. It is not circular — the digest is over file text, and no file contains the tag — and
it belongs in the set because it passes the build arguments, the secret mounts and the hook's `Anvil-BuildSecrets` output
into the build.

The cost is that editing any recipe renames the image and the next run rebuilds it. That is the correct trade: a tag
that can name contents the image does not have makes every guarantee below meaningless.

The hook file's **content** is an input, since it determines what the build installs. Its **output** is deliberately
excluded: a credential must never influence a tag.

### 4.2 Digest

Inputs are sorted by relative path with an ordinal comparison, then serialized into one stream in which each entry
contributes a literal `file`, the UTF-8 byte length of its relative path, the path, the UTF-8 byte length of its
content, and the content. Length-prefixing the two variable-length fields is what makes the stream self-delimiting:
newline framing would let a file whose body happened to contain `file`, a path and a newline serialize identically to
two files splitting at that point, so two different input sets could name one image. The lengths are byte counts of
the same UTF-8 encoding the stream is hashed in, so an independent re-implementation arrives at the same bytes.
Tagging entries this way
prevents a rearrangement of names and contents from colliding. Line endings are normalized to LF for `.just` recipes
and the declared text inputs, so those agree across a CRLF and an LF checkout; every other file the walk admits is
hashed as the bytes the build context copies, because bytes are what `COPY` puts in the layer. The sort is ordinal
because a case-insensitive one would drop one of two inputs differing
only in case on the case-sensitive filesystem where the image is built.

Anvil's own `.anvil-proposed` review siblings are excluded from both the digest and the build context. They are
written beside a host when a template moves under a customized region, and they cannot reach the image, so digesting
one would rename it for as long as the proposal went undismissed.

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

A third sits outside it by necessity: the `apt-get install` layer names packages without versions, and Ubuntu's
archive moves. Two clean builds of a byte-identical Dockerfile weeks apart can therefore install different package
versions under one tag. Pinning every apt version would trade this for a harder failure, since the archive drops
superseded versions and the build would simply stop working. So the guarantee the tag gives is precise: **the inputs
that define the image are fixed, and everything anvil itself installs is version-pinned** — the toolchain, the tool
catalog, `just`, `pwsh`, `rustup` and `cargo-binstall`, each with a checksum. The system packages beneath them track
the base distribution. `ANVIL_CONTAINER_NO_CACHE=1` forces a from-scratch rebuild when that distinction matters.

The base tracks the Linux runner the generated workflows use, `ubuntu-latest` (currently 24.04). The catalog is
installed with `binstall`, and those prebuilt binaries require that runner's glibc, which is backward but not forward
compatible. A catalog on an older base installs from source instead.

## 5. Execution model

One container is created per `anvil-container` invocation, however many checks the requested recipe runs, and is
removed on exit (`--rm`).

### 5.1 Mounts and working directory

| Mount | Target | Purpose |
| --- | --- | --- |
| repository root (bind) | `/workspace` | The worktree under test, including `target/`. |
| common git directory (bind, linked worktrees only) | `/anvil/gitdir` | Git history, when the checkout does not carry it. |
| `anvil-<repo>-cargo-registry` (volume) | `/usr/local/cargo/registry` | Downloaded crate sources. |
| `anvil-<repo>-cargo-git` (volume) | `/usr/local/cargo/git` | Git checkouts of git dependencies. |

A linked worktree (`git worktree add`) keeps its git directory outside the checkout and stores an absolute host path
in `.git`, which does not exist inside the container. The recipe resolves this while assembling the run, before the
container starts: it compares `git rev-parse --git-dir` against `--git-common-dir`, and when they differ it adds a
bind mount for the common directory and a second one placing a generated `.git` file over the checkout's own, naming
that mount. Git then resolves the history by ordinary discovery. This is what lets the checks that read history — the
impact-scoped filters, `anvil-mutants-diff`, `anvil-semver-check` — work from a worktree at all; without it git
resolves nothing inside the container and each of them fails a long way from the cause.

The redirection is confined to the checkout: a git command run elsewhere in the container, such as `git init` in a
scratch directory, is unaffected. That is why a generated `.git` file is used rather than `GIT_DIR`, which is ambient
and would be inherited by every process in the container. An ordinary clone carries its git directory inside the bind
mount and takes none of this. The generated `.git` file is written to the host temp directory, bind-mounted from
there, and removed when the run ends; nothing is written into the checkout, and no flag or variable selects the
behaviour.

Only cargo's content-addressed download caches are volumes, so the write-heavy download path never crosses the host
boundary and the host's own toolchain is untouched. `$CARGO_HOME` and `$RUSTUP_HOME` themselves are **not** mounted:
they carry the installed tools and toolchains, and an engine populates a named volume from the image only when that
volume is first created. Mounting them would pin the first image's binaries over every later one, so a tool bump
would change the tag, build a new image, and still run the old tools — defeating the identity guarantee in §4.
Tools and toolchains therefore always come from the image layer the tag names.

`target/` stays on the bind mount, shared with the host and visible from it. A native run and a containerized run write
incompatible artifacts to the same paths, so switching between them recompiles the workspace. Giving the container its
own build directory through `CARGO_TARGET_DIR` avoids that but breaks `cargo-semver-checks`, which builds a baseline
and the current crate and then cannot find its rustdoc output; the recompilation is the lesser cost.

The caller's working directory is mapped to its in-container equivalent, so relative paths resolve when
`anvil-container` is invoked from a subdirectory.

Image and volume names derive from the repository directory name, lowercased with every run of characters outside
`[a-z0-9]` replaced by a single `-` and any trailing `-` removed: a checkout in `ox-tools (copy)` becomes
`anvil-ox-tools-copy` rather than ending in a separator, which the engine rejects. A directory name with no `[a-z0-9]`
character at all degrades to plain `anvil` — still a valid reference, but no longer repository-specific. Two checkouts
with the same directory name therefore share cache volumes. That is harmless, since both volumes hold only
content-addressed downloads, but `anvil-container-down` then removes volumes the other checkout is also using.

### 5.2 Process identity

On a Linux host the run passes `--user <uid>:<gid>`, matching the invoking user, unless that user is root. Without it,
everything written under the bind mount is owned by root on the host, and the next native `cargo build` or `git clean`
fails with `EACCES` far from the cause. Docker Desktop on Windows and macOS maps ownership itself, and `id` is not
available to query, so the flag is not passed there. That uid has no `passwd` entry, so `HOME` is set to `/tmp`;
otherwise the engine leaves it as `/`, and anything falling back to `$HOME` writes to a read-only root.

### 5.3 Environment

The run passes `ANVIL_IN_CONTAINER=1` (§5.4) and forwards `GITHUB_TOKEN` by name, resolved the way the recipe resolves
it natively: the environment first, then the gh CLI's stored token. `anvil-aprz` runs in the `scheduled-advisories` group and
queries the GitHub advisory API, which allows 60 requests an hour unauthenticated and then sleeps until the quota
resets, so a tier needs the token to terminate rather than merely to run quickly.

The two sources are not treated alike. An **exported** `GITHUB_TOKEN` is forwarded whatever the target is — that is
exact parity, since a native run exposes it to every process the shell spawns too. A token **derived** from the gh CLI
is a credential the developer never put in this environment, and PID 1's environment is inherited by every build script
and proc macro in the container, where natively `anvil-aprz` would mint it inside its own process. So it is derived
only when the target's plan (`just --dry-run <target>`) reads `GITHUB_TOKEN`, or when there is no target at all: an
interactive session can run anything, and refusing there would reintroduce the stall the token exists to prevent. The
predicate is the variable rather than the name of a check, so a catalog that adds another GitHub-authenticated check is
covered without touching the driver.

It also forwards the recipe contract's own inputs when they are set — `PR_TITLE`, `BASE_REF`, `GITHUB_BASE_REF`,
`SYSTEM_PULLREQUEST_TARGETBRANCH` and the `ANVIL_INCLUDE_*` filters — because a check that reads one natively must read
the same value in a container. `anvil-pr-title` is the sharp case: with `PR_TITLE` unset it exits 0 with a skip notice,
so dropping it at the boundary would let a title a native run rejects pass in a container while the tier still reported
green. They are forwarded by name and only when set, so an unset variable stays unset rather than arriving empty.

A resolved token is set on the driver process, passed by name, and unset after the run, so it never reaches a host
command line. Inside the container it is readable by everything the run executes, including build scripts and proc
macros.

Everything else a run needs comes from the hook (§7).

### 5.4 Re-entry

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

  Only a repository whose hook defines `Anvil-BuildSecrets` (§7.1) is affected; building, running, and tag reuse are not.
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
| `Anvil-BuildSecrets` | before a build | `@{ Secrets = @{ <id> = <value> } }` |
| `Anvil-RunEnv` | before a run | `@{ Env = @{ <NAME> = <value> } }` |
| `Anvil-ResolveImage $tag` | before a build, when no local image matches | an image reference, or nothing |

Both value-returning functions **fail closed on an empty value**, which the engine does not: BuildKit accepts
`--secret id=t,env=UNSET`, mounts an empty secret and exits 0, so the build would install a reduced tool set, be
tagged with the digest a credentialed build produces, and be reused by every later run.

Credentials are passed to the engine **by name** in both phases, as a `--secret … env=` reference at build time and
`-e <NAME>` at run time, so a value never appears in a process command line, where endpoint telemetry records and
retains it far longer than a short-lived token is intended to live. The variables are removed once the engine call
returns, and when the engine is reached through WSL the names are exported through `WSLENV` so the values cross that
boundary.

### 7.1 Anvil-BuildSecrets

Each entry becomes a BuildKit `--secret id=<id>,env=ANVIL_SECRET_<id>` mount, which BuildKit keeps out of every image
layer.

```powershell
function Anvil-BuildSecrets {
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

### 7.2 Anvil-RunEnv

Each entry is forwarded with `-e <NAME>` and is an ordinary environment variable inside the image. The forwarded
names, never their values, are echoed to stderr, because everything executing in the container can read them.

```powershell
function Anvil-RunEnv {
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
- **The reference is checked for presence before use.** Runs pass `--pull=never`, so a hook reporting an image it had
  not actually fetched would otherwise fail later and further from the cause. This is a presence check, not a
  verification: `image inspect` proves something carries that reference, not that its contents match the digest the tag
  claims. Trusting the publisher is the contract (§4.3).
- **Every resolve failure falls through to a local build**, with the reason printed — including a hook that cannot be
  loaded at all, which is why the dot-source sits inside the same `try`. A publisher that has not caught up with a
  change must not block the developer who made it.

  That tolerance is scoped to *resolution*. The build and run phases load the same file again to obtain credentials
  (§7.1, §7.2) and are deliberately fail-closed, so a `hooks.ps1` that cannot be parsed stops the run there instead —
  after resolution has already forgiven it. The two are not in conflict: a hook that yields no image costs nothing,
  while a hook that cannot yield its credentials would otherwise produce an image built without them and tag it as if
  it had them.

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
| Extra packages, or another base image, in one repository | Add them in the matching gap in `.anvil/container/Dockerfile` (§3) | repository |
| A different base OS, everywhere | `replace_artifact(artifacts::container::dockerfile_base_image().with_body(…))`, usually with `dockerfile_tools()` | catalog |
| Credentials, or a published image | Add `.anvil/container/hooks.ps1`, or ship `artifacts::container::hooks(…)` | either |
| No containerized execution at all | `without_artifact` for each artifact in the group | catalog |

A repository adds to the Dockerfile without editing anything anvil owns, so pin bumps keep landing (§3). A change that
belongs everywhere is still better made in a catalog, where every consumer gets it.

Replacing a *region* rather than the whole file is what makes a downstream catalog cheap to keep current:
`dockerfile_setup()` and `dockerfile_entry()` are the contract with the driver and are inherited, so an Azure Linux or
msrustup catalog rewrites the base and tool layers and nothing else. Replacing `dockerfile_setup()` reintroduces the
second tool list the design exists to avoid, and is almost never right.

**A replacement must keep the ignore file in step.** A region that `COPY`s anything beyond `justfiles/anvil/` and
`rust-toolchain.toml` must also replace `artifacts::container::dockerignore()` (§3), or the added paths never reach the
build context and the build fails on a missing file.

**Anything extra it copies is digested, provided it lives under `.anvil/container/`.** The hashed set is that whole
directory (§4.1), so an installer script, a config file or a certificate placed beside the Dockerfile is an input:
editing it renames the tag and the next run rebuilds. Content copied from elsewhere in the repository is not, and the
tag will not move when it changes — keep it under `.anvil/container/` and the identity guarantee holds without a
manual `ANVIL_CONTAINER_NO_CACHE=1`.

`justfiles/anvil/` must contain `.just` recipes and nothing else, which `CatalogBuilder::build` enforces for
catalog-owned files. The reason is legibility rather than identity: the directory is the recipe tree, `just` parses
every file the image copies, and a catalog that hides an installer script there makes the tool set harder to reason
about than one that keeps it in `.anvil/`. Identity is safe either way, because the digest covers every file the build
context admits (§4.1), not only the recipes — a repository that adds a non-recipe file by hand still renames the tag
when it edits it.

A fork inherits everything else: the recipes, the identity scheme, the cache volumes, the mounts, and the re-entry
guard. A different base OS with a different toolchain source is two region replacements plus one hook.

## 9. Limitations

- On ARM64 hosts the `linux/amd64` image is emulated and is substantially slower.
- The first build takes several minutes, installing a toolchain and the entire pinned tool catalog. Later runs reuse
  it until an input changes.
- Any edit under `justfiles/anvil/` renames the image and rebuilds it, including edits to a check body that cannot
  change what the image contains. Precision here would mean deriving the install closure rather than hashing the files
  that express it; until then the digest errs towards rebuilding, because the alternative error — a tag that names
  contents the image does not have — is silent (§4.1).
- **The set of hashed inputs is fixed (§4.1) and a fork cannot extend it.** A replacement Dockerfile is itself
  hashed, so changing the build recipe always renames the tag — but any *additional* file it copies is outside the
  tag. Such a file can change what a build produces while naming a tag that already resolves, and the existing image
  is then reused, so the change is never built. A fork that needs extra content should carry it in the Dockerfile
  itself, or accept that edits to it require `ANVIL_CONTAINER_NO_CACHE=1`.
- anvil never pushes or promotes an image. It builds one, and will use one a hook fetched (§7.3); publishing belongs
  to whoever owns the registry.

[design]: ./README.md
