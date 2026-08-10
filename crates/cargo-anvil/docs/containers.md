# Containers

`cargo-anvil` can run your generated `just` recipes inside a container, build OCI images, and stand up a throwaway
Kubernetes cluster for integration tests.

All three are **opt-in and independent**. They are configured in a single file — `anvil.toml` at the repo root — which is
itself optional. A repository with no `anvil.toml` regenerates byte-for-byte identically to a build with no container
support at all, so adopting nothing costs nothing.

- [1. Quick start](#1-quick-start)
- [2. Usage](#2-usage)
- [3. Configuration reference](#3-configuration-reference)
- [4. How it works](#4-how-it-works)
- [5. Extension points](#5-extension-points)
- [6. Prerequisites and limits](#6-prerequisites-and-limits)

## 1. Quick start

Create `anvil.toml` in the repo root:

```toml
[container]
enabled = true
```

Re-run the generator and start working:

```bash
cargo anvil                  # regenerate; now emits the container recipes
just anvil-pr                # builds the image on first use, then runs inside it
```

That is the whole adoption story for containerized execution: no image to publish, no registry to reach, no Dockerfile
to write. Anvil generates `.anvil/container/Dockerfile`, which installs the toolchain and tools this repository
already pins, and builds it the first time it is needed.

Point `image` at a pre-built reference to pull one instead, or `dockerfile` at your own to build something else — see
[`[container]`](#container--containerized-execution). Image builds and the cluster harness are added by declaring
`[[image]]` and `[cluster]` sections.

## 2. Usage

Everything is a `just` recipe. `cargo anvil` writes the recipes; it is never on the runtime path.

### Containerized execution

Emitted when `[container] enabled = true`.

| Recipe | Purpose |
| --- | --- |
| `just anvil-container-up` | Make the image available so the next run starts warm: build it, or pull it if `image` is set. |
| `just anvil-container-status` | Report the engine, workdir, image reference, and whether it is present and current. |
| `just anvil-container-shell` | Interactive shell in the image with the same mounts a recipe run uses. |
| `just anvil-container-rebuild` | Rebuild from scratch, ignoring every cached layer. |
| `just anvil-container-down` | Remove this repo's cache volumes. The image is left in place. |

You rarely need `anvil-container-up`: every recipe that needs the image resolves it first, and builds it if it is
missing.

### Staying current

When the image is built locally, its tag **is** a hash of the inputs that define it:

```text
anvil-<repo>:<16 hex characters>
```

Change a pinned tool version, the toolchain, or the Dockerfile, and the tag changes to one that cannot already exist —
so the next run builds it. Change nothing, and the tag resolves instantly. There is no staleness check because there is
nothing to check: an image that is present is, by construction, an image built from the current inputs.

The inputs are the Dockerfile and its ignore file, `rust-toolchain.toml`, the resolved `build-args`, anything listed in
`hash-inputs`, and the generated recipe tree — because the image installs its tools by running `just anvil-setup`,
whose dependency chain reaches the tier, group, check and tool files. Only the recipes that drive the container from
the host are held back, `container.just` above all: it is the file that computes the hash.

With `extends` there are two tags, and only the one you changed rebuilds:

```text
anvil-<repo>:<hash>        base       toolchain + tool catalog   minutes to build
anvil-<repo>-ext:<hash>    extension  your additions             seconds to build
```

The base tag is folded into the extension's hash, so a rebuilt base always renames the extension. `anvil-container-status`
reports which of the two is missing, since that decides how long the next run takes.

| Variable | Effect |
| --- | --- |
| `ANVIL_CONTAINER_BASE_IMAGE` | Build on a different base. Must be digest-pinned (`image@sha256:…`); participates in the hash. |
| `ANVIL_CONTAINER_NO_REBUILD=1` | Fail instead of building when the image is missing. Distinguishes a cache miss from a build failure in CI. |
| `ANVIL_CONTAINER_NO_CACHE=1` | Rebuild even when the tag resolves. This is what `anvil-container-rebuild` sets. |

Build secrets are excluded from the hash: a secret's value must never influence a tag.

The tier and group recipes (`anvil-pr`, `anvil-scheduled`, `anvil-full`, and each `anvil-pr-*` / `anvil-scheduled-*`
group) gain a re-entry guard, so the command you already use is unchanged:

```bash
just anvil-pr      # transparently re-invoked inside the container
```

Set `ANVIL_IN_CONTAINER=1` on the host to force a single invocation to run natively — the escape hatch when you need to
bypass the container without editing config.

### Image builds

Emitted when at least one `[[image]]` is declared.

| Recipe | Purpose |
| --- | --- |
| `just anvil-image <name>` | Build one declared image. With no name, list the declared images. |
| `just anvil-images` | Build every declared image in dependency order. |

Every image is built by the same logic, so the generated file holds **one** recipe body plus a table of per-image data
(`dockerfile`, `context`, `target`, `stage-artifacts`, `build-args`, `repository`). An unknown name is rejected with the
valid list.

Both take the same three optional parameters, after the name where there is one:

```bash
just anvil-image <name> <profile> <tag> <registry>   # defaults: debug dev local
just anvil-images       <profile> <tag> <registry>
```

`profile` selects which prebuilt binaries are staged, `tag` is the image tag, and `registry` is the ref prefix. The final
reference is `<registry>/<name|repository>:<tag>`.

### Cluster harness

Emitted when a `[cluster]` section is declared.

| Recipe | Purpose |
| --- | --- |
| `just anvil-cluster-preflight` | Verify the engine and `kind` / `kubectl` / `helm` are present. Installs nothing. |
| `just anvil-cluster-bootstrap` | Install the pinned, checksum-verified cluster tooling on the host. |
| `just anvil-cluster-up` | Create the cluster if absent. Idempotent. |
| `just anvil-cluster-load` | Load the declared images into the cluster. |
| `just anvil-cluster-deploy` | Apply dependencies, then install or upgrade the declared charts. |
| `just anvil-cluster-test` | The full flow with bounded retries — the one CI calls. |
| `just anvil-cluster-diagnostics` | Dump the configured resources and logs. Runs automatically on failure. |
| `just anvil-cluster-down` | Delete the cluster. |
| `just anvil-cluster-clean` | `mode=cluster` deletes the cluster; `mode=full` also removes the built images. |

`up`, `load`, `deploy`, and `test` take the same `profile` / `tag` / `registry` parameters as the image recipes, so the
references loaded into the cluster always match the references built.

## 3. Configuration reference

`anvil.toml` has four sibling top-level sections plus one bare key. Each is independently optional. Unknown keys are a
hard error, so a typo is reported rather than silently ignored.

Because `image-output-dir` is a bare key, TOML requires it to appear **before** any `[section]` header.

### `[container]` — containerized execution

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch for containerized execution. |
| `image` | string | — | Pre-built image to pull and run recipes in. |
| `dockerfile` | string | — | Repo-relative Dockerfile that **replaces** anvil's and builds the image alone. |
| `extends` | string | — | Repo-relative Dockerfile layered **on top of** anvil's image. |
| `build-args` | table | `{}` | `--build-arg` pairs for the build. Part of the image identity. **Never put a credential here** — see below. |
| `build-secrets` | array | `[]` | BuildKit `--secret` specifications. Excluded from the image identity. Anvil refuses to build when a declared source is unset or empty. |
| `hash-inputs` | array | `[]` | Extra files that define the image identity — anything the Dockerfile `COPY`s. |
| `engine` | `auto` \| `docker` \| `podman` | `auto` | `auto` probes for `docker`, then `podman`. |
| `name` | string | directory name | Repo identity; prefixes cache-volume names so repos on one host do not collide. |
| `workdir` | string | `/workspaces/<name>` | Mount point for the repo root inside the container. |
| `cache-volumes` | array | `["cargo", "rustup"]` | Named volumes to persist across runs. |
| `forward-env` | array | `[]` | Glob patterns; matching host environment variables are forwarded in. Forwarded **by name** — the engine copies the value, so it never reaches a command line. The matched names are printed to stderr on each run. |
| `devcontainer` | bool | `false` | Also emit `.devcontainer/devcontainer.json` from the same settings. |
| `native-when` | table | — | Host match that runs natively instead of containerizing. |

#### Choosing where the image comes from

`image`, `dockerfile` and `extends` are alternatives. Setting none is the ordinary case:
anvil generates `.anvil/container/Dockerfile` and builds it. Setting two is an error —
"pull this", "build that" and "build on top of mine" cannot all be the answer, and
quietly preferring one would hide the mistake.

| You want | Use | Anvil's Dockerfile |
| --- | --- | --- |
| The default environment | *(nothing)* | generated and built |
| An image you already publish | `image` | not emitted |
| Anvil's tools **plus your own** | `extends` | generated, built, and used as the base |
| A different base OS entirely | `dockerfile` | not emitted |

Prefer `extends` over `dockerfile` whenever the OS is the same. `dockerfile` makes you
re-implement the toolchain, tool catalog and PowerShell install, and re-do it on every
anvil upgrade; `extends` inherits all of that.

#### Credentials

A private feed needs a credential in two places: during the image build, to install the
tool catalog, and at run time, to fetch the workspace's own dependencies.

**Build time — use `build-secrets`, never `build-args`.** A build-arg value is folded into
the image identity and written verbatim into files your repository commits: the generated
`container.just`, and `.devcontainer/devcontainer.json` when `devcontainer = true`. A
credential placed there ends up in git. Anvil rejects build-arg *names* that look like
credentials (`*TOKEN*`, `*SECRET*`, `*PASSWORD*`, `*CRED*`, `*PAT`), but that is a guard
against the obvious mistake, not a control — it cannot know that `FEED_AUTH` holds a
bearer.

Anvil refuses to build when a `build-secrets` entry names an environment variable or file
that is unset or empty. BuildKit does not: it mounts an empty file and the build proceeds,
which would install a reduced tool set and tag the result with the *same* content hash a
credentialed build produces — so every later run would reuse the broken image. Also write
`required=true` on the mount, which closes the same hole from the Dockerfile's side:

```dockerfile
RUN --mount=type=secret,id=feed_token,required=true \
    TOKEN="$(cat /run/secrets/feed_token)" ... 
```

Anything the build *writes* with a secret is ordinary content. Anvil's own Dockerfile
deletes `credentials.toml` and `.netrc` in the same layer as the install; a Dockerfile you
own must do the same, or the credential is baked into a layer.

**Run time — use `forward-env`.** Matching variables are forwarded by name, so the engine
copies the value out of the environment it already inherits and the credential never
appears in a host command line. The matched names are printed to stderr so a broad pattern
does not silently hand extra variables to a process that runs third-party build scripts.

What this does **not** do: everything inside the container runs as one user in one mount
namespace, so a forwarded credential is reachable by any code the checks execute,
including dependency build scripts and proc macros. Keep `forward-env` patterns narrow,
and keep the token short-lived.

Choosing any one of them replaces the choice wholesale rather than merging with it, so a
catalog default naming a pre-built image cannot collide with a repository that decides to
build its own.

`native-when` accepts `os-release-id` and `version-id`, matched against `/etc/os-release`. When the host matches, recipes
run directly — useful when the host already *is* the target environment.

Cache volume names map to conventional in-container paths: `cargo` and `rustup` to the official Rust image's
`CARGO_HOME` and `RUSTUP_HOME`, `target` to `<workdir>/target`, and any other name to `/anvil-cache/<name>`.

#### `extends` — add tools to anvil's image

```toml
[container]
enabled = true
extends = "ci/substrate.Dockerfile"
build-secrets = ["id=feed_token,env=FEED_TOKEN"]
hash-inputs = ["ci/install-internal-tools.sh"]
```

```dockerfile
ARG ANVIL_BASE_IMAGE          # injected by anvil; do not give it a default
FROM ${ANVIL_BASE_IMAGE}

RUN --mount=type=secret,id=feed_token ./ci/install-internal-tools.sh
```

Anvil resolves and builds its own image first, then builds yours with the resolved
reference injected as `ANVIL_BASE_IMAGE`. You cannot write that reference yourself: it is
a content tag, so it is not knowable until it is computed.

Leave `ARG ANVIL_BASE_IMAGE` without a default. BuildKit warns about that
(`InvalidDefaultArgInFrom`), and the warning is expected: a default would let the build
quietly succeed against the wrong base if the injection ever failed, which is worse than
a noisy line.

This produces **two** images with **two** identities:

| Image | Changes when | Cost |
| --- | --- | --- |
| `anvil-<name>:<hash>` | the toolchain, tool pins or anvil's Dockerfile change | minutes |
| `anvil-<name>-ext:<hash>` | your Dockerfile, `build-args` or `hash-inputs` change | seconds |

The base tag is folded into the extension's hash, so a rebuilt base always renames the
extension — the two can never drift apart. The reverse does not hold: editing your layer
leaves the expensive base cached, which is the point.

`build-args`, `build-secrets` and `hash-inputs` describe **your** layer, not the base.
Applying them to the base would rebuild the expensive half for a change that only
concerns the cheap one.

`devcontainer = true` is rejected with `extends`: a descriptor names one image or one
build, and cannot express a base plus a layer.

#### `dockerfile` — replace anvil's image

```toml
[container]
enabled = true
dockerfile = "ci/anvil.Dockerfile"
build-args = { RUST_CHANNEL = "1.93" }
build-secrets = ["id=feed_token,env=FEED_TOKEN"]
hash-inputs = ["ci/install-tools.sh"]
cache-volumes = ["cargo", "rustup", "target"]
forward-env = ["CARGO_*", "RUST_LOG"]
```

Reach for this only when the base OS itself has to differ — a different package
ecosystem cannot be layered onto Debian with `extends`.

Keep `ARG BASE_IMAGE` at the top and pin it by digest, so `ANVIL_CONTAINER_BASE_IMAGE`
keeps working and the identity hash stays meaningful. Anvil does not emit its own
Dockerfile in this mode: a generated file nothing reads is an invitation to edit the
wrong one.

### `[[image]]` — locally built OCI images

Repeat the table once per image.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | — | Required. Must match `[A-Za-z][A-Za-z0-9_-]*` — it selects the image in `just anvil-image <name>`. |
| `repository` | string | `name` | Published path when it differs from the name (a `just` argument is fine with `/`, but the name is also the default repository). |
| `dockerfile` | string | — | Required. Path to the Dockerfile. |
| `target` | string | — | Build stage to target in a multi-stage Dockerfile. |
| `context` | string | — | Build context. Must be nested under `image-output-dir`. |
| `stage-artifacts` | array of tables | `[]` | `{ from, to }` copies made into the staged context before the build. |
| `build-args` | table | `{}` | Build arguments, in declaration order. |
| `depends-on` | array | `[]` | Other image names that must build first. |

Set the guard root with the bare `image-output-dir` key (default `out`).

Every image is built from a **staged context**, never the repo root: prebuilt binaries are copied in by
`stage-artifacts`, and nothing is compiled in-image. A `context` outside `image-output-dir` is rejected, so the whole
repository can never be sent to the engine as build context.

`stage-artifacts` and `build-args` values may use the `{profile}` and `{tag}` tokens, expanded from the recipe
parameters at run time.

`depends-on` is validated: unknown targets and cycles are hard errors, and the build order is a deterministic
topological sort with declaration order breaking ties.

There is deliberately **no registry push, no authentication, and no promotion**. Images are built locally and consumed
locally.

```toml
image-output-dir = "out"

[[image]]
name = "api"
dockerfile = "docker/service.Dockerfile"
context = "out/api"
build-args = { RUST_PROFILE = "{profile}" }
stage-artifacts = [{ from = "target/{profile}/api", to = "api" }]
```

### `[cluster]` — ephemeral Kubernetes cluster

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | `anvil-kind` | Cluster name. |
| `node-image` | string | — | Pin the Kind node image. |
| `workers` | integer | `0` | Worker nodes in addition to the control plane. |
| `load-images` | array | `[]` | Which declared `[[image]]` names to load. Validated against the image set. |

Sub-tables:

| Table | Repeats | Keys |
| --- | --- | --- |
| `[[cluster.dependency]]` | yes | `name`, `manifest`, `version`, `namespace`, `preload-images`, `wait` |
| `[[cluster.chart]]` | yes | `name`, `path`, `namespace`, `crds`, `set`, `wait` |
| `[cluster.diagnostics]` | no | `resources`, `logs`, `namespace` |
| `[cluster.retry]` | no | `attempts` (default `1`), `delay-seconds` (default `0`) |
| `[cluster.hooks]` | no | `pre-install`, `post-install`, `pre-test`, `on-failure` |

Dependencies are external, pinned charts or manifests applied **before** your own charts. A chart's `crds` path is
installed and established before the chart itself, which is what makes a chart that depends on a custom resource install
reliably rather than racing a webhook that is not yet serving.

`wait` entries are readiness targets, passed to `kubectl rollout status` after the item is applied and resolved in the
enclosing dependency's or chart's `namespace`. Set that `namespace` — without it the wait resolves against `default` and
will appear to pass while the real workload is still starting.

```toml
[cluster]
name = "svc-test"
load-images = ["api"]

[[cluster.dependency]]
name = "cert-manager"
manifest = "https://github.com/cert-manager/cert-manager/releases/download/v1.16.1/cert-manager.yaml"
namespace = "cert-manager"
wait = ["deployment/cert-manager-webhook"]

[[cluster.chart]]
name = "api"
path = "charts/api"
namespace = "svc"
set = { "image.tag" = "dev" }
wait = ["deployment/api"]

[cluster.retry]
attempts = 3
delay-seconds = 10

[cluster.diagnostics]
namespace = "svc"
resources = ["pods", "events"]
logs = ["deployment/api"]
```

### `[anvil]` — partial adoption

By default anvil manages every artifact in its catalog. The `artifacts` allow-list narrows that to named groups:

```toml
[anvil]
artifacts = ["recipes", "container"]
```

| Group | Covers |
| --- | --- |
| `recipes` | The `justfiles/anvil/` tree and the `Justfile` imports region. |
| `config` | Managed regions in `Cargo.toml`, `deny.toml`, `rustfmt.toml`, and friends. |
| `backends` | The generated GitHub Actions / Azure DevOps files. |
| `container` | Everything documented on this page. |

This lets an established repository adopt container support without simultaneously handing over its lint and dependency
configuration.

- **Omitting the key selects every group** — identical to the behaviour before the allow-list existed.
- An unknown name is a hard error listing the valid groups; an empty list is an error too.
- Removing a group later cleanly retracts what it emitted: owned files anvil created are deleted and managed regions it
  spliced are removed. Nothing anvil never owned is touched.

Group selection **composes with** the existing gates, it never overrides them — an artifact is emitted only when its
group is selected *and* its own gate is open.

## 4. How it works

### Everything is baked at generation time

`cargo anvil` resolves your configuration and writes the resulting values directly into the emitted recipes. Nothing
re-reads `anvil.toml` at run time, so the shim, the image recipes, the cluster harness, and the devcontainer descriptor
cannot drift from each other or from the file that produced them. Changing configuration means re-running the generator.

```text
anvil.toml ──► cargo anvil ──► justfiles/anvil/container.just         (the shim)
                               justfiles/anvil/container-images.just  (per-image recipes)
                               justfiles/anvil/cluster.just           (cluster harness)
                               justfiles/anvil/cluster-bootstrap.just (host tooling)
                               .devcontainer/devcontainer.json        (optional)
                               ── plus a re-entry guard spliced into
                                  the tier and group recipes
```

### Layered configuration

Effective settings come from three layers, each overriding the previous **field by field**:

1. Built-in defaults.
2. Catalog defaults, if you consume a downstream tool built on anvil's engine (see
   [extensibility.md](./design/extensibility.md)). An organization can ship a fork that pre-fills `dockerfile` and its
   build inputs so its repositories only need `enabled = true`.
3. Your `anvil.toml`.

A value you set wins for that field only; every unset field falls through. The one exception is the pair
`image` / `dockerfile`: naming either one replaces the whole choice of where the image comes from, so a catalog default
cannot leave a stale `image` behind when a repository decides to build its own.

### The re-entry guard

There is exactly one indirection point, `_anvil-container-run`, and two environment variables:

| Variable | Set by | Meaning |
| --- | --- | --- |
| `ANVIL_CONTAINER` | the generated shim | Container execution is configured for this repo. |
| `ANVIL_IN_CONTAINER` | the shim, when starting the container | We are already inside; run directly. |

A guarded recipe re-invokes itself through the shim only when `ANVIL_CONTAINER=1` and `ANVIL_IN_CONTAINER` is unset.
Inside the container the second variable is set, so nested invocations run natively and the work happens exactly once —
one container per top-level command, not one per check.

The guard is applied to the three tier recipes and the group recipes only, never to individual checks. That keeps a
fast local check on the host instead of paying container start-up for it.

### What the shim does

`_anvil-container-run` resolves the image (building it if needed), mounts the repo root at `workdir`, maps your current
working directory to its in-container equivalent so relative paths keep working, mounts the configured cache volumes,
forwards environment variables matching `forward-env`, and propagates the exit code faithfully. It runs with
`--pull=never`: a locally-built tag names local content, so a miss is a bug to surface rather than an invitation to
fetch something unrelated.

Credentials are never mounted. If a recipe needs a token inside the container, forward it through `forward-env`. If the
*build* needs one, declare it in `build-secrets`, where BuildKit keeps it out of every layer.

### The generated image

`.anvil/container/Dockerfile` starts from a digest-pinned Debian base, installs `just`, rustup and PowerShell
against published checksums, then runs `just anvil-setup`.

That last step is the point: the image installs its tools by running the same recipe the checks use, from the same
generated pins. There is no second list of tools to keep in step with the first, so "the image has the right tools" is
true by construction rather than by discipline. It is also why a tool-pin bump changes the image identity —
`versions.just` is both what the image installs and part of what names it.

### CI

When container execution is enabled, the generated CI for both backends gains a job-level container so each check group
runs in the same pinned image as your local runs. When it is disabled, the generated CI is byte-identical to a build
without container support.

### Devcontainer

With `devcontainer = true`, the emitted `.devcontainer/devcontainer.json` is rendered from the same resolved settings —
same image, same workdir, same volumes — so the editor and the command line cannot disagree.

## 5. Extension points

No generic harness fits every repository. `[cluster.hooks]` names ordinary `just` recipes that anvil invokes at fixed
points, without ever modelling what they do:

| Hook | When |
| --- | --- |
| `pre-install` | Before the charts are installed. |
| `post-install` | After the charts are installed. |
| `pre-test` | After deploy, before the readiness re-check. |
| `on-failure` | On a failed attempt, before diagnostics are dumped. |

An absent hook means no invocation. A failing hook fails the step.

```toml
[cluster.hooks]
pre-install = "discover-issuer"
on-failure = "dump-extra-state"
```

Because a hook lives in your own `.just` file, it survives regeneration — you never edit a generated file to extend the
harness. The four escape valves described in the crate README apply here too.

## 6. Prerequisites and limits

- A container engine on the host: Docker is the supported path. Podman is detected best-effort by the same `engine`
  knob; there are no podman-specific recipe variants.
- On Windows, the engine must be reachable from the shell running `just`. Docker installed inside WSL is the tested
  configuration; Docker Desktop is not required.
- A repository-owned `rust-toolchain.toml`. It is both what the image installs and part of what names it, so building
  the exec image without one is not supported.
- The first build of the exec image is expected to take several minutes: it installs a toolchain and the whole pinned
  tool catalog. Later runs reuse it until an input changes.
- The cluster harness additionally needs `kind`, `kubectl`, and `helm`. Run `just anvil-cluster-preflight` to check, or
  `just anvil-cluster-bootstrap` to install pinned, checksum-verified versions.
- Images destined for the cluster must be built without provenance attestations. The generated recipes already do this;
  a hand-rolled build that emits a multi-manifest artifact will be silently refused by the cluster loader.
- No registry integration for the exec image: no push, no authentication, no promotion. It is built and consumed
  locally, which is why it needs no published artifact to exist.
- The cluster harness targets Kind. Other Kubernetes distributions are out of scope.
